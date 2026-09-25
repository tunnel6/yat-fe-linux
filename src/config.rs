//! Configuration data types and loading for yat-cli.
//!
//! Pure data layer: `AppConfig`, `DeviceIdentity`, config file I/O,
//! and the `config_dir()` path helper. Business-level path resolution
//! (certs, WG storage) lives in `managers::CertManager`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── Config types ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
  pub edge: EdgeConfig,
  pub identity: IdentityConfig,
  pub network: Option<serde_json::Value>,
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
  pub user_id: String,
  pub device_id: String,
  #[serde(default)]
  pub username: Option<String>,
  #[serde(default)]
  pub hostname: Option<String>,
}

impl AppConfig {
  /// Extract network ID from the stored network value.
  /// Tries both `networkId` (camelCase from Electron) and `network_id`.
  pub fn network_id(&self) -> Option<String> {
    self.network.as_ref().and_then(|n| {
      n.get("networkId")
        .or_else(|| n.get("network_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    })
  }
}

// ─── Device identity (persistent across runs, used for WG key material) ──────

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
  let path = device_identity_path();
  if path.exists() {
    let content = std::fs::read_to_string(&path)?;
    let identity: DeviceIdentity = serde_json::from_str(&content)?;
    Ok(identity)
  } else {
    // Fall back to config identity
    let config = load_config()?;
    Ok(DeviceIdentity {
      device_id: config.identity.device_id.clone(),
      hostname: config.identity.hostname.clone(),
    })
  }
}

#[allow(dead_code)]
pub fn save_device_identity(identity: &DeviceIdentity) -> Result<()> {
  let path = device_identity_path();
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  std::fs::write(&path, serde_json::to_string_pretty(identity)?)?;
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

pub fn device_identity_path() -> PathBuf {
  config_dir().join("device-identity.json")
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
