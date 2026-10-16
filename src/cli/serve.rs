use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

use yat_wireguard::types::ClearNetworkStateParams;
use yat_network::WgSyncController;

use crate::config::AppConfig;
use crate::daemon::loop_runner::{LogBuffer, SyncState};
use crate::managers::cert::CertManager;
use crate::managers::WgManager;
use crate::web::server::AppState;

/// Maximum reconnect attempts before entering the 30-minute deferred wait.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;
/// Delay before retrying after all immediate attempts are exhausted.
const DEFERRED_RECONNECT_SECS: u64 = 30 * 60; // 30 minutes

/// Run as daemon with web management UI.
pub async fn run(listen: &str) -> Result<()> {
  // Install OS-level SIGINT handler BEFORE anything else.
  // This is necessary because `tokio::signal::ctrl_c()` is unreliable
  // under `sudo` — the signal may not be delivered to the tokio runtime.
  // A dedicated thread using `sigwait` receives SIGINT directly from the
  // kernel and calls `process::exit(0)` to force-terminate.
  start_signal_handler();

  // Check root permission (needed for WireGuard interface creation)
  let is_root = unsafe { libc::geteuid() == 0 };
  if !is_root {
    eprintln!("⚠ Warning: not running as root. WireGuard interface creation will fail.");
    eprintln!("  Please use: sudo yat-cli serve");
  }

  let log_buffer = Arc::new(Mutex::new(LogBuffer::new()));
  log_buffer.lock().await.push("INFO", "YAT daemon starting...".into());

  // 1. Load config (non-fatal)
  let config = match crate::config::load_config() {
    Ok(c) => Some(Arc::new(c)),
    Err(e) => {
      warn!("Config not available: {e}");
      log_buffer.lock().await.push("WARN", format!("Config not available: {e}"));
      None
    }
  };

  // 2. Build shared state
  let sync_state = Arc::new(Mutex::new(SyncState::empty()));
  let cert_manager = Arc::new(CertManager::new(&crate::config::config_dir()));
  let config_mutex = Arc::new(Mutex::new(config.clone()));

  // 3. Restart channel — import triggers restart
  let (restart_tx, mut restart_rx) = tokio::sync::mpsc::channel::<()>(4);

  // 4. Reconnect notification — manual reconnect button wakes up the loop
  let reconnect_notify = Arc::new(Notify::new());

  let app_state = AppState {
    sync: Arc::clone(&sync_state),
    logs: Arc::clone(&log_buffer),
    config: Arc::clone(&config_mutex),
    cert_manager: Arc::clone(&cert_manager),
    restart_tx: Arc::new(restart_tx),
    reconnect_notify: Arc::clone(&reconnect_notify),
    edge_client: Arc::new(Mutex::new(None)),
  };
  // Pre-clone for the cert rotation monitor (config and cert_manager are
  // moved into spawned tasks below).
  let cfg_for_renew = config.clone();
  let cm_for_restart = Arc::clone(&cert_manager);

  if let Some(cfg) = config {
    let ss = Arc::clone(&sync_state);
    let lb = Arc::clone(&log_buffer);
    let rn = Arc::clone(&reconnect_notify);
    let cm = Arc::clone(&cert_manager);
    let ec = Arc::clone(&app_state.edge_client);
    tokio::spawn(async move {
      connect_edge(cfg, ss, lb, rn, cm, ec).await;
    });
  } else {
    log_buffer.lock().await.push("INFO", "No config — import a share.json via Web UI.".into());
  }

  // 5. Restart listener — when import triggers restart, reconnect Edge
  let ss_restart = Arc::clone(&sync_state);
  let lb_restart = Arc::clone(&log_buffer);
  let cfg_restart = Arc::clone(&config_mutex);
  let ec_restart = Arc::clone(&app_state.edge_client);
  tokio::spawn(async move {
    while restart_rx.recv().await.is_some() {
      info!("Restart triggered — reloading config and reconnecting...");
      lb_restart.lock().await.push("INFO", "Reloading config...".into());

      // Reset sync state
      {
        let mut s = ss_restart.lock().await;
        *s = SyncState::empty();
      }

      // Load new config
      let new_config = match crate::config::load_config() {
        Ok(c) => {
          let arc = Arc::new(c);
          *cfg_restart.lock().await = Some(Arc::clone(&arc));
          Some(arc)
        }
        Err(e) => {
          warn!("Config reload failed: {e}");
          lb_restart.lock().await.push("ERROR", format!("Config reload failed: {e}"));
          None
        }
      };

      // Reconnect Edge
      if let Some(cfg) = new_config {
        lb_restart.lock().await.push("INFO", "Connecting to Edge...".into());
        let ss = Arc::clone(&ss_restart);
        let lb = Arc::clone(&lb_restart);
        let rn = Arc::new(Notify::new());
        let cm = Arc::clone(&cm_for_restart);
        // Reuse the same edge_client_slot so the web API (e.g. /api/address)
        // can access the new EdgeClient after reconnection.
        let ec = Arc::clone(&ec_restart);
        tokio::spawn(async move {
          connect_edge(cfg, ss, lb, rn, cm, ec).await;
        });
      }
    }
  });

  // 6. Certificate rotation monitor — checks every hour, auto-renews with OAuth token
  let cert_mgr = Arc::clone(&cert_manager);
  let lb_cert = Arc::clone(&log_buffer);
  tokio::spawn(async move {
    cert_rotation_monitor(cert_mgr, lb_cert, cfg_for_renew).await;
  });

  // 7. Start web server (blocks until shutdown)
  eprintln!("✓ Web UI available at http://{}", listen);
  crate::web::server::start(listen, app_state).await?;
  Ok(())
}

/// Background task: connect to Edge + join WG network + start sync loop.
///
/// Implements auto-reconnect with exponential back-off (1→30 s).
/// After `MAX_RECONNECT_ATTEMPTS` consecutive failures, enters a 30-minute
/// deferred wait that can be interrupted by the manual reconnect button.
///
/// **Key design**: Edge connection and WireGuard interface creation are
/// decoupled.  When WG creation fails (e.g. invalid peer key), the Edge
/// client stays alive so the dashboard can keep receiving server-pushed
/// peer / network updates.
async fn connect_edge(
  cfg: Arc<AppConfig>,
  sync_state: Arc<Mutex<SyncState>>,
  log_buffer: Arc<Mutex<LogBuffer>>,
  reconnect_notify: Arc<Notify>,
  cert_manager: Arc<CertManager>,
  edge_client_slot: Arc<Mutex<Option<Arc<yat_transport::EdgeClient>>>>,
) {
  log_buffer.lock().await.push("INFO", "Connecting to Edge...".into());

  // ── Certificate bootstrap ─────────────────────────────────────────
  // If no client certificate exists and an invite token is available,
  // request one from the Edge before proceeding.
  if !cert_manager.has_client_cert() {
    if let Some(invite_token) = &cfg.invite_token() {
      log_buffer.lock().await.push("INFO", "No client cert — bootstrapping via invite token...".into());
      let device_id = &cfg.identity.device_id;
      match cert_manager
        .bootstrap_certificate(
          &cfg.edge,
          device_id,
          invite_token,
        )
        .await
      {
        Ok(()) => {
          info!("Certificate bootstrap successful.");
          log_buffer.lock().await.push("INFO", "Certificate obtained from Edge.".into());
        }
        Err(e) => {
          // Extract root cause (last item in error chain)
          let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
          let root_cause = chain.last().cloned().unwrap_or_else(|| e.to_string());

          // Terminal: ANSI red + bold
          let red = "\x1b[1;31m";
          let reset = "\x1b[0m";
          eprintln!();
          eprintln!("{red}  ✗ Certificate bootstrap failed{reset}");
          eprintln!("{red}    Edge:   {}:{}{reset}", cfg.edge.host, cfg.edge.port);
          eprintln!("{red}    Device: {}{reset}", cfg.identity.device_id);
          eprintln!("{red}    Reason: {root_cause}{reset}");
          eprintln!();

          error!("Certificate bootstrap failed: {root_cause}");
          log_buffer.lock().await.push("ERROR", format!("Cert bootstrap failed: {root_cause}"));
          return;
        }
      }
    } else {
      log_buffer.lock().await.push("ERROR", "No client cert and no invite token. Re-import share.json.".into());
      return;
    }
  }

  // ── Initial connection ──────────────────────────────────────────────
  let mut wg = match WgManager::from_config((*cfg).clone()) {
    Ok(wg) => wg,
    Err(e) => {
      error!("Failed to init WgManager: {e}");
      log_buffer.lock().await.push("ERROR", format!("WgManager init failed: {e}"));
      return;
    }
  };

  // Phase 1: Edge connection + network join
  let ctx = match wg.connect_edge().await {
    Ok(ctx) => {
      log_buffer.lock().await.push("INFO", "Connected to Edge, network joined.".into());
      info!("Edge connection established.");
      ctx
    }
    Err(e) => {
      error!("Edge connection failed: {e}");
      log_buffer.lock().await.push("ERROR", format!("Edge connection failed: {e}"));
      return;
    }
  };

  // Populate SyncState from edge context — dashboard shows Connected
  // even before WG interface is created.
  let mut network_id = ctx.network_id.clone();
  let mut storage_dir = ctx.storage_dir.clone();
  {
    let mut s = sync_state.lock().await;
    s.network_id = ctx.network_id.clone();
    s.member_id = ctx.member_id.clone();
    s.self_address = ctx.self_address.clone();
    s.hostname = ctx.hostname.clone();
    s.interface_name = ctx.interface_name.clone();
    s.peer_count = ctx.peer_count;
    s.connected = true;
    s.reconnect_at = None;
  }

  // Keep a reference to the edge client before apply_wg consumes ctx.
  // If apply_wg fails, self.edge is never set — this clone is our fallback.
  let edge_client = Arc::clone(&ctx.edge);
  let client_id = ctx.client_id.clone();
  let self_address = ctx.self_address.clone();
  let initial_snapshot = ctx.snapshot.clone();
  let storage_dir_for_sync = ctx.storage_dir.clone();

  // Publish edge_client so the web API can use it (e.g. address update)
  {
    let mut slot = edge_client_slot.lock().await;
    *slot = Some(Arc::clone(&edge_client));
  }

  // Phase 2: WireGuard interface creation (may fail independently)
  if let Err(e) = wg.apply_wg(ctx).await {
    warn!("WireGuard interface creation failed (edge stays connected): {e}");
    log_buffer.lock().await.push("WARN", format!("WG interface failed: {e}"));
  }

  // ── Set up WgSyncController (yat-network) for ongoing WG sync ──────
  let sync_controller = {
    let id_params = yat_wireguard::types::LocalIdentityParams {
      storage_dir: storage_dir_for_sync.clone(),
      preferred_device_id: None,
      hostname: None,
    };
    match tokio::task::spawn_blocking(move || {
      yat_wireguard::identity::get_or_create_local_identity(id_params)
    }).await {
      Ok(Ok(identity)) => {
        let engine = yat_network::wg::engine::create_engine(None);
        let controller = WgSyncController::new(
          engine, storage_dir_for_sync.clone(), true,
        );
        controller.set_private_key(identity.private_key);
        let controller = Arc::new(controller);
        controller.mark_joined(&network_id).await;
        // Feed the initial snapshot to trigger the first sync pipeline.
        if let Err(e) = controller.handle_typed_snapshot(&network_id, &initial_snapshot).await {
          warn!("Initial WgSyncController sync failed: {e:#}");
        }
        Some(controller)
      }
      Ok(Err(e)) => {
        warn!("Failed to load WG identity for sync controller: {e}");
        None
      }
      Err(e) => {
        warn!("spawn_blocking join error: {e}");
        None
      }
    }
  };

  // Subscribe to P2P events (gather/punch) for same-NAT LAN discovery.
  // Must be AFTER apply_wg so the WG interface (and its listen port) exists
  // when a gather event arrives.
  crate::gather::subscribe_p2p_events(
    Arc::clone(&edge_client), client_id.clone(),
    storage_dir.clone(), network_id.clone(),
    self_address.clone(),
  ).await;

  // ── Sync loop (blocks until Edge disconnects) ───────────────────────
  run_wg_sync_loop(
    edge_client, Arc::clone(&sync_state), Arc::clone(&log_buffer),
    network_id.clone(), storage_dir.clone(), sync_controller,
  ).await;

  // ── Reconnect loop with exponential back-off + deferred retry ───────
  let mut backoff_secs = 1u64;
  let max_backoff = 30u64;
  let mut attempt = 0u32;

  loop {
    attempt += 1;
    warn!("Edge disconnected. Reconnect attempt #{attempt}...");
    log_buffer.lock().await.push("WARN", format!(
      "Edge disconnected. Reconnecting (attempt #{attempt})..."
    ));

    // Tear down stale WG interface before re-creating
    info!("Reconnect: cleaning up WireGuard state...");
    if let Err(e) = yat_wireguard::runtime::clear_network_state(ClearNetworkStateParams {
      storage_dir: storage_dir.clone(),
      network_id: network_id.clone(),
    }) {
      warn!("Reconnect: WG cleanup failed: {e}");
    }

    // Fresh WgManager for the new connection
    let mut new_wg = match WgManager::from_config((*cfg).clone()) {
      Ok(w) => w,
      Err(e) => {
        warn!("Reconnect: WgManager init failed: {e}");
        log_buffer.lock().await.push("ERROR", format!("Reconnect init failed: {e}"));
        if attempt >= MAX_RECONNECT_ATTEMPTS {
          enter_deferred_wait(&sync_state, &log_buffer, &reconnect_notify).await;
          attempt = 0;
        } else {
          interruptible_sleep(backoff_secs, &reconnect_notify).await;
          backoff_secs = (backoff_secs * 2).min(max_backoff);
        }
        continue;
      }
    };

    // Phase 1: Edge connection + network join
    let ctx = match new_wg.connect_edge().await {
      Ok(ctx) => {
        info!("Edge reconnected successfully on attempt #{attempt}.");
        log_buffer.lock().await.push("INFO", "Edge reconnected successfully.".into());
        ctx
      }
      Err(e) => {
        warn!("Reconnect attempt #{attempt} failed: {e}.");
        log_buffer.lock().await.push("WARN", format!("Reconnect failed: {e}."));

        if attempt >= MAX_RECONNECT_ATTEMPTS {
          enter_deferred_wait(&sync_state, &log_buffer, &reconnect_notify).await;
          attempt = 0;
        } else {
          info!("Backing off {backoff_secs}s before next attempt...");
          interruptible_sleep(backoff_secs, &reconnect_notify).await;
          backoff_secs = (backoff_secs * 2).min(max_backoff);
        }
        continue;
      }
    };

    // Update SyncState from edge context
    network_id = ctx.network_id.clone();
    storage_dir = ctx.storage_dir.clone();
    {
      let mut s = sync_state.lock().await;
      s.network_id = ctx.network_id.clone();
      s.member_id = ctx.member_id.clone();
      s.self_address = ctx.self_address.clone();
      s.hostname = ctx.hostname.clone();
      s.interface_name = ctx.interface_name.clone();
      s.peer_count = ctx.peer_count;
      s.connected = true;
      s.reconnect_at = None;
    }
    backoff_secs = 1;
    attempt = 0;

    // Keep a reference before apply_wg consumes ctx.
    let edge_client = Arc::clone(&ctx.edge);
    let client_id = ctx.client_id.clone();
    let self_address = ctx.self_address.clone();
    let initial_snapshot = ctx.snapshot.clone();

    // Update the shared edge_client slot for web API access
    {
      let mut slot = edge_client_slot.lock().await;
      *slot = Some(Arc::clone(&edge_client));
    }

    // Phase 2: WireGuard interface creation (non-fatal)
    if let Err(e) = new_wg.apply_wg(ctx).await {
      warn!("WireGuard interface creation failed (edge stays connected): {e}");
      log_buffer.lock().await.push("WARN", format!("WG interface failed: {e}"));
    }

    // Set up WgSyncController for the reconnected session.
    let sync_controller = {
      let id_params = yat_wireguard::types::LocalIdentityParams {
        storage_dir: storage_dir.clone(),
        preferred_device_id: None,
        hostname: None,
      };
      match tokio::task::spawn_blocking(move || {
        yat_wireguard::identity::get_or_create_local_identity(id_params)
      }).await {
        Ok(Ok(identity)) => {
          let engine = yat_network::wg::engine::create_engine(None);
          let controller = WgSyncController::new(
            engine, storage_dir.clone(), true,
          );
          controller.set_private_key(identity.private_key);
          let controller = Arc::new(controller);
          controller.mark_joined(&network_id).await;
          if let Err(e) = controller.handle_typed_snapshot(&network_id, &initial_snapshot).await {
            warn!("Reconnect: initial WgSyncController sync failed: {e:#}");
          }
          Some(controller)
        }
        _ => None,
      }
    };

    // Subscribe to P2P events AFTER WG interface creation.
    crate::gather::subscribe_p2p_events(
      Arc::clone(&edge_client), client_id.clone(),
      storage_dir.clone(), network_id.clone(),
      self_address.clone(),
    ).await;

    // Re-enter sync loop (blocks until next disconnect)
    run_wg_sync_loop(
      edge_client, Arc::clone(&sync_state), Arc::clone(&log_buffer),
      network_id.clone(), storage_dir.clone(), sync_controller,
    ).await;
  }
}

/// Sync loop: periodically fetches snapshots, feeds them to `WgSyncController`
/// for WG config sync, and updates the dashboard `SyncState`.
///
/// Returns when the Edge connection is lost, so the caller can trigger reconnection.
async fn run_wg_sync_loop(
  edge_client: Arc<yat_transport::EdgeClient>,
  state: Arc<Mutex<SyncState>>,
  log_buffer: Arc<Mutex<LogBuffer>>,
  network_id: String,
  storage_dir: String,
  sync_controller: Option<Arc<WgSyncController>>,
) {
  use crate::managers::wg::{extract_public_ip, is_valid_wireguard_key};
  use crate::daemon::loop_runner::PeerInfo;

  info!("WG sync loop started for network {network_id}.");

  let mut disconnect_cycles: u32 = 0;
  const MAX_DISCONNECT_CYCLES: u32 = 2;

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
    disconnect_cycles = 0;

    // Fetch network snapshot from Edge
    let snapshot_result = edge_client.request_wg_get_network(&network_id).await;
    let snapshot = match snapshot_result {
      Ok(response) if response.success => {
        if let Some(snap) = response.snapshot {
          Some(snap)
        } else {
          continue;
        }
      }
      Ok(response) => {
        warn!("Sync: get_network failed: {} (code: {})", response.error_message, response.error_code);
        log_buffer.lock().await.push("WARN", format!("Sync failed: {}", response.error_message));
        None
      }
      Err(e) => {
        warn!("Sync: get_network error: {e}");
        log_buffer.lock().await.push("ERROR", format!("Sync error: {e}"));
        None
      }
    };

    // Feed snapshot to WgSyncController for WG config sync
    if let (Some(ref controller), Some(ref snap)) = (&sync_controller, &snapshot) {
      match controller.handle_typed_snapshot(&network_id, snap).await {
        Ok(Some(result)) => {
          if result.applied {
            info!("WgSyncController applied config changes ({} peers)", result.local_config.peer_count);
            log_buffer.lock().await.push("INFO", format!(
              "WG config synced: {} peers", result.local_config.peer_count
            ));
          }
        }
        Ok(None) => {} // no changes
        Err(e) => {
          warn!("WgSyncController sync error: {e:#}");
        }
      }
    }

    // Update dashboard SyncState from snapshot
    if let Some(snap) = snapshot {
      let self_observed_ip = snap.members.iter()
        .find(|m| m.is_current_device)
        .map(|m| extract_public_ip(&m.observed_endpoint))
        .unwrap_or_default();
      let lan_map: std::collections::HashMap<&str, &str> = snap.lan_peer_endpoints.iter()
        .map(|ep| (ep.public_key.as_str(), ep.lan_endpoint.as_str()))
        .collect();

      let peers: Vec<PeerInfo> = snap.members.iter()
        .filter(|m| !m.is_current_device && is_valid_wireguard_key(&m.public_key))
        .map(|m| {
          let peer_ip = extract_public_ip(&m.observed_endpoint);
          let is_same_nat = !self_observed_ip.is_empty()
            && !peer_ip.is_empty()
            && self_observed_ip == peer_ip;
          let lan_ep = if is_same_nat {
            lan_map.get(m.public_key.as_str()).copied().unwrap_or("")
          } else {
            ""
          };
          let display_endpoint = if !lan_ep.is_empty() {
            lan_ep.to_string()
          } else {
            m.observed_endpoint.clone()
          };
          PeerInfo {
            hostname: m.hostname.clone(),
            assigned_ip: m.assigned_ip.clone(),
            endpoint: display_endpoint,
            public_key: m.public_key.clone(),
            presence_status: m.presence_status,
            last_handshake_at: None,
          }
        })
        .collect();
      let peer_count = peers.len() as u32;

      let mut s = state.lock().await;
      s.network_name = snap.network.name.clone();
      s.subnet = snap.network.subnet.clone();
      s.peer_count = peer_count;
      s.peers = peers;
      s.last_sync_at = Some(chrono::Local::now().to_rfc3339());
      if let Some(self_member) = snap.members.iter().find(|m| m.is_current_device) {
        if !self_member.hostname.is_empty() {
          s.hostname = self_member.hostname.clone();
        }
      }
    }

    // Refresh live handshake stats
    let sdir = storage_dir.clone();
    let nid = network_id.clone();
    let stats_result = tokio::task::spawn_blocking(move || {
      yat_wireguard::stats::get_runtime_stats(yat_wireguard::types::RuntimeStatsParams {
        storage_dir: sdir,
        network_id: nid,
      })
    }).await;

    if let Ok(Ok(Some(stats))) = stats_result {
      let hs_map: std::collections::HashMap<String, Option<f64>> = stats.peers.into_iter()
        .map(|p| (p.public_key, p.last_handshake_at))
        .collect();
      let mut s = state.lock().await;
      for peer in s.peers.iter_mut() {
        if let Some(Some(hs)) = hs_map.get(&peer.public_key) {
          peer.last_handshake_at = Some(*hs);
        }
      }
    }
  }
}

/// Enter a 30-minute deferred wait. Can be interrupted by the manual
/// reconnect button (via `reconnect_notify`).
async fn enter_deferred_wait(
  sync_state: &Arc<Mutex<SyncState>>,
  log_buffer: &Arc<Mutex<LogBuffer>>,
  reconnect_notify: &Arc<Notify>,
) {
  warn!("All reconnect attempts exhausted. Waiting 30 minutes before retry.");
  log_buffer.lock().await.push("WARN",
    "All reconnect attempts exhausted. Will retry in 30 minutes (or click Reconnect).".into(),
  );

  // Publish the scheduled reconnect time so the dashboard can show a countdown
  let next_at = chrono::Local::now() + chrono::Duration::seconds(DEFERRED_RECONNECT_SECS as i64);
  {
    let mut s = sync_state.lock().await;
    s.reconnect_at = Some(next_at.to_rfc3339());
  }

  // Sleep, but wake up if the user clicks Reconnect
  interruptible_sleep(DEFERRED_RECONNECT_SECS, reconnect_notify).await;

  // Clear the scheduled time (we're about to retry)
  {
    let mut s = sync_state.lock().await;
    s.reconnect_at = None;
  }

  log_buffer.lock().await.push("INFO", "Deferred wait ended. Retrying connection...".into());
}

/// Sleep for `secs` seconds, but wake up early if `notify` is triggered.
async fn interruptible_sleep(secs: u64, notify: &Arc<Notify>) {
  tokio::select! {
    _ = tokio::time::sleep(tokio::time::Duration::from_secs(secs)) => {}
    _ = notify.notified() => {
      info!("Reconnect wait interrupted by manual trigger.");
    }
  }
}

/// Background task: monitor certificate expiry and warn the user.
///
/// Checks every hour. If the certificate is within 3 days of expiry,
/// logs a warning. Auto-renewal requires re-import from the desktop app
/// (invite tokens are single-use; OAuth is not used in the Linux CLI).
async fn cert_rotation_monitor(
  cert_manager: Arc<CertManager>,
  log_buffer: Arc<Mutex<LogBuffer>>,
  _config: Option<Arc<AppConfig>>,
) {
  loop {
    tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;

    if !cert_manager.needs_renewal() {
      continue;
    }

    match cert_manager.cert_info() {
      Ok(info) => {
        let days = info.days_remaining.unwrap_or(0);
        warn!("Certificate expires in {} days! Re-import from desktop app to renew.", days);
        log_buffer.lock().await.push(
          "WARN",
          format!("Certificate expires in {} days. Re-import from desktop app to renew.", days),
        );
      }
      Err(_) => {
        log_buffer.lock().await.push(
          "WARN",
          "No valid certificate found. Import from desktop app.".into(),
        );
      }
    }
  }
}

/// Install an OS-level signal handler using a dedicated thread.
///
/// This spawns a background thread that blocks on `sigwait` for SIGINT
/// and SIGTERM. When either signal is received, the thread cleans up
/// the WireGuard interface (yat0) before force-terminating the process.
///
/// This is necessary because `tokio::signal::ctrl_c()` is unreliable under
/// `sudo` — the signal may not be delivered to the tokio runtime, especially
/// when background tasks are blocking worker threads.
///
/// SIGTERM handling is also required for container environments (Docker),
/// where `docker stop` and `docker kill` send SIGTERM by default.
fn start_signal_handler() {
  // SIGINT and SIGTERM are already blocked in main() BEFORE the tokio
  // runtime was created.  All threads (including tokio workers) inherited
  // the blocked mask.  This sigwait thread is the ONLY thread that can
  // catch these signals.
  std::thread::spawn(|| {
    unsafe {
      let mut set: libc::sigset_t = std::mem::zeroed();
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::sigaddset(&mut set, libc::SIGTERM);

      // Block until SIGINT or SIGTERM is received
      let mut sig: libc::c_int = 0;
      libc::sigwait(&set, &mut sig);

      eprintln!("\nShutting down.");

      // Clean up WireGuard interface before exiting.
      // `process::exit(0)` does NOT run Drop destructors, so we must
      // explicitly remove the yat0 interface here.
      perform_shutdown_cleanup();

      // Force-exit immediately. Background tasks (sync loop, reconnect,
      // cert monitor) may hold blocking resources and never respond to
      // cancellation. `process::exit(0)` terminates without waiting.
      std::process::exit(0);
    }
  });
}

/// Clean up all WireGuard interfaces before process exit.
///
/// Uses yat-network's `teardown_all_networks` to scan the WG storage
/// directory and remove all known network interfaces. This is more
/// robust than the old approach (reading state.json for a single network_id)
/// because it handles multiple networks and doesn't depend on state.json.
///
/// Called from the signal handler thread — creates a temporary tokio runtime
/// since `teardown_all_networks` is async.
fn perform_shutdown_cleanup() {
  let storage_dir = crate::config::config_dir()
    .join("wireguard")
    .to_string_lossy()
    .to_string();

  eprintln!("Cleaning up WireGuard interfaces...");

  // Create a temporary tokio runtime for the async teardown call.
  // This runs on the signal handler thread, not the main tokio runtime.
  let rt = match tokio::runtime::Builder::new_current_thread().build() {
    Ok(rt) => rt,
    Err(e) => {
      eprintln!("Warning: failed to create runtime for teardown: {e}");
      return;
    }
  };

  let engine = yat_network::wg::engine::create_engine(None);
  rt.block_on(yat_network::teardown_all_networks(&engine, &storage_dir));

  // Remove stale state file
  let state_path = crate::config::config_dir().join("state.json");
  let _ = std::fs::remove_file(&state_path);
}
