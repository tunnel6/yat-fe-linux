use anyhow::Result;

use yat_wireguard::types::{AppliedRuntimeStateParams, RuntimeStatsParams};

use crate::managers::WgManager;

/// Show current network status.
pub fn run() -> Result<()> {
  let wg = WgManager::new()?;

  // 1. Aggregate status from WgManager
  let status = match wg.status() {
    Ok(s) => s,
    Err(_) => {
      println!("No active network. Run `yat-cli start` to connect.");
      return Ok(());
    }
  };

  println!("Network:    {}", status.network_id);
  println!("Member:     {}", status.member_id);
  println!("Interface:  {} ({})", status.interface_name, status.interface_driver);
  println!("Address:    {}", status.self_address);
  println!("Peers:      {}", status.peer_count);
  println!("Connected:  {}", if status.connected { "yes" } else { "no (daemon mode or stale)" });
  println!();

  // 2. Engine applied state
  let storage_dir_str = wg.certs().wg_storage_dir().to_string_lossy().to_string();
  match yat_wireguard::applied::get_applied_runtime_state(AppliedRuntimeStateParams {
    storage_dir: storage_dir_str.clone(),
    network_id: status.network_id.clone(),
  }) {
    Ok(Some(applied)) => {
      println!("── WireGuard Engine ──");
      println!("Backend:    {}", applied.backend);
      println!("Interface:  {} ({})", applied.resolved_interface_name, applied.interface_driver);
      println!("Address:    {}", applied.self_address);
      if let Some(port) = applied.listen_port {
        println!("Listen:     {port}");
      }
      println!("Public key: {}", applied.local_public_key);
      println!("Peers:      {}", applied.peer_count);
      println!();
    }
    Ok(None) => {
      println!("⚠ WireGuard interface not applied (engine has no applied state).");
      println!("  The saved state may be stale. Run `yat-cli leave` to clean up.\n");
    }
    Err(e) => {
      println!("⚠ Failed to read engine state: {e}\n");
    }
  }

  // 3. Live stats
  match yat_wireguard::stats::get_runtime_stats(RuntimeStatsParams {
    storage_dir: storage_dir_str,
    network_id: status.network_id.clone(),
  }) {
    Ok(Some(stats)) => {
      println!("── Live Stats ──");
      println!("Backend:    {} ({})", stats.backend, stats.resolved_interface_name);
      if let Some(port) = stats.listen_port {
        println!("Listen:     {port}");
      }
      println!("Peers:      {}", stats.peer_count);
      for peer in &stats.peers {
        let ep = peer.endpoint.as_deref().unwrap_or("(none)");
        let rx_mb = peer.rx_bytes / 1_000_000.0;
        let tx_mb = peer.tx_bytes / 1_000_000.0;
        let hostname = peer.hostname.as_deref().unwrap_or(&peer.device_id);
        println!("  [{hostname}] ep={ep} rx={rx_mb:.1}MB tx={tx_mb:.1}MB");
      }
    }
    Ok(None) => {
      println!("(No live stats available — interface may not be running.)");
    }
    Err(e) => {
      println!("(Live stats unavailable: {e})");
    }
  }

  Ok(())
}
