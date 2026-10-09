use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use std::time::Instant;

use bytes::Bytes;
use juicity_common::consts;
use juicity_common::protocol;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::client::JuicityClient;

const INBOUND_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_HTTP_LINE_LEN: usize = 8 * 1024;
const MAX_HTTP_HEADERS: usize = 100;

pub(crate) async fn copy_direction<R, W>(reader: &mut R, writer: &mut W) -> std::io::Result<u64>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let copied = tokio::io::copy_buf(reader, writer).await?;
    writer.shutdown().await?;
    Ok(copied)
}

async fn read_http_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<String> {
    let mut line = String::new();
    reader
        .take((MAX_HTTP_LINE_LEN + 1) as u64)
        .read_line(&mut line)
        .await?;
    anyhow::ensure!(line.len() <= MAX_HTTP_LINE_LEN, "HTTP header line too long");
    anyhow::ensure!(line.ends_with('\n'), "incomplete HTTP request head");
    Ok(line)
}

/// Local proxy server that handles SOCKS5 and HTTP proxy.
///
/// Both protocols are served on the same address: the first byte of every
/// connection selects the handler (see [`handle_connection`]), so a single
/// port acts as a mixed inbound.
pub struct LocalServer {
    bind_addr: String,
    client: JuicityClient,
}

#[derive(Clone)]
struct UdpOutboundDatagram {
    addr: String,
    port: u16,
    payload: Bytes,
}

struct UdpSessionEntry {
    id: u64,
    /// Last time a datagram was forwarded through this session.
    /// Used by the cleanup task to detect zombie sessions whose tx channel
    /// remains open but no data has flowed for CLIENT_UDP_SESSION_IDLE_TIMEOUT.
    last_used: Instant,
    tx: mpsc::Sender<UdpOutboundDatagram>,
}

impl LocalServer {
    pub fn new(bind_addr: String, client: JuicityClient) -> Self {
        Self { bind_addr, client }
    }

    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.bind_addr).await?;
        self.serve_with_listener(listener).await
    }

    /// Serve connections accepted from an already bound listener.
    ///
    /// Embedders (e.g. the GUI) use this to surface bind errors synchronously
    /// before handing the accept loop to a background task.
    pub async fn serve_with_listener(&self, listener: TcpListener) -> anyhow::Result<()> {
        tracing::info!("Local proxy listening on {}", self.bind_addr);

        // Limit concurrent inbound TCP connections to avoid unbounded memory growth
        // during connection bursts (mirrors the UDP Semaphore(256) in the Forwarder).
        let sem = Arc::new(Semaphore::new(consts::MAX_CONCURRENT_TCP_CONNECTIONS));

        loop {
            // Acquire a permit before accepting; this blocks new accepts when the
            // limit is reached, providing back-pressure at the OS TCP accept queue.
            let permit = sem.clone().acquire_owned().await?;
            let (stream, addr) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    tracing::warn!(error = %e, "Local TCP accept error");
                    drop(permit);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let client = self.client.clone();

            tokio::spawn(async move {
                let _permit = permit; // held for the lifetime of the connection
                if let Err(e) = handle_connection(stream, addr, client).await {
                    tracing::info!(error = %e, "Connection handler error");
                }
            });
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    client: JuicityClient,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + INBOUND_HEAD_TIMEOUT;
    let local_addr = stream.local_addr()?;
    let mut buf = [0u8; 1];
    tokio::time::timeout_at(deadline, stream.peek(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("inbound handshake timed out"))??;

    // Mixed inbound: SOCKS5 always starts with the version byte 0x05, while
    // HTTP requests start with an ASCII method name (GET, CONNECT, ...).
    match buf[0] {
        0x05 => handle_socks5(stream, local_addr, peer_addr, client, deadline).await,
        _ => handle_http_proxy(stream, client, deadline).await,
    }
}

/// Handle a SOCKS5 proxy connection
async fn handle_socks5(
    mut stream: TcpStream,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
    client: JuicityClient,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let (cmd, host, port) = tokio::time::timeout_at(deadline, async {
        // Handshake: read methods
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await?;
        let n_methods = buf[1] as usize;
        // Stack-allocate the methods buffer (max 255 bytes per SOCKS5 spec)
        // instead of a Vec heap allocation for each connection.
        let mut methods = [0u8; 255];
        stream.read_exact(&mut methods[..n_methods]).await?;
        // Accept no-auth
        stream.write_all(&[0x05, 0x00]).await?;

        // Read request
        let mut req = [0u8; 4];
        stream.read_exact(&mut req).await?;
        let _ver = req[0];
        let cmd = req[1]; // 1=CONNECT, 3=UDP ASSOCIATE
        let _rsv = req[2];
        let addr_type = req[3];

        let (host, port) = match addr_type {
            0x01 => {
                let mut ip = [0u8; 4];
                stream.read_exact(&mut ip).await?;
                let mut port_buf = [0u8; 2];
                stream.read_exact(&mut port_buf).await?;
                (
                    std::net::Ipv4Addr::from(ip).to_string(),
                    u16::from_be_bytes(port_buf),
                )
            }
            0x03 => {
                let mut len_buf = [0u8; 1];
                stream.read_exact(&mut len_buf).await?;
                let len = len_buf[0] as usize;
                // Stack-allocate the domain buffer (max 255 bytes per SOCKS5 spec)
                // instead of a Vec heap allocation for each domain lookup.
                let mut domain = [0u8; 255];
                stream.read_exact(&mut domain[..len]).await?;
                let mut port_buf = [0u8; 2];
                stream.read_exact(&mut port_buf).await?;
                (
                    String::from_utf8(domain[..len].to_vec())?,
                    u16::from_be_bytes(port_buf),
                )
            }
            0x04 => {
                let mut ip = [0u8; 16];
                stream.read_exact(&mut ip).await?;
                let mut port_buf = [0u8; 2];
                stream.read_exact(&mut port_buf).await?;
                (
                    std::net::Ipv6Addr::from(ip).to_string(),
                    u16::from_be_bytes(port_buf),
                )
            }
            _ => anyhow::bail!("unsupported address type: {}", addr_type),
        };
        Ok::<_, anyhow::Error>((cmd, host, port))
    })
    .await
    .map_err(|_| anyhow::anyhow!("SOCKS5 handshake timed out"))??;

    match cmd {
        0x01 => {
            // TCP CONNECT
            tracing::info!(
                target_addr = %host,
                target_port = %port,
                "SOCKS5 CONNECT"
            );
            tracing::info!(
                client_addr = %peer_addr,
                target_addr = %host,
                target_port = %port,
                protocol = "socks5",
                "New connection"
            );
            let (mut quic_send, quic_recv) = match client.open_tcp_stream(&host, port).await {
                Ok(pair) => pair,
                Err(e) => {
                    stream.write_all(&build_socks5_response(0x01, "0.0.0.0", 0)).await?;
                    return Err(e);
                }
            };
            let response = build_socks5_response(0x00, &host, port);
            stream.write_all(&response).await?;

            let (local_rx, mut local_tx) = stream.split();

            // Use 16KB buffered readers (reduced from 64KB) for high-throughput bidirectional copy.
            // 64KB × 2 × 256 concurrent connections = 32MB; 16KB × 2 × 256 = 8MB — saves 24MB.
            let mut local_rx = tokio::io::BufReader::with_capacity(16 * 1024, local_rx);
            let mut quic_recv = tokio::io::BufReader::with_capacity(16 * 1024, quic_recv);

            let (r1, r2) = tokio::join!(
                copy_direction(&mut local_rx, &mut quic_send),
                copy_direction(&mut quic_recv, &mut local_tx),
            );
            if let Err(e) = r1 {
                tracing::info!(error = %e, direction = "local->quic", protocol = "socks5", "SOCKS5 copy error");
            }
            if let Err(e) = r2 {
                tracing::info!(error = %e, direction = "quic->local", protocol = "socks5", "SOCKS5 copy error");
            }
        }
        0x03 => {
            // UDP ASSOCIATE
            tracing::info!(
                target_addr = %host,
                target_port = %port,
                "SOCKS5 UDP ASSOCIATE"
            );

            // Bind a local UDP port for the SOCKS5 client to send UDP datagrams to.
            // Use the same IP family as the incoming TCP connection so the address
            // returned in the SOCKS5 response is reachable by the client
            // (127.0.0.1 for IPv4 connections, ::1 for IPv6 connections).
            let udp_bind_addr = SocketAddr::new(local_addr.ip(), 0);
            let bind_socket = Arc::new(UdpSocket::bind(udp_bind_addr).await?);
            let udp_listen_addr = bind_socket.local_addr()?;

            // Send success response with the actual UDP listening address
            let response = build_socks5_response(
                0x00,
                &udp_listen_addr.ip().to_string(),
                udp_listen_addr.port(),
            );
            stream.write_all(&response).await?;

            let client_clone = client.clone();
            let bind_socket_clone = bind_socket.clone();
            let sessions: Arc<Mutex<HashMap<SocketAddr, UdpSessionEntry>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let session_seq = Arc::new(AtomicU64::new(1));

            // ctrl_cancel is signalled when the TCP control connection drops.
            let ctrl_cancel = CancellationToken::new();
            let ctrl_cancel_clone = ctrl_cancel.clone();

            // Per-session CancellationToken: when the forwarder task exits for any reason
            // (TCP control close, NAT timeout, socket error), all session supervisor tasks
            // are cancelled promptly via drop_guard instead of waiting for NAT timeout.
            let session_cancel = CancellationToken::new();
            let cancel_guard = session_cancel.clone().drop_guard();

            // Periodic cleanup: remove UDP ASSOCIATE sessions whose writer channel has
            // been closed (e.g., supervisor task paniced without removing its entry)
            // or whose idle time exceeds the NAT timeout.
            // This mirrors the cleanup task in forwarder.rs to keep them consistent.
            let sessions_cleanup = sessions.clone();
            let ctrl_cancel_cleanup = ctrl_cancel_clone.clone();
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(consts::CLIENT_UDP_SESSION_CLEANUP_INTERVAL);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let idle_cutoff = Instant::now() - consts::CLIENT_UDP_SESSION_IDLE_TIMEOUT;
                            let mut guard = sessions_cleanup.lock().await;
                            let before = guard.len();
                            guard.retain(|_, s| !s.tx.is_closed() && s.last_used > idle_cutoff);
                            let after = guard.len();
                            drop(guard);
                            if before != after {
                                tracing::debug!(
                                    "UDP ASSOCIATE cleanup: removed {} orphaned session(s)",
                                    before - after
                                );
                            }
                        }
                        _ = ctrl_cancel_cleanup.cancelled() => break,
                    }
                }
            });

            // Spawn UDP forwarder. Per Juicity spec, datagrams from the same source
            // address triplet SHOULD share one stream and be recycled by NAT timeout.
            tokio::spawn(async move {
                // When this task exits (any path), cancel_guard fires and cancels all
                // session supervisors, releasing their Arc references promptly.
                let _cancel_guard = cancel_guard;
                let mut buf = vec![0u8; consts::MAX_UDP_PAYLOAD];
                // Use a persistent sleep_until so the timer is only created once and
                // can be reset on each received datagram without recreating the future.
                let nat_deadline = tokio::time::Instant::now() + consts::DEFAULT_NAT_TIMEOUT;
                let nat_timer = tokio::time::sleep_until(nat_deadline);
                tokio::pin!(nat_timer);
                loop {
                    tokio::select! {
                        result = bind_socket_clone.recv_from(&mut buf) => {
                            match result {
                                Ok((n, src)) => {
                                    if src.ip() != peer_addr.ip() {
                                        tracing::debug!(source = %src, "Dropping SOCKS5 UDP datagram from another IP");
                                        continue;
                                    }
                                    // Reset the NAT timeout on each received datagram.
                                    nat_timer.as_mut().reset(
                                        tokio::time::Instant::now() + consts::DEFAULT_NAT_TIMEOUT,
                                    );

                                    let datagram = match parse_socks5_udp_request(&buf[..n]) {
                                        Some(v) => v,
                                        None => continue,
                                    };
                                    if datagram.payload.len() > consts::MAX_UDP_PAYLOAD {
                                        tracing::warn!(len = datagram.payload.len(), "Dropping oversized UDP payload");
                                        continue;
                                    }

                                    let existing = {
                                        let guard = sessions.lock().await;
                                        guard.get(&src).map(|s| (s.id, s.tx.clone()))
                                    };

                                    if let Some((session_id, tx)) = existing {
                                        if tx.send(datagram.clone()).await.is_ok() {
                                            // Update last_used so the cleanup task does not
                                            // consider this session stale.
                                            if let Some(s) = sessions.lock().await.get_mut(&src) {
                                                s.last_used = Instant::now();
                                            }
                                            continue;
                                        }
                                        remove_session_if_match(&sessions, src, session_id).await;
                                    }

                                    let new_session_id = session_seq.fetch_add(1, Ordering::Relaxed);
                                    match start_udp_assoc_session(
                                        client_clone.clone(),
                                        bind_socket_clone.clone(),
                                        sessions.clone(),
                                        src,
                                        new_session_id,
                                        datagram,
                                        session_cancel.clone(),
                                    )
                                    .await
                                    {
                                        Ok(tx) => {
                                            let mut guard = sessions.lock().await;
                                            guard.insert(
                                                src,
                                                UdpSessionEntry {
                                                    id: new_session_id,
                                                    last_used: Instant::now(),
                                                    tx,
                                                },
                                            );
                                        }
                                        Err(e) => {
                                            tracing::info!(error = %e, "UDP ASSOCIATE session open error");
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::info!(error = %e, "UDP read error");
                                    break;
                                }
                            }
                        }
                        _ = &mut nat_timer => {
                            // NAT timeout — clean up all sessions before breaking
                            let mut guard = sessions.lock().await;
                            guard.clear();
                            break;
                        }
                        _ = ctrl_cancel_clone.cancelled() => {
                            // TCP control connection dropped — clean up all sessions
                            tracing::info!("UDP ASSOCIATE control connection closed, cleaning up sessions");
                            let mut guard = sessions.lock().await;
                            guard.clear();
                            break;
                        }
                    }
                }
            });

            // Keep the TCP control connection alive until the client disconnects.
            // When the client disconnects, cancel the forwarder task to clean up sessions.
            let mut dummy = [0u8; 1];
            let _ = stream.read(&mut dummy).await;
            ctrl_cancel.cancel();
        }
        _ => {
            let response = build_socks5_response(0x07, "0.0.0.0", 0);
            stream.write_all(&response).await?;
        }
    }

    Ok(())
}

async fn start_udp_assoc_session(
    client: JuicityClient,
    bind_socket: Arc<UdpSocket>,
    sessions: Arc<Mutex<HashMap<SocketAddr, UdpSessionEntry>>>,
    local_client_addr: SocketAddr,
    session_id: u64,
    first_datagram: UdpOutboundDatagram,
    cancel: CancellationToken,
) -> anyhow::Result<mpsc::Sender<UdpOutboundDatagram>> {
    let (send, mut recv) = client
        .open_udp_stream(
            &first_datagram.addr,
            first_datagram.port,
            &first_datagram.payload[..],
        )
        .await?;

    let (tx, mut rx) = mpsc::channel::<UdpOutboundDatagram>(256);

    let sessions_for_supervisor = sessions.clone();
    let bind_socket_for_reader = bind_socket.clone();

    tokio::spawn(async move {
        // Reusable scratch buffer to avoid per-packet heap allocation for address headers.
        let mut addr_buf = Vec::with_capacity(32);
        let mut writer = tokio::spawn(async move {
            // RAII guard: ensure send.finish() is called even when this task is
            // aborted (e.g. via cancel).  Without this, the QUIC send stream
            // would be left in a half-closed state until the connection idle
            // timeout fires (up to 600s), holding stream resources unnecessarily.
            struct SendGuard {
                send: Option<quinn::SendStream>,
            }
            impl Drop for SendGuard {
                fn drop(&mut self) {
                    if let Some(ref mut s) = self.send {
                        let _ = s.finish();
                    }
                }
            }
            let mut guard = SendGuard { send: Some(send) };
            loop {
                match tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, rx.recv()).await {
                    Ok(Some(datagram)) => {
                        if JuicityClient::send_udp_datagram(
                            guard.send.as_mut().unwrap(),
                            &datagram.addr,
                            datagram.port,
                            &datagram.payload[..],
                            &mut addr_buf,
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });

        let mut reader = tokio::spawn(async move {
            // Pre-allocate a reusable buffer (max UDP datagram size) to avoid
            // per-packet heap allocation inside the hot loop.
            let mut recv_buf = Vec::with_capacity(65535);
            loop {
                let (resp_addr, resp_port) =
                    match read_one_udp_response(&mut recv, &mut recv_buf).await {
                        Ok(v) => v,
                        Err(_) => break,
                    };

                let socks5_packet = build_socks5_udp_packet(&resp_addr, resp_port, &recv_buf);
                if bind_socket_for_reader
                    .send_to(&socks5_packet, local_client_addr)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        tokio::select! {
            _ = &mut writer => {
                reader.abort();
                let _ = reader.await;
            }
            _ = &mut reader => {
                writer.abort();
                let _ = writer.await;
            }
            _ = cancel.cancelled() => {
                // Forwarder exited: abort both tasks immediately instead of
                // waiting for QUIC I/O to time out (up to DEFAULT_NAT_TIMEOUT).
                writer.abort();
                reader.abort();
            }
        }

        remove_session_if_match(&sessions_for_supervisor, local_client_addr, session_id).await;
    });

    Ok(tx)
}

async fn read_one_udp_response(
    recv: &mut quinn::RecvStream,
    buf: &mut Vec<u8>,
) -> anyhow::Result<(String, u16)> {
    // Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
    let (resp_addr, resp_port) = tokio::time::timeout(
        consts::DEFAULT_NAT_TIMEOUT,
        protocol::read_trojanc_addr_async(recv),
    )
    .await??;

    let mut len_buf = [0u8; 2];
    tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, recv.read_exact(&mut len_buf)).await??;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    buf.resize(pkt_len, 0);
    tokio::time::timeout(
        consts::DEFAULT_NAT_TIMEOUT,
        recv.read_exact(&mut buf[..pkt_len]),
    )
    .await??;

    Ok((resp_addr, resp_port))
}

async fn remove_session_if_match(
    sessions: &Arc<Mutex<HashMap<SocketAddr, UdpSessionEntry>>>,
    local_client_addr: SocketAddr,
    session_id: u64,
) {
    let mut guard = sessions.lock().await;
    if let Some(existing) = guard.get(&local_client_addr) {
        if existing.id == session_id {
            guard.remove(&local_client_addr);
        }
    }
}

fn parse_socks5_udp_request(packet: &[u8]) -> Option<UdpOutboundDatagram> {
    // SOCKS5 UDP request: RSV(2) + FRAG(1) + ATYP(1) + DST.ADDR + DST.PORT(2) + DATA
    if packet.len() < 4 {
        return None;
    }

    // Fragmented UDP is not supported.
    if packet[2] != 0x00 {
        return None;
    }

    let mut offset = 3usize;
    let atyp = *packet.get(offset)?;
    offset += 1;

    let (addr, port) = match atyp {
        0x01 => {
            if packet.len() < offset + 4 + 2 {
                return None;
            }
            let ip = std::net::Ipv4Addr::new(
                packet[offset],
                packet[offset + 1],
                packet[offset + 2],
                packet[offset + 3],
            );
            offset += 4;
            let p = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
            offset += 2;
            (ip.to_string(), p)
        }
        0x03 => {
            let dlen = *packet.get(offset)? as usize;
            offset += 1;
            if packet.len() < offset + dlen + 2 {
                return None;
            }
            let domain = String::from_utf8(packet[offset..offset + dlen].to_vec()).ok()?;
            offset += dlen;
            let p = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
            offset += 2;
            (domain, p)
        }
        0x04 => {
            if packet.len() < offset + 16 + 2 {
                return None;
            }
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&packet[offset..offset + 16]);
            offset += 16;
            let p = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
            offset += 2;
            (std::net::Ipv6Addr::from(ip).to_string(), p)
        }
        _ => return None,
    };

    if packet.len() < offset {
        return None;
    }

    Some(UdpOutboundDatagram {
        addr,
        port,
        payload: Bytes::copy_from_slice(&packet[offset..]),
    })
}

fn build_socks5_udp_packet(addr: &str, port: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(payload.len() + 32);
    packet.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV, RSV, FRAG

    if let Ok(ipv4) = addr.parse::<std::net::Ipv4Addr>() {
        packet.push(0x01);
        packet.extend_from_slice(&ipv4.octets());
    } else if let Ok(ipv6) = addr.parse::<std::net::Ipv6Addr>() {
        packet.push(0x04);
        packet.extend_from_slice(&ipv6.octets());
    } else {
        let domain_bytes = addr.as_bytes();
        packet.push(0x03);
        packet.push(domain_bytes.len() as u8);
        packet.extend_from_slice(domain_bytes);
    }

    packet.extend_from_slice(&port.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

/// Handle an HTTP proxy connection.
///
/// Two request forms are supported, so the listener behaves as a real mixed
/// inbound next to SOCKS5:
///
/// * `CONNECT host:port` — used by HTTPS clients, which then tunnel raw bytes.
/// * absolute-form requests (`GET http://host/path HTTP/1.1`) — plain HTTP
///   proxying. The target is rewritten to origin-form and the upstream
///   connection is asked to close after the response, which gives the relay a
///   well-defined end even for close-delimited bodies.
async fn handle_http_proxy(
    stream: TcpStream,
    client: JuicityClient,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::with_capacity(8 * 1024, read_half);

    let (method, target, version, headers) = tokio::time::timeout_at(deadline, async {
        // ── Request line ──
        let request_line = read_http_line(&mut reader).await?;

        let mut parts = request_line.split_whitespace();
        let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
        else {
            write_half
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .await?;
            anyhow::bail!("invalid HTTP request line: {}", request_line.trim());
        };
        let (method, target, version) = (method.to_string(), target.to_string(), version.to_string());

        // ── Headers ──
        let mut headers = Vec::new();
        loop {
            let line = read_http_line(&mut reader).await?;
            if line.trim().is_empty() {
                break;
            }
            anyhow::ensure!(headers.len() < MAX_HTTP_HEADERS, "too many HTTP headers");
            headers.push(line);
        }
        Ok::<_, anyhow::Error>((method, target, version, headers))
    })
    .await
    .map_err(|_| anyhow::anyhow!("HTTP request head timed out"))??;

    if method == "CONNECT" {
        return handle_http_connect(reader, write_half, &target, client).await;
    }

    handle_http_forward(
        reader, write_half, &method, &target, &version, &headers, client,
    )
    .await
}

/// Handle an HTTP `CONNECT` tunnel.
async fn handle_http_connect(
    reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    target: &str,
    client: JuicityClient,
) -> anyhow::Result<()> {
    // Parse host:port from the CONNECT target, properly handling IPv6 addresses like [::1]:443.
    let (host, port) = match juicity_common::link::parse_host_port(target) {
        Ok((host, port)) => (host, port),
        Err(_) => (target.to_string(), 443u16),
    };

    tracing::info!(
        target_addr = %host,
        target_port = %port,
        "HTTP CONNECT"
    );

    // Bytes read past the request headers already belong to the tunnel (e.g.
    // the TLS ClientHello), so they must be forwarded before the raw copy.
    let leftover = reader.buffer().to_vec();

    let (mut quic_send, quic_recv) = client.open_tcp_stream(&host, port).await?;

    write_half
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    if !leftover.is_empty() {
        quic_send.write_all(&leftover).await?;
    }

    let mut local_rx = tokio::io::BufReader::with_capacity(16 * 1024, reader.into_inner());
    let mut quic_recv = tokio::io::BufReader::with_capacity(16 * 1024, quic_recv);

    let (r1, r2) = tokio::join!(
        copy_direction(&mut local_rx, &mut quic_send),
        copy_direction(&mut quic_recv, &mut write_half),
    );
    if let Err(e) = r1 {
        tracing::info!(error = %e, direction = "local->quic", protocol = "http", "HTTP CONNECT copy error");
    }
    if let Err(e) = r2 {
        tracing::info!(error = %e, direction = "quic->local", protocol = "http", "HTTP CONNECT copy error");
    }

    Ok(())
}

/// Handle a plain HTTP proxy request (absolute-form request target).
async fn handle_http_forward(
    mut reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    method: &str,
    target: &str,
    version: &str,
    headers: &[String],
    client: JuicityClient,
) -> anyhow::Result<()> {
    let Some((host, port, path)) = split_proxy_target(target, headers) else {
        write_half
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await?;
        anyhow::bail!("unsupported HTTP request target: {target}");
    };

    tracing::info!(
        target_addr = %host,
        target_port = %port,
        method = %method,
        "HTTP proxy request"
    );

    let request = build_origin_request(method, &path, version, headers, &host, port);

    let (mut quic_send, quic_recv) = client.open_tcp_stream(&host, port).await?;
    quic_send.write_all(request.as_bytes()).await?;

    // Whatever is left on the client connection is the request body. Relay it
    // and the response concurrently, but finish as soon as the response is
    // complete so the client sees the end of the (possibly close-delimited)
    // body instead of waiting for us to close first.
    let body_relay = tokio::spawn(async move {
        let _ = tokio::io::copy_buf(&mut reader, &mut quic_send).await;
        let _ = quic_send.finish();
    });

    let mut quic_recv = tokio::io::BufReader::with_capacity(16 * 1024, quic_recv);
    if let Err(e) = tokio::io::copy_buf(&mut quic_recv, &mut write_half).await {
        tracing::info!(error = %e, direction = "quic->local", protocol = "http", "HTTP relay error");
    }
    // Signal the end of the response to the client, then stop relaying.
    let _ = write_half.shutdown().await;
    body_relay.abort();

    Ok(())
}

/// Extract `(host, port, path)` from a proxy request target.
///
/// Handles absolute-form targets (`http://host/path`) and, as a fallback,
/// origin-form targets whose host comes from the `Host` header.
fn split_proxy_target(target: &str, headers: &[String]) -> Option<(String, u16, String)> {
    if let Some(rest) = target.strip_prefix("http://") {
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        // Drop any userinfo part (`user:pass@host`).
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        if authority.is_empty() {
            return None;
        }
        let (host, port) = juicity_common::link::parse_host_port(authority)
            .unwrap_or_else(|_| (authority.to_string(), 80));
        return Some((host, port, path.to_string()));
    }

    // Origin-form target: the authority has to come from the Host header.
    let authority = headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("host")
            .then(|| value.trim().to_string())
    })?;
    if authority.is_empty() {
        return None;
    }

    let (host, port) = juicity_common::link::parse_host_port(&authority)
        .unwrap_or_else(|_| (authority.clone(), 80));
    Some((host, port, target.to_string()))
}

/// Rewrite a proxy request into an origin-form request for the target server.
///
/// Hop-by-hop proxy headers are dropped and `Connection: close` is forced so
/// the response relay finishes on upstream EOF.
fn build_origin_request(
    method: &str,
    path: &str,
    version: &str,
    headers: &[String],
    host: &str,
    port: u16,
) -> String {
    let mut request = String::with_capacity(256);
    request.push_str(method);
    request.push(' ');
    request.push_str(path);
    request.push(' ');
    request.push_str(version);
    request.push_str("\r\n");

    let mut has_host = false;
    for header in headers {
        let name = header.split(':').next().unwrap_or_default().trim();
        if name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        has_host |= name.eq_ignore_ascii_case("host");
        request.push_str(header.trim_end_matches(['\r', '\n']));
        request.push_str("\r\n");
    }

    if !has_host {
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        request.push_str("Host: ");
        request.push_str(&authority);
        request.push_str("\r\n");
    }

    request.push_str("Connection: close\r\n\r\n");
    request
}

/// Build a SOCKS5 response, automatically detecting the address type from the host string.
fn build_socks5_response(reply: u8, host: &str, port: u16) -> Vec<u8> {
    let (addr_type, addr_bytes): (u8, Vec<u8>) = if let Ok(ip) = host.parse::<std::net::Ipv4Addr>()
    {
        (0x01, ip.octets().to_vec())
    } else if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        (0x04, ip.octets().to_vec())
    } else {
        let domain_bytes = host.as_bytes();
        let mut bytes = Vec::with_capacity(1 + domain_bytes.len());
        bytes.push(domain_bytes.len() as u8);
        bytes.extend_from_slice(domain_bytes);
        (0x03, bytes)
    };

    let mut response = Vec::with_capacity(4 + addr_bytes.len() + 2);
    response.extend_from_slice(&[0x05, reply, 0x00, addr_type]);
    response.extend_from_slice(&addr_bytes);
    response.extend_from_slice(&port.to_be_bytes());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_preserves_reply_after_client_half_close() {
        let (mut client, local) = tokio::io::duplex(64);
        let (remote, mut upstream) = tokio::io::duplex(64);
        let relay = tokio::spawn(async move {
            let (local_rx, mut local_tx) = tokio::io::split(local);
            let (remote_rx, mut remote_tx) = tokio::io::split(remote);
            let mut local_rx = tokio::io::BufReader::new(local_rx);
            let mut remote_rx = tokio::io::BufReader::new(remote_rx);
            tokio::try_join!(
                copy_direction(&mut local_rx, &mut remote_tx),
                copy_direction(&mut remote_rx, &mut local_tx),
            ).unwrap();
        });
        let reply = vec![42; 4096];
        let expected = reply.clone();
        let exchange = async {
            let server = tokio::spawn(async move {
                let mut request = Vec::new();
                upstream.read_to_end(&mut request).await.unwrap();
                assert_eq!(request, b"request");
                upstream.write_all(&reply).await.unwrap();
                upstream.shutdown().await.unwrap();
            });
            client.write_all(b"request").await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, expected);
            server.await.unwrap();
            relay.await.unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), exchange).await.unwrap();
    }

    #[tokio::test]
    async fn http_line_limit_bounds_unterminated_input() {
        let at_limit = format!("{}\r\n", "a".repeat(MAX_HTTP_LINE_LEN - 2));
        assert_eq!(read_http_line(&mut at_limit.as_bytes()).await.unwrap(), at_limit);
        let over_limit = format!("{}\r\n", "a".repeat(MAX_HTTP_LINE_LEN - 1));
        assert!(read_http_line(&mut over_limit.as_bytes()).await.is_err());
        let unterminated = "a".repeat(MAX_HTTP_LINE_LEN + 1);
        assert!(read_http_line(&mut unterminated.as_bytes()).await.is_err());
    }

    fn header(line: &str) -> String {
        format!("{line}\r\n")
    }

    #[test]
    fn proxy_target_parses_absolute_form() {
        assert_eq!(
            split_proxy_target("http://www.google.com/", &[]),
            Some(("www.google.com".to_string(), 80, "/".to_string()))
        );
        assert_eq!(
            split_proxy_target("http://example.com:8080/a/b?c=d", &[]),
            Some(("example.com".to_string(), 8080, "/a/b?c=d".to_string()))
        );
        // No path -> "/".
        assert_eq!(
            split_proxy_target("http://example.com", &[]),
            Some(("example.com".to_string(), 80, "/".to_string()))
        );
        // IPv6 authority and credentials are handled.
        assert_eq!(
            split_proxy_target("http://user:pw@[::1]:8080/x", &[]),
            Some(("::1".to_string(), 8080, "/x".to_string()))
        );
        assert_eq!(split_proxy_target("http://", &[]), None);
    }

    #[test]
    fn proxy_target_falls_back_to_host_header() {
        let headers = vec![header("Host: example.com")];
        assert_eq!(
            split_proxy_target("/index.html", &headers),
            Some(("example.com".to_string(), 80, "/index.html".to_string()))
        );

        let headers = vec![header("host: example.com:8443")];
        assert_eq!(
            split_proxy_target("/", &headers),
            Some(("example.com".to_string(), 8443, "/".to_string()))
        );

        // No Host header and no absolute URI -> unusable.
        assert_eq!(split_proxy_target("/", &[]), None);
    }

    #[test]
    fn origin_request_rewrites_target_and_drops_proxy_headers() {
        let headers = vec![
            header("Host: www.google.com"),
            header("User-Agent: curl/8.21.0"),
            header("Proxy-Connection: Keep-Alive"),
            header("Connection: keep-alive"),
            header("Proxy-Authorization: Basic Zm9v"),
        ];

        let request = build_origin_request("GET", "/", "HTTP/1.1", &headers, "www.google.com", 80);

        assert!(request.starts_with("GET / HTTP/1.1\r\n"));
        assert!(request.contains("\r\nHost: www.google.com\r\n"));
        assert!(request.contains("\r\nUser-Agent: curl/8.21.0\r\n"));
        assert!(!request.to_lowercase().contains("proxy-connection"));
        assert!(!request.to_lowercase().contains("proxy-authorization"));
        // The client's Connection header is replaced by our own.
        assert_eq!(request.to_lowercase().matches("connection:").count(), 1);
        assert!(request.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn origin_request_synthesizes_missing_host_header() {
        let request = build_origin_request("GET", "/x", "HTTP/1.1", &[], "example.com", 8080);
        assert!(request.contains("\r\nHost: example.com:8080\r\n"));

        let request = build_origin_request("GET", "/x", "HTTP/1.1", &[], "::1", 80);
        assert!(request.contains("\r\nHost: [::1]:80\r\n"));
    }
}
