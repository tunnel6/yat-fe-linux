//! One connection's bounded invalidation state. Callbacks only record facts;
//! the original supervisor performs every query and native operation.
use std::sync::{
  atomic::{AtomicBool, AtomicU8, Ordering},
  Arc,
};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PendingEvent {
  None = 0,
  Refresh = 1,
  Deleted = 2,
}

pub struct NetworkEvents {
  network_id: String,
  active: AtomicBool,
  pending: AtomicU8,
  wakeup: Arc<Notify>,
}

impl NetworkEvents {
  pub fn new(network_id: String, wakeup: Arc<Notify>) -> Self {
    Self {
      network_id,
      active: AtomicBool::new(true),
      pending: AtomicU8::new(0),
      wakeup,
    }
  }

  pub fn observe(&self, network_id: &str, action: u8) {
    if !self.active.load(Ordering::Acquire) || network_id != self.network_id {
      return;
    }
    let pending = match action {
      1 | 3 => PendingEvent::Refresh, // UPSERT / RUNTIME_UPDATE
      2 => PendingEvent::Deleted,
      _ => return,
    };
    // DELETE wins over every pending progress notification. Revisions alone
    // cannot deduplicate events: different pair barriers may share a revision.
    self.pending.fetch_max(pending as u8, Ordering::AcqRel);
    self.wakeup.notify_one();
  }

  pub fn take(&self) -> PendingEvent {
    match self.pending.swap(0, Ordering::AcqRel) {
      1 => PendingEvent::Refresh,
      2 => PendingEvent::Deleted,
      _ => PendingEvent::None,
    }
  }

  /// Retain work during a failed-query cooldown without creating a busy loop.
  pub fn defer_refresh(&self) {
    self
      .pending
      .fetch_max(PendingEvent::Refresh as u8, Ordering::AcqRel);
  }

  pub fn close(&self) {
    self.active.store(false, Ordering::Release);
    self.pending.store(0, Ordering::Release);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bursts_coalesce_and_work_arriving_during_refresh_is_preserved() {
    let events = NetworkEvents::new("net".into(), Arc::new(Notify::new()));
    events.observe("other", 2);
    assert_eq!(events.take(), PendingEvent::None);
    for _ in 0..1000 {
      events.observe("net", 3);
    }
    assert_eq!(events.take(), PendingEvent::Refresh);
    assert_eq!(events.take(), PendingEvent::None);
    // The same revision can describe a new phase arriving during the GET.
    events.observe("net", 3);
    assert_eq!(events.take(), PendingEvent::Refresh);
  }

  #[test]
  fn deletion_wins_over_progress_and_retry() {
    let events = NetworkEvents::new("net".into(), Arc::new(Notify::new()));
    events.observe("net", 2);
    events.observe("net", 1);
    events.defer_refresh();
    assert_eq!(events.take(), PendingEvent::Deleted);
  }

  #[test]
  fn retired_connection_cannot_delete_a_replacement() {
    let wakeup = Arc::new(Notify::new());
    let old = NetworkEvents::new("net".into(), wakeup.clone());
    old.close();
    let current = NetworkEvents::new("net".into(), wakeup);
    old.observe("net", 2);
    assert_eq!(old.take(), PendingEvent::None);
    assert_eq!(current.take(), PendingEvent::None);
    current.observe("net", 3);
    assert_eq!(current.take(), PendingEvent::Refresh);
  }
}
