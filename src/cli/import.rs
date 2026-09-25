use anyhow::Result;
use std::path::Path;
use tracing::info;

/// Import a shared network package from the desktop UI.
///
/// Reads share.json, validates required fields, and writes config to
/// `~/.config/yat-cli/`.
///
/// ## Certificate bootstrap
///
/// **New format** (with `auth` section): the share contains an OAuth token
/// and provider.  The CLI persists these in config.json; on first `serve`
/// it uses them to request its own mTLS certificate from the Edge via
/// `CertRequest`, ensuring the certificate is bound to the local deviceId.
///
/// **Legacy format** (with `certificates.clientCert`): the share contains
/// pre-built mTLS certificates.  These are imported as-is for backward
/// compatibility.
///
/// ## Device identity
///
/// The source `deviceId` from share.json is intentionally **not** imported.
/// Instead a stable local device ID is generated once and persisted in
/// `~/.config/yat-cli/device-id`.  This ensures:
/// - Each physical device gets a unique identity on the edge
/// - Re-importing the same share.json does not create duplicate peers
pub fn run(file: &Path) -> Result<()> {
  info!("Importing share package from: {}", file.display());

  let content = std::fs::read_to_string(file)?;
  let share: serde_json::Value = serde_json::from_str(&content)?;

  // Validate required fields
  let edge = share
    .get("edge")
    .ok_or_else(|| anyhow::anyhow!("share.json missing 'edge' section"))?;
  let identity = share
    .get("identity")
    .ok_or_else(|| anyhow::anyhow!("share.json missing 'identity' section"))?;
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

  // ── Auth credentials (new format) ───────────────────────────────────
  let auth_section = share.get("auth");
  let is_new_format = auth_section.is_some();

  // Normalize camelCase (from desktop share.json) → snake_case for config.json
  let edge = serde_json::json!({
    "host": edge.get("host").cloned().unwrap_or_default(),
    "port": edge.get("port").cloned().unwrap_or_default(),
    "edge_id": edge.get("edgeId").or_else(|| edge.get("edge_id")).cloned(),
    "server_name": edge.get("serverName").or_else(|| edge.get("server_name")).cloned(),
  });
  let identity = serde_json::json!({
    // userId is kept for display/lookup, but deviceId is always the local one
    "user_id": identity.get("userId").or_else(|| identity.get("user_id")).cloned().unwrap_or_default(),
    "device_id": local_device_id,
    "username": identity.get("username").cloned(),
    "hostname": identity.get("hostname").cloned(),
  });
  let network = serde_json::json!({
    "network_id": network.get("networkId").or_else(|| network.get("network_id")).cloned().unwrap_or_default(),
  });

  // Build config — include auth section only for new-format shares
  let mut config = serde_json::json!({
    "edge": edge,
    "identity": identity,
    "network": network,
  });
  if let Some(auth) = auth_section {
    let oauth_token = auth
      .get("oauthToken")
      .or_else(|| auth.get("oauth_token"))
      .and_then(|v| v.as_str())
      .unwrap_or("");
    let oauth_provider = auth
      .get("oauthProvider")
      .or_else(|| auth.get("oauth_provider"))
      .and_then(|v| v.as_str())
      .unwrap_or("");
    if oauth_token.is_empty() || oauth_provider.is_empty() {
      anyhow::bail!("share.json 'auth' section missing oauthToken or oauthProvider");
    }
    config["auth"] = serde_json::json!({
      "oauth_token": oauth_token,
      "oauth_provider": oauth_provider,
    });
  }
  std::fs::write(
    config_dir.join("config.json"),
    serde_json::to_string_pretty(&config)?,
  )?;

  // ── Certificates ────────────────────────────────────────────────────
  let certs_dir = config_dir.join("certs");
  std::fs::create_dir_all(&certs_dir)?;

  if is_new_format {
    // New format: only CA certs (client cert will be bootstrapped on serve)
    write_cert_if_present(&certs_dir, "ca.crt", certificates, "caCert")?;
    write_cert_if_present(&certs_dir, "captain-ca.crt", certificates, "captainCA")?;
    // Remove any stale client cert/key from a previous import
    let _ = std::fs::remove_file(certs_dir.join("client.crt"));
    let _ = std::fs::remove_file(certs_dir.join("client.key"));
    info!("Imported CA certificates (client cert will be bootstrapped on serve).");
  } else {
    // Legacy format: import full cert bundle
    write_cert_if_present(&certs_dir, "client.crt", certificates, "clientCert")?;
    write_cert_if_present(&certs_dir, "client.key", certificates, "clientKey")?;
    write_cert_if_present(&certs_dir, "ca.crt", certificates, "caCert")?;
    write_cert_if_present(&certs_dir, "captain-ca.crt", certificates, "captainCA")?;
    info!("Imported legacy certificate bundle.");
  }

  info!("Import complete. Config written to {}", config_dir.display());
  println!("✓ Network package imported successfully.");
  println!("  Device ID: {local_device_id}");
  if is_new_format {
    println!("  Mode: OAuth token (certificate will be obtained on first connect).");
  } else {
    println!("  Mode: Pre-built certificates (legacy).");
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
