# Juicity-RS

A Rust implementation of the [Juicity](https://github.com/juicity/juicity) protocol. Juicity is a QUIC-based proxy protocol that improves upon TUIC's UDP handling with **UDP over Stream**, using bidirectional streams to multiplex UDP data efficiently.

## Features

- **QUIC-based transport** — built on [`quinn`](https://github.com/quinn-rs/quinn) (Rust QUIC implementation)
- **SOCKS5/HTTP proxy client** — local proxy server supporting both SOCKS5 and HTTP CONNECT
- **TCP/UDP port forwarding** — forward local ports to remote targets through the Juicity QUIC connection
- **Share link generation** — generate `juicity://` share links from configuration
- **QR code export** — print QR codes to terminal or save as PNG images
- **Full-cone NAT UDP** — full-cone NAT support via underlay UDP (compatible with Go version)
- **BBR congestion control** — BBR congestion control enabled by default
- **Certificate pinning** — `pinned_certchain_sha256` support for enhanced security
- **TLS Export Keying Material** — RFC 5705-based authentication (compatible with upstream)
- **Configurable congestion control** — supports BBR, Cubic, and other quinn congestion controllers

## Project Structure

```
juicity-common/       # Shared library: configuration, protocol definitions, crypto, constants, share link generation
juicity-client/       # Client binary: connects to Juicity server, provides local SOCKS5/HTTP proxy and port forwarding
juicity-server/       # Server binary: accepts Juicity client connections, relays TCP/UDP traffic
```

### Crate Overview

| Crate | Description |
|-------|-------------|
| [`juicity-common`](juicity-common/src/lib.rs) | Shared types: [`Config`](juicity-common/src/config.rs) (all 18 fields), [`protocol`](juicity-common/src/protocol.rs) (TUIC-compatible wire format), [`crypto`](juicity-common/src/crypto.rs) (cert chain hashing, AES-128-GCM AEAD, ChaCha20-Poly1305 underlay), [`consts`](juicity-common/src/consts.rs) (timeouts, MTU, stream limits), [`link`](juicity-common/src/link.rs) (share link & QR code generation) |
| [`juicity-client`](juicity-client/src/main.rs) | Client binary with [`JuicityClient`](juicity-client/src/client.rs) (QUIC connection, TLS auth, stream management), [`LocalServer`](juicity-client/src/local.rs) (SOCKS5/HTTP proxy), [`Forwarder`](juicity-client/src/forwarder.rs) (TCP/UDP port forwarding) |
| [`juicity-server`](juicity-server/src/lib.rs) | Server binary with [`JuicityServer`](juicity-server/src/server.rs) (QUIC listener, auth, TCP/UDP relay), [`Dialer`](juicity-server/src/dialer.rs) (outbound TCP/UDP with optional bind address), [`InFlightUnderlayKey`](juicity-server/src/inflight.rs) (underlay auth key management), [`UdpEndpointPool`](juicity-server/src/udp.rs) (full-cone NAT session pool), [`DemuxUdpSocket`](juicity-server/src/underlay_socket.rs) (QUIC/non-QUIC packet demux) |

## Build

```bash
# Build all binaries in release mode
cargo build --release

# The binaries will be at:
#   target/release/juicity-client.exe
#   target/release/juicity-server.exe
```

### Requirements

- Rust 2021 edition (MSRV: stable)
- OpenSSL-compatible crypto (via `rustls/aws-lc-rs`)

## Configuration

### Server Configuration (`server.json`)

All 18 configuration fields are shown below:

```json
{
  "listen": "0.0.0.0:443",
  "users": {
    "00000000-0000-0000-0000-000000000000": "your-password"
  },
  "certificate": "/path/to/certificate.pem",
  "private_key": "/path/to/private_key.pem",
  "fwmark": "",
  "send_through": "",
  "dialer_link": "",
  "disable_outbound_udp443": false,
  "server": "",
  "uuid": "",
  "password": "",
  "sni": "",
  "allow_insecure": false,
  "pinned_certchain_sha256": "",
  "protect_path": "",
  "forward": {},
  "congestion_control": "bbr",
  "log_level": "info"
}
```

**Server field descriptions:**

| Field | Type | Description |
|-------|------|-------------|
| `listen` | string | Server listen address (`host:port`) |
| `users` | object | Map of UUID → password for authentication |
| `certificate` | string | Path to TLS certificate file (PEM format) |
| `private_key` | string | Path to TLS private key file (PEM format) |
| `fwmark` | string | Linux fwmark for outbound sockets (optional) |
| `send_through` | string | Bind outbound connections to a specific IP address (optional) |
| `dialer_link` | string | Dialer link configuration (optional, compatible with Go version) |
| `disable_outbound_udp443` | bool | Block outbound UDP on port 443 (optional) |
| `congestion_control` | string | QUIC congestion control algorithm (default: `"bbr"`) |
| `log_level` | string | Log level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"` |

### Client Configuration (`client.json`)

All 18 configuration fields are shown below:

```json
{
  "listen": "127.0.0.1:1080",
  "server": "example.com:443",
  "uuid": "00000000-0000-0000-0000-000000000000",
  "password": "your-password",
  "sni": "example.com",
  "allow_insecure": false,
  "pinned_certchain_sha256": "",
  "protect_path": "",
  "forward": {
    "127.0.0.1:8080": "example.com:80"
  },
  "users": {},
  "certificate": "",
  "private_key": "",
  "fwmark": "",
  "send_through": "",
  "dialer_link": "",
  "disable_outbound_udp443": false,
  "congestion_control": "bbr",
  "log_level": "info"
}
```

**Client field descriptions:**

| Field | Type | Description |
|-------|------|-------------|
| `listen` | string | Local proxy listen address (`host:port`); can be empty if only using forwarder |
| `server` | string | Juicity server address (`host:port`) |
| `uuid` | string | User UUID for authentication |
| `password` | string | User password (minimum 8 characters) |
| `sni` | string | TLS SNI (Server Name Indication); defaults to server IP if empty |
| `allow_insecure` | bool | Skip TLS certificate verification |
| `pinned_certchain_sha256` | string | SHA-256 hash of the pinned certificate chain (base64 or hex) |
| `protect_path` | string | Path to the protect_path socket (compatible with Go version) |
| `forward` | object | Port forwarding rules: `"local_addr"` → `"remote_target"` |
| `congestion_control` | string | QUIC congestion control algorithm (default: `"bbr"`) |
| `log_level` | string | Log level |

## Usage

### Running the Server

```bash
juicity-server -c server.json
```

The server listens for QUIC connections on the configured address, authenticates clients via TLS Export Keying Material (RFC 5705), and relays TCP/UDP traffic to the target destinations.

### Running the Client

```bash
# Start local SOCKS5/HTTP proxy
juicity-client -c client.json

# Start with custom log level
juicity-client -c client.json --log-level debug
```

The client connects to the Juicity server, authenticates, and provides a local SOCKS5/HTTP proxy on the configured `listen` address.

### Port Forwarding

The client supports TCP/UDP port forwarding through the `forward` configuration field. Each entry maps a local address to a remote target:

```json
{
  "forward": {
    "127.0.0.1:8080": "example.com:80",
    "127.0.0.1:5353/udp": "8.8.8.8:53",
    "0.0.0.0:1080/tcp": "proxy.example.com:1080"
  }
}
```

**Forward entry format:** `local_addr[/protocol]` → `remote_target`

- **Protocol suffix** (optional): `/tcp`, `/udp`, or omitted (defaults to both TCP and UDP)
- **Examples:**
  - `"127.0.0.1:8080": "example.com:80"` — forward TCP and UDP
  - `"127.0.0.1:5353/udp": "8.8.8.8:53"` — forward UDP only (DNS)
  - `"0.0.0.0:1080/tcp": "proxy.example.com:1080"` — forward TCP only

When running in forward-only mode (no `listen` configured), the client stays alive automatically:

```bash
juicity-client -c client-forward-only.json
```

### Share Link & QR Code

Both the client and server binaries support share link generation and QR code export without starting the proxy service.

#### Generate a share link

```bash
# Print the share link to terminal
juicity-client -c client.json --gen-link
# Output: juicity://00000000-0000-0000-0000-000000000000:your-password@example.com:443?sni=example.com&congestion_control=bbr&allow_insecure=0

juicity-server -c server.json --gen-link
```

#### Generate a QR code (terminal)

```bash
# Print a QR code to the terminal using ANSI block characters
juicity-client -c client.json --gen-qrcode
```

#### Generate a QR code (PNG file)

```bash
# Save the QR code as a PNG image
juicity-client -c client.json --gen-qrcode-png ./juicity-qrcode.png

juicity-server -c server.json --gen-qrcode-png ./juicity-server-qrcode.png
```

#### Share link format

```
juicity://<uuid>:<password>@<host>:<port>?sni=<sni>&congestion_control=<cc>&allow_insecure=<0|1>&pinned_certchain_sha256=<hash>
```

**Query parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `sni` | Yes | TLS SNI (defaults to server host) |
| `congestion_control` | No | Congestion control algorithm (e.g., `bbr`, `cubic`) |
| `allow_insecure` | No | Whether to skip TLS verification (`0` or `1`) |
| `pinned_certchain_sha256` | No | SHA-256 hash of the pinned certificate chain |

## Protocol

Juicity is an improvement over the TUIC protocol, addressing the following UDP issues:

1. **TUIC native mode**: When `udp_relay_mode` is set to `native`, packet loss causes severe application-layer retransmission.
2. **TUIC QUIC mode**: When `udp_relay_mode` is set to `quic`, each UDP datagram uses a separate unidirectional stream, leading to overhead.

Juicity uses **UDP over Stream** to solve these problems, multiplexing UDP data over bidirectional streams. The wire format is compatible with the Go implementation:

- **Authentication**: `[version=0][cmd_type=Authenticate(0x00)][uuid(16)][token(32)]` — token generated via TLS Export Keying Material (RFC 5705)
- **TCP relay**: `[proxy_header(Network=TCP=1, addr, port)]` followed by bidirectional stream copy
- **UDP relay**: `[proxy_header(Network=UDP=3, addr, port)][len(2)][payload]` — each datagram is self-contained
- **Underlay UDP**: Full-cone NAT support via non-QUIC UDP packets encrypted with ChaCha20-Poly1305 (HKDF-SHA1 key derivation, compatible with upstream)

### Command types

| Code | Command |
|------|---------|
| `0x00` | Authenticate |
| `0x01` | Connect (TCP) |
| `0x02` | Packet (UDP) |
| `0x03` | Dissociate |
| `0x04` | Heartbeat |

## Differences from Go Juicity

| Aspect | Go Juicity | Juicity-RS |
|--------|------------|------------|
| QUIC implementation | [`quic-go`](https://github.com/quic-go/quic-go) | [`quinn`](https://github.com/quinn-rs/quinn) (v0.11) |
| TLS library | Go standard library `crypto/tls` | [`rustls`](https://github.com/rustls/rustls) (v0.23) with `aws-lc-rs` |
| Logging | Custom logging | [`tracing`](https://github.com/tokio-rs/tracing) with `EnvFilter` |
| Configuration | JSON | JSON (same format, all 18 fields) |
| Congestion control | BBR (via quic-go) | BBR (via quinn's built-in `BbrConfig`) |
| ALPN | `h3` | `h3` (same) |
| Authentication | TLS Export Keying Material (RFC 5705) | Same algorithm |
| Underlay crypto | ChaCha20-Poly1305 + HKDF-SHA1 | Same algorithm |
| Certificate pinning | Supported | Supported (`pinned_certchain_sha256`) |
| Full-cone NAT | Supported | Supported (underlay UDP demux) |
| Port forwarding | Supported | Supported (TCP/UDP with protocol filter) |
| Share link / QR code | Not built-in | Built-in (`--gen-link`, `--gen-qrcode`, `--gen-qrcode-png`) |

## Recent Improvements

The following improvements have been made during the Rust rewrite, including compatibility alignment with the Go version and additional features:

### New Features
- **Client Forwarder** — TCP/UDP port forwarding with protocol filtering (`/tcp`, `/udp`, or both)
- **Share link generation** — `juicity://` URI generation from config (`--gen-link`)
- **QR code export** — terminal ANSI QR code (`--gen-qrcode`) and PNG file export (`--gen-qrcode-png`)
- **Full-cone NAT UDP** — underlay UDP support with ChaCha20-Poly1305 encryption (compatible with Go version)
- **BBR congestion control** — enabled by default via quinn's `BbrConfig`
- **Certificate pinning** — `pinned_certchain_sha256` with base64/hex auto-detection
- **`send_through` support** — bind outbound connections to a specific IP address via `BindDialer`
- **`disable_outbound_udp443`** — option to block outbound UDP on port 443
- **`protect_path`** — protect_path socket support (compatible with Go version)

### Compatibility Alignment
- **`maxIncomingStreams`** — configured via `MAX_OPEN_INCOMING_STREAMS` (100, matching Go default)
- **NAT timeout** — 3 minutes (180 seconds, matching Go version)
- **DNS query timeout** — 17 seconds (RFC 5452, matching Go version)
- **Keep-alive interval** — 10 seconds (matching Go version)
- **Authentication timeout** — 10 seconds
- **Underlay salt generation** — first 2 bytes zeroed (matching Go behavior)
- **Underlay PSK length** — 32 bytes (matching Go version)
- **Underlay auth wire format** — trojanc metadata format (compatible with upstream)
- **ALPN** — `h3` (matching Go version)
- **Token generation** — TLS Export Keying Material with UUID as context and password as label (RFC 5705)

### Bug Fixes & Code Quality
- Fixed TOCTOU race condition in connection state management (read-lock → write-lock pattern)
- Fixed underlay auth stream reset on write failure (set to `None` on error)
- Fixed multiple private key handling (warn on multiple keys, use first)
- Fixed IPv6 address parsing in share link generation
- Fixed URL encoding of special characters in share link parameters
- Fixed SOCKS5 UDP ASSOCIATE with NAT timeout cleanup
- Fixed UDP relay domain resolution caching (`domain_ip_map`)
- Fixed underlay session cleanup on send failure
- Proper error handling for all QUIC stream operations
- Comprehensive test coverage for share link generation and address parsing
- Code deduplication and dead code removal

## License

GNU AFFERO GENERAL PUBLIC LICENSE Version 3 (AGPL-3.0), see [LICENSE](LICENSE) for details.
