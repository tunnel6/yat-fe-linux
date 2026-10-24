//! Read-only projection of local WireGuard routes. Candidate addresses and
//! coordination targets never stand in for a live route or endpoint.
use super::loop_runner::{LocalPath, PeerInfo};
use std::net::IpAddr;
use yat_wireguard::types::{RuntimeStats, RuntimeStatsPeer};

pub fn update(peers: &mut [PeerInfo], stats: Option<&RuntimeStats>, relay_keys: &[String]) {
  for peer in peers {
    // Missing/failed reads must also clear an earlier successful projection.
    peer.local_path = LocalPath::Unknown;
    peer.endpoint.clear();
    peer.last_handshake_at = None;
    let Some(owner) = stats.and_then(|stats| route_owner(&peer.assigned_ip, &stats.peers)) else {
      continue;
    };
    peer.local_path = if owner.public_key == peer.public_key {
      LocalPath::Direct
    } else if relay_keys.contains(&owner.public_key) {
      LocalPath::Relay
    } else {
      continue;
    };
    peer.endpoint = owner.endpoint.clone().unwrap_or_default();
    peer.last_handshake_at = owner.last_handshake_at;
  }
}

fn route_owner<'a>(
  assigned_ip: &str,
  peers: &'a [RuntimeStatsPeer],
) -> Option<&'a RuntimeStatsPeer> {
  let ip: IpAddr = assigned_ip.split('/').next()?.parse().ok()?;
  let mut best: Option<(&RuntimeStatsPeer, u8)> = None;
  let mut ambiguous = false;
  for peer in peers {
    for cidr in &peer.allowed_ips {
      let Ok((network, bits)) = yat_wireguard::util::parse_cidr(cidr, "runtime.allowedIps") else {
        continue;
      };
      let matches = match (network, ip) {
        (IpAddr::V4(network), IpAddr::V4(ip)) => {
          (u32::from(network) ^ u32::from(ip))
            .checked_shr(32 - u32::from(bits))
            .unwrap_or(0)
            == 0
        }
        (IpAddr::V6(network), IpAddr::V6(ip)) => {
          (u128::from(network) ^ u128::from(ip))
            .checked_shr(128 - u32::from(bits))
            .unwrap_or(0)
            == 0
        }
        _ => false,
      };
      if !matches {
        continue;
      }
      match best {
        None => best = Some((peer, bits)),
        Some((_, previous_bits)) if bits > previous_bits => {
          best = Some((peer, bits));
          ambiguous = false;
        }
        Some((previous, previous_bits)) if bits == previous_bits => {
          ambiguous |= previous.public_key != peer.public_key;
        }
        _ => (),
      }
    }
  }
  if ambiguous {
    None
  } else {
    best.map(|(peer, _)| peer)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn member(ip: &str) -> PeerInfo {
    PeerInfo {
      hostname: "target".into(),
      assigned_ip: ip.into(),
      local_path: LocalPath::Unknown,
      endpoint: String::new(),
      public_key: "target-key".into(),
      presence_status: 1,
      last_handshake_at: None,
    }
  }

  fn live(key: &str, ips: &[&str], endpoint: &str) -> RuntimeStatsPeer {
    RuntimeStatsPeer {
      member_id: String::new(),
      hostname: None,
      device_id: String::new(),
      public_key: key.into(),
      endpoint: Some(endpoint.into()),
      configured_endpoint: Some("candidate.invalid:1234".into()),
      allowed_ips: ips.iter().map(|ip| (*ip).into()).collect(),
      persistent_keepalive: 25,
      last_handshake_at: Some(if key == "target-key" { 1000.0 } else { 2000.0 }),
      rx_bytes: 0.0,
      tx_bytes: 0.0,
    }
  }

  fn stats(peers: Vec<RuntimeStatsPeer>) -> RuntimeStats {
    RuntimeStats {
      format_version: 1,
      backend: "test".into(),
      network_id: "net".into(),
      resolved_interface_name: "wg0".into(),
      interface_driver: "test".into(),
      local_public_key: "self-key".into(),
      listen_port: Some(58020),
      peer_count: peers.len() as u32,
      peers,
      collected_at: 3000.0,
    }
  }

  #[test]
  fn relay_uses_carrying_peer_even_when_direct_peer_still_has_endpoint_and_handshake() {
    let mut peers = [member("10.24.0.8")];
    let live = stats(vec![
      live("target-key", &[], "192.168.5.183:58020"),
      live("relay-key", &["10.24.0.8/32"], "203.0.113.1:58020"),
    ]);
    update(&mut peers, Some(&live), &["relay-key".into()]);
    assert_eq!(peers[0].local_path, LocalPath::Relay);
    assert_eq!(peers[0].endpoint, "203.0.113.1:58020");
    assert_eq!(peers[0].last_handshake_at, Some(2000.0));
    let json = serde_json::to_value(&peers[0]).unwrap();
    assert_eq!(json["local_path"], "relay");
    assert_eq!(json["endpoint"], "203.0.113.1:58020");
  }

  #[test]
  fn longest_prefix_selects_direct_over_relay_subnet_regardless_of_iteration_order() {
    let mut peers = [member("10.24.0.8/24")];
    let mut live = stats(vec![
      live(
        "relay-key",
        &["0.0.0.0/0", "10.24.0.0/24"],
        "203.0.113.1:58020",
      ),
      live("target-key", &["10.24.0.8/32"], "192.168.5.183:58021"),
    ]);
    for _ in 0..2 {
      update(&mut peers, Some(&live), &["relay-key".into()]);
      assert_eq!(peers[0].local_path, LocalPath::Direct);
      assert_eq!(peers[0].endpoint, "192.168.5.183:58021");
      assert_eq!(peers[0].last_handshake_at, Some(1000.0));
      live.peers.reverse();
    }
  }

  #[test]
  fn fixed_relay_and_ipv6_follow_live_prefixes() {
    for (ip, cidr) in [("10.24.0.8", "10.24.0.0/24"), ("fd24::8", "fd24::/64")] {
      let mut peers = [member(ip)];
      let live = stats(vec![live("relay-key", &[cidr], "[2001:db8::1]:58020")]);
      update(&mut peers, Some(&live), &["relay-key".into()]);
      assert_eq!(peers[0].local_path, LocalPath::Relay);
      assert_eq!(peers[0].endpoint, "[2001:db8::1]:58020");
    }
  }

  #[test]
  fn missing_routes_unrecognized_or_ambiguous_owners_never_guess_a_path() {
    let cases = [
      vec![live("target-key", &[], "192.168.5.183:58020")],
      vec![live("other-key", &["10.24.0.8/32"], "203.0.113.2:58020")],
      vec![
        live("target-key", &["10.24.0.8/32"], "192.168.5.183:58020"),
        live("relay-key", &["10.24.0.8/32"], "203.0.113.1:58020"),
      ],
      vec![live(
        "target-key",
        &["bad-cidr", "fd24::8/128"],
        "192.168.5.183:58020",
      )],
    ];
    for live_peers in cases {
      let mut peers = [member("10.24.0.8")];
      let live = stats(live_peers);
      update(&mut peers, Some(&live), &["relay-key".into()]);
      assert_eq!(peers[0].local_path, LocalPath::Unknown);
      assert!(peers[0].endpoint.is_empty());
      assert_eq!(peers[0].last_handshake_at, None);
    }
  }

  #[test]
  fn failed_read_clears_old_path_and_missing_live_endpoint_never_uses_candidate() {
    let mut peers = [member("10.24.0.8")];
    let mut live = stats(vec![live(
      "target-key",
      &["10.24.0.8/32"],
      "192.168.5.183:58020",
    )]);
    update(&mut peers, Some(&live), &[]);
    assert_eq!(peers[0].local_path, LocalPath::Direct);
    update(&mut peers, None, &[]);
    assert_eq!(peers[0].local_path, LocalPath::Unknown);
    assert!(peers[0].endpoint.is_empty());
    assert_eq!(peers[0].last_handshake_at, None);

    live.peers[0].endpoint = None;
    update(&mut peers, Some(&live), &[]);
    assert_eq!(peers[0].local_path, LocalPath::Direct);
    assert!(peers[0].endpoint.is_empty());
  }
}
