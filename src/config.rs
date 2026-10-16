//! Configuration data types and loading for yat-cli.
//!
//! Pure data layer: `AppConfig`, config file I/O, and the `config_dir()`
//! path helper. Business-level path resolution (certs, WG storage) lives
//! in `managers::CertManager`.
//!
//! ## Share package format (v1)
//!
//! The desktop UI exports two modes:
//! - **Full cert mode**: `edge` + `network(networkId)` + `certificates(caCert, clientCert, clientKey)`
//! - **Token mode**: `edge` + `network(networkId, inviteToken)` + `certificates(caCert)`
//!
//! There is no `identity` or `auth` section — the CLI generates its own
//! device identity locally and uses invite tokens for cert bootstrap.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── Config types ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
  pub edge: EdgeConfig,
  pub identity: IdentityConfig,
  pub network: Option<NetworkConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeConfig {
  pub host: String,
  pub port: u16,
  #[serde(default)]
  pub edge_id: Option<String>,
  #[serde(default)]
  pub server_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityConfig {
  pub device_id: String,
  #[serde(default)]
  pub hostname: Option<String>,
}

/// Network configuration imported from a share package.
///
/// `invite_token` is present in token mode (client cert will be bootstrapped
/// on first serve). It is `None` in full cert mode (certs already imported).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
  pub network_id: String,
  #[serde(default)]
  pub invite_token: Option<String>,
}

impl AppConfig {
  /// Get the network ID, if a network has been imported.
  pub fn network_id(&self) -> Option<String> {
    self.network.as_ref().map(|n| n.network_id.clone()).filter(|s| !s.is_empty())
  }

  /// Get the invite token, if present (token mode import).
  pub fn invite_token(&self) -> Option<String> {
    self.network.as_ref().and_then(|n| n.invite_token.clone())
  }
}

// ─── Device identity (convenience view over config identity fields) ──────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceIdentity {
  pub device_id: String,
  pub hostname: Option<String>,
}

// ─── Load / Save ─────────────────────────────────────────────────────────────

pub fn load_config() -> Result<AppConfig> {
  let path = config_file_path();
  let content = std::fs::read_to_string(&path)
    .with_context(|| format!("No config found at {}. Run `yat-cli import <share.json>` first.", path.display()))?;
  let config: AppConfig =
    serde_json::from_str(&content).with_context(|| "Failed to parse config.json")?;
  Ok(config)
}

pub fn load_device_identity() -> Result<DeviceIdentity> {
  let config = load_config()?;
  Ok(DeviceIdentity {
    device_id: config.identity.device_id,
    hostname: config.identity.hostname,
  })
}

#[allow(dead_code)]
pub fn save_device_identity(identity: &DeviceIdentity) -> Result<()> {
  // Update the identity fields in config.json
  let mut config = load_config()?;
  config.identity.device_id = identity.device_id.clone();
  config.identity.hostname = identity.hostname.clone();
  let path = config_file_path();
  std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
  Ok(())
}

// ─── Path helpers ────────────────────────────────────────────────────────────

pub fn config_dir() -> PathBuf {
  let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
  PathBuf::from(home).join(".config/yat-cli")
}

pub fn config_file_path() -> PathBuf {
  config_dir().join("config.json")
}

/// Get hostname for the current machine.
pub fn get_hostname() -> String {
  std::fs::read_to_string("/etc/hostname")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .or_else(|| std::env::var("HOSTNAME").ok())
    .unwrap_or_else(|| "yat-cli-linux".to_string())
}
