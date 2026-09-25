use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use yat_transport::EdgeClient;
use yat_wireguard::types::RuntimeStatsParams;

use crate::managers::wg::WgStatus;

/// Maximum number of log entries kept in the ring buffer.
const LOG_BUFFER_CAP: usize = 200;

// ─── Shared types ────────────────────────────────────────────────────────────

/// A single peer's information for the dashboard.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PeerInfo {
  pub hostname: String,
  pub assigned_ip: String,
  pub endpoint: String,
  pub public_key: String,
  pub presence_status: u8,
}

/// A single log entry for the dashboard log viewer.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LogEntry {
  pub timestamp: String,
  pub level: String,
  pub message: String,
}

/// Shared state for the background sync loop and web dashboard.
#[derive(serde::Serialize)]
pub struct SyncState {
  pub network_id: String,
  pub network_name: String,
  pub subnet: String,
  pub member_id: String,
  pub self_address: String,
  pub interface_name: String,
  pub peer_count: u32,
  pub connected: bool,
  pub last_sync_at: Option<String>,
  pub peers: Vec<PeerInfo>,
  /// When the next reconnect attempt is scheduled (None if not waiting).
  pub reconnect_at: Option<String>,
}

impl SyncState {
  /// Build initial SyncState from a WgStatus returned by WgManager::start().
  pub fn from_wg_status(status: &WgStatus) -> Self {
    Self {
      network_id: status.network_id.clone(),
      network_name: String::new(),
      subnet: String::new(),
      member_id: status.member_id.clone(),
      self_address: status.self_address.clone(),
      interface_name: status.interface_name.clone(),
      peer_count: status.peer_count,
      connected: true,
      last_sync_at: None,
      peers: Vec::new(),
      reconnect_at: None,
    }
  }

  /// Build an empty SyncState (used when no config / Edge connection is available).
  pub fn empty() -> Self {
    Self {
      network_id: String::new(),
      network_name: String::new(),
      subnet: String::new(),
      member_id: String::new(),
      self_address: String::new(),
      interface_name: String::new(),
      peer_count: 0,
      connected: false,
      last_sync_at: None,
      peers: Vec::new(),
      reconnect_at: None,
    }
  }
}

/// Thread-safe ring buffer for log entries.
pub struct LogBuffer {
  entries: VecDeque<LogEntry>,
}

impl LogBuffer {
  pub fn new() -> Self {
    Self {
      entries: VecDeque::with_capacity(LOG_BUFFER_CAP),
    }
  }

  pub fn push(&mut self, level: &str, message: String) {
    if self.entries.len() >= LOG_BUFFER_CAP {
      self.entries.pop_front();
    }
    self.entries.push_back(LogEntry {
      timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
      level: level.to_string(),
      message,
    });
  }

  pub fn entries(&self) -> &VecDeque<LogEntry> {
    &self.entries
  }
}

/// Maximum number of consecutive disconnect checks before returning from sync loop.
const MAX_DISCONNECT_CYCLES: u32 = 2;

/// Background sync loop: periodically refreshes endpoints and peer config.
///
/// Takes an `EdgeClient` (already connected + authenticated) and a shared
/// `SyncState` that the web dashboard reads from.
///
/// Returns when the Edge connection is lost (after `MAX_DISCONNECT_CYCLES`
/// consecutive checks), so the caller can trigger reconnection.
pub async fn run_sync_loop(
  edge_client: Arc<EdgeClient>,
  state: Arc<Mutex<SyncState>>,
  log_buffer: Arc<Mutex<LogBuffer>>,
  network_id: String,
  storage_dir: String,
) {
  info!("Sync loop started for network {network_id}.");

  let mut disconnect_cycles: u32 = 0;

  loop {
    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

    // Check if Edge is still connected
    if !edge_client.is_connected() {
      disconnect_cycles += 1;
      if disconnect_cycles >= MAX_DISCONNECT_CYCLES {
        warn!("Edge connection lost (confirmed after {} checks). Returning for reconnect.", disconnect_cycles);
        let mut s = state.lock().await;
        s.connected = false;
        log_buffer.lock().await.push("WARN", "Edge connection lost".into());
        return;
      }
      warn!("Edge connection check failed ({}/{}), will re-check.", disconnect_cycles, MAX_DISCONNECT_CYCLES);
      continue;
    }

    // Reset disconnect counter on successful check
    disconnect_cycles = 0;

    // Refresh network snapshot
    match edge_client.request_wg_get_network(&network_id).await {
      Ok(response) if response.success => {
        if let Some(snapshot) = response.snapshot {
          let peers: Vec<PeerInfo> = snapshot
            .members
            .iter()
            .filter(|m| !m.is_current_device && !m.public_key.is_empty())
            .map(|m| PeerInfo {
              hostname: m.hostname.clone(),
              assigned_ip: m.assigned_ip.clone(),
              endpoint: m.observed_endpoint.clone(),
              public_key: m.public_key.clone(),
              presence_status: m.presence_status,
            })
            .collect();
          let peer_count = peers.len() as u32;

          let mut s = state.lock().await;
          s.network_name = snapshot.network.name.clone();
          s.subnet = snapshot.network.subnet.clone();
          s.peer_count = peer_count;
          s.peers = peers;
          s.last_sync_at = Some(chrono::Local::now().to_rfc3339());
          info!("Sync: {peer_count} peers, network healthy.");
          log_buffer.lock().await.push("INFO", format!("Sync: {peer_count} peers, network healthy."));
        }
      }
      Ok(response) => {
        warn!("Sync: get_network failed: {} (code: {})", response.error_message, response.error_code);
        log_buffer.lock().await.push("WARN", format!("Sync failed: {}", response.error_message));
      }
      Err(e) => {
        warn!("Sync: get_network error: {e}");
        log_buffer.lock().await.push("ERROR", format!("Sync error: {e}"));
      }
    }

    // Refresh live stats (non-fatal if unavailable)
    // Blocking call: must run in spawn_blocking to avoid blocking the worker thread
    let stats_dir = storage_dir.clone();
    let stats_nid = network_id.clone();
    let _ = tokio::task::spawn_blocking(move || {
      yat_wireguard::stats::get_runtime_stats(RuntimeStatsParams {
        storage_dir: stats_dir,
        network_id: stats_nid,
      })
    }).await;
  }
}
