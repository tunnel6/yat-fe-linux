use crate::{
  client_owner::{self, Command, Handle, OwnerLock},
  daemon::runtime::{Runtime, Shared},
};
use anyhow::{Context, Result};
use std::{
  os::unix::thread::JoinHandleExt,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
};

pub async fn run(listen: &str) -> Result<()> {
  let owner = OwnerLock::acquire(&crate::config::config_dir())?
    .context("Another client owns this configuration; use its local command interface")?;
  run_owned(owner, Some(listen)).await
}

pub(crate) async fn run_owned(owner: OwnerLock, listen: Option<&str>) -> Result<()> {
  let shared = Shared::new();
  let (handle, mut supervisor) =
    client_owner::supervisor::spawn(Runtime::new(Some(shared.clone())));
  let mut services = tokio::task::JoinSet::new();
  // Install the notifier even if later socket/Web startup fails, so a second
  // signal never bypasses a failed drain and the retained process lock.
  let signals = SignalNotifier::start(handle.clone());
  let mut failure = None;
  match client_owner::serve(&owner, handle.clone()) {
    Ok(ipc) => {
      services.spawn(async move { ipc.await? });
    }
    Err(error) => {
      failure = Some(error);
      handle.shutdown();
    }
  }
  if let Some(listen) = listen {
    if !handle.is_stopping() {
      let listen = listen.to_owned();
      let state = crate::web::server::AppState {
        sync: shared.sync.clone(),
        logs: shared.logs.clone(),
        config: shared.config.clone(),
        cert_manager: shared.cert_manager.clone(),
        client_owner: handle.clone(),
      };
      services.spawn(async move { crate::web::server::start(&listen, state).await });
    }
  }
  let mut cert_shutdown = handle.shutdown_receiver();
  services.spawn(async move {
    loop {
      if *cert_shutdown.borrow() { return Ok(()); }
      tokio::select! {
        biased;
        _ = cert_shutdown.changed() => return Ok(()),
        _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {
          if shared.cert_manager.needs_renewal() {
            shared.logs.lock().await.push("WARN", "Certificate renewal needed; re-import from desktop app".into());
          }
        }
      }
    }
  });
  let initial = handle.clone();
  let configured = crate::config::load_config().is_ok();
  let startup = tokio::spawn(async move {
    if configured && !initial.is_stopping() {
      match initial.request(Command::Start {}).await {
        Ok(reply) if !reply.success => {
          tracing::warn!("Initial connection failed: {}", reply.message)
        }
        Err(error) => tracing::warn!("Initial connection unavailable: {error:#}"),
        _ => (),
      }
    }
  });
  let supervisor_result = tokio::select! {
    result = &mut supervisor => result,
    service = services.join_next() => {
      if !handle.is_stopping() {
        failure = Some(match service {
          Some(Ok(Err(error))) => error,
          Some(Err(error)) => error.into(),
          _ => anyhow::anyhow!("Client owner service stopped unexpectedly"),
        });
      }
      handle.shutdown();
      supervisor.await
    }
  };
  if let Err(error) = supervisor_result {
    handle.shutdown();
    tracing::error!("Supervisor failed; retaining process lock and runtime: {error}");
    // A panic cannot prove that independent native receipt work drained.
    // Keep ownership rather than permit a replacement writer.
    std::future::pending::<()>().await;
  }
  handle.shutdown();
  if let Err(error) = startup.await {
    failure.get_or_insert(error.into());
  }
  while let Some(result) = services.join_next().await {
    if let Err(error) = result
      .context("Client owner service task failed")
      .and_then(|r| r)
    {
      failure.get_or_insert(error);
    }
  }
  signals.stop();
  drop(owner); // Only after controller drain, subscriptions, IPC and Web exit.
  match failure {
    Some(error) => Err(error),
    None => Ok(()),
  }
}

/// The signal thread never creates a runtime/engine or exits the process.
pub(crate) struct SignalNotifier {
  stop: Arc<AtomicBool>,
  thread: std::thread::JoinHandle<()>,
}
impl SignalNotifier {
  pub(crate) fn start(handle: Handle) -> Self {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::spawn(move || unsafe {
      let mut set: libc::sigset_t = std::mem::zeroed();
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::sigaddset(&mut set, libc::SIGTERM);
      loop {
        let mut signal = 0;
        let error = libc::sigwait(&set, &mut signal);
        if stopping.load(Ordering::Acquire) {
          break;
        }
        if error != 0 {
          tracing::error!("Signal notification failed: {error}");
        }
        handle.shutdown();
      }
    });
    Self { stop, thread }
  }
  pub(crate) fn stop(self) {
    self.stop.store(true, Ordering::Release);
    // Wake only the notifier thread so it can be joined on normal shutdown.
    unsafe {
      // `JoinHandleExt` exposes the platform-independent raw pthread value,
      // while libc's `pthread_t` is an integer on glibc and a pointer on some
      // musl targets. Both are the same native handle; convert at this ABI
      // boundary instead of assuming one libc representation.
      let thread: libc::pthread_t = self.thread.as_pthread_t() as libc::pthread_t;
      libc::pthread_kill(thread, libc::SIGTERM);
    }
    let _ = self.thread.join();
  }
}
