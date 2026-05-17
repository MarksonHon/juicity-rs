use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Options for creating a UDP endpoint
pub struct UdpEndpointOptions {
    pub handler: Box<dyn Fn(&[u8], SocketAddr) -> anyhow::Result<()> + Send + Sync>,
    pub nat_timeout: Duration,
    pub dial_target: String,
}

/// A UDP endpoint representing a full-cone NAT session
pub struct UdpEndpoint {
    pub socket: std::net::UdpSocket,
    pub dial_target: String,
    created_at: Instant,
    nat_timeout: Duration,
    handler: Box<dyn Fn(&[u8], SocketAddr) -> anyhow::Result<()> + Send + Sync>,
}

impl UdpEndpoint {
    pub fn new(options: UdpEndpointOptions) -> anyhow::Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;

        Ok(Self {
            socket,
            dial_target: options.dial_target,
            created_at: Instant::now(),
            nat_timeout: options.nat_timeout,
            handler: options.handler,
        })
    }

    pub async fn start(&self) {
        let mut buf = vec![0u8; 1500];
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, addr)) => {
                    if let Err(e) = (self.handler)(&buf[..n], addr) {
                        tracing::debug!("UDP endpoint handler error: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!("UDP endpoint read error: {:?}", e);
                    break;
                }
            }
        }
    }

    pub async fn write_to(&self, data: &[u8], target: &str) -> anyhow::Result<usize> {
        Ok(self.socket.send_to(data, target)?)
    }

    pub fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.created_at) > self.nat_timeout
    }

    pub fn touch(&mut self) {
        self.created_at = Instant::now();
    }
}

/// Pool of UDP endpoints for full-cone NAT
pub struct UdpEndpointPool {
    inner: Mutex<HashMap<SocketAddr, UdpEndpoint>>,
}

impl UdpEndpointPool {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_or_create(
        &self,
        addr: SocketAddr,
        options: UdpEndpointOptions,
    ) -> anyhow::Result<((std::net::UdpSocket, String), bool)> {
        let mut inner = self.inner.lock().await;

        // Check if already exists
        if let Some(endpoint) = inner.get_mut(&addr) {
            if !endpoint.is_expired() {
                endpoint.touch();
                return Ok((
                    (endpoint.socket.try_clone()?, endpoint.dial_target.clone()),
                    false,
                ));
            }
        }

        let endpoint = UdpEndpoint::new(options)?;
        let dial_target = endpoint.dial_target.clone();
        let socket = endpoint.socket.try_clone()?;
        inner.insert(addr, endpoint);

        Ok(((socket, dial_target), true))
    }

    pub async fn remove(&self, addr: &SocketAddr) {
        let mut inner = self.inner.lock().await;
        inner.remove(addr);
    }

    pub fn cleanup(&self) {
        // cleanup is called from a sync context (tokio::spawn with block on lock)
        // We use try_lock to avoid blocking the async runtime
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.retain(|_, endpoint| !endpoint.is_expired());
        }
    }
}
