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
    local_addrs.into_iter().filter(|a| a != self_address).collect()
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
      "",  // public_addr — not needed for same-NAT gather
      0,   // nat_type — unknown
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
pub async fn subscribe_p2p_events(
  edge: Arc<EdgeClient>,
  client_id: String,
  storage_dir: String,
  network_id: String,
  self_address: String,
) {
  let storage_dir = Arc::new(storage_dir);
  let network_id = Arc::new(network_id);
  let self_address = Arc::new(self_address);

  let handler_edge = Arc::clone(&edge);
  let handler = move |event: WGP2PEvent| {
    let edge = Arc::clone(&handler_edge);
    let client_id = client_id.clone();
    let storage_dir = Arc::clone(&storage_dir);
    let network_id = Arc::clone(&network_id);
    let self_address = Arc::clone(&self_address);

    match event {
      WGP2PEvent::Gather { session_id } => {
        tokio::spawn(async move {
          handle_gather(&edge, &session_id, &client_id, &storage_dir, &network_id, &self_address).await;
        });
      }
      WGP2PEvent::Punch { session_id, client_id: peer_client_id, punch_addrs } => {
        // For same-NAT, punch is a no-op — the WG interface will auto-handshake
        // once both peers have each other's LAN endpoint from the gather flow.
        // Report success so Edge knows the punch addresses are reachable.
        info!("[P2P] punch: session_id={session_id}, peer={peer_client_id}, addrs={punch_addrs:?}");
        let edge = Arc::clone(&edge);
        tokio::spawn(async move {
          let connected_addr = punch_addrs.first().cloned().unwrap_or_default();
          if let Err(e) = edge
            .send_wg_punch_response(&session_id, &peer_client_id, true, &connected_addr)
            .await
          {
            warn!("[P2P] punch: failed to send response: {e}");
          }
        });
      }
    }
  };

  if let Err(e) = edge.subscribe_wg_p2p_events(handler).await {
    warn!("[P2P] failed to subscribe P2P events: {e}");
  } else {
    info!("[P2P] P2P event handler subscribed (gather/punch enabled)");
  }
}
