//! Supervisor wakeup adapter over the shared event manager. No topology cache.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Notify;
use yat_network::events::{EventFilter, EventSubscription, NetworkEventKind, NetworkEventManager};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingEvent {
    None,
    Refresh,
    Deleted,
}
pub struct NetworkEvents {
    events: Arc<NetworkEventManager>,
    subscription: std::sync::Mutex<EventSubscription>,
    edge_id: String,
    network_id: String,
    retry: AtomicBool,
    wake_task: tokio::task::JoinHandle<()>,
}
impl NetworkEvents {
    pub fn new(
        events: Arc<NetworkEventManager>,
        edge_id: String,
        network_id: String,
        wakeup: Arc<Notify>,
    ) -> Self {
        let filter = EventFilter {
            edge_id: Some(edge_id.clone()),
            network_id: Some(network_id.clone()),
            class: Some(yat_network::events::EventClass::Topology),
        };
        let subscription = events.subscribe_with_snapshot(filter.clone());
        let mut wake = events.subscribe_with_snapshot(filter.clone());
        let wake_events = events.clone();
        let wake_task = tokio::spawn(async move {
            loop {
                match wake.recv().await {
                    Ok(_) => wakeup.notify_one(),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        wake = wake_events.subscribe_with_snapshot(filter.clone());
                        wakeup.notify_one();
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            events,
            subscription: std::sync::Mutex::new(subscription),
            edge_id,
            network_id,
            retry: AtomicBool::new(false),
            wake_task,
        }
    }
    pub fn take(&self) -> PendingEvent {
        let mut result = if self.retry.swap(false, Ordering::AcqRel) {
            PendingEvent::Refresh
        } else {
            PendingEvent::None
        };
        let mut subscription = self.subscription.lock().unwrap();
        loop {
            match subscription.try_recv() {
                Ok(event) => match event.kind {
                    NetworkEventKind::Invalidated(event) if event.action == 2 => {
                        return PendingEvent::Deleted
                    }
                    NetworkEventKind::Invalidated(_) => result = PendingEvent::Refresh,
                    _ => (),
                },
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    *subscription = self.events.subscribe_with_snapshot(EventFilter {
                        edge_id: Some(self.edge_id.clone()),
                        network_id: Some(self.network_id.clone()),
                        class: Some(yat_network::events::EventClass::Topology),
                    });
                    return if subscription.removed_networks.is_empty() {
                        PendingEvent::Refresh
                    } else {
                        PendingEvent::Deleted
                    };
                }
                Err(_) => return result,
            }
        }
    }
    pub fn defer_refresh(&self) {
        self.retry.store(true, Ordering::Release);
    }
    pub fn close(&self) {
        self.wake_task.abort();
    }
}
impl Drop for NetworkEvents {
    fn drop(&mut self) {
        self.wake_task.abort();
    }
}
