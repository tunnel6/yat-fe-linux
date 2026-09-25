use anyhow::Result;
use tracing::info;

use crate::managers::WgManager;

/// Connect to Edge and start WireGuard interface (foreground mode).
pub async fn run() -> Result<()> {
  info!("Starting YAT client...");

  let mut wg = WgManager::new()?;
  wg.start().await?;

  println!("\n✓ Connected. Press Ctrl+C to disconnect.");
  info!("Client started. Waiting for shutdown signal...");

  // Wait for shutdown signal
  tokio::signal::ctrl_c().await?;
  info!("Shutdown signal received.");

  // Graceful disconnect
  wg.stop().await?;
  println!("\n✓ Disconnected.");
  Ok(())
}
