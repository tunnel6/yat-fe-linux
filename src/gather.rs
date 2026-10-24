//! Same-NAT gather protocol handler for yat-fe-linux.
//!
//! When Edge detects two peers behind the same NAT (same observed public IP),
//! it sends a `WG_GATHER_INFO` request.  The client must respond with its
//! local LAN addresses so Edge can forward them to the peer, enabling direct
//! LAN communication without hairpin NAT.
//!
//! This module:
//! 1. Subscribes to P2P events (gather/punch) from Edge
//! 2. On gather: collects local LAN IPs via `getifaddrs(3)`
//! 3. Reads the WG listen port from runtime stats
//! 4. Sends the gather response back to Edge

use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::sync::Arc;

use tracing::{info, warn};

use yat_transport::{EdgeClient, WGP2PEvent};

fn scoped_network_id(session_id: &str) -> Option<&str> {
  session_id
    .strip_prefix("lan:")
    .filter(|network_id| !network_id.is_empty())
}

// ─── Local address collection ────────────────────────────────────────────────

/// Collect non-loopback IPv4 addresses from system network interfaces.
///
/// Uses `getifaddrs(3)` via libc — no extra crate dependency needed.
/// Filters out:
/// - Loopback interfaces (`lo*`)
/// - Loopback addresses (`127.0.0.0/8`)
/// - Non-IPv4 addresses (IPv6 not needed for same-NAT gather)
fn collect_local_addrs() -> Vec<String> {
  let mut addrs = Vec::new();

  unsafe {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    if libc::getifaddrs(&mut ifap) != 0 || ifap.is_null() {
      warn!("getifaddrs failed: {}", std::io::Error::last_os_error());
      return addrs;
    }

    let mut cur = ifap;
    while !cur.is_null() {
      let ifa = &*cur;
      cur = ifa.ifa_next;

      if ifa.ifa_addr.is_null() {
        continue;
      }

      // Only IPv4 (AF_INET)
      if (*ifa.ifa_addr).sa_family as u32 != libc::AF_INET as u32 {
        continue;
      }

      // Skip loopback interfaces
      let iface_name = CStr::from_ptr(ifa.ifa_name).to_string_lossy();
      if iface_name.starts_with("lo") {
        continue;
      }

      let sockaddr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
      let ip = Ipv4Addr::from(u32::from_be(sockaddr.sin_addr.s_addr));

      // Skip loopback addresses
      if ip.is_loopback() {
        continue;
      }

      let ip_str = ip.to_string();
      if !addrs.contains(&ip_str) {
        addrs.push(ip_str);
      }
    }

    libc::freeifaddrs(ifap);
  }

  addrs
}

// ─── Gather event handler ────────────────────────────────────────────────────

/// Handle a single gather event: collect LAN addresses, get WG listen port,
/// and send the gather response to Edge.
async fn handle_gather(
  edge: &Arc<EdgeClient>,
  session_id: &str,
  client_id: &str,
  storage_dir: &str,
  network_id: &str,
  self_address: &str,
) {
  info!("[P2P] gather: session_id={session_id}, collecting LAN addresses...");

  let local_addrs = collect_local_addrs();

  // Filter out the WG TUN self address (e.g. 10.24.0.8) — it's a virtual IP,
  // not a real LAN address that peers can use for same-NAT communication.
  let local_addrs: Vec<String> = if !self_address.is_empty() {
    local_addrs
      .into_iter()
      .filter(|a| a != self_address.split('/').next().unwrap_or(self_address))
      .collect()
  } else {
    local_addrs
  };
  info!("[P2P] gather: local_addrs={local_addrs:?}");

  // Get WG listen port via submodule's `wg show <iface> dump` parser.
  let listen_port = {
    let sdir = storage_dir.to_string();
    let nid = network_id.to_string();
    tokio::task::spawn_blocking(move || {
      yat_wireguard::stats::get_runtime_stats(yat_wireguard::types::RuntimeStatsParams {
        storage_dir: sdir,
        network_id: nid,
      })
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten()
    .and_then(|s| s.listen_port)
  };

  // Build local WG endpoint: <lan_ip>:<listen_port>
  let local_wg_endpoint = if let (Some(lan_ip), Some(port)) = (local_addrs.first(), listen_port) {
    format!("{lan_ip}:{port}")
  } else {
    if listen_port.is_none() {
      warn!("[P2P] gather: no WG listen port available (interface may not be applied yet)");
    }
    String::new()
  };

  info!("[P2P] gather: sending response: session_id={session_id}, client_id={client_id}, local_wg_endpoint={local_wg_endpoint}");

  if let Err(e) = edge
    .send_wg_gather_response(
      session_id,
      client_id,
      &local_addrs,
      "", // public_addr — not needed for same-NAT gather
      0,  // nat_type — unknown
      &local_wg_endpoint,
    )
    .await
  {
    warn!("[P2P] gather: failed to send response: {e}");
  } else {
    info!("[P2P] gather: response sent successfully");
  }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Subscribe to Edge P2P events and handle gather/punch requests.
///
/// Call this after the Edge connection is established and the WG identity
/// is loaded.  The subscription lives as long as the `EdgeClient`.
pub struct Subscription {
  stop: tokio::sync::watch::Sender<bool>,
  task: tokio::task::JoinHandle<()>,
  failed: bool,
}
impl Subscription {
  pub async fn stop(&mut self) -> anyhow::Result<()> {
    anyhow::ensure!(
      !self.failed,
      "Gather task previously failed; ownership retained"
    );
    self.stop.send_replace(true);
    if let Err(error) = (&mut self.task).await {
      self.failed = true;
      anyhow::bail!("Gather task failed: {error}");
    }
    Ok(())
  }
}

pub async fn subscribe_p2p_events(
  edge: Arc<EdgeClient>,
  client_id: String,
  storage_dir: String,
  network_id: String,
  self_address: String,
) -> anyhow::Result<Subscription> {
  let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
  let scope = network_id.clone();
  edge
    .subscribe_wg_p2p_events(move |event| {
      if let WGP2PEvent::Gather { session_id } = event {
        if scoped_network_id(&session_id) == Some(scope.as_str()) {
          if tx.try_send(session_id).is_err() {
            warn!("Gather queue full or stopping; event dropped");
          }
        }
      }
    })
    .await?;
  let (stop, mut stopping) = tokio::sync::watch::channel(false);
  let task = tokio::spawn(async move {
    loop {
      if *stopping.borrow() {
        break;
      }
      tokio::select! {
        biased;
        _ = stopping.changed() => break,
        session = rx.recv() => match session {
          Some(session) => handle_gather(&edge, &session, &client_id, &storage_dir, &network_id, &self_address).await,
          None => break,
        }
      }
    }
    // In-flight stats/response completed above; queued events are discarded.
  });
  Ok(Subscription {
    stop,
    task,
    failed: false,
  })
}

#[cfg(test)]
mod tests {
  use super::scoped_network_id;

  #[test]
  fn legacy_event_scope_requires_explicit_network_id() {
    assert_eq!(scoped_network_id("lan:net-a"), Some("net-a"));
    assert_eq!(scoped_network_id("lan:"), None);
    assert_eq!(scoped_network_id("gather:session"), None);
  }
}
