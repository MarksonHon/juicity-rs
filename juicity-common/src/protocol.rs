use uuid::Uuid;

/// Error types for the Juicity protocol
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("unexpected version: {0}")]
    UnexpectedVersion(u8),
    #[error("unexpected command type: {0}")]
    UnexpectedCmdType(u8),
    #[error("unexpected network type: {0}")]
    UnexpectedNetwork(u8),
    #[error("address type not supported: {0}")]
    UnsupportedAddressType(u8),
}

/// Represents the metadata for proxied connections
#[derive(Debug, Clone)]
pub struct ProxyMetadata {
    pub network: String,
    pub hostname: String,
    pub port: u16,
    pub uuid: Uuid,
}

impl ProxyMetadata {
    pub fn target_addr(&self) -> String {
        format!("{}:{}", self.hostname, self.port)
    }
}

/// Underlay authentication information for full-cone NAT UDP
#[derive(Debug, Clone)]
pub struct UnderlayAuth {
    pub iv: [u8; 32],
    pub psk: Vec<u8>,
    pub metadata: ProxyMetadata,
    pub uuid: Uuid,
}

pub type UnderlaySalt = [u8; crate::consts::UNDERLAY_SALT_LEN];
pub type Token = [u8; 32];

// ============================================================
// TUIC/Juicity command types (compatible with upstream)
// ============================================================
pub const AUTHENTICATE_TYPE: u8 = 0x00;
pub const CONNECT_TYPE: u8 = 0x01;
pub const PACKET_TYPE: u8 = 0x02;
pub const DISSOCIATE_TYPE: u8 = 0x03;
pub const HEARTBEAT_TYPE: u8 = 0x04;

// ============================================================
// Network types per spec: TCP=1, UDP=3
// ============================================================
pub const NETWORK_TCP: u8 = 1;
pub const NETWORK_UDP: u8 = 3;

// ============================================================
// Address type codes (per spec)
// ============================================================
pub const ADDR_TYPE_IPV4: u8 = 0;
pub const ADDR_TYPE_IPV6: u8 = 1;
pub const ADDR_TYPE_DOMAIN: u8 = 2;
pub const ADDR_TYPE_NONE: u8 = 255;

// trojanc metadata address type codes used in upstream underlay auth payload
const TROJAN_METADATA_TYPE_IPV4: u8 = 1;
const TROJAN_METADATA_TYPE_MSG: u8 = 2;
const TROJAN_METADATA_TYPE_DOMAIN: u8 = 3;
const TROJAN_METADATA_TYPE_IPV6: u8 = 4;
const UNDERLAY_PSK_LEN: usize = 32;

// ============================================================
// Protocol version
// ============================================================
pub const PROTOCOL_VERSION: u8 = 0;

/// Read a command head from a byte stream (TUIC compatible)
/// Returns (version, cmd_type)
pub fn read_command_head<R: std::io::Read>(reader: &mut R) -> anyhow::Result<(u8, u8)> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok((buf[0], buf[1]))
}

/// Read a command head from an async byte stream (TUIC compatible)
/// Returns (version, cmd_type)
pub async fn read_command_head_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<(u8, u8)> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf).await?;
    Ok((buf[0], buf[1]))
}

/// Read authenticate data: UUID (16 bytes) + TOKEN (32 bytes)
pub fn read_authenticate<R: std::io::Read>(reader: &mut R) -> anyhow::Result<(Uuid, Token)> {
    let mut uuid_bytes = [0u8; 16];
    reader.read_exact(&mut uuid_bytes)?;
    let uuid = Uuid::from_bytes(uuid_bytes);

    let mut token = [0u8; 32];
    reader.read_exact(&mut token)?;

    Ok((uuid, token))
}

/// Read authenticate data from an async reader: UUID (16 bytes) + TOKEN (32 bytes)
pub async fn read_authenticate_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<(Uuid, Token)> {
    let mut uuid_bytes = [0u8; 16];
    reader.read_exact(&mut uuid_bytes).await?;
    let uuid = Uuid::from_bytes(uuid_bytes);

    let mut token = [0u8; 32];
    reader.read_exact(&mut token).await?;

    Ok((uuid, token))
}

/// Write one UnderlayAuth message to a stream.
/// Layout (upstream-compatible): [iv(32)][psk(32)][trojanc_metadata]
pub async fn write_underlay_auth_async<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    auth: &UnderlayAuth,
) -> anyhow::Result<()> {
    writer.write_all(&auth.iv).await?;
    anyhow::ensure!(
        auth.psk.len() == UNDERLAY_PSK_LEN,
        "invalid underlay psk length: expected {}, got {}",
        UNDERLAY_PSK_LEN,
        auth.psk.len()
    );
    writer.write_all(&auth.psk).await?;
    write_underlay_metadata_async(writer, &auth.metadata).await?;
    Ok(())
}

/// Read one UnderlayAuth message from a stream.
/// Layout (upstream-compatible): [iv(32)][psk(32)][trojanc_metadata]
pub async fn read_underlay_auth_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<UnderlayAuth> {
    let mut iv = [0u8; 32];
    reader.read_exact(&mut iv).await?;

    let mut psk = vec![0u8; UNDERLAY_PSK_LEN];
    reader.read_exact(&mut psk).await?;

    let (hostname, port) = read_underlay_metadata_async(reader).await?;
    // UUID is hardcoded as nil here because the upstream underlay auth
    // payload (trojanc metadata) does not carry a UUID field.
    let uuid = Uuid::nil();

    Ok(UnderlayAuth {
        iv,
        psk,
        metadata: ProxyMetadata {
            network: "udp".to_string(),
            hostname,
            port,
            uuid,
        },
        uuid,
    })
}

async fn write_underlay_metadata_async<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    metadata: &ProxyMetadata,
) -> anyhow::Result<()> {
    if let Ok(ipv4) = metadata.hostname.parse::<std::net::Ipv4Addr>() {
        writer.write_all(&[TROJAN_METADATA_TYPE_IPV4]).await?;
        writer.write_all(&ipv4.octets()).await?;
        writer.write_all(&metadata.port.to_be_bytes()).await?;
        return Ok(());
    }

    if let Ok(ipv6) = metadata.hostname.parse::<std::net::Ipv6Addr>() {
        writer.write_all(&[TROJAN_METADATA_TYPE_IPV6]).await?;
        writer.write_all(&ipv6.octets()).await?;
        writer.write_all(&metadata.port.to_be_bytes()).await?;
        return Ok(());
    }

    let host_bytes = metadata.hostname.as_bytes();
    let host_len = u8::try_from(host_bytes.len())
        .map_err(|_| anyhow::anyhow!("underlay host too long: {}", host_bytes.len()))?;
    writer.write_all(&[TROJAN_METADATA_TYPE_DOMAIN]).await?;
    writer.write_all(&[host_len]).await?;
    writer.write_all(host_bytes).await?;
    writer.write_all(&metadata.port.to_be_bytes()).await?;
    Ok(())
}

async fn read_underlay_metadata_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<(String, u16)> {
    let mut typ = [0u8; 1];
    reader.read_exact(&mut typ).await?;

    match typ[0] {
        TROJAN_METADATA_TYPE_IPV4 => {
            let mut raw = [0u8; 6];
            reader.read_exact(&mut raw).await?;
            let host = std::net::Ipv4Addr::from([raw[0], raw[1], raw[2], raw[3]]).to_string();
            let port = u16::from_be_bytes([raw[4], raw[5]]);
            Ok((host, port))
        }
        TROJAN_METADATA_TYPE_IPV6 => {
            let mut raw = [0u8; 18];
            reader.read_exact(&mut raw).await?;
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&raw[..16]);
            let host = std::net::Ipv6Addr::from(ip).to_string();
            let port = u16::from_be_bytes([raw[16], raw[17]]);
            Ok((host, port))
        }
        TROJAN_METADATA_TYPE_DOMAIN => {
            let mut lb = [0u8; 1];
            reader.read_exact(&mut lb).await?;
            let dlen = lb[0] as usize;
            if dlen == 0 {
                anyhow::bail!("underlay metadata domain length is zero");
            }
            let mut domain = vec![0u8; dlen + 2];
            reader.read_exact(&mut domain).await?;
            let host = String::from_utf8_lossy(&domain[..dlen]).to_string();
            let port = u16::from_be_bytes([domain[dlen], domain[dlen + 1]]);
            Ok((host, port))
        }
        TROJAN_METADATA_TYPE_MSG => {
            let mut cmd = [0u8; 1];
            reader.read_exact(&mut cmd).await?;
            anyhow::bail!("underlay metadata message type is not supported")
        }
        other => anyhow::bail!("unknown underlay metadata type: {}", other),
    }
}

/// Read proxy header (network, addr_type, addr, port) from an async reader
pub async fn read_proxy_header_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> anyhow::Result<(u8, String, u16)> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf).await?;
    let network = buf[0];
    let addr_type = buf[1];

    let addr = match addr_type {
        ADDR_TYPE_IPV4 => {
            let mut ip = [0u8; 4];
            reader.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        ADDR_TYPE_IPV6 => {
            let mut ip = [0u8; 16];
            reader.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        ADDR_TYPE_DOMAIN => {
            let mut len_buf = [0u8; 1];
            reader.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            reader.read_exact(&mut domain).await?;
            String::from_utf8_lossy(&domain).to_string()
        }
        ADDR_TYPE_NONE => String::new(),
        _ => anyhow::bail!("unknown address type: {}", addr_type),
    };

    let port = if addr_type != ADDR_TYPE_NONE {
        let mut port_buf = [0u8; 2];
        reader.read_exact(&mut port_buf).await?;
        u16::from_be_bytes(port_buf)
    } else {
        0
    };

    Ok((network, addr, port))
}

/// Write proxy header bytes into a buffer and return them (sync helper for building headers)
pub fn build_proxy_header(network: u8, addr: &str, port: u16) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(32);

    let addr_type = if addr.is_empty() {
        ADDR_TYPE_NONE
    } else if addr.contains(':') {
        ADDR_TYPE_IPV6
    } else if addr.parse::<std::net::Ipv4Addr>().is_ok() {
        ADDR_TYPE_IPV4
    } else {
        ADDR_TYPE_DOMAIN
    };

    buf.push(network);
    buf.push(addr_type);

    match addr_type {
        ADDR_TYPE_IPV4 => {
            let ip: std::net::Ipv4Addr = addr.parse()?;
            buf.extend_from_slice(&ip.octets());
        }
        ADDR_TYPE_IPV6 => {
            let ip: std::net::Ipv6Addr = addr.parse()?;
            buf.extend_from_slice(&ip.octets());
        }
        ADDR_TYPE_DOMAIN => {
            let domain_bytes = addr.as_bytes();
            buf.push(domain_bytes.len() as u8);
            buf.extend_from_slice(domain_bytes);
        }
        ADDR_TYPE_NONE => {}
        _ => anyhow::bail!("unknown address type: {}", addr_type),
    }

    if addr_type != ADDR_TYPE_NONE {
        buf.extend_from_slice(&port.to_be_bytes());
    }

    Ok(buf)
}

/// Read proxy header from a sync reader (for testing)
pub fn read_proxy_header<R: std::io::Read>(reader: &mut R) -> anyhow::Result<(u8, String, u16)> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    let network = buf[0];
    let addr_type = buf[1];

    let addr = match addr_type {
        ADDR_TYPE_IPV4 => {
            let mut ip = [0u8; 4];
            reader.read_exact(&mut ip)?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        ADDR_TYPE_IPV6 => {
            let mut ip = [0u8; 16];
            reader.read_exact(&mut ip)?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        ADDR_TYPE_DOMAIN => {
            let mut len_buf = [0u8; 1];
            reader.read_exact(&mut len_buf)?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            reader.read_exact(&mut domain)?;
            String::from_utf8_lossy(&domain).to_string()
        }
        ADDR_TYPE_NONE => String::new(),
        _ => anyhow::bail!("unknown address type: {}", addr_type),
    };

    let port = if addr_type != ADDR_TYPE_NONE {
        let mut port_buf = [0u8; 2];
        reader.read_exact(&mut port_buf)?;
        u16::from_be_bytes(port_buf)
    } else {
        0
    };

    Ok((network, addr, port))
}

/// Generate token using the connection's export_keying_material (RFC 5705)
/// Compatible with upstream: token = state.TLS.ExportKeyingMaterial(string(uuid[:]), []byte(password), 32)
pub fn gen_token_via_connection(
    conn: &quinn::Connection,
    uuid: &Uuid,
    password: &str,
) -> anyhow::Result<Token> {
    let mut token = [0u8; 32];
    conn.export_keying_material(&mut token, uuid.as_bytes(), password.as_bytes())
        .map_err(|e| anyhow::anyhow!("export_keying_material failed: {:?}", e))?;
    Ok(token)
}
