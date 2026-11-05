use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
  future::Future,
  pin::Pin,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch, Notify};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
  Start {},
  Reconnect {},
  Leave {},
  Import { content: String },
  Pair { edge: String, token: String },
  UpdateAddress { new_ip: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
  pub success: bool,
  pub message: String,
}
impl Reply {
  pub fn from_result(result: Result<()>) -> Self {
    match result {
      Ok(()) => Self {
        success: true,
        message: "Operation completed".into(),
      },
      Err(error) => Self {
        success: false,
        message: format!("{error:#}"),
      },
    }
  }
}

pub type Work<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
pub trait Driver: Send + 'static {
  fn wakeup(&self) -> Option<Arc<Notify>> {
    None
  }
  fn execute<'a>(&'a mut self, command: Command, stopping: &'a AtomicBool) -> Work<'a>;
  fn tick<'a>(&'a mut self, stopping: &'a AtomicBool) -> Work<'a>;
  fn stop(&mut self) -> Work<'_>;
}

pub(crate) struct Envelope {
  command: Command,
  reply: oneshot::Sender<Reply>,
}
#[derive(Clone)]
pub struct Handle {
  tx: mpsc::Sender<Envelope>,
  shutdown: watch::Sender<bool>,
  stopping: Arc<AtomicBool>,
}
impl Handle {
  pub async fn request(&self, command: Command) -> Result<Reply> {
    anyhow::ensure!(
      !self.stopping.load(Ordering::Acquire),
      "Client is shutting down"
    );
    let (reply, receipt) = oneshot::channel();
    self
      .tx
      .try_send(Envelope { command, reply })
      .map_err(|_| anyhow::anyhow!("Client owner command queue unavailable or full"))?;
    // Dropping this receipt never cancels the supervisor's operation.
    receipt
      .await
      .map_err(|_| anyhow::anyhow!("Client owner stopped before replying"))
  }
  pub fn shutdown(&self) {
    self.stopping.store(true, Ordering::Release);
    self.shutdown.send_replace(true);
  }
  pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
    self.shutdown.subscribe()
  }
  pub fn is_stopping(&self) -> bool {
    self.stopping.load(Ordering::Acquire)
  }
}

pub fn spawn<D: Driver>(mut driver: D) -> (Handle, tokio::task::JoinHandle<()>) {
  let wakeup = driver.wakeup().unwrap_or_default();
  let (tx, mut rx) = mpsc::channel::<Envelope>(16);
  let (shutdown, mut stop_rx) = watch::channel(false);
  let stopping = Arc::new(AtomicBool::new(false));
  let handle = Handle {
    tx,
    shutdown,
    stopping: stopping.clone(),
  };
  let task = tokio::spawn(async move {
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    timer.tick().await;
    loop {
      tokio::select! {
        biased;
        _ = stop_rx.changed() => break,
        envelope = rx.recv() => match envelope {
          Some(envelope) => {
            let result = if stopping.load(Ordering::Acquire) {
              Err(anyhow::anyhow!("Client is shutting down"))
            } else { driver.execute(envelope.command, &stopping).await };
            let _ = envelope.reply.send(Reply::from_result(result));
          }
          None => break,
        },
        _ = async {
          tokio::select! {
            _ = timer.tick() => (),
            _ = wakeup.notified() => (),
          }
        } => {
          if stopping.load(Ordering::Acquire) { break; }
          if let Err(error) = driver.tick(&stopping).await { tracing::warn!("Client owner refresh failed: {error:#}"); }
        }
      }
    }
    stopping.store(true, Ordering::Release);
    rx.close();
    while let Some(envelope) = rx.recv().await {
      let _ = envelope.reply.send(Reply::from_result(Err(anyhow::anyhow!(
        "Client is shutting down"
      ))));
    }
    // Retain the driver, controller, process lock (in the caller), and runtime
    // on a failed drain. Never release ownership while native work may remain.
    loop {
      match driver.stop().await {
        Ok(()) => break,
        Err(error) => {
          tracing::error!("Shutdown drain failed; ownership retained: {error:#}");
          tokio::time::sleep(Duration::from_secs(1)).await;
        }
      }
    }
  });
  (handle, task)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokio::sync::Semaphore;

  struct Fake {
    events: mpsc::UnboundedSender<&'static str>,
    apply: Arc<Semaphore>,
    drain: Arc<Semaphore>,
    fail_drain: Arc<AtomicBool>,
  }
  impl Driver for Fake {
    fn execute<'a>(&'a mut self, command: Command, _: &'a AtomicBool) -> Work<'a> {
      Box::pin(async move {
        match command {
          Command::Start {} => {
            self.events.send("apply-start").unwrap();
            self.apply.acquire().await?.forget();
            self.events.send("apply-done").unwrap();
          }
          Command::Import { .. } => {
            self.stop().await?;
            self.events.send("new-generation").unwrap();
          }
          _ => (),
        }
        Ok(())
      })
    }
    fn tick<'a>(&'a mut self, _: &'a AtomicBool) -> Work<'a> {
      Box::pin(async { Ok(()) })
    }
    fn stop(&mut self) -> Work<'_> {
      Box::pin(async move {
        self.events.send("drain-start").unwrap();
        if self.fail_drain.swap(false, Ordering::AcqRel) {
          self.events.send("drain-failed").unwrap();
          anyhow::bail!("injected teardown failure");
        }
        self.drain.acquire().await?.forget();
        self.events.send("drain-done").unwrap();
        Ok(())
      })
    }
  }
  fn fixture() -> (Fake, mpsc::UnboundedReceiver<&'static str>) {
    let (events, rx) = mpsc::unbounded_channel();
    (
      Fake {
        events,
        apply: Arc::new(Semaphore::new(0)),
        drain: Arc::new(Semaphore::new(0)),
        fail_drain: Arc::new(AtomicBool::new(false)),
      },
      rx,
    )
  }
  async fn event(rx: &mut mpsc::UnboundedReceiver<&'static str>, expected: &str) {
    assert_eq!(
      tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap(),
      expected
    );
  }

  #[tokio::test]
  async fn network_notification_wakes_original_driver_before_periodic_tick() {
    struct Woken {
      wakeup: Arc<Notify>,
      events: mpsc::UnboundedSender<&'static str>,
    }
    impl Driver for Woken {
      fn wakeup(&self) -> Option<Arc<Notify>> {
        Some(self.wakeup.clone())
      }
      fn execute<'a>(&'a mut self, _: Command, _: &'a AtomicBool) -> Work<'a> {
        Box::pin(async { Ok(()) })
      }
      fn tick<'a>(&'a mut self, _: &'a AtomicBool) -> Work<'a> {
        Box::pin(async move {
          self.events.send("tick")?;
          Ok(())
        })
      }
      fn stop(&mut self) -> Work<'_> {
        Box::pin(async move {
          self.events.send("stop")?;
          Ok(())
        })
      }
    }
    let wakeup = Arc::new(Notify::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (handle, task) = spawn(Woken {
      wakeup: wakeup.clone(),
      events: tx,
    });
    wakeup.notify_one();
    assert_eq!(
      tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .unwrap(),
      Some("tick")
    );
    handle.shutdown();
    wakeup.notify_one();
    task.await.unwrap();
    assert_eq!(rx.recv().await, Some("stop"));
    assert_eq!(rx.recv().await, None);
  }

  #[tokio::test]
  async fn cancelled_receipt_does_not_cancel_apply_or_allow_early_replacement() {
    let (driver, mut events) = fixture();
    let (apply, drain) = (driver.apply.clone(), driver.drain.clone());
    let (handle, task) = spawn(driver);
    let first = tokio::spawn({
      let h = handle.clone();
      async move { h.request(Command::Start {}).await }
    });
    event(&mut events, "apply-start").await;
    first.abort(); // Only the requester's receipt is cancelled.
    let replacement = tokio::spawn({
      let h = handle.clone();
      async move {
        h.request(Command::Import {
          content: "new".into(),
        })
        .await
      }
    });
    tokio::task::yield_now().await;
    assert!(events.try_recv().is_err());
    apply.add_permits(1);
    event(&mut events, "apply-done").await;
    event(&mut events, "drain-start").await;
    assert!(!replacement.is_finished());
    assert!(events.try_recv().is_err());
    drain.add_permits(1);
    event(&mut events, "drain-done").await;
    event(&mut events, "new-generation").await;
    assert!(replacement.await.unwrap().unwrap().success);
    handle.shutdown();
    drain.add_permits(1);
    task.await.unwrap();
  }

  #[tokio::test]
  async fn shutdown_notification_rejects_queue_and_retains_failed_drain() {
    let (driver, mut events) = fixture();
    let (apply, drain) = (driver.apply.clone(), driver.drain.clone());
    driver.fail_drain.store(true, Ordering::Release);
    let (handle, task) = spawn(driver);
    let first = tokio::spawn({
      let h = handle.clone();
      async move { h.request(Command::Start {}).await }
    });
    event(&mut events, "apply-start").await;
    let (reply, receipt) = oneshot::channel();
    handle
      .tx
      .try_send(Envelope {
        command: Command::Import {
          content: "new".into(),
        },
        reply,
      })
      .unwrap();
    handle.shutdown(); // The signal thread uses precisely this notification.
    assert!(handle.request(Command::Start {}).await.is_err());
    assert!(!task.is_finished());
    apply.add_permits(1);
    event(&mut events, "apply-done").await;
    assert!(first.await.unwrap().unwrap().success);
    assert!(!receipt.await.unwrap().success);
    event(&mut events, "drain-start").await;
    event(&mut events, "drain-failed").await;
    assert!(!task.is_finished());
    event(&mut events, "drain-start").await;
    assert!(events.try_recv().is_err());
    drain.add_permits(1);
    event(&mut events, "drain-done").await;
    task.await.unwrap();
    assert!(events.try_recv().is_err());
  }

  #[tokio::test]
  async fn queue_is_bounded_while_apply_is_in_flight() {
    let (driver, mut events) = fixture();
    let (apply, drain) = (driver.apply.clone(), driver.drain.clone());
    let (handle, task) = spawn(driver);
    let first = tokio::spawn({
      let h = handle.clone();
      async move { h.request(Command::Start {}).await }
    });
    event(&mut events, "apply-start").await;
    let mut receipts = Vec::new();
    for _ in 0..16 {
      let (reply, receipt) = oneshot::channel();
      handle
        .tx
        .try_send(Envelope {
          command: Command::Leave {},
          reply,
        })
        .unwrap();
      receipts.push(receipt);
    }
    assert!(handle.request(Command::Start {}).await.is_err());
    handle.shutdown();
    apply.add_permits(1);
    drain.add_permits(1);
    first.await.unwrap().unwrap();
    for receipt in receipts {
      assert!(!receipt.await.unwrap().success);
    }
    task.await.unwrap();
  }
}
