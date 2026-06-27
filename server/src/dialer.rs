use juicity_common::consts;
use tokio::net::{TcpStream, UdpSocket};

/// A trait for dialing outbound TCP/UDP connections
#[async_trait::async_trait]
pub trait Dialer: Send + Sync {
    async fn dial_tcp(&self, addr: &str) -> anyhow::Result<TcpStream>;
    async fn dial_udp(&self, addr: &str) -> anyhow::Result<UdpSocket>;
}

/// Default dialer that connects directly
pub struct DefaultDialer;

#[async_trait::async_trait]
impl Dialer for DefaultDialer {
    async fn dial_tcp(&self, addr: &str) -> anyhow::Result<TcpStream> {
        let stream =
            tokio::time::timeout(consts::DEFAULT_DIAL_TIMEOUT, TcpStream::connect(addr)).await??;
        stream.set_nodelay(true)?;
        // Enable TCP keep-alive probes.
        let sock_ref = socket2::SockRef::from(&stream);
        sock_ref.set_keepalive(true)?;
        // Platform-specific keep-alive parameters:
        // - Linux: idle 60s, interval 10s, 3 probes (total ~90s to detect dead peer)
        // - Other Unix: set via raw setsockopt or skip (OS defaults are reasonable)
        // - Windows: uses SIO_KEEPALIVE_VALS via socket2
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let ka = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(60))
                .with_interval(std::time::Duration::from_secs(10));
            sock_ref.set_tcp_keepalive(&ka)?;
        }
        Ok(stream)
    }

    async fn dial_udp(&self, _addr: &str) -> anyhow::Result<UdpSocket> {
        // The addr parameter is the target address we'll send packets to.
        // We bind to a local ephemeral port without connecting, so the caller
        // can use send_to() to send packets to different targets if needed.
        // Pre-connecting via socket.connect(addr) would restrict us to a single target.
        // Use "[::]:0" (IPv6 any) for dual-stack binding.
        // On Linux, binding to "[::]" by default has IPV6_V6ONLY=false,
        // accepting both IPv4 and IPv6 connections.
        let socket = UdpSocket::bind("[::]:0").await?;
        Ok(socket)
    }
}

/// Dialer that binds to a specific IP address
pub struct BindDialer {
    pub bind_addr: std::net::IpAddr,
}

#[async_trait::async_trait]
impl Dialer for BindDialer {
    async fn dial_tcp(&self, addr: &str) -> anyhow::Result<TcpStream> {
        // Use tokio's TcpSocket for binding
        // Select IPv4 or IPv6 based on bind_addr address family
        let socket = if self.bind_addr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        socket.bind(std::net::SocketAddr::new(self.bind_addr, 0))?;
        let addr: std::net::SocketAddr = addr.parse()?;
        let stream =
            tokio::time::timeout(consts::DEFAULT_DIAL_TIMEOUT, socket.connect(addr)).await??;
        stream.set_nodelay(true)?;
        // Enable TCP keep-alive probes.
        let sock_ref = socket2::SockRef::from(&stream);
        sock_ref.set_keepalive(true)?;
        // Platform-specific keep-alive parameters:
        // - Linux: idle 60s, interval 10s, 3 probes (total ~90s to detect dead peer)
        // - Other Unix: set via raw setsockopt or skip (OS defaults are reasonable)
        // - Windows: uses SIO_KEEPALIVE_VALS via socket2
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let ka = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(60))
                .with_interval(std::time::Duration::from_secs(10));
            sock_ref.set_tcp_keepalive(&ka)?;
        }
        Ok(stream)
    }

    async fn dial_udp(&self, _addr: &str) -> anyhow::Result<UdpSocket> {
        // The addr parameter is the target address we'll send packets to.
        // We bind to a local ephemeral port without connecting, so the caller
        // can use send_to() to send packets to different targets if needed.
        let bind_addr = std::net::SocketAddr::new(self.bind_addr, 0);
        let socket = UdpSocket::bind(bind_addr).await?;
        Ok(socket)
    }
}
