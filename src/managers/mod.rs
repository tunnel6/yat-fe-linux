//! Business-logic managers for yat-cli.
//!
//! The manager layer sits between the CLI/web entry points and the engine crates
//! (transport, yat-wireguard). Each manager owns a clear business role:
//!
//! - [`CertManager`] — certificate and key material management
//! - [`WgManager`] — WireGuard network lifecycle orchestration

pub mod cert;
pub mod wg;

pub use wg::WgManager;
