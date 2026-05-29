use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use juicity_common::consts;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

#[derive(Debug)]
pub struct UnderlayPacket {
    pub peer: SocketAddr,
    pub payload: Vec<u8>,
}

/// Maximum number of non-QUIC underlay packets that can be queued before new ones are dropped.
/// This provides back-pressure and prevents unbounded memory growth under high load.
pub const UNDERLAY_CHANNEL_CAPACITY: usize = 1024;

/// Split non-QUIC packets away from Quinn while keeping one shared UDP port.
#[derive(Debug)]
pub struct DemuxUdpSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    underlay_tx: tokio::sync::mpsc::Sender<UnderlayPacket>,
}

impl DemuxUdpSocket {
    pub fn new(
        inner: Arc<dyn AsyncUdpSocket>,
        underlay_tx: tokio::sync::mpsc::Sender<UnderlayPacket>,
    ) -> Self {
        Self { inner, underlay_tx }
    }

    #[inline]
    fn is_probably_quic_long_header(packet: &[u8]) -> bool {
        // Invariants packet:
        // - Header form bit set (0x80)
        // - Fixed bit set (0x40)
        // - 4-byte version present
        if packet.len() < 7 {
            return false;
        }
        let first = packet[0];
        if (first & 0x80) == 0 || (first & 0x40) == 0 {
            return false;
        }

        let version = u32::from_be_bytes([packet[1], packet[2], packet[3], packet[4]]);
        // Accept QUIC v1/v2 and Version Negotiation packet version (0).
        if version != 0 && version != 1 && version != 2 {
            return false;
        }

        let dcid_len = packet[5] as usize;
        if packet.len() < 6 + dcid_len + 1 {
            return false;
        }
        let scid_len_index = 6 + dcid_len;
        let scid_len = packet[scid_len_index] as usize;
        packet.len() >= scid_len_index + 1 + scid_len
    }

    #[inline]
    fn is_probably_quic_short_header(packet: &[u8]) -> bool {
        // Invariants packet:
        // - Header form bit clear (0x80 == 0)
        // - Fixed bit set (0x40)
        // We also require a minimal practical payload length to filter random noise.
        if packet.len() < 9 {
            return false;
        }
        let first = packet[0];
        (first & 0x80) == 0 && (first & 0x40) == 0x40
    }

    #[inline]
    fn is_probably_quic_packet(packet: &[u8]) -> bool {
        if packet.is_empty() {
            return false;
        }
        if (packet[0] & 0x80) != 0 {
            Self::is_probably_quic_long_header(packet)
        } else {
            Self::is_probably_quic_short_header(packet)
        }
    }
}

impl AsyncUdpSocket for DemuxUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        loop {
            let msgs = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(msgs)) => msgs,
            };

            let mut keep = 0usize;
            for i in 0..msgs {
                let first_len = meta[i].stride.min(meta[i].len);
                let first_pkt = &bufs[i][..first_len];
                if Self::is_probably_quic_packet(first_pkt) {
                    if keep != i {
                        bufs.swap(keep, i);
                        meta.swap(keep, i);
                    }
                    keep += 1;
                    continue;
                }

                let mut offset = 0usize;
                let stride = meta[i].stride.max(1);
                while offset < meta[i].len {
                    let end = (offset + stride).min(meta[i].len);
                    if end - offset < consts::UNDERLAY_SALT_LEN {
                        // Drop malformed underlay datagrams early at demux layer.
                        offset = end;
                        continue;
                    }
                    // Use Bytes::copy_from_slice to create a reference-counted
                    // copy of the payload, avoiding per-packet Vec allocation overhead.
                    // Allocate a Vec<u8> directly — avoids the extra to_vec() copy
                    // that would be needed if we used Bytes here.
                    if self.underlay_tx.try_send(UnderlayPacket {
                        peer: meta[i].addr,
                        payload: bufs[i][offset..end].to_vec(),
                    }).is_err() {
                        // Channel full: drop the packet and warn. Under sustained high
                        // underlay load this may cause UDP handshake failures.
                        tracing::warn!(
                            "underlay channel full, dropping non-QUIC packet from {}",
                            meta[i].addr
                        );
                    }
                    offset = end;
                }
            }

            if keep > 0 {
                return Poll::Ready(Ok(keep));
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}
