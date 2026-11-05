use crate::client_owner::Command;
use anyhow::{Context, Result};
use std::io::Read;

pub async fn run(edge: String, token: String) -> Result<()> {
    parse_address(&edge)?;
    anyhow::ensure!(!token.trim().is_empty(), "Pairing token is required");
    crate::client_owner::dispatch(Command::Pair { edge, token }).await
}

fn parse_address(address: &str) -> Result<(String, u16)> {
    // Explicit host:port also supports bracketed IPv6.
    let (host, port) = address
        .trim()
        .rsplit_once(':')
        .context("Use --edge host:port (IPv6: [address]:port)")?;
    let host = if host.starts_with('[') {
        let ipv6 = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .context("Use --edge [IPv6-address]:port")?;
        ipv6.parse::<std::net::Ipv6Addr>()
            .context("Invalid IPv6 address")?;
        ipv6
    } else {
        anyhow::ensure!(
            !host.contains(':') && !host.contains('[') && !host.contains(']'),
            "Use brackets around IPv6 addresses"
        );
        host
    };
    let port: u16 = port.parse().context("Invalid Edge port")?;
    anyhow::ensure!(
        !host.is_empty()
            && port > 0
            && !host.contains('/')
            && !host.chars().any(char::is_whitespace),
        "Invalid Edge address"
    );
    Ok((host.to_owned(), port))
}

// The serialized client supervisor owns this operation, including publication.
pub(crate) async fn pair_owned(edge: &str, token: &str) -> Result<()> {
    let (host, port) = parse_address(edge)?;
    let directory = crate::config::config_dir();
    let identity_file = directory.join("device-id");
    let device_id = std::fs::read_to_string(&identity_file)
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| id.starts_with("device-") && id.len() == 39)
        .unwrap_or_else(|| String::new());
    let device_id = if device_id.is_empty() {
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        format!(
            "device-{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    } else {
        device_id
    };
    let paired = yat_network::pairing::pair_with_edge(&host, port, token, &device_id).await?;
    let config = crate::config::AppConfig {
        edge: crate::config::EdgeConfig {
            host,
            port,
            edge_id: Some(paired.edge_id.clone()),
            server_name: None,
        },
        identity: crate::config::IdentityConfig {
            device_id: device_id.clone(),
            hostname: Some(crate::config::get_hostname()),
        },
        network: None,
    };
    // An interrupted publication blocks startup instead of using mixed credentials.
    let certs = directory.join("certs");
    std::fs::create_dir_all(&certs)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&certs, std::fs::Permissions::from_mode(0o700))?;
    let marker = directory.join("import.pending");
    crate::cli::import::write_private(
        &marker,
        b"Incomplete pairing; pair again with a fresh token",
    )?;
    let bundle = paired.certificate;
    let chain = if bundle.certificate_chain.is_empty() {
        &bundle.certificate
    } else {
        &bundle.certificate_chain
    };
    for (path, bytes) in [
        (identity_file, device_id.into_bytes()),
        (certs.join("ca.crt"), bundle.ca_certificate),
        (certs.join("client.crt"), chain.to_vec()),
        (certs.join("client.key"), bundle.private_key),
        (
            directory.join("config.json"),
            serde_json::to_vec_pretty(&config)?,
        ),
    ] {
        crate::cli::import::write_private(&path, &bytes)?;
    }
    let wireguard = directory.join("wireguard");
    if wireguard.exists() {
        std::fs::remove_dir_all(wireguard)?;
    }
    // A successful explicit pairing also replaces an interrupted invite enrollment.
    match std::fs::remove_file(certs.join("enrollment.pending")) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    std::fs::remove_file(marker)?;
    println!(
        "Paired with Edge {}. Import a network package before starting WireGuard.",
        paired.edge_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_hostname_and_ipv6_without_defaulting_bad_ports() {
        assert_eq!(
            parse_address("edge.example:9443").unwrap(),
            ("edge.example".into(), 9443)
        );
        assert_eq!(parse_address("[::1]:9443").unwrap(), ("::1".into(), 9443));
        for address in [
            "edge",
            "host:0",
            "host:65536",
            "https://host:9443",
            ":443",
            "::1:9443",
            "[::1:9443",
            "::1]:9443",
            "[[::1]]:9443",
            "[host]:9443",
        ] {
            assert!(parse_address(address).is_err(), "{address}");
        }
    }
}
