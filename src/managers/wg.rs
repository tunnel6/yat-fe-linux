//! WireGuard network lifecycle manager.
//!
//! `WgManager` is the **business orchestrator** — it coordinates Edge connection,
//! WireGuard engine operations, certificate loading, and state persistence to
//! provide a clean start/stop/status API for the CLI and web layers.
//!
//! # Architecture
//! ```text
//! cli / web
//!   ↓
//! WgManager (this module)
//!   ├── EdgeClient  (transport crate — channel layer)
//!   ├── CertManager (cert loading, paths)
//!   └── yat-wireguard engine (staging, applied, identity, stats)
//! ```

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

use yat_network::WgSyncController;
use yat_transport::{EdgeClient, EdgeClientConfig, WGNetworkRuntimeSnapshotData};
use yat_wireguard::types::LocalIdentityParams;

// ─── EdgeJoinContext ─────────────────────────────────────────────────────────

/// Intermediate result from [`WgManager::connect_edge`].
///
/// Contains everything needed to later apply the WireGuard interface
/// (or to keep the Edge connection alive for dashboard / sync even when
/// WG interface creation fails).
pub struct EdgeJoinContext {
  pub edge: Arc<EdgeClient>,
  pub network_id: String,
  pub storage_dir: String,
  pub member_id: String,
  pub client_id: String,
  pub self_address: String,
  pub snapshot: WGNetworkRuntimeSnapshotData,
}

use crate::config::{self, DeviceIdentity};
use crate::managers::cert::CertManager;

// ─── WgManager ───────────────────────────────────────────────────────────────

/// WireGuard network lifecycle manager.
///
/// Owns the Edge connection (when active) and coordinates all WG operations.
pub struct WgManager {
  config: config::AppConfig,
  certs: CertManager,
  edge: Option<Arc<EdgeClient>>,
  network_id: Option<String>,
  member_id: Option<String>,
}

impl WgManager {
  /// Create a new WgManager by loading config from the default location.
  pub fn new() -> Result<Self> {
    let config = config::load_config()?;
    Self::from_config(config)
  }

  /// Create a new WgManager from an already-loaded config.
  pub fn from_config(config: config::AppConfig) -> Result<Self> {
    let config_dir = config::config_dir();
    let certs = CertManager::new(&config_dir);
    Ok(Self {
      config,
      certs,
      edge: None,
      network_id: None,
      member_id: None,
    })
  }

  /// Access the Edge client (available after `start()` or `connect_edge()`).
  #[allow(dead_code)]
  pub fn edge_client(&self) -> Option<Arc<EdgeClient>> {
    self.edge.clone()
  }

  /// Current network ID (available after `start()`).
  #[allow(dead_code)]
  pub fn network_id(&self) -> Option<&str> {
    self.network_id.as_deref()
  }

  /// Current member ID (available after `start()`).
  #[allow(dead_code)]
  pub fn member_id(&self) -> Option<&str> {
    self.member_id.as_deref()
  }

  /// Reference to the cert manager.
  pub fn certs(&self) -> &CertManager {
    &self.certs
  }

  // ─── connect_edge: Edge connection + network join ──────────────────────

  /// Phase 1 of the connection flow:
  /// 1. Load certs + device identity
  /// 2. EdgeClient → connect (mTLS) → authenticate
  /// 3. Get/create WG identity
  /// 4. Join WG network (creates member record on Edge)
  /// 5. Fetch snapshot (post-join, passes isNetworkOwnerOrMember check)
  ///
  /// Returns an [`EdgeJoinContext`] that carries everything the caller needs
  /// to later apply the WireGuard interface (phase 2) **or** to keep the
  /// Edge connection alive for dashboard / sync even when WG fails.
  pub async fn connect_edge(&mut self) -> Result<EdgeJoinContext> {
    let device_identity = DeviceIdentity {
      device_id: self.config.identity.device_id.clone(),
      hostname: self.config.identity.hostname.clone(),
    };
    let hostname = config::get_hostname();
    let storage_dir = self.certs.wg_storage_dir();
    let storage_dir_str = storage_dir.to_string_lossy().to_string();

    let network_id = self.config.network_id().ok_or_else(|| {
      anyhow::anyhow!("No network configured. Run `yat-cli import <share.json>` first.")
    })?;

    info!(
      "Edge: {}:{} | Device: {} | Network: {}",
      self.config.edge.host, self.config.edge.port, device_identity.device_id, network_id,
    );

    // ── Edge connection ──────────────────────────────────────────────────
    let edge = Arc::new(self.create_edge_client(&device_identity, &hostname).await?);
    // Publish ownership before any later await can fail. The supervisor retains
    // this manager and will disconnect it on every partial-startup failure.
    self.edge = Some(edge.clone());
    self.network_id = Some(network_id.clone());

    // ── WireGuard identity ───────────────────────────────────────────────
    info!("Loading WireGuard identity...");
    let id_params = LocalIdentityParams {
      storage_dir: storage_dir_str.clone(),
      preferred_device_id: Some(device_identity.device_id.clone()),
      hostname: Some(hostname.clone()),
    };
    let identity = tokio::task::spawn_blocking(move || {
      yat_wireguard::identity::get_or_create_local_identity(id_params)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?
    .map_err(|e| anyhow::anyhow!("Failed to get/create WG identity: {e}"))?;
    let client_id = identity.client_id.clone();
    info!(
      "WG public key: {}, client_id: {}",
      identity.public_key, client_id
    );

    // ── Join network FIRST ─────────────────────────────────────────────
    // Must join before fetching the snapshot: the Edge WG_GET_NETWORK handler
    // enforces isNetworkOwnerOrMember — a new device that hasn't joined yet
    // will get "network not found" even with a valid scoped certificate.
    let invite_token = self.config.invite_token().unwrap_or_default();
    let (member_id, assigned_ip) = self
      .resolve_membership(&edge, &network_id, &identity, &hostname, &invite_token)
      .await?;

    self.member_id = Some(member_id.clone());

    // ── Fetch snapshot (post-join) ─────────────────────────────────────
    let snapshot = {
      let refresh = edge
        .request_wg_get_network(&network_id)
        .await
        .context("Failed to refresh network snapshot")?;
      anyhow::ensure!(
        refresh.success,
        "Initial snapshot request rejected: {}",
        refresh.error_message
      );
      refresh
        .snapshot
        .ok_or_else(|| anyhow::anyhow!("Network snapshot is empty"))?
    };

    validate_snapshot_identity(&snapshot, &network_id, &member_id, &identity.public_key)?;
    let raw_ip = if assigned_ip.is_empty() {
      snapshot
        .members
        .iter()
        .find(|m| m.is_current_device)
        .map(|m| m.assigned_ip.clone())
        .unwrap_or_default()
    } else {
      assigned_ip
    };

    let self_address = if raw_ip.contains('/') {
      raw_ip
    } else {
      let prefix = snapshot.network.subnet.split('/').nth(1).unwrap_or("32");
      format!("{raw_ip}/{prefix}")
    };

    Ok(EdgeJoinContext {
      edge,
      network_id: network_id.to_string(),
      storage_dir: storage_dir_str,
      member_id,
      client_id,
      self_address,
      snapshot,
    })
  }

  /// Apply the initial snapshot through the shared network controller. This is
  /// the daemon-safe entry point: staging, native apply, retries, and teardown
  /// all use the controller's NetworkSyncGate.
  pub async fn apply_wg_via_controller(
    &mut self,
    ctx: EdgeJoinContext,
    controller: Arc<WgSyncController>,
  ) -> Result<WgStatus> {
    let EdgeJoinContext {
      edge,
      network_id,
      storage_dir,
      member_id,
      client_id: _,
      self_address: _,
      snapshot,
    } = ctx;
    controller.mark_joined(&network_id).await;
    let result = controller
      .handle_typed_snapshot(&network_id, &snapshot)
      .await?
      .ok_or_else(|| anyhow::anyhow!("initial WG snapshot produced no runtime result"))?;
    let applied = yat_wireguard::applied::get_applied_runtime_state(
      yat_wireguard::types::AppliedRuntimeStateParams {
        storage_dir: storage_dir.clone(),
        network_id: network_id.clone(),
      },
    )?
    .ok_or_else(|| anyhow::anyhow!("controller applied no WireGuard runtime"))?;
    validate_initial_apply(
      &snapshot,
      &network_id,
      &member_id,
      result.applied,
      &result.local_config.interface_name,
      result.local_config.self_address.as_deref(),
      result.local_config.peer_count,
      &applied,
    )?;
    self.network_id = Some(network_id.clone());
    self.member_id = Some(member_id.clone());
    self.edge = Some(edge);
    let peer_count = applied.peer_count as u32;
    save_state(
      &network_id,
      &member_id,
      &applied.self_address,
      &applied.resolved_interface_name,
      peer_count,
    )?;
    Ok(WgStatus {
      connected: true,
      network_id,
      member_id,
      interface_name: applied.resolved_interface_name,
      interface_driver: applied.interface_driver,
      self_address: applied.self_address,
      peer_count,
    })
  }

  /// Stop control traffic before the owning supervisor drains native work.
  pub async fn disconnect_owned(&mut self, leave: bool) -> Result<()> {
    if let Some(edge) = self.edge.as_ref() {
      if leave {
        if let (Some(network_id), Some(member_id)) = (&self.network_id, &self.member_id) {
          // A stalled remote leave must not prevent local resource cleanup.
          let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            edge.leave_wg_network(network_id, member_id, false),
          )
          .await;
        }
      }
      edge.disconnect_and_wait().await?;
    }
    self.edge = None;
    self.network_id = None;
    self.member_id = None;
    Ok(())
  }

  pub async fn leave_saved_member(&mut self, network_id: &str, member_id: &str) {
    let identity = DeviceIdentity {
      device_id: self.config.identity.device_id.clone(),
      hostname: self.config.identity.hostname.clone(),
    };
    if let Ok(edge) = self
      .create_edge_client(&identity, &config::get_hostname())
      .await
    {
      let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        edge.leave_wg_network(network_id, member_id, false),
      )
      .await;
      let _ = edge.disconnect_and_wait().await;
    }
  }

  // ─── status: aggregate state ────────────────────────────────────────────

  /// Get aggregated network status (reads saved state + engine state).
  /// Does NOT require an active Edge connection.
  pub fn status(&self) -> Result<WgStatus> {
    let state = load_state()
      .ok_or_else(|| anyhow::anyhow!("No active network. Run `yat-cli start` to connect."))?;

    let storage_dir_str = self.certs.wg_storage_dir().to_string_lossy().to_string();

    // Try to get applied state from engine
    let (iface_name, iface_driver) = match yat_wireguard::applied::get_applied_runtime_state(
      yat_wireguard::types::AppliedRuntimeStateParams {
        storage_dir: storage_dir_str.clone(),
        network_id: state.network_id.clone(),
      },
    ) {
      Ok(Some(applied)) => (applied.resolved_interface_name, applied.interface_driver),
      _ => (state.interface_name.clone(), "unknown".to_string()),
    };

    Ok(WgStatus {
      connected: self.edge.is_some(),
      network_id: state.network_id,
      member_id: state.member_id,
      interface_name: iface_name,
      interface_driver: iface_driver,
      self_address: state.self_address,
      peer_count: state.peer_count,
    })
  }

  // ─── Private helpers ────────────────────────────────────────────────────

  /// Create EdgeClient, connect (mTLS), and authenticate.
  async fn create_edge_client(
    &self,
    device_identity: &DeviceIdentity,
    hostname: &str,
  ) -> Result<EdgeClient> {
    let (ca_cert, client_cert, client_key) = self.certs.load_edge_certs()?;

    let edge_config = EdgeClientConfig {
      host: self.config.edge.host.clone(),
      port: self.config.edge.port,
      ca_cert,
      client_cert: Some(client_cert),
      client_key: Some(client_key),
      heartbeat_interval_secs: 30,
      quic_keepalive_secs: 15,
      quic_idle_timeout_secs: 300,
      edge_id: self.config.edge.edge_id.clone(),
      server_name: self.config.edge.server_name.clone(),
    };

    let client = EdgeClient::new(edge_config);

    // Connect
    info!("Connecting to Edge...");
    client
      .connect()
      .await
      .context("Failed to connect to Edge")?;
    info!("Edge connection established (mTLS).");

    // Authenticate
    info!("Authenticating...");
    // S2a-A4: advertise path-coordination only when the platform engine supports it.
    let path_activation = yat_wireguard::platform::inspect_platform_apply_support().supported;
    let auth_result = client
      .authenticate(
        "", // no user_id — invite-token cert has network scope
        &device_identity.device_id,
        hostname,
        env!("CARGO_PKG_VERSION"),
        &yat_network::wg::sync_controller::product::capabilities(vec!["wg".to_string()], path_activation),
        1,
        0,
        &[],
        &[],
      )
      .await;
    let auth_result = match auth_result {
      Ok(result) => result,
      Err(error) => {
        let _ = client.disconnect_and_wait().await;
        return Err(error).context("Authentication failed");
      }
    };

    if !auth_result.success {
      let _ = client.disconnect_and_wait().await;
      anyhow::bail!(
        "Authentication rejected: {} (code: {})",
        auth_result.error_message,
        auth_result.error_code
      );
    }
    info!("Authenticated. Session: {}", auth_result.session_id);

    Ok(client)
  }

  /// Resolve network membership by (re-)joining the network.
  ///
  /// Always sends `join_wg_network` with the real WireGuard public key,
  /// even when the member is already joined.  The edge's `JoinNetwork`
  /// handler is idempotent — if the member already exists, it performs a
  /// public-key upgrade (placeholder → real key) in place.
  ///
  /// This avoids a timing issue on first startup where the snapshot
  /// returned by `get_network` may still contain placeholder public keys
  /// for other members that haven't connected yet.
  async fn resolve_membership(
    &self,
    edge: &EdgeClient,
    network_id: &str,
    identity: &yat_wireguard::types::LocalIdentity,
    hostname: &str,
    invite_token: &str,
  ) -> Result<(String, String)> {
    info!("Joining network (sending real public key for upgrade)...");
    let join_result = edge
      .join_wg_network(
        network_id,
        &identity.client_id,
        &identity.public_key,
        hostname,
        invite_token,
      )
      .await
      .context("Failed to join network")?;
    if !join_result.success {
      anyhow::bail!(
        "Join failed: {} (code: {})",
        join_result.error_message,
        join_result.error_code
      );
    }
    info!(
      "Joined network. Member: {} IP: {}",
      join_result.member_id, join_result.assigned_ip
    );
    Ok((join_result.member_id, join_result.assigned_ip))
  }
}

pub(crate) fn validate_snapshot_identity(
  snapshot: &WGNetworkRuntimeSnapshotData,
  network_id: &str,
  member_id: &str,
  public_key: &str,
) -> Result<()> {
  let mut local = snapshot
    .members
    .iter()
    .filter(|member| member.is_current_device);
  let member = local.next().context("Snapshot has no local member")?;
  anyhow::ensure!(
    local.next().is_none()
      && member.network_id == network_id
      && member.member_id == member_id
      && member.public_key == public_key
      && is_valid_wireguard_key(public_key)
      && !member.assigned_ip.is_empty()
      && snapshot.network.network_id == network_id,
    "Snapshot identity mismatch"
  );
  Ok(())
}

fn validate_initial_apply(
  snapshot: &WGNetworkRuntimeSnapshotData,
  network_id: &str,
  member_id: &str,
  applied_now: bool,
  interface_name: &str,
  self_address: Option<&str>,
  peer_count: usize,
  applied: &yat_wireguard::types::AppliedRuntimeState,
) -> Result<()> {
  let mut local = snapshot
    .members
    .iter()
    .filter(|member| member.is_current_device);
  let member = local
    .next()
    .context("Initial snapshot has no local member")?;
  anyhow::ensure!(
    local.next().is_none()
      && member.network_id == network_id
      && member.member_id == member_id
      && snapshot.network.network_id == network_id,
    "Initial snapshot identity mismatch"
  );
  anyhow::ensure!(
    applied_now,
    "Initial controller did not apply this snapshot; old disk state is insufficient"
  );
  anyhow::ensure!(
    applied.network_id == network_id
      && applied.requested_interface_name == interface_name
      && Some(applied.self_address.as_str()) == self_address
      && applied.local_public_key == member.public_key
      && applied.peer_count as usize == peer_count
      && !applied.resolved_interface_name.is_empty()
      && !applied.interface_driver.is_empty()
      && applied.staged_at.is_finite()
      && applied.applied_at.is_finite()
      && applied.staged_at > 0.0
      && applied.applied_at >= applied.staged_at,
    "Applied runtime does not match the initial controller result"
  );
  Ok(())
}

// ─── WgStatus ────────────────────────────────────────────────────────────────

/// Aggregated WireGuard network status.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WgStatus {
  pub connected: bool,
  pub network_id: String,
  pub member_id: String,
  pub interface_name: String,
  pub interface_driver: String,
  pub self_address: String,
  pub peer_count: u32,
}

// ─── ConnectionState (persisted to state.json) ───────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ConnectionState {
  pub network_id: String,
  pub member_id: String,
  pub self_address: String,
  pub interface_name: String,
  pub peer_count: u32,
  pub connected_at: String,
}

pub fn load_state() -> Option<ConnectionState> {
  let path = config::config_dir().join("state.json");
  let content = std::fs::read_to_string(path).ok()?;
  serde_json::from_str(&content).ok()
}

fn save_state(
  network_id: &str,
  member_id: &str,
  self_address: &str,
  interface_name: &str,
  peer_count: u32,
) -> Result<()> {
  let state = ConnectionState {
    network_id: network_id.to_string(),
    member_id: member_id.to_string(),
    self_address: self_address.to_string(),
    interface_name: interface_name.to_string(),
    peer_count,
    connected_at: chrono::Local::now().to_rfc3339(),
  };
  let path = config::config_dir().join("state.json");
  std::fs::write(&path, serde_json::to_string_pretty(&state)?)?;
  Ok(())
}

/// Returns true if `key` looks like a valid WireGuard public key.
///
/// Accepts:
/// - 44-character base64 (32 bytes encoded with "=" padding)
/// - 64-character lowercase hex (32 bytes)
pub(crate) fn is_valid_wireguard_key(key: &str) -> bool {
  match key.len() {
    44 => {
      // Standard base64 alphabet + padding
      key.as_bytes().iter().all(|&b| {
        b.is_ascii_uppercase()
          || b.is_ascii_lowercase()
          || b.is_ascii_digit()
          || b == b'+'
          || b == b'/'
          || b == b'='
      })
    }
    64 => key.as_bytes().iter().all(|&b| b.is_ascii_hexdigit()),
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  fn snapshot() -> WGNetworkRuntimeSnapshotData {
    use yat_transport::{WGNetworkDetailData, WGNetworkMemberData};
    WGNetworkRuntimeSnapshotData {
      path_pairs: vec![],
      network: WGNetworkDetailData {
        network_id: "net".into(),
        owner_user_id: "user".into(),
        name: "test".into(),
        description: String::new(),
        subnet: "10.0.0.0/24".into(),
        relay_mode: 0,
        lifecycle_status: 0,
        runtime_health: 0,
        self_joined: true,
        self_member_id: "member".into(),
        self_device_id: "device".into(),
        member_count: 1,
        online_member_count: 1,
        created_at: 0,
        updated_at: 0,
        coordinator_edge_id: String::new(),
      },
      members: vec![WGNetworkMemberData {
        member_id: "member".into(),
        network_id: "net".into(),
        user_id: "user".into(),
        device_id: "device".into(),
        hostname: "local".into(),
        assigned_ip: "10.0.0.2".into(),
        public_key: "A".repeat(43) + "=",
        public_key_fingerprint: String::new(),
        membership_status: 0,
        presence_status: 0,
        is_current_device: true,
        joined_at: 0,
        last_seen_at: 0,
        updated_at: 0,
        is_relay_node: false,
        observed_endpoint: String::new(),
        observed_at: 0,
        membership_generation: String::new(),
      }],
      peer_sessions: vec![],
      relay_node: None,
      lan_peer_endpoints: vec![],
      revision: 1,
      collected_at: 1,
      path_authority_epoch: String::new(),
      network_policy_revision: 0,
    }
  }
  fn applied() -> yat_wireguard::types::AppliedRuntimeState {
    yat_wireguard::types::AppliedRuntimeState {
      format_version: 1,
      backend: "wireguard".into(),
      network_id: "net".into(),
      logical_interface_name: "yat0".into(),
      requested_interface_name: "yat0".into(),
      resolved_interface_name: "utun42".into(),
      interface_driver: "test".into(),
      self_address: "10.0.0.2/24".into(),
      listen_port: None,
      local_public_key: "A".repeat(43) + "=",
      peer_count: 0,
      staged_at: 100.0,
      applied_at: 101.0,
    }
  }
  #[test]
  fn snapshot_identity_is_checked_before_native_write() {
    let valid = snapshot();
    let key = valid.members[0].public_key.clone();
    assert!(validate_snapshot_identity(&valid, "net", "member", &key).is_ok());
    for mut wrong in [
      valid.clone(),
      valid.clone(),
      valid.clone(),
      valid.clone(),
      valid.clone(),
    ]
    .into_iter()
    .enumerate()
    {
      match wrong.0 {
        0 => wrong.1.network.network_id = "other".into(),
        1 => wrong.1.members[0].member_id = "other".into(),
        2 => wrong.1.members[0].public_key = "B".repeat(43) + "=",
        3 => wrong.1.members[0].network_id = "other".into(),
        _ => wrong.1.members.push(valid.members[0].clone()),
      }
      assert!(validate_snapshot_identity(&wrong.1, "net", "member", &key).is_err());
    }
  }
  #[test]
  fn initial_apply_rejects_stale_or_mismatched_disk_state() {
    let snapshot = snapshot();
    let valid = applied();
    let check = |now, state: &yat_wireguard::types::AppliedRuntimeState| {
      validate_initial_apply(
        &snapshot,
        "net",
        "member",
        now,
        "yat0",
        Some("10.0.0.2/24"),
        0,
        state,
      )
    };
    assert!(check(true, &valid).is_ok());
    assert!(check(false, &valid).is_err());
    for index in 0..8 {
      let mut wrong = applied();
      match index {
        0 => wrong.network_id = "other".into(),
        1 => wrong.local_public_key = "B".repeat(43) + "=",
        2 => wrong.self_address = "10.0.0.3/24".into(),
        3 => wrong.peer_count = 1,
        4 => wrong.requested_interface_name = "other".into(),
        5 => wrong.applied_at = 99.0,
        6 => wrong.staged_at = f64::NAN,
        _ => wrong.resolved_interface_name.clear(),
      }
      assert!(check(true, &wrong).is_err(), "variant {index}");
    }
  }
}
