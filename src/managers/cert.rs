//! Certificate and key material management.
//!
//! Handles loading mTLS certificates for Edge connections,
//! WireGuard identity directory paths, and certificate expiry checks.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Manages certificate and key material for the YAT client.
pub struct CertManager {
  config_dir: PathBuf,
}

impl CertManager {
  pub fn new(config_dir: &Path) -> Self {
    Self {
      config_dir: config_dir.to_path_buf(),
    }
  }

  /// Load mTLS certificates for Edge connections.
  /// Returns (ca_cert, client_cert, client_key) as PEM bytes.
  pub fn load_edge_certs(&self) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let dir = self.certs_dir();
    let ca_cert = std::fs::read(dir.join("ca.crt")).context("Failed to read ca.crt")?;
    let client_cert =
      std::fs::read(dir.join("client.crt")).context("Failed to read client.crt")?;
    let client_key =
      std::fs::read(dir.join("client.key")).context("Failed to read client.key")?;
    Ok((ca_cert, client_cert, client_key))
  }

  /// Inspect the client certificate and return structured info.
  pub fn cert_info(&self) -> Result<ClientCertInfo> {
    let cert_path = self.certs_dir().join("client.crt");
    if !cert_path.exists() {
      anyhow::bail!("No client certificate found at {}", cert_path.display());
    }
    let pem = std::fs::read_to_string(&cert_path)?;

    // Parse PEM to extract X.509 fields
    let (subject, issuer, not_before, not_after) = parse_x509_fields(&pem)?;

    let days_remaining = not_after
      .parse::<chrono::DateTime<chrono::Utc>>()
      .ok()
      .map(|exp| {
        let now = chrono::Utc::now();
        (exp - now).num_days().max(0) as u32
      });

    // Compute SHA-256 fingerprint of the DER
    let fingerprint = pem_sha256_fingerprint(&pem);

    Ok(ClientCertInfo {
      cert_path: cert_path.display().to_string(),
      subject,
      issuer,
      not_before,
      not_after,
      days_remaining,
      fingerprint,
    })
  }

  /// Check if the certificate needs renewal (within 3 days of expiry).
  pub fn needs_renewal(&self) -> bool {
    match self.cert_info() {
      Ok(info) => match info.days_remaining {
        Some(days) => days <= 3,
        None => false,
      },
      Err(_) => true, // missing cert = needs renewal
    }
  }

  /// Directory where WireGuard identity and network state are stored.
  pub fn wg_storage_dir(&self) -> PathBuf {
    self.config_dir.join("wireguard")
  }

  /// Directory where mTLS certificates are stored.
  pub fn certs_dir(&self) -> PathBuf {
    self.config_dir.join("certs")
  }
}

// ─── Types ───────────────────────────────────────────────────────────────────

/// Client certificate information for the dashboard.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ClientCertInfo {
  pub cert_path: String,
  pub subject: String,
  pub issuer: String,
  pub not_before: String,
  pub not_after: String,
  pub days_remaining: Option<u32>,
  pub fingerprint: String,
}

// ─── X.509 parsing via x509-parser ──────────────────────────────────────────

/// Decode PEM or raw base64 text to DER bytes.
///
/// Handles:
/// 1. Standard PEM (with -----BEGIN CERTIFICATE----- headers)
///    If multiple PEM blocks exist (certificate chain), only the first is decoded.
/// 2. Raw base64-encoded DER (as stored from share.json)
fn decode_pem_to_der(pem_text: &str) -> Result<Vec<u8>> {
  use base64::Engine;

  let b64 = if pem_text.contains("-----BEGIN") {
    // Standard PEM: extract only the FIRST certificate block
    let mut collecting = false;
    let mut lines = Vec::new();
    for line in pem_text.lines() {
      let trimmed = line.trim();
      if trimmed.starts_with("-----BEGIN") {
        if collecting {
          break; // second block — stop
        }
        collecting = true;
        continue;
      }
      if trimmed.starts_with("-----END") {
        if collecting {
          break; // end of first block
        }
        continue;
      }
      if collecting {
        lines.push(trimmed);
      }
    }
    lines.join("")
  } else {
    // Raw base64 (from share.json): use as-is, strip whitespace
    pem_text
      .lines()
      .map(|l| l.trim())
      .collect::<String>()
  };

  base64::engine::general_purpose::STANDARD
    .decode(b64.trim())
    .map_err(|e| anyhow::anyhow!("Base64 decode error: {}", e))
}

/// Extract subject, issuer, validity dates from a PEM certificate.
fn parse_x509_fields(pem_str: &str) -> Result<(String, String, String, String)> {
  use x509_parser::prelude::*;

  let der_bytes = decode_pem_to_der(pem_str)?;
  let (_, x509) = X509Certificate::from_der(&der_bytes)
    .map_err(|e| anyhow::anyhow!("X.509 parse error: {}", e))?;

  let subject = x509.subject().to_string();
  let issuer = x509.issuer().to_string();

  let not_before = format_datetime(x509.validity().not_before.timestamp());
  let not_after = format_datetime(x509.validity().not_after.timestamp());

  Ok((subject, issuer, not_before, not_after))
}

/// Format a UNIX timestamp as a human-readable UTC string.
fn format_datetime(ts: i64) -> String {
  chrono::DateTime::from_timestamp(ts, 0)
    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
    .unwrap_or_else(|| "N/A".into())
}

/// Compute SHA-256 fingerprint of the DER inside a PEM certificate.
fn pem_sha256_fingerprint(pem_str: &str) -> String {
  use sha2::{Digest, Sha256};

  match decode_pem_to_der(pem_str) {
    Ok(der) => {
      let hash = Sha256::digest(&der);
      let hex: Vec<String> = hash.iter().map(|b| format!("{:02x}", b)).collect();
      format!("SHA256:{}", hex.join(":"))
    }
    Err(_) => "N/A".into(),
  }
}
