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
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use yat_transport::{
  EdgeClient, EdgeClientConfig,
  WGGetNetworkResponseData, WGNetworkRuntimeSnapshotData,
};
use yat_wireguard::types::{
  ApplyRuntimeParams, ClearNetworkStateParams, LocalConfigPeerInput,
  LocalIdentityParams, SyncLocalConfigParams,
};
use std::collections::HashMap;

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
  pub hostname: String,
  pub peer_count: u32,
  pub interface_name: String,
  snapshot: WGNetworkRuntimeSnapshotData,
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
  /// 4. Join WG network (if needed)
  /// 5. Re-fetch snapshot (post-join)
  ///
  /// Returns an [`EdgeJoinContext`] that carries everything the caller needs
  /// to later apply the WireGuard interface (phase 2) **or** to keep the
  /// Edge connection alive for dashboard / sync even when WG fails.
  pub async fn connect_edge(&mut self) -> Result<EdgeJoinContext> {
    let device_identity = config::load_device_identity()?;
    let hostname = config::get_hostname();
    let storage_dir = self.certs.wg_storage_dir();
    let storage_dir_str = storage_dir.to_string_lossy().to_string();

    let network_id = self
      .config
      .network_id()
      .ok_or_else(|| anyhow::anyhow!("No network configured. Run `yat-cli import <share.json>` first."))?;

    info!(
      "Edge: {}:{} | User: {} | Device: {} | Network: {}",
      self.config.edge.host, self.config.edge.port,
      self.config.identity.user_id, device_identity.device_id,
      network_id,
    );

    // ── Edge connection ──────────────────────────────────────────────────
    let edge = self.create_edge_client(&device_identity, &hostname).await?;

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
    info!("WG public key: {}, client_id: {}", identity.public_key, client_id);

    // ── Get network snapshot ─────────────────────────────────────────────
    info!("Fetching network snapshot...");
    let net_response = edge
      .request_wg_get_network(&network_id)
      .await
      .context("Failed to get network info")?;
    if !net_response.success {
      anyhow::bail!(
        "Failed to get network: {} (code: {})",
        net_response.error_message, net_response.error_code
      );
    }

    // ── Join if needed ───────────────────────────────────────────────────
    let (member_id, assigned_ip) = self.resolve_membership(
      &edge, &network_id, &identity, &hostname, &net_response,
    ).await?;

    // ── Re-fetch snapshot (post-join) ────────────────────────────────────
    let snapshot = {
      let refresh = edge
        .request_wg_get_network(&network_id)
        .await
        .context("Failed to refresh network snapshot")?;
      refresh.snapshot.ok_or_else(|| anyhow::anyhow!("Network snapshot is empty"))?
    };

    let raw_ip = if assigned_ip.is_empty() {
      snapshot.members.iter()
        .find(|m| m.is_current_device)
        .map(|m| m.assigned_ip.clone())
        .unwrap_or_default()
    } else {
      assigned_ip
    };

    let self_address = if raw_ip.contains('/') {
      raw_ip
    } else {
      let prefix = snapshot.network.subnet
        .split('/')
        .nth(1)
        .unwrap_or("32");
      format!("{raw_ip}/{prefix}")
    };

    let peers = build_peers_from_snapshot(&snapshot);
    let peer_count = peers.len() as u32;

    let self_hostname = snapshot.members.iter()
      .find(|m| m.is_current_device)
      .map(|m| m.hostname.clone())
      .unwrap_or_else(|| hostname.clone());

    Ok(EdgeJoinContext {
      edge: Arc::new(edge),
      network_id: network_id.to_string(),
      storage_dir: storage_dir_str,
      member_id,
      client_id,
      self_address,
      hostname: self_hostname,
      peer_count,
      interface_name: "yat0".to_string(),
      snapshot,
    })
  }

  // ─── apply_wg: WireGuard interface creation ────────────────────────────

  /// Phase 2: build WG config, stage, and create the OS interface.
  ///
  /// Consumes the [`EdgeJoinContext`] from phase 1.  On success also
  /// persists connection state and prints a summary.
  pub async fn apply_wg(&mut self, ctx: EdgeJoinContext) -> Result<WgStatus> {
    let EdgeJoinContext {
      edge, network_id, storage_dir, member_id, client_id: _, self_address,
      hostname: _, peer_count: _, interface_name, snapshot,
    } = ctx;

    let storage_dir_path = Path::new(&storage_dir);

    // ── Build peers + WG config ──────────────────────────────────────────
    let peers = build_peers_from_snapshot(&snapshot);
    let peer_count = peers.len() as u32;

    // Re-create identity (we need the private key for config text)
    let id_params = LocalIdentityParams {
      storage_dir: storage_dir.clone(),
      preferred_device_id: None,
      hostname: None,
    };
    let identity = tokio::task::spawn_blocking(move || {
      yat_wireguard::identity::get_or_create_local_identity(id_params)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?
    .map_err(|e| anyhow::anyhow!("Failed to load WG identity: {e}"))?;

    info!("Building WireGuard config: {peer_count} peers, subnet={}", snapshot.network.subnet);
    let config_text = build_wg_config_text(&identity.private_key, &self_address, None, &peers);

    // ── Stage + Apply ────────────────────────────────────────────────────
    let network_dir = storage_dir_path.join("networks").join(&network_id);
    std::fs::create_dir_all(&network_dir)?;
    let config_path = network_dir.join("wg.conf");

    info!("Syncing WireGuard local config...");
    // Remove any stale staged state from a previous run to ensure
    // apply_staged_runtime reads the file we're about to write.
    let runtime_state_path = network_dir.join("runtime-state.json");
    if runtime_state_path.exists() {
      let _ = std::fs::remove_file(&runtime_state_path);
    }
    let sync_params = SyncLocalConfigParams {
      storage_dir: storage_dir.clone(),
      network_id: network_id.clone(),
      interface_name: interface_name.clone(),
      config_path: config_path.to_string_lossy().to_string(),
      config_text,
      self_address: Some(self_address.clone()),
      listen_port: None,
      peers,
    };
    tokio::task::spawn_blocking(move || {
      yat_wireguard::staging::sync_local_config(sync_params)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?
    .map_err(|e| anyhow::anyhow!("sync_local_config failed: {e}"))?;

    info!("Applying WireGuard runtime (creating interface)...");
    let apply_params = ApplyRuntimeParams {
      storage_dir: storage_dir.clone(),
      network_id: network_id.clone(),
    };
    let applied = tokio::task::spawn_blocking(move || {
      yat_wireguard::applied::apply_staged_runtime(apply_params)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?
    .map_err(|e| anyhow::anyhow!("apply_staged_runtime failed: {e}"))?;

    info!(
      "WireGuard interface created: {} (driver: {})",
      applied.resolved_interface_name, applied.interface_driver
    );

    // ── Persist state ────────────────────────────────────────────────────
    self.network_id = Some(network_id.clone());
    self.member_id = Some(member_id.clone());
    self.edge = Some(edge);
    save_state(&network_id, &member_id, &self_address, &interface_name, peer_count)?;

    let status = WgStatus {
      connected: true,
      network_id: network_id.clone(),
      member_id: member_id.clone(),
      interface_name: applied.resolved_interface_name.clone(),
      interface_driver: applied.interface_driver.clone(),
      self_address: self_address.clone(),
      peer_count,
    };

    println!("✓ Connected to network \"{network_id}\"");
    println!("  Interface: {} ({})", applied.resolved_interface_name, applied.interface_driver);
    println!("  Address:   {self_address}");
    println!("  Peers:     {peer_count}");
    println!("  Member:    {member_id}");

    Ok(status)
  }

  // ─── start: full connect flow (convenience wrapper) ────────────────────

  /// Execute the full connection flow (phase 1 + phase 2).
  ///
  /// Equivalent to calling [`connect_edge`](Self::connect_edge) followed by
  /// [`apply_wg`](Self::apply_wg).  Used by the manual `yat-cli start`
  /// command where an all-or-nothing result is expected.
  pub async fn start(&mut self) -> Result<WgStatus> {
    let ctx = self.connect_edge().await?;
    self.apply_wg(ctx).await
  }

  // ─── stop: full disconnect flow ─────────────────────────────────────────

  /// Execute the full disconnect flow:
  /// 1. Teardown WG interface + clear state
  /// 2. Leave network via Edge (best-effort)
  /// 3. Disconnect Edge
  pub async fn stop(&mut self) -> Result<()> {
    let network_id = self.config.network_id()
      .ok_or_else(|| anyhow::anyhow!("No network configured."))?;
    let storage_dir_str = self.certs.wg_storage_dir().to_string_lossy().to_string();

    // 1. Clear WG state (teardown interface)
    info!("Clearing WireGuard state for {network_id}...");
    match yat_wireguard::runtime::clear_network_state(ClearNetworkStateParams {
      storage_dir: storage_dir_str.clone(),
      network_id: network_id.clone(),
    }) {
      Ok(r) if r.removed => info!("WireGuard state cleared."),
      Ok(_) => info!("No WireGuard state found for {network_id}."),
      Err(e) => warn!("Failed to clear WG state: {e}"),
    }

    // 2. Leave via Edge (best-effort)
    self.try_leave_via_edge(&network_id).await;

    // 3. Cleanup
    if let Some(edge) = self.edge.take() {
      let _ = edge.disconnect().await;
    }
    self.network_id = None;
    self.member_id = None;
    remove_state_file();

    println!("✓ Left network and cleaned up.");
    Ok(())
  }

  // ─── status: aggregate state ────────────────────────────────────────────

  /// Get aggregated network status (reads saved state + engine state).
  /// Does NOT require an active Edge connection.
  pub fn status(&self) -> Result<WgStatus> {
    let state = load_state().ok_or_else(|| {
      anyhow::anyhow!("No active network. Run `yat-cli start` to connect.")
    })?;

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

  // ─── import_share ───────────────────────────────────────────────────────

  /// Import a share.json file (delegates to cli::import logic).
  #[allow(dead_code)]
  pub fn import_share(&self, file: &Path) -> Result<()> {
    crate::cli::import::run(file)
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
    client.connect().await.context("Failed to connect to Edge")?;
    info!("Edge connection established (mTLS).");

    // Authenticate
    info!("Authenticating...");
    let auth_result = client
      .authenticate(
        &self.config.identity.user_id,
        &device_identity.device_id,
        hostname,
        env!("CARGO_PKG_VERSION"),
        &["wg".to_string()],
        1,
        0,
        &[],
        &[],
      )
      .await
      .context("Authentication failed")?;

    if !auth_result.success {
      anyhow::bail!(
        "Authentication rejected: {} (code: {})",
        auth_result.error_message, auth_result.error_code
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
    _net_response: &WGGetNetworkResponseData,
  ) -> Result<(String, String)> {
    info!("Joining network (sending real public key for upgrade)...");
    let join_result = edge
      .join_wg_network(network_id, &identity.client_id, &identity.public_key, hostname)
      .await
      .context("Failed to join network")?;
    if !join_result.success {
      anyhow::bail!("Join failed: {} (code: {})", join_result.error_message, join_result.error_code);
    }
    info!("Joined network. Member: {} IP: {}", join_result.member_id, join_result.assigned_ip);
    Ok((join_result.member_id, join_result.assigned_ip))
  }

  /// Best-effort leave via Edge (used during stop).
  async fn try_leave_via_edge(&self, network_id: &str) {
    let device_identity = match config::load_device_identity() {
      Ok(d) => d,
      Err(_) => return,
    };
    let hostname = config::get_hostname();
    let edge = match self.create_edge_client(&device_identity, &hostname).await {
      Ok(e) => e,
      Err(e) => {
        warn!("Failed to connect to Edge for leave: {e}");
        return;
      }
    };

    let member_id = load_state().map(|s| s.member_id).unwrap_or_default();
    if member_id.is_empty() {
      let _ = edge.disconnect().await;
      return;
    }

    info!("Leaving network {network_id} (member: {member_id})...");
    match edge.leave_wg_network(network_id, &member_id, false).await {
      Ok(r) if r.success => info!("Left network successfully."),
      Ok(r) => warn!("Leave rejected: {} (code: {})", r.error_message, r.error_code),
      Err(e) => warn!("Leave failed: {e}"),
    }
    let _ = edge.disconnect().await;
  }
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

fn remove_state_file() {
  let _ = std::fs::remove_file(config::config_dir().join("state.json"));
}

// ─── WG config text generation ──────────────────────────────────────────────

fn build_wg_config_text(
  private_key: &str,
  self_address: &str,
  listen_port: Option<u32>,
  peers: &[LocalConfigPeerInput],
) -> String {
  let mut config = String::new();
  config.push_str("[Interface]\n");
  config.push_str(&format!("PrivateKey = {private_key}\n"));
  config.push_str(&format!("Address = {self_address}\n"));
  if let Some(port) = listen_port {
    config.push_str(&format!("ListenPort = {port}\n"));
  }
  for peer in peers {
    config.push_str("\n[Peer]\n");
    if let Some(ref pk) = peer.public_key {
      config.push_str(&format!("PublicKey = {pk}\n"));
    }
    if let Some(ref ep) = peer.endpoint {
      config.push_str(&format!("Endpoint = {ep}\n"));
    }
    config.push_str("PersistentKeepalive = 25\n");
    let allowed = peer.allowed_ips.join(", ");
    if !allowed.is_empty() {
      config.push_str(&format!("AllowedIPs = {allowed}\n"));
    }
  }
  config
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

/// Extract the public IP address from an endpoint string.
///
/// Handles formats:
/// - `1.2.3.4:58021` → `1.2.3.4`
/// - `[::1]:58021`   → `::1`
/// - `host:port`     → `host`
pub(crate) fn extract_public_ip(endpoint: &str) -> String {
  let trimmed = endpoint.trim();
  if trimmed.is_empty() {
    return String::new();
  }
  // IPv6 bracket notation: [::1]:port
  if trimmed.starts_with('[') {
    if let Some(bracket_end) = trimmed.find(']') {
      return trimmed[1..bracket_end].to_string();
    }
  }
  // IPv4 or hostname: host:port — split on last ':' to handle IPv4 only
  if let Some(colon_pos) = trimmed.rfind(':') {
    return trimmed[..colon_pos].to_string();
  }
  trimmed.to_string()
}

/// Build peer list from a network snapshot (excludes self).
///
/// Members whose `public_key` is empty or not a valid WireGuard key
/// (e.g. placeholder values like a client-ID or port number) are silently
/// skipped — they cannot be programmed into the interface.
///
/// Same-NAT optimization: when self and a remote peer share the same
/// observed public IP (same-NAT), prefer the LAN endpoint from
/// `lan_peer_endpoints` over the public observed endpoint. Without this,
/// hairpin NAT is required which often fails on consumer routers.
fn build_peers_from_snapshot(
  snapshot: &WGNetworkRuntimeSnapshotData,
) -> Vec<LocalConfigPeerInput> {
  // Find self's observed public IP for same-NAT detection.
  let self_observed_ip = snapshot
    .members
    .iter()
    .find(|m| m.is_current_device)
    .map(|m| extract_public_ip(&m.observed_endpoint))
    .unwrap_or_default();

  // Build lookup: public_key → LAN endpoint from edge's same-NAT detection.
  let lan_map: HashMap<&str, &str> = snapshot
    .lan_peer_endpoints
    .iter()
    .map(|ep| (ep.public_key.as_str(), ep.lan_endpoint.as_str()))
    .collect();

  let mut peers = Vec::with_capacity(snapshot.members.len());
  for m in &snapshot.members {
    if m.is_current_device {
      continue;
    }
    if !is_valid_wireguard_key(&m.public_key) {
      warn!(
        "Skipping member {} (device {}): public_key {:?} is not a valid WireGuard key",
        m.member_id, m.device_id, m.public_key
      );
      continue;
    }

    // Determine the best endpoint for this peer.
    // Same-NAT: prefer LAN endpoint; fall back to observed (hairpin NAT).
    // Diff-NAT: use observed public endpoint directly.
    let public_direct_ip = extract_public_ip(&m.observed_endpoint);
    let is_same_nat = !self_observed_ip.is_empty()
      && !public_direct_ip.is_empty()
      && self_observed_ip == public_direct_ip;

    let lan_endpoint = if is_same_nat {
      lan_map.get(m.public_key.as_str()).copied().unwrap_or("")
    } else {
      ""
    };

    let endpoint = if !lan_endpoint.is_empty() {
      Some(lan_endpoint.to_string())
    } else if !m.observed_endpoint.is_empty() {
      Some(m.observed_endpoint.clone())
    } else {
      None
    };

    if is_same_nat && lan_endpoint.is_empty() {
      info!(
        "Same-NAT detected for peer {} (device {}) but no LAN endpoint available yet, \
         falling back to observed endpoint (hairpin NAT)",
        m.member_id, m.device_id
      );
    }

    // Each peer's allowed_ips is its own assigned IP /32, not the whole subnet.
    let allowed_ip = if m.assigned_ip.contains('/') {
      m.assigned_ip.clone()
    } else {
      format!("{}/32", m.assigned_ip)
    };
    peers.push(LocalConfigPeerInput {
      member_id: m.member_id.clone(),
      hostname: Some(m.hostname.clone()),
      device_id: m.device_id.clone(),
      public_key: Some(m.public_key.clone()),
      allowed_ips: vec![allowed_ip],
      endpoint,
    });
  }
  peers
}
