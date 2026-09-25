use anyhow::Result;
use tracing::info;

use crate::managers::WgManager;

/// Leave network and teardown WireGuard interface.
pub async fn run() -> Result<()> {
  info!("Leaving network...");
  let mut wg = WgManager::new()?;
  wg.stop().await
}
