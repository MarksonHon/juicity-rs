use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use juicity_common::consts;
use juicity_common::protocol;
use juicity_common::Config;
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, RecvStream, SendStream, VarInt};
use uuid::Uuid;

/// A single slot in the connection pool, holding one QUIC connection.
struct ConnectionSlot {
    conn: Option<Connection>,
}

/// A Juicity client that connects to a remote Juicity server.
///
/// Maintains a pool of `pool_size` QUIC connections for stream multiplexing
/// and reduced head-of-line blocking.  Streams are distributed across the pool
/// in round-robin order.
#[derive(Clone)]
pub struct JuicityClient {
    endpoint: Arc<Endpoint>,
    server_addr: SocketAddr,
    uuid: Uuid,
    password: zeroize::Zeroizing<String>,
    sni: String,
    quic_config: Arc<ClientConfig>,
    /// Pool of QUIC connection slots.  Each slot may independently reconnect.
    pool: Arc<tokio::sync::RwLock<Vec<ConnectionSlot>>>,
    /// Number of slots in the pool.
    pool_size: usize,
    /// Round-robin cursor for selecting the next slot to use.
    next_slot: Arc<AtomicUsize>,
    /// Persistent auth unidirectional stream (shared across all pool slots —
    /// only one auth stream is needed since it's per-client, not per-connection).
    /// Protected by a Mutex because SendStream is !Clone and we need &mut access.
    auth_uni_stream: Arc<tokio::sync::Mutex<Option<SendStream>>>,
    /// Serialises reconnection: only one task may execute the slow reconnect
    /// path at a time (across all slots).
    reconnect_lock: Arc<tokio::sync::Mutex<()>>,
    /// Notifies waiting tasks when a reconnection attempt completes.
    reconnect_notify: Arc<tokio::sync::Notify>,
    /// Tracks the last reconnection failure time for exponential backoff.
    last_reconnect_failure: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
    /// Counts consecutive reconnection failures for exponential backoff.
    /// Reset to 0 on successful reconnection.
    reconnect_attempts: Arc<tokio::sync::Mutex<u32>>,
}

impl JuicityClient {
    /// Build a TLS client config based on the allow_insecure / pinned_certchain_sha256 settings.
    fn build_tls_config(
        allow_insecure: bool,
        pinned_hash: &[u8],
        provider: &rustls::crypto::CryptoProvider,
        enable_early_data: bool,
    ) -> anyhow::Result<rustls::ClientConfig> {
        let mut tls_config: rustls::ClientConfig = if allow_insecure {
            rustls::ClientConfig::builder_with_provider(provider.clone().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SkipVerify::new(provider.clone())))
                .with_no_client_auth()
        } else if !pinned_hash.is_empty() {
            let hash_clone = pinned_hash.to_vec();
            rustls::ClientConfig::builder_with_provider(provider.clone().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinVerify::new(
                    provider.clone(),
                    hash_clone,
                )))
                .with_no_client_auth()
        } else {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder_with_provider(provider.clone().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        // Juicity spec requires ALPN to be h3.
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        // Enable 0-RTT (Early Data) to reduce reconnection latency
        tls_config.enable_early_data = enable_early_data;

        Ok(tls_config)
    }

    /// Build a QUIC client config (TLS + transport settings).
    fn build_quic_config(
        allow_insecure: bool,
        pinned_hash: &[u8],
        provider: &rustls::crypto::CryptoProvider,
        congestion_control: &str,
        initial_rtt: Option<u64>,
        keep_alive_interval: Option<u64>,
        enable_0rtt: bool,
    ) -> anyhow::Result<ClientConfig> {
        if allow_insecure {
            tracing::warn!("TLS certificate verification is DISABLED (allow_insecure=true). This is insecure and should only be used for testing.");
        }
        let tls_config =
            Self::build_tls_config(allow_insecure, pinned_hash, provider, enable_0rtt)?;

        let mut quic_config = ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?,
        ));

        let mut transport_config = quinn::TransportConfig::default();

        // Set initial_rtt if configured
        if let Some(initial_rtt_ms) = initial_rtt {
            transport_config.initial_rtt(std::time::Duration::from_millis(initial_rtt_ms));
        }

        // Set keep_alive_interval if configured; otherwise use default
        let keep_alive = keep_alive_interval
            .map(std::time::Duration::from_secs)
            .unwrap_or(consts::KEEP_ALIVE_PERIOD);
        transport_config.keep_alive_interval(Some(keep_alive));

        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        transport_config
            .max_concurrent_uni_streams(VarInt::from_u32(consts::MAX_OPEN_INCOMING_STREAMS as u32));
        // Set an explicit idle timeout for defense-in-depth.
        transport_config.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(consts::MAX_QUIC_IDLE_TIMEOUT)
                .map_err(|e| anyhow::anyhow!("invalid idle timeout: {:?}", e))?,
        ));
        transport_config
            .stream_receive_window(VarInt::from_u32(consts::QUIC_STREAM_RECEIVE_WINDOW));
        transport_config.receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW));
        transport_config.send_window(consts::QUIC_SEND_WINDOW);

        // Dynamically adjust window size based on initial_rtt
        if let Some(rtt_ms) = initial_rtt {
            if rtt_ms < 50 {
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW / 2,
                ));
                transport_config
                    .receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW / 2));
            } else if rtt_ms > 200 {
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW * 2,
                ));
                transport_config
                    .receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW * 2));
            }
        }

        match congestion_control.to_lowercase().as_str() {
            "cubic" => transport_config
                .congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default())),
            "newreno" | "new_reno" => transport_config.congestion_controller_factory(Arc::new(
                quinn::congestion::NewRenoConfig::default(),
            )),
            _ => {
                let mut bbr_config = quinn::congestion::BbrConfig::default();
                bbr_config.initial_window(10 * consts::ETHERNET_MTU as u64);
                transport_config.congestion_controller_factory(Arc::new(bbr_config))
            }
        };
        quic_config.transport_config(Arc::new(transport_config));

        Ok(quic_config)
    }

    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let uuid = Uuid::parse_str(&config.uuid)?;
        let server_addr: SocketAddr = config.server.parse()?;
        let sni = if config.sni.is_empty() {
            server_addr.ip().to_string()
        } else {
            config.sni.clone()
        };

        let pinned_hash = if config.pinned_certchain_sha256.is_empty() {
            Vec::new()
        } else {
            use base64::Engine;
            let engine_url = base64::engine::general_purpose::URL_SAFE;
            if let Ok(hash) = engine_url.decode(&config.pinned_certchain_sha256) {
                hash
            } else {
                let engine_std = base64::engine::general_purpose::STANDARD;
                if let Ok(hash) = engine_std.decode(&config.pinned_certchain_sha256) {
                    hash
                } else {
                    hex::decode(&config.pinned_certchain_sha256)?
                }
            }
        };

        let bind_addr: SocketAddr = "[::]:0".parse()?;

        let endpoint = if let Some(fwmark) = config.fwmark {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use socket2::{Domain, Protocol, Socket, Type};

                let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
                sock.set_only_v6(false)?;

                #[cfg(target_os = "linux")]
                sock.set_mark(fwmark)?;

                #[cfg(not(target_os = "linux"))]
                println!(
                    "Warning: fwmark is only supported on Linux, ignoring fwmark={}",
                    fwmark
                );

                sock.bind(&bind_addr.into())?;
                let std_socket: std::net::UdpSocket = sock.into();

                let runtime = quinn::default_runtime()
                    .ok_or_else(|| anyhow::anyhow!("No quinn runtime available"))?;
                let wrapped = runtime.wrap_udp_socket(std_socket)?;

                let endpoint = Endpoint::new_with_abstract_socket(
                    EndpointConfig::default(),
                    None,
                    wrapped,
                    runtime,
                )?;
                Ok(endpoint)
            })
            .await??
        } else {
            tokio::task::spawn_blocking(move || Endpoint::client(bind_addr)).await??
        };
        let endpoint = Arc::new(endpoint);

        // Determine pool size.  Default to min(available_parallelism, hardware_concurrency).
        // A pool of 2-4 connections provides meaningful multi-stream concurrency without
        // excessive resource usage.
        let pool_size = std::thread::available_parallelism()
            .map(|n| n.get().min(4).max(2))
            .unwrap_or(2);

        // Build and cache the QUIC client config once.
        let allow_insecure = config.allow_insecure;
        let pinned_hash_for_config = pinned_hash.clone();
        let cc = config.congestion_control.clone();
        let initial_rtt = config.initial_rtt;
        let keep_alive_interval = config.keep_alive_interval;
        let enable_0rtt = config.enable_0rtt.unwrap_or(true);
        let quic_config = tokio::task::spawn_blocking(move || {
            let provider = rustls::crypto::aws_lc_rs::default_provider();
            Self::build_quic_config(
                allow_insecure,
                &pinned_hash_for_config,
                &provider,
                &cc,
                initial_rtt,
                keep_alive_interval,
                enable_0rtt,
            )
        })
        .await??;
        let quic_config = Arc::new(quic_config);

        // Pre-allocate the pool with empty slots.
        let pool: Vec<ConnectionSlot> = (0..pool_size)
            .map(|_| ConnectionSlot { conn: None })
            .collect();

        Ok(Self {
            endpoint,
            server_addr,
            uuid,
            password: zeroize::Zeroizing::new(config.password.clone()),
            sni,
            quic_config,
            pool: Arc::new(tokio::sync::RwLock::new(pool)),
            pool_size,
            next_slot: Arc::new(AtomicUsize::new(0)),
            auth_uni_stream: Arc::new(tokio::sync::Mutex::new(None)),
            reconnect_lock: Arc::new(tokio::sync::Mutex::new(())),
            reconnect_notify: Arc::new(tokio::sync::Notify::new()),
            last_reconnect_failure: Arc::new(tokio::sync::Mutex::new(None)),
            reconnect_attempts: Arc::new(tokio::sync::Mutex::new(0)),
        })
    }

    /// Pick a live QUIC connection from the pool using round-robin selection.
    ///
    /// If no live connection is found, one task is elected to perform a
    /// reconnection while others wait efficiently on a [`Notify`].
    ///
    /// Returns a live, authenticated [`quinn::Connection`] ready for opening streams.
    pub async fn connect(&self) -> anyhow::Result<Connection> {
        loop {
            // ── Fast path: try each pool slot in round-robin order ──
            {
                let guard = self.pool.read().await;
                let start = self.next_slot.fetch_add(1, Ordering::Relaxed) % self.pool_size;
                for offset in 0..self.pool_size {
                    let idx = (start + offset) % self.pool_size;
                    if let Some(ref conn) = guard[idx].conn {
                        if conn.close_reason().is_none() {
                            return Ok(conn.clone());
                        }
                    }
                }
            }

            // ── Try to become the designated reconnector ──
            let reconnect_guard = match self.reconnect_lock.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    self.reconnect_notify.notified().await;
                    continue;
                }
            };

            // ── Apply exponential backoff ──
            {
                let last_failure = self.last_reconnect_failure.lock().await;
                let attempts = self.reconnect_attempts.lock().await;
                if let Some(last) = *last_failure {
                    let base_delay = std::time::Duration::from_secs(1);
                    let delay = base_delay * 2u32.pow(*attempts);
                    let max_delay = std::time::Duration::from_secs(30);
                    let delay = delay.min(max_delay);
                    let elapsed = last.elapsed();
                    if elapsed < delay {
                        tokio::time::sleep(delay - elapsed).await;
                    }
                }
            }

            // ── Double-check: another task may have reconnected while we waited ──
            {
                let guard = self.pool.read().await;
                for slot in guard.iter() {
                    if let Some(ref conn) = slot.conn {
                        if conn.close_reason().is_none() {
                            drop(reconnect_guard);
                            self.reconnect_notify.notify_waiters();
                            return Ok(conn.clone());
                        }
                    }
                }
            }

            // ── Find the first dead/empty slot and reconnect it ──
            let slot_idx = {
                let guard = self.pool.read().await;
                guard
                    .iter()
                    .position(|s| s.conn.as_ref().map_or(true, |c| c.close_reason().is_some()))
                    .unwrap_or(0) // fallback to slot 0 if all live (unlikely but safe)
            };

            // Clear the slot's stale state.
            {
                let mut guard = self.pool.write().await;
                guard[slot_idx].conn = None;
            }
            {
                let mut auth_guard = self.auth_uni_stream.lock().await;
                *auth_guard = None;
            }

            tracing::info!(
                "Connecting to Juicity server at {} (slot {}/{})",
                self.server_addr,
                slot_idx + 1,
                self.pool_size
            );

            // ── Perform QUIC connection + authentication ──
            let connect_result = (async {
                let addr = SocketAddr::new(self.server_addr.ip(), self.server_addr.port());
                let quinn_conn = self
                    .endpoint
                    .connect_with((*self.quic_config).clone(), addr, &self.sni)?
                    .await?;

                let mut uni = quinn_conn.open_uni().await?;

                let conn_for_token = quinn_conn.clone();
                let uuid_for_token = self.uuid;
                let password_for_token = (*self.password).clone();
                let token = tokio::task::spawn_blocking(move || {
                    protocol::gen_token_via_connection(
                        &conn_for_token,
                        &uuid_for_token,
                        &password_for_token,
                    )
                })
                .await??;

                let mut auth_buf = [0u8; 50];
                auth_buf[0] = protocol::PROTOCOL_VERSION;
                auth_buf[1] = protocol::AUTHENTICATE_TYPE;
                auth_buf[2..18].copy_from_slice(self.uuid.as_bytes());
                auth_buf[18..50].copy_from_slice(&token);
                uni.write_all(&auth_buf).await?;

                anyhow::Ok((quinn_conn, uni))
            })
            .await;

            let (quinn_conn, uni) = match connect_result {
                Ok(pair) => pair,
                Err(e) => {
                    *self.last_reconnect_failure.lock().await = Some(std::time::Instant::now());
                    *self.reconnect_attempts.lock().await += 1;
                    drop(reconnect_guard);
                    self.reconnect_notify.notify_waiters();
                    return Err(e);
                }
            };

            tracing::info!(
                "Authenticated as user {} (slot {}/{})",
                self.uuid,
                slot_idx + 1,
                self.pool_size
            );

            // Store the new connection and auth stream.
            {
                let mut guard = self.pool.write().await;
                guard[slot_idx].conn = Some(quinn_conn.clone());
            }
            {
                let mut auth_guard = self.auth_uni_stream.lock().await;
                *auth_guard = Some(uni);
            }

            *self.reconnect_attempts.lock().await = 0;

            drop(reconnect_guard);
            self.reconnect_notify.notify_waiters();

            return Ok(quinn_conn);
        }
    }

    /// Send one underlay authentication message on the persistent auth uni stream.
    pub async fn send_underlay_auth(&self, auth: &protocol::UnderlayAuth) -> anyhow::Result<()> {
        self.connect().await?;

        let mut auth_guard = self.auth_uni_stream.lock().await;
        let stream = auth_guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("auth uni stream not available"))?;

        if let Err(e) = protocol::write_underlay_auth_async(stream, auth).await {
            *auth_guard = None;
            return Err(e);
        }
        Ok(())
    }

    /// Open a TCP stream: sends proxy_header(TCP) once
    pub async fn open_tcp_stream(
        &self,
        addr: &str,
        port: u16,
    ) -> anyhow::Result<(SendStream, RecvStream)> {
        let conn = self.connect().await?;
        let (mut send, recv) = conn.open_bi().await?;

        // Build and send proxy header: [network=TCP(1)][addr_type][addr][port]
        let header = protocol::build_proxy_header(protocol::NETWORK_TCP, addr, port)?;
        send.write_all(&header).await?;

        Ok((send, recv))
    }

    /// Open a UDP stream with first datagram.
    ///
    /// Wire format (upstream-compatible):
    ///   stream header:   [network=3][trojanc_addr]
    ///   first datagram:  [trojanc_addr][len(2)][payload]
    pub async fn open_udp_stream(
        &self,
        addr: &str,
        port: u16,
        first_packet: &[u8],
    ) -> anyhow::Result<(SendStream, RecvStream)> {
        let conn = self.connect().await?;
        let (mut send, recv) = conn.open_bi().await?;

        // Batch stream header + first datagram into a single write:
        //   stream header:  [network=3][trojanc_addr]
        //   first datagram: [trojanc_addr][len(2)][payload]
        let stream_header = protocol::build_proxy_header(protocol::NETWORK_UDP, addr, port)?;
        let dgram_addr = protocol::build_trojanc_addr(addr, port)?;
        let pkt_len = (first_packet.len() as u16).to_be_bytes();
        let mut buf =
            Vec::with_capacity(stream_header.len() + dgram_addr.len() + 2 + first_packet.len());
        buf.extend_from_slice(&stream_header);
        buf.extend_from_slice(&dgram_addr);
        buf.extend_from_slice(&pkt_len);
        buf.extend_from_slice(first_packet);
        send.write_all(&buf).await?;

        Ok((send, recv))
    }

    /// Send a subsequent UDP datagram on an existing stream.
    ///
    /// Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
    /// No leading network byte — each datagram carries only its own address.
    ///
    /// The `addr_buf` is a reusable scratch buffer to avoid per-packet heap
    /// allocation. It is cleared before each use.
    pub async fn send_udp_datagram(
        send: &mut SendStream,
        addr: &str,
        port: u16,
        data: &[u8],
        addr_buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let cached = protocol::CachedAddr::from_host_port(addr, port);
        addr_buf.clear();
        protocol::build_trojanc_addr_cached(addr_buf, &cached)?;
        send.write_all(addr_buf).await?;
        let len = (data.len() as u16).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(data).await?;
        Ok(())
    }
}

// ── TLS certificate verifiers ──

#[derive(Debug)]
struct SkipVerify {
    provider: rustls::crypto::CryptoProvider,
}
impl SkipVerify {
    fn new(provider: rustls::crypto::CryptoProvider) -> Self {
        Self { provider }
    }
}
impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinVerify {
    provider: rustls::crypto::CryptoProvider,
    pinned_hash: Vec<u8>,
}
impl PinVerify {
    fn new(provider: rustls::crypto::CryptoProvider, pinned_hash: Vec<u8>) -> Self {
        Self {
            provider,
            pinned_hash,
        }
    }
}
impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let mut raw_certs = vec![end_entity.as_ref()];
        for cert in intermediates {
            raw_certs.push(cert.as_ref());
        }
        let computed_hash = juicity_common::crypto::generate_cert_chain_hash(&raw_certs);
        if computed_hash == self.pinned_hash {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "pinned cert chain hash mismatch".to_string(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
