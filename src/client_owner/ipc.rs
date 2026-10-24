use super::{Command, Handle, Reply};
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{
  fs::{File, OpenOptions},
  os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
  },
  path::{Path, PathBuf},
  time::Duration,
};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::{UnixListener, UnixStream},
  task::JoinSet,
};

const MAX_FRAME: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The lock file is permanent: unlinking a locked inode would allow two owners.
pub struct OwnerLock {
  _file: File,
  directory: PathBuf,
}
impl OwnerLock {
  pub fn acquire(directory: &Path) -> Result<Option<Self>> {
    std::fs::create_dir_all(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
      metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
      "Runtime directory must be owned by this user"
    );
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let file = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .mode(0o600)
      .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
      .open(directory.join("yat-fe-linux.lock"))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
      metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
      "Invalid client owner lock owner/type"
    );
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
      let error = std::io::Error::last_os_error();
      if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
      }
      return Err(error.into());
    }
    Ok(Some(Self {
      _file: file,
      directory: directory.canonicalize()?,
    }))
  }
  fn socket_path(&self) -> PathBuf {
    self.directory.join("yat-fe-linux.sock")
  }
}

async fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
  tokio::time::timeout(IO_TIMEOUT, async {
    let size = stream.read_u32().await? as usize;
    anyhow::ensure!(
      size > 0 && size <= MAX_FRAME,
      "Invalid client owner frame size"
    );
    let mut data = vec![0; size];
    stream.read_exact(&mut data).await?;
    Ok(serde_json::from_slice(&data)?)
  })
  .await
  .context("Client owner frame read timed out")?
}

async fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
  let data = serde_json::to_vec(value)?;
  anyhow::ensure!(data.len() <= MAX_FRAME, "Client owner command too large");
  tokio::time::timeout(IO_TIMEOUT, async {
    stream.write_u32(data.len() as u32).await?;
    stream.write_all(&data).await?;
    Ok(())
  })
  .await
  .context("Client owner frame write timed out")?
}

pub async fn request(directory: &Path, command: &Command) -> Result<Reply> {
  // This function is used only when flock reported an existing owner. Failure
  // never falls back to native writes or removal of that owner's socket/lock.
  let mut stream = tokio::time::timeout(
    IO_TIMEOUT,
    UnixStream::connect(directory.join("yat-fe-linux.sock")),
  )
  .await
  .context("Client owner connection timed out")?
  .context("Client owner holds the lock but its command socket is unavailable; retry later")?;
  write_frame(&mut stream, command).await?;
  // Native drain may take longer than the frame I/O timeout. Wait for the
  // header without a deadline; once it arrives, enforce the bounded frame read.
  let size = stream.read_u32().await? as usize;
  anyhow::ensure!(
    size > 0 && size <= MAX_FRAME,
    "Invalid client owner reply size"
  );
  tokio::time::timeout(IO_TIMEOUT, async {
    let mut data = vec![0; size];
    stream.read_exact(&mut data).await?;
    Ok(serde_json::from_slice(&data)?)
  })
  .await
  .context("Client owner reply body timed out")?
}

pub(crate) fn serve(
  owner: &OwnerLock,
  handle: Handle,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
  let path = owner.socket_path();
  if path.exists() {
    std::fs::remove_file(&path)?;
  }
  let listener = UnixListener::bind(&path)?;
  std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
  let mut shutdown = handle.shutdown_receiver();
  Ok(tokio::spawn(async move {
    let mut clients = JoinSet::new();
    let mut failure = None;
    loop {
      if *shutdown.borrow() {
        break;
      }
      tokio::select! {
        biased;
        _ = shutdown.changed() => break,
        Some(result) = clients.join_next(), if !clients.is_empty() => {
          if let Err(error) = result { tracing::warn!("Local command task failed: {error}"); }
        },
        accepted = listener.accept(), if clients.len() < 16 => {
          let (mut stream, _) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => { failure = Some(error); handle.shutdown(); break; }
          };
          let handle = handle.clone();
          clients.spawn(async move {
            let response = match read_frame::<Command>(&mut stream).await {
              Ok(command) => match handle.request(command).await {
                Ok(reply) => reply,
                Err(error) => Reply::from_result(Err(error)),
              },
              Err(error) => Reply::from_result(Err(error)),
            };
            let _ = write_frame(&mut stream, &response).await;
          });
        }
      }
    }
    drop(listener);
    while clients.join_next().await.is_some() {}
    std::fs::remove_file(path)?;
    match failure {
      Some(error) => Err(error.into()),
      None => Ok(()),
    }
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::client_owner::supervisor::{self, Driver, Work};
  use std::sync::atomic::{AtomicBool, AtomicUsize};
  static NEXT: AtomicUsize = AtomicUsize::new(0);
  fn directory() -> PathBuf {
    PathBuf::from("/tmp").join(format!(
      "yat-ipc-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
  }
  struct NoNative;
  impl Driver for NoNative {
    fn execute<'a>(&'a mut self, _: Command, _: &'a AtomicBool) -> Work<'a> {
      Box::pin(async { Ok(()) })
    }
    fn tick<'a>(&'a mut self, _: &'a AtomicBool) -> Work<'a> {
      Box::pin(async { Ok(()) })
    }
    fn stop(&mut self) -> Work<'_> {
      Box::pin(async { Ok(()) })
    }
  }
  #[test]
  fn child_observes_lock() {
    if let Some(directory) = std::env::var_os("YAT_TEST_LOCK_DIRECTORY") {
      assert!(OwnerLock::acquire(Path::new(&directory)).unwrap().is_none());
    }
  }
  #[tokio::test]
  async fn process_lock_survives_cross_process_conflict_and_socket_failure() {
    let directory = directory();
    let owner = OwnerLock::acquire(&directory).unwrap().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
      .args(["--exact", "client_owner::ipc::tests::child_observes_lock"])
      .env("YAT_TEST_LOCK_DIRECTORY", &directory)
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stdout)
    );
    assert!(request(&directory, &Command::Leave {}).await.is_err());
    assert!(OwnerLock::acquire(&directory).unwrap().is_none());
    drop(owner);
    assert!(OwnerLock::acquire(&directory).unwrap().is_some());
    std::fs::remove_dir_all(directory).unwrap();
  }
  #[tokio::test]
  async fn local_commands_are_framed_private_and_reject_malformed_input() {
    let directory = directory();
    let owner = OwnerLock::acquire(&directory).unwrap().unwrap();
    let (handle, actor) = supervisor::spawn(NoNative);
    let server = serve(&owner, handle.clone()).unwrap();
    assert_eq!(std::fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
      std::fs::metadata(owner.socket_path()).unwrap().mode() & 0o777,
      0o600
    );
    assert!(
      request(&directory, &Command::Leave {})
        .await
        .unwrap()
        .success
    );
    let mut stream = UnixStream::connect(owner.socket_path()).await.unwrap();
    stream.write_u32(MAX_FRAME as u32 + 1).await.unwrap();
    let reply: Reply = read_frame(&mut stream).await.unwrap();
    assert!(!reply.success);
    let mut stream = UnixStream::connect(owner.socket_path()).await.unwrap();
    write_frame(
      &mut stream,
      &serde_json::json!({"command":"start","unknown":true}),
    )
    .await
    .unwrap();
    assert!(!read_frame::<Reply>(&mut stream).await.unwrap().success);
    assert!(
      request(&directory, &Command::Start {})
        .await
        .unwrap()
        .success
    );
    handle.shutdown();
    actor.await.unwrap();
    server.await.unwrap().unwrap();
    assert!(!owner.socket_path().exists());
    assert!(OwnerLock::acquire(&directory).unwrap().is_none());
    drop(owner);
    std::fs::remove_dir_all(directory).unwrap();
  }
  #[test]
  fn rejects_symlink_lock_and_runtime_directory() {
    let directory = directory();
    let owner = OwnerLock::acquire(&directory).unwrap().unwrap();
    drop(owner);
    std::fs::remove_file(directory.join("yat-fe-linux.lock")).unwrap();
    std::os::unix::fs::symlink("/dev/null", directory.join("yat-fe-linux.lock")).unwrap();
    assert!(OwnerLock::acquire(&directory).is_err());
    let alias = directory.with_extension("alias");
    std::os::unix::fs::symlink(&directory, &alias).unwrap();
    assert!(OwnerLock::acquire(&alias).is_err());
    std::fs::remove_file(alias).unwrap();
    std::fs::remove_dir_all(directory).unwrap();
  }
}
