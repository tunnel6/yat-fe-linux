use anyhow::Result;
use std::path::Path;
use tracing::info;

/// Import a shared network package from the desktop UI.
///
/// Reads share.json, validates certificates, and writes config to
/// `~/.config/yat-cli/`.
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

  // Normalize camelCase (from desktop share.json) → snake_case for config.json
  let edge = serde_json::json!({
    "host": edge.get("host").cloned().unwrap_or_default(),
    "port": edge.get("port").cloned().unwrap_or_default(),
    "edge_id": edge.get("edgeId").or_else(|| edge.get("edge_id")).cloned(),
    "server_name": edge.get("serverName").or_else(|| edge.get("server_name")).cloned(),
  });
  let identity = serde_json::json!({
    "user_id": identity.get("userId").or_else(|| identity.get("user_id")).cloned().unwrap_or_default(),
    "device_id": identity.get("deviceId").or_else(|| identity.get("device_id")).cloned().unwrap_or_default(),
    "username": identity.get("username").cloned(),
    "hostname": identity.get("hostname").cloned(),
  });
  let network = serde_json::json!({
    "network_id": network.get("networkId").or_else(|| network.get("network_id")).cloned().unwrap_or_default(),
  });

  let config = serde_json::json!({
    "edge": edge,
    "identity": identity,
    "network": network,
  });
  std::fs::write(
    config_dir.join("config.json"),
    serde_json::to_string_pretty(&config)?,
  )?;

  // Write certificates
  let certs_dir = config_dir.join("certs");
  std::fs::create_dir_all(&certs_dir)?;
  write_cert_if_present(&certs_dir, "client.crt", certificates, "clientCert")?;
  write_cert_if_present(&certs_dir, "client.key", certificates, "clientKey")?;
  write_cert_if_present(&certs_dir, "ca.crt", certificates, "caCert")?;
  write_cert_if_present(&certs_dir, "captain-ca.crt", certificates, "captainCA")?;

  info!("Import complete. Config written to {}", config_dir.display());
  println!("✓ Network package imported successfully.");
  println!("  Run `yat-cli start` to connect.");
  Ok(())
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
