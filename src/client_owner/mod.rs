//! One process owns native writes; every command uses its serial supervisor.
mod ipc;
pub mod supervisor;

pub use ipc::{request, OwnerLock};
pub use supervisor::{Command, Handle, Reply};

pub async fn dispatch(command: Command) -> anyhow::Result<()> {
  match OwnerLock::acquire(&crate::config::config_dir())? {
    Some(owner) => match command {
      Command::Start {} => crate::cli::serve::run_owned(owner, None).await,
      command @ (Command::Leave {} | Command::Import { .. }) => {
        // Short commands use the same supervisor/drain path and hold the OS
        // lock through failure cleanup. Import alone does not start a client.
        let (handle, task) = supervisor::spawn(crate::daemon::runtime::Runtime::new(None));
        let signals = crate::cli::serve::SignalNotifier::start(handle.clone());
        let reply = handle.request(command).await;
        handle.shutdown();
        finish(task).await;
        signals.stop();
        drop(owner);
        let reply = reply?;
        anyhow::ensure!(reply.success, "{}", reply.message);
        Ok(())
      }
      Command::UpdateAddress { .. } => anyhow::bail!("No running client"),
    },
    None => {
      // A forwarding CLI owns no native work. Restore normal signal behavior
      // for this thread; terminating its receipt cannot cancel the live owner.
      unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        anyhow::ensure!(error == 0, "Cannot restore CLI signal mask: {error}");
      }
      let reply = request(&crate::config::config_dir(), &command).await?;
      if !reply.success {
        anyhow::bail!("{}", reply.message);
      }
      println!("{}", reply.message);
      Ok(())
    }
  }
}

pub(crate) use ipc::serve;

/// Never release the caller's process lock after an unproven native drain.
pub(crate) async fn finish(task: tokio::task::JoinHandle<()>) {
  if let Err(error) = task.await {
    tracing::error!("Client owner failed; process lock retained: {error}");
    std::future::pending::<()>().await;
  }
}
