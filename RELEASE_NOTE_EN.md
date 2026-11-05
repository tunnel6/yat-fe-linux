# YAT Linux Server Client Release Notes

> Current source (2026-10-19): Docker runs `yat-client serve` in the foreground by default, including the Web Dashboard at `0.0.0.0:9909` inside the container. See [README](README.md) for deployment and shutdown details. Historical release entries do not imply that native path coordination is enabled or validated on real devices.

The YAT Linux Server client (`yat-client`, directory `yat-fe-linux`) is a standalone Rust CLI/service for Linux servers. It is separate from the Electron Linux desktop build in `yat-fe`. It provides WireGuard networking capabilities and a built-in Web Dashboard for management. Supports both x86_64 and arm64 architectures, deployable via Docker or standalone binary.

---

## Changelog

### Edge restart recovery / Unreleased (2026-10-26)

- Transport loss, connection errors and control request timeouts recover through the existing supervisor: drain the old instance, authenticate and rejoin, then restore network and path responder subscriptions while preserving pair settings.
- Retry waits are 2, 4, 8, 16 and 30 seconds, capped at 30 seconds for subsequent attempts. Removed the 30-minute cooldown after ten failures. TCP/TLS setup has a 10-second deadline and authentication a 30-second deadline; native operations retain their existing drain requirements.
- Authorization/coordination rejections and native errors alone do not trigger rejoining. Recovery logs and the Web retry deadline remain available. Local regressions do not replace real Edge restart acceptance; see [Code Plan §11](../docs/wireguard/WIREGUARD_LINUX_RESPONDER_CODE_PLAN_2026-10-24.md#11-edge-重启后的自动恢复修正2026-10-26).

### Current source / Unreleased (2026-10-19)

- Server `start/serve/leave/import` and Web operations use the same `client_owner`. A permanent lock file and private `yat-fe-linux.sock` prevent a second native configuration writer for the same directory. Desktop Linux does not use this Server IPC/Docker lifecycle.
- Initial apply, refresh, reconnect, import replacement and shutdown use the owned controller. Shutdown fences writes and closes the Edge control connection before awaiting original tasks and native cleanup. Failed cleanup retains ownership and retries. Ordinary stop preserves membership; explicit `leave` changes it.
- Both Linux products require kernel **≥ 5.6**, including Docker's host kernel. Distribution backports to 5.4 are outside the supported baseline; installed tools do not replace kernel support or privileges.
- The exec ENTRYPOINT runs foreground `serve` by default; override with `start` to omit Web. The Web management API currently has no authentication. Limit it to a trusted management boundary; examples below publish only to host loopback.
- SIGKILL, crashes and host failures cannot guarantee exit callbacks. Edge detects disconnection/timeouts; cleanup does not depend on the exit status code.
- Native path coordination has controlled implementations and offline regressions. Production Owner/PathControlSession assembly, authoritative manual requests, automatic stability policy and real-device acceptance remain incomplete; the capability is unpublished. See the [current code plan, §25](../docs/wireguard/WIREGUARD_NATIVE_PATH_COORDINATION_CODE_PLAN_2026-10-17.md).

### v0.2.0 (2026-10-16)

**New Share Package Format**
- Supports the desktop app's new Share Package export format with two modes:
  - **Full Certificate Mode**: Exports the complete client certificate and private key — ready to connect immediately after import
  - **Token Mode**: Exports an invite token — the client automatically obtains a certificate from the Edge server on first connection (no OAuth login required)
- Both modes auto-generate filenames: `yat-net-{EdgeID}-{NetworkID}.json`

**Certificate Provisioning Changes**
- No longer depends on OAuth login for certificate provisioning
- In token mode, the client automatically requests a certificate from the Edge via invite token on first startup
- After certificate expiry, re-export a new share package from the desktop app to update

**WireGuard Network Sync Upgrade (historical local selection; not acceptance of new bilateral coordination)**
- Powered by a new network sync engine with:
  - Automatic relay path management (falls back to relay when direct connection is unavailable)
  - Dynamic path switching (direct ↔ relay)
  - Peer health checks (based on WireGuard handshake status)
  - Automatic WireGuard config updates on network topology changes

**Other Improvements**
- Automatic cleanup of all WireGuard interfaces on shutdown (supports multi-network scenarios)
- Simplified config structure — removed user identity and OAuth-related configuration

### v0.1.0

- Initial release
- WireGuard networking + Web Dashboard
- Edge connection + auto-reconnect
- Certificate management & monitoring
- Same-NAT LAN endpoint discovery

---

## Quick Start

Get a Linux machine connected to your YAT WireGuard network in three steps:

```bash
# 1. Start the client (Docker)
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:9909:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest

# 2. Open the Dashboard in your browser
#    http://127.0.0.1:9909 (use SSH forwarding for remote access)

# 3. Upload share.json in the Settings tab — the client connects automatically
```

> **Tip**: Export `share.json` from the desktop app (macOS/Windows) and import it via the Dashboard or CLI.

---

## Requirements

| Item | Requirement |
|------|-------------|
| **OS** | Linux (x86_64 / aarch64) |
| **Host kernel** | Linux ≥ 5.6 with built-in or module WireGuard support; 5.4 backports are outside the baseline |
| **Tools** | `iproute2`, `wireguard-tools`; tools do not replace kernel support |
| **Docker** | Docker 20.10+ (when running via Docker) |
| **Privileges** | `root` or `CAP_NET_ADMIN` (required for WireGuard interface creation) |
| **Network** | Reachable YAT Edge server (port determined by Edge config) |
| **Memory** | Minimum 64 MB available |

### Additional Docker Permissions

```bash
# Keep deployment device access; Linux uses kernel WireGuard and requires NET_ADMIN
--cap-add NET_ADMIN \
--device /dev/net/tun
```

---

## Running the Client

### Option 1: Docker (Recommended)

#### Pull Image

```bash
# Pull from Alibaba Cloud Registry (supports amd64 / arm64)
docker pull registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest
```

#### Start Container

```bash
# Basic start (port mapping mode)
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:9909:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest

# Optional host networking: bind Web to loopback; do not expose the management API directly
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  --network host \
  -e YAT_LISTEN=127.0.0.1:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest
```

#### Custom Port

```bash
# Change the Web UI listen port
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:8080:8080 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest \
  serve --listen 0.0.0.0:8080
```

#### Persist Configuration

```bash
# Mount config directory to survive container restarts
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:9909:9909 \
  -v ~/.config/yat:/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest
```

#### View Logs

```bash
docker logs -f yat-client
```

---

### Option 2: CLI Binary

#### Download Binary

```bash
# Download the binary for your architecture (x86_64 example)
chmod +x yat-client
```

#### CLI Command Reference

| Command | Description |
|---------|-------------|
| `yat-client import <share.json>` | Import a network configuration package |
| `yat-client serve [--listen ADDR]` | Run the foreground owner with Web Dashboard |
| `yat-client start` | Foreground connection mode (no Web UI) |
| `yat-client status` | Show current network status |
| `yat-client leave` | Leave network and teardown WireGuard interface |
| `yat-client cert-info` | Display certificate information |

#### Typical Workflow

```bash
# 1. Import network config (share.json exported from desktop app)
sudo yat-client import share.json

# 2. Run foreground Web (source default is 0.0.0.0:9909; restrict to loopback here)
sudo yat-client serve --listen 127.0.0.1:9909

# Or specify a custom listen address
sudo yat-client serve --listen 127.0.0.1:8080

# 3. Open the Dashboard in your browser
#    http://127.0.0.1:9909
```

> **Note**: Creating/updating WireGuard interfaces requires root or `CAP_NET_ADMIN`. Without a running owner, `import` only writes configuration. Use the same identity and configuration directory for all commands; a running owner handles import replacement and reconnect.

#### Foreground Mode (No Web UI)

```bash
# Suitable for scripting or temporary use
sudo yat-client start
# Press Ctrl+C to disconnect
```

#### Check Status

```bash
sudo yat-client status
```

`status` reads saved membership/interface data, applied state and available native stats. It does not currently query the running owner's control connection, so `Connected: no (daemon mode or stale)` cannot establish that the service is offline. Web `/api/status` reads the running owner's state; its `connected` field still does not prove peer business reachability. Linux uses kernel WireGuard, not boringtun/userspace.

---

## Web Dashboard Guide

After starting the `serve` command, a feature-rich Web Dashboard is available in your browser.

### Dashboard Tab

- **Network Info** — View Network ID, Member ID, assigned IP address, interface name, and other core details
- **Network Status** — Real-time connection status, online peer count, last sync timestamp
- **Peers** — Complete peer list with hostname, IP address, endpoint, and RX/TX traffic statistics
- **Certificate** — Certificate details (Subject / Issuer / Validity / Fingerprint) with status checks and re-import guidance (no automatic certificate issuance)
- **Latest Logs** — Last 50 runtime log entries, color-coded by severity (INFO / WARN / ERROR)

### Settings Tab

- **Edge Connection** — View Edge server host, port, and Edge ID
- **Identity** — View Device ID and hostname
- **Import** — Drag-and-drop `share.json` upload; the existing owner drains old resources, updates configuration and reconnects
- **Reconnect** — Manually trigger Edge reconnection (useful when disconnected)
- **Update Address** — Dynamically change this device's IP address within the network

### Dashboard Highlights

- Dark theme with YAT green accent (#22c55e)
- Auto-refresh status data every 5 seconds
- Zero-config startup — launch the Dashboard first, then import config via the Web UI
- Automatic certificate monitoring — checks hourly, shows a reminder when nearing expiry
- Auto-reconnect on disconnect — exponential backoff with continuous retries and waits capped at 30 seconds in current source (older versions waited 30 minutes after ten failures)

---

## Configuration

Configuration files are stored in `~/.config/yat/`:

```
~/.config/yat/
├── config.json   # Main config (Edge connection + device identity + network config)
├── device-id     # Local device ID (stable across re-imports)
├── yat-fe-linux.lock # Permanent lock file; do not remove while running
├── yat-fe-linux.sock # Private command socket for the current owner
├── import.pending   # Blocks startup with an incomplete/mixed import
├── state.json       # Saved connection/interface data, not proof of current presence
├── wireguard/       # Identity, configuration and applied state
└── certs/
    ├── ca.crt      # CA certificate
    ├── client.crt  # Client certificate (full cert mode / generated after token bootstrap)
    └── client.key  # Client private key (full cert mode / generated after token bootstrap)
```

---

## Architecture Support

| Architecture | Docker Image | Binary |
|-------------|-------------|--------|
| linux/amd64 (x86_64) | ✅ | ✅ |
| linux/arm64 (aarch64) | ✅ | ✅ |

The Dockerfile supports Alpine 3.20 (musl) and Debian bookworm (gnu) variants; the gnu build matrix also includes armv7. Consult the published image manifest for available architectures. This review did not build or run images.

---

## FAQ

**Q: Why does it need root privileges?**  
A: Linux uses kernel WireGuard. Creating WG interfaces and configuring addresses/routes requires root or CAP_NET_ADMIN in the target network namespace.

**Q: Configuration lost after container restart?**  
A: Mount a persistent volume with `-v ~/.config/yat:/root/.config/yat`.

**Q: What if the certificate expires?**
A: Re-export a new share package (full certificate mode or token mode) from the desktop app and import it — the certificate will be updated automatically.

**Q: How to run in the background?**  
A: Let Docker or systemd manage the foreground application. Add `-d` to Docker if needed; stop with `docker stop --time 60 yat-client` so SIGTERM reaches the same PID 1 owner. The grace period does not guarantee cleanup completion.
