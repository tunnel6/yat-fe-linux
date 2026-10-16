use std::collections::VecDeque;

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
  /// Epoch millis of last successful WireGuard handshake, or None if unknown.
  pub last_handshake_at: Option<f64>,
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
  pub hostname: String,
  pub interface_name: String,
  pub peer_count: u32,
  pub connected: bool,
  pub last_sync_at: Option<String>,
  pub peers: Vec<PeerInfo>,
  /// When the next reconnect attempt is scheduled (None if not waiting).
  pub reconnect_at: Option<String>,
}

impl SyncState {
  /// Build an empty SyncState (used when no config / Edge connection is available).
  pub fn empty() -> Self {
    Self {
      network_id: String::new(),
      network_name: String::new(),
      subnet: String::new(),
      member_id: String::new(),
      self_address: String::new(),
      hostname: String::new(),
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
