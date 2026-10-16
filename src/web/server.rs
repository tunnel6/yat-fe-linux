use anyhow::Result;
use axum::{
  extract::State,
  response::{Html, IntoResponse},
  routing::{get, post},
  Json, Router,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use yat_transport::EdgeClient;

use crate::config::AppConfig;
use crate::daemon::loop_runner::{LogBuffer, SyncState};
use crate::managers::cert::CertManager;

// ─── App state shared across all handlers ────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
  pub sync: Arc<Mutex<SyncState>>,
  pub logs: Arc<Mutex<LogBuffer>>,
  pub config: Arc<Mutex<Option<Arc<AppConfig>>>>,
  pub cert_manager: Arc<CertManager>,
  /// Trigger a service restart (config reload + Edge reconnect).
  pub restart_tx: Arc<tokio::sync::mpsc::Sender<()>>,
  /// Wake up the reconnect loop for manual reconnect.
  pub reconnect_notify: Arc<tokio::sync::Notify>,
  /// Edge client reference (set after successful Edge connection).
  pub edge_client: Arc<Mutex<Option<Arc<EdgeClient>>>>,
}

// ─── Server entry point ─────────────────────────────────────────────────────

pub async fn start(listen: &str, app_state: AppState) -> Result<()> {
  let app = Router::new()
    .route("/", get(index_handler))
    .route("/tailwindcss.js", get(static_handler))
    .route("/api/status", get(api_status))
    .route("/api/peers", get(api_peers))
    .route("/api/logs", get(api_logs))
    .route("/api/cert", get(api_cert))
    .route("/api/cert/renew", post(api_cert_renew))
    .route("/api/config", get(api_config))
    .route("/api/import", post(api_import))
    .route("/api/reconnect", post(api_reconnect))
    .route("/api/address", post(api_update_address))
    .with_state(app_state);

  let listener = tokio::net::TcpListener::bind(listen).await?;
  info!("Web UI listening on http://{}", listen);

  // Graceful shutdown is handled by the OS-level signal thread in serve.rs,
  // which calls process::exit(0) on SIGINT. No need for with_graceful_shutdown
  // here because tokio::signal::ctrl_c() is unreliable under sudo.
  axum::serve(listener, app).await?;

  info!("Web UI shut down gracefully.");
  Ok(())
}

// ─── HTML handler ────────────────────────────────────────────────────────────

async fn index_handler() -> Html<&'static str> {
  Html(include_str!("../../web/dashboard.html"))
}

async fn static_handler() -> impl IntoResponse {
  (
    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
    include_str!("../../web/tailwindcss-3.4.17.js"),
  )
}

// ─── API handlers ────────────────────────────────────────────────────────────

/// GET /api/status — network overview + status.
async fn api_status(State(state): State<AppState>) -> Json<serde_json::Value> {
  let s = state.sync.lock().await;
  Json(serde_json::json!({
    "network_id": s.network_id,
    "network_name": s.network_name,
    "subnet": s.subnet,
    "member_id": s.member_id,
    "self_address": s.self_address,
    "hostname": s.hostname,
    "interface_name": s.interface_name,
    "peer_count": s.peer_count,
    "connected": s.connected,
    "last_sync_at": s.last_sync_at,
    "reconnect_at": s.reconnect_at,
  }))
}

/// GET /api/peers — peer list with details.
async fn api_peers(State(state): State<AppState>) -> Json<serde_json::Value> {
  let s = state.sync.lock().await;
  Json(serde_json::json!({
    "peers": s.peers,
  }))
}

/// GET /api/logs — recent log entries.
async fn api_logs(State(state): State<AppState>) -> Json<serde_json::Value> {
  let logs = state.logs.lock().await;
  Json(serde_json::json!({
    "logs": logs.entries(),
  }))
}

/// GET /api/cert — client certificate info.
async fn api_cert(State(state): State<AppState>) -> Json<serde_json::Value> {
  match state.cert_manager.cert_info() {
    Ok(cert) => Json(serde_json::json!({ "cert": cert })),
    Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
  }
}

/// POST /api/cert/renew — trigger certificate renewal.
///
/// Since CLI doesn't have OAuth tokens, this checks cert status and
/// advises the user to re-import from the desktop app if renewal is needed.
async fn api_cert_renew(State(state): State<AppState>) -> Json<serde_json::Value> {
  match state.cert_manager.cert_info() {
    Ok(info) => {
      let days = info.days_remaining.unwrap_or(0);
      if days <= 3 {
        Json(serde_json::json!({
          "success": false,
          "needs_renewal": true,
          "message": format!("Certificate expires in {} days. Please re-import from the desktop app to renew.", days),
        }))
      } else {
        Json(serde_json::json!({
          "success": true,
          "needs_renewal": false,
          "message": format!("Certificate is valid for {} more days. No renewal needed.", days),
        }))
      }
    }
    Err(e) => Json(serde_json::json!({
      "success": false,
      "needs_renewal": true,
      "message": format!("No valid certificate found: {}. Please import from the desktop app.", e),
    })),
  }
}

/// GET /api/config — current config (edge + identity).
async fn api_config(State(state): State<AppState>) -> Json<serde_json::Value> {
  let config = state.config.lock().await;
  let sync = state.sync.lock().await;
  match &*config {
    Some(c) => {
      let hostname = if sync.hostname.is_empty() {
        c.identity.hostname.clone()
      } else {
        Some(sync.hostname.clone())
      };
      Json(serde_json::json!({
        "edge": {
          "host": c.edge.host,
          "port": c.edge.port,
          "edge_id": c.edge.edge_id,
          "server_name": c.edge.server_name,
        },
        "identity": {
          "device_id": c.identity.device_id,
          "hostname": hostname,
        },
        "network_id": c.network_id(),
        "invite_token": c.invite_token().is_some(),
        "config_path": crate::config::config_file_path().display().to_string(),
      }))
    }
    None => Json(serde_json::json!({
      "edge": null,
      "identity": null,
      "network_id": null,
      "config_path": crate::config::config_file_path().display().to_string(),
      "error": "No config loaded. Import a share.json to get started.",
    })),
  }
}

/// POST /api/import — import a share.json package and restart service.
///
/// Accepts JSON body: `{ "content": "<share.json text>" }`
async fn api_import(
  State(state): State<AppState>,
  Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
  let content = match body.get("content").and_then(|v| v.as_str()) {
    Some(c) => c,
    None => return Json(serde_json::json!({ "success": false, "error": "Missing 'content' field" })),
  };

  // Write content to a temp file, then delegate to import::run
  let tmp = std::env::temp_dir().join("yat-cli-import.json");
  if let Err(e) = std::fs::write(&tmp, content) {
    return Json(serde_json::json!({ "success": false, "error": format!("Failed to write temp file: {e}") }));
  }

  match crate::cli::import::run(&tmp) {
    Ok(()) => {
      let _ = std::fs::remove_file(&tmp);

      // Reload config in background
      {
        let mut cfg = state.config.lock().await;
        *cfg = crate::config::load_config().ok().map(Arc::new);
      }

      state.logs.lock().await.push("INFO", "Import successful, restarting service...".into());

      // Trigger restart (config reload + Edge reconnect)
      let _ = state.restart_tx.send(()).await;

      Json(serde_json::json!({ "success": true, "message": "Import successful. Restarting service..." }))
    }
    Err(e) => {
      let _ = std::fs::remove_file(&tmp);
      Json(serde_json::json!({ "success": false, "error": e.to_string() }))
    }
  }
}

/// POST /api/reconnect — manually trigger Edge reconnection.
///
/// Wakes up the reconnect loop immediately (interrupts any back-off wait).
async fn api_reconnect(State(state): State<AppState>) -> Json<serde_json::Value> {
  let s = state.sync.lock().await;
  if s.connected {
    return Json(serde_json::json!({
      "success": false,
      "message": "Already connected.",
    }));
  }
  // Wake up the reconnect loop
  state.reconnect_notify.notify_one();
  // Clear the scheduled reconnect time
  drop(s);
  let mut s = state.sync.lock().await;
  s.reconnect_at = None;
  Json(serde_json::json!({
    "success": true,
    "message": "Reconnect triggered. Check status in a few seconds.",
  }))
}

/// POST /api/address — update this device's IP address in the network.
///
/// Accepts JSON body: `{ "new_ip": "10.0.0.5" }`
/// Calls Edge `update_wg_member` to apply the change server-side.
async fn api_update_address(
  State(state): State<AppState>,
  Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
  let new_ip = match body.get("new_ip").and_then(|v| v.as_str()).map(|s| s.trim().to_string()) {
    Some(ip) if !ip.is_empty() => ip,
    _ => return Json(serde_json::json!({ "success": false, "error": "Missing or empty 'new_ip' field" })),
  };

  let (network_id, member_id) = {
    let s = state.sync.lock().await;
    (s.network_id.clone(), s.member_id.clone())
  };
  if network_id.is_empty() || member_id.is_empty() {
    return Json(serde_json::json!({ "success": false, "error": "Not connected to any network" }));
  }

  let edge_client = {
    let guard = state.edge_client.lock().await;
    guard.clone()
  };
  let Some(client) = edge_client else {
    return Json(serde_json::json!({ "success": false, "error": "Edge client not available" }));
  };

  match client.update_wg_member(&network_id, &member_id, &new_ip, "").await {
    Ok(resp) if resp.success => {
      // Update local state so dashboard reflects the new address immediately
      let mut s = state.sync.lock().await;
      s.self_address = new_ip.clone();
      info!("Address updated to {new_ip}");
      state.logs.lock().await.push("INFO", format!("Address updated to {new_ip}"));
      Json(serde_json::json!({ "success": true, "message": format!("Address updated to {new_ip}") }))
    }
    Ok(_resp) => {
      warn!("Address update failed: member_id={member_id}, new_ip={new_ip}");
      Json(serde_json::json!({ "success": false, "error": "Edge rejected the update" }))
    }
    Err(e) => {
      warn!("Address update error: {e}");
      Json(serde_json::json!({ "success": false, "error": format!("Update failed: {e}") }))
    }
  }
}
