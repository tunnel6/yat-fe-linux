use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

use yat_wireguard::types::ClearNetworkStateParams;

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
  };
  if let Some(cfg) = config {
    let ss = Arc::clone(&sync_state);
    let lb = Arc::clone(&log_buffer);
    let rn = Arc::clone(&reconnect_notify);
    tokio::spawn(async move {
      connect_edge(cfg, ss, lb, rn).await;
    });
  } else {
    log_buffer.lock().await.push("INFO", "No config — import a share.json via Web UI.".into());
  }

  // 5. Restart listener — when import triggers restart, reconnect Edge
  let ss_restart = Arc::clone(&sync_state);
  let lb_restart = Arc::clone(&log_buffer);
  let cfg_restart = Arc::clone(&config_mutex);
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
        tokio::spawn(async move {
          connect_edge(cfg, ss, lb, rn).await;
        });
      }
    }
  });

  // 6. Certificate rotation monitor — checks every hour, warns if < 3 days
  let cert_mgr = Arc::clone(&cert_manager);
  let lb_cert = Arc::clone(&log_buffer);
  tokio::spawn(async move {
    cert_rotation_monitor(cert_mgr, lb_cert).await;
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
async fn connect_edge(
  cfg: Arc<AppConfig>,
  sync_state: Arc<Mutex<SyncState>>,
  log_buffer: Arc<Mutex<LogBuffer>>,
  reconnect_notify: Arc<Notify>,
) {
  log_buffer.lock().await.push("INFO", "Connecting to Edge...".into());

  // ── Initial connection ──────────────────────────────────────────────
  let mut wg = match WgManager::from_config((*cfg).clone()) {
    Ok(wg) => wg,
    Err(e) => {
      error!("Failed to init WgManager: {e}");
      log_buffer.lock().await.push("ERROR", format!("WgManager init failed: {e}"));
      return;
    }
  };

  let (mut network_id, mut storage_dir) = match wg.start().await {
    Ok(status) => {
      {
        let mut s = sync_state.lock().await;
        *s = SyncState::from_wg_status(&status);
      }
      log_buffer.lock().await.push("INFO", "Connected to Edge, network joined.".into());
      info!("Edge connection established.");
      (status.network_id.clone(), wg.certs().wg_storage_dir().to_string_lossy().to_string())
    }
    Err(e) => {
      error!("Edge connection failed: {e}");
      log_buffer.lock().await.push("ERROR", format!("Edge connection failed: {e}"));
      return;
    }
  };

  // ── Sync loop (blocks until Edge disconnects) ───────────────────────
  let edge_client = wg.edge_client().expect("edge client available after start");
  crate::daemon::loop_runner::run_sync_loop(
    edge_client, Arc::clone(&sync_state), Arc::clone(&log_buffer),
    network_id.clone(), storage_dir.clone(),
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
        // Interruptible back-off
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

    match new_wg.start().await {
      Ok(status) => {
        info!("Edge reconnected successfully on attempt #{attempt}.");
        log_buffer.lock().await.push("INFO", "Edge reconnected successfully.".into());

        {
          let mut s = sync_state.lock().await;
          *s = SyncState::from_wg_status(&status);
          s.reconnect_at = None;
        }

        // Reset for next potential disconnect
        network_id = status.network_id.clone();
        storage_dir = new_wg.certs().wg_storage_dir().to_string_lossy().to_string();
        backoff_secs = 1;
        attempt = 0;

        // Re-enter sync loop (blocks until next disconnect)
        let edge_client = new_wg.edge_client().expect("edge client available after start");
        crate::daemon::loop_runner::run_sync_loop(
          edge_client, Arc::clone(&sync_state), Arc::clone(&log_buffer),
          network_id.clone(), storage_dir.clone(),
        ).await;
      }
      Err(e) => {
        warn!("Reconnect attempt #{attempt} failed: {e}.");
        log_buffer.lock().await.push("WARN", format!(
          "Reconnect failed: {e}."
        ));

        if attempt >= MAX_RECONNECT_ATTEMPTS {
          // Exhausted all attempts — enter 30-minute deferred wait
          enter_deferred_wait(&sync_state, &log_buffer, &reconnect_notify).await;
          attempt = 0;
        } else {
          // Interruptible back-off
          info!("Backing off {backoff_secs}s before next attempt...");
          interruptible_sleep(backoff_secs, &reconnect_notify).await;
          backoff_secs = (backoff_secs * 2).min(max_backoff);
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

/// Background task: monitor certificate expiry and log warnings.
///
/// Checks every hour. If the certificate is within 3 days of expiry,
/// logs a warning. This implements the "daily retry" semantics by
/// checking frequently and advising renewal.
async fn cert_rotation_monitor(
  cert_manager: Arc<CertManager>,
  log_buffer: Arc<Mutex<LogBuffer>>,
) {
  loop {
    tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;

    if cert_manager.needs_renewal() {
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
}

/// Install an OS-level SIGINT handler using a dedicated thread.
///
/// This spawns a background thread that blocks on `sigwait` for SIGINT.
/// When Ctrl+C is pressed, the thread receives the signal directly from
/// the kernel and calls `process::exit(0)` to force-terminate the process.
///
/// This is necessary because `tokio::signal::ctrl_c()` is unreliable under
/// `sudo` — the signal may not be delivered to the tokio runtime, especially
/// when background tasks are blocking worker threads.
fn start_signal_handler() {
  std::thread::spawn(|| {
    unsafe {
      // Block SIGINT in the current thread's signal mask.
      // New threads inherit the signal mask, so the spawned thread will
      // also have SIGINT blocked. `sigwait` then atomically unblocks and
      // waits for the signal.
      let mut set: libc::sigset_t = std::mem::zeroed();
      libc::sigemptyset(&mut set);
      libc::sigaddset(&mut set, libc::SIGINT);
      libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());

      // Block until SIGINT is received
      let mut sig: libc::c_int = 0;
      libc::sigwait(&set, &mut sig);

      // Force-exit immediately. Background tasks (sync loop, reconnect,
      // cert monitor) may hold blocking resources and never respond to
      // cancellation. `process::exit(0)` terminates without waiting.
      eprintln!("\nShutting down.");
      std::process::exit(0);
    }
  });
}
