use anyhow::{Context, Result};
use std::{
  io::{Read, Write},
  os::unix::fs::{OpenOptionsExt, PermissionsExt},
  path::Path,
};

const MAX_IMPORT: usize = 256 * 1024;

pub async fn run(file: &Path) -> Result<()> {
  let mut content = String::new();
  std::fs::File::open(file)?
    .take((MAX_IMPORT + 1) as u64)
    .read_to_string(&mut content)?;
  validate_content(&content)?;
  crate::client_owner::dispatch(crate::client_owner::Command::Import { content }).await
}

/// Validate before stopping the current generation. This function never writes.
pub(crate) fn validate_content(content: &str) -> Result<serde_json::Value> {
  anyhow::ensure!(content.len() <= MAX_IMPORT, "Share package exceeds 256 KiB");
  let share: serde_json::Value = serde_json::from_str(content)?;
  let required = |section: &str, key: &str, alias: &str| -> Result<&str> {
    share
      .get(section)
      .and_then(|v| v.get(key).or_else(|| v.get(alias)))
      .and_then(|v| v.as_str())
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .with_context(|| format!("Missing or empty {section}.{key}"))
  };
  required("edge", "host", "host")?;
  let port = share["edge"]["port"]
    .as_u64()
    .context("Invalid edge.port")?;
  anyhow::ensure!(port > 0 && port <= u16::MAX as u64, "Invalid edge.port");
  required("network", "networkId", "network_id")?;
  required("certificates", "caCert", "caCert")?;
  anyhow::ensure!(
    share["certificates"].get("clientCert").is_none()
      && share["certificates"].get("clientKey").is_none(),
    "Full certificate sharing is no longer supported; export a network invite package"
  );
  required("network", "inviteToken", "invite_token")?;
  // Deserialize the exact normalized configuration before changing disk state.
  let _: crate::config::AppConfig = serde_json::from_value(normalize(&share, "validation-device"))?;
  Ok(share)
}

fn normalize(share: &serde_json::Value, device_id: &str) -> serde_json::Value {
  let edge = &share["edge"];
  let network = &share["network"];
  serde_json::json!({
    "edge": { "host": edge["host"], "port": edge["port"],
      "edge_id": edge.get("edgeId").or_else(|| edge.get("edge_id")),
      "server_name": edge.get("serverName").or_else(|| edge.get("server_name")) },
    "identity": { "device_id": device_id, "hostname": crate::config::get_hostname() },
    "network": { "network_id": network.get("networkId").or_else(|| network.get("network_id")),
      "invite_token": network.get("inviteToken").or_else(|| network.get("invite_token")) }
  })
}

/// Called only while the process lock is held and the previous generation drained.
pub(crate) fn import_content(content: &str) -> Result<()> {
  let share = validate_content(content)?;
  let directory = crate::config::config_dir();
  let certs = directory.join("certs");
  std::fs::create_dir_all(&certs)?;
  std::fs::set_permissions(&certs, std::fs::Permissions::from_mode(0o700))?;
  let identity_file = directory.join("device-id");
  let device_id = match std::fs::read_to_string(&identity_file) {
    Ok(id) if id.trim().starts_with("device-") && id.trim().len() == 39 => id.trim().to_owned(),
    _ => {
      let mut bytes = [0u8; 16];
      std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
      let id = format!(
        "device-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
      );
      write_private(&identity_file, id.as_bytes())?;
      id
    }
  };
  let config = serde_json::to_vec_pretty(&normalize(&share, &device_id))?;
  // A failed multi-file publication blocks subsequent startup until a complete
  // import succeeds. Never connect using a mixed old/new certificate bundle.
  let marker = directory.join("import.pending");
  write_private(&marker, b"Incomplete import; re-import share package")?;
  write_private(&certs.join("ca.crt"), share["certificates"]["caCert"].as_str().unwrap().as_bytes())?;
  // A network invite obtains a fresh Device certificate under this device's key.
  for filename in ["client.crt", "client.key", "enrollment.pending"] {
    match std::fs::remove_file(certs.join(filename)) {
      Ok(()) => (),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
      Err(e) => return Err(e.into()),
    }
  }
  write_private(&directory.join("config.json"), &config)?;
  // Remove stale WireGuard identity (keypair + per-network state) so the next
  // connect generates a fresh keypair. Without this, re-importing after peer
  // deletion reuses the old public key, causing handshake failures when the
  // frontend re-added the peer with a different key.
  let wg_dir = directory.join("wireguard");
  if wg_dir.exists() {
    std::fs::remove_dir_all(&wg_dir)?;
  }
  std::fs::remove_file(marker)?;
  println!("Network package imported successfully.");
  Ok(())
}

pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
  let temporary = path.with_extension("import-tmp");
  let mut file = std::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .truncate(true)
    .mode(0o600)
    .custom_flags(libc::O_NOFOLLOW)
    .open(&temporary)?;
  file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
  file.write_all(data)?;
  file.sync_all()?;
  std::fs::rename(temporary, path)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn rejects_invalid_import_before_any_write() {
    let valid = serde_json::json!({"edge":{"host":"edge.test","port":443},
      "network":{"networkId":"net","inviteToken":"token"},"certificates":{"caCert":"ca"}});
    assert!(validate_content(&valid.to_string()).is_ok());
    for (section, field, value) in [
      ("edge", "port", serde_json::json!(65536)),
      ("network", "networkId", serde_json::json!("")),
      ("certificates", "clientCert", serde_json::json!("cert")),
    ] {
      let mut invalid = valid.clone();
      invalid[section][field] = value;
      assert!(validate_content(&invalid.to_string()).is_err());
    }
    assert!(validate_content(&"x".repeat(MAX_IMPORT + 1)).is_err());
    let mut old_full_export = valid.clone();
    old_full_export["certificates"]["clientCert"] = serde_json::json!("owner certificate");
    old_full_export["certificates"]["clientKey"] = serde_json::json!("owner private key");
    assert!(validate_content(&old_full_export.to_string()).is_err());
    let mut missing_invite = valid;
    missing_invite["network"].as_object_mut().unwrap().remove("inviteToken");
    assert!(validate_content(&missing_invite.to_string()).is_err());
  }
}
