use anyhow::Result;
use std::path::Path;
use tracing::info;

/// Import a shared network package from the desktop UI.
///
/// Reads share.json, validates required fields, and writes config to
/// `~/.config/yat-cli/`.
///
/// ## Share package format (v1)
///
/// Two export modes are supported:
///
/// **Full cert mode** — the share contains pre-built mTLS certificates:
/// ```json
/// {
///   "version": 1,
///   "edge": { "host", "port", "serverName", "edgeId" },
///   "network": { "networkId" },
///   "certificates": { "caCert", "clientCert", "clientKey" }
/// }
/// ```
///
/// **Token mode** — the share contains an invite token; the client cert
/// will be bootstrapped on first `serve`:
/// ```json
/// {
///   "version": 1,
///   "edge": { "host", "port", "serverName", "edgeId" },
///   "network": { "networkId", "inviteToken" },
///   "certificates": { "caCert" }
/// }
/// ```
///
/// ## Device identity
///
/// The source `deviceId` from the share is intentionally **not** imported.
/// Instead a stable local device ID is generated once and persisted in
/// `~/.config/yat-cli/device-id`.  This ensures:
/// - Each physical device gets a unique identity on the edge
/// - Re-importing the same share.json does not create duplicate peers
pub fn run(file: &Path) -> Result<()> {
  info!("Importing share package from: {}", file.display());

  let content = std::fs::read_to_string(file)?;
  let share: serde_json::Value = serde_json::from_str(&content)?;

  // Validate required sections
  let edge = share
    .get("edge")
    .ok_or_else(|| anyhow::anyhow!("share.json missing 'edge' section"))?;
  let certificates = share
    .get("certificates")
    .ok_or_else(|| anyhow::anyhow!("share.json missing 'certificates' section"))?;
  let network = share
    .get("network")
    .ok_or_else(|| anyhow::anyhow!("share.json missing 'network' section"))?;

  let config_dir = config_dir();
  std::fs::create_dir_all(&config_dir)?;

  // ── Local device ID (stable across re-imports) ──────────────────────
  let local_device_id = get_or_create_local_device_id(&config_dir)?;
  info!("Local device ID: {local_device_id}");

  // ── Detect export mode ──────────────────────────────────────────────
  let has_client_cert = certificates.get("clientCert").and_then(|v| v.as_str()).is_some();
  let is_full_cert_mode = has_client_cert;

  // ── Normalize edge section (camelCase → snake_case) ─────────────────
  let edge = serde_json::json!({
    "host": edge.get("host").cloned().unwrap_or_default(),
    "port": edge.get("port").cloned().unwrap_or_default(),
    "edge_id": edge.get("edgeId").or_else(|| edge.get("edge_id")).cloned(),
    "server_name": edge.get("serverName").or_else(|| edge.get("server_name")).cloned(),
  });

  // ── Identity (local only — no user info from share) ─────────────────
  let hostname = get_hostname();
  let identity = serde_json::json!({
    "device_id": local_device_id,
    "hostname": hostname,
  });

  // ── Network section ─────────────────────────────────────────────────
  let network_id = network
    .get("networkId")
    .or_else(|| network.get("network_id"))
    .and_then(|v| v.as_str())
    .unwrap_or("")
    .trim()
    .to_string();
  if network_id.is_empty() {
    anyhow::bail!("share.json 'network' section missing networkId");
  }

  let invite_token = network
    .get("inviteToken")
    .or_else(|| network.get("invite_token"))
    .and_then(|v| v.as_str())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());

  let mut network_json = serde_json::json!({
    "network_id": network_id,
  });
  if let Some(ref token) = invite_token {
    network_json["invite_token"] = serde_json::json!(token);
  }

  // ── Write config.json ───────────────────────────────────────────────
  let config = serde_json::json!({
    "edge": edge,
    "identity": identity,
    "network": network_json,
  });
  std::fs::write(
    config_dir.join("config.json"),
    serde_json::to_string_pretty(&config)?,
  )?;

  // ── Certificates ────────────────────────────────────────────────────
  let certs_dir = config_dir.join("certs");
  std::fs::create_dir_all(&certs_dir)?;

  // Always write CA cert
  write_cert_if_present(&certs_dir, "ca.crt", certificates, "caCert")?;

  if is_full_cert_mode {
    // Full cert mode: import client cert + key
    write_cert_if_present(&certs_dir, "client.crt", certificates, "clientCert")?;
    write_cert_if_present(&certs_dir, "client.key", certificates, "clientKey")?;
    info!("Imported full certificate bundle.");
  } else {
    // Token mode: only CA cert; remove any stale client cert/key from a previous import
    let _ = std::fs::remove_file(certs_dir.join("client.crt"));
    let _ = std::fs::remove_file(certs_dir.join("client.key"));
    info!("Imported CA certificate (client cert will be bootstrapped via invite token on serve).");
  }

  info!("Import complete. Config written to {}", config_dir.display());
  println!("✓ Network package imported successfully.");
  println!("  Device ID: {local_device_id}");
  if is_full_cert_mode {
    println!("  Mode: Full certificates (ready to connect).");
  } else {
    println!("  Mode: Invite token (certificate will be obtained on first connect).");
  }
  println!("  Run `yat-cli serve` to connect.");
  Ok(())
}

/// Read or create a stable local device ID persisted in `config_dir/device-id`.
///
/// The ID format is `device-{32 hex chars}` (matching the desktop convention).
/// On first call a new ID is generated from `/dev/urandom` and written to disk.
/// Subsequent calls (or re-imports) return the same ID so the edge sees one
/// stable peer per physical device.
fn get_or_create_local_device_id(config_dir: &Path) -> Result<String> {
  let path = config_dir.join("device-id");
  if let Ok(existing) = std::fs::read_to_string(&path) {
    let trimmed = existing.trim().to_string();
    if trimmed.starts_with("device-") && trimmed.len() == 39 {
      return Ok(trimmed);
    }
  }
  // Generate a new device ID from 16 random bytes → 32 hex chars.
  let mut buf = [0u8; 16];
  if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
    use std::io::Read;
    let _ = std::io::BufReader::new(&mut f).read_exact(&mut buf);
  } else {
    // Fallback: use process id + timestamp for uniqueness.
    let ts = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos();
    let pid = std::process::id() as u128;
    buf[..8].copy_from_slice(&ts.to_le_bytes());
    buf[8..10].copy_from_slice(&pid.to_le_bytes());
  }
  let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
  let device_id = format!("device-{hex}");
  std::fs::write(&path, &device_id)?;
  info!("Generated new local device ID: {device_id}");
  Ok(device_id)
}

fn config_dir() -> std::path::PathBuf {
  let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
  std::path::PathBuf::from(home).join(".config/yat-cli")
}

fn get_hostname() -> String {
  std::fs::read_to_string("/etc/hostname")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .or_else(|| std::env::var("HOSTNAME").ok())
    .unwrap_or_else(|| "yat-cli-linux".to_string())
}

fn write_cert_if_present(
  dir: &Path,
  filename: &str,
  certificates: &serde_json::Value,
  key: &str,
) -> Result<()> {
  if let Some(value) = certificates.get(key).and_then(|v| v.as_str()) {
    std::fs::write(dir.join(filename), value)?;
    info!("Wrote certificate: {}", filename);
  }
  Ok(())
}
