//! Library interface of the Juicity client.
//!
//! Exposes the proxy core (QUIC client, forwarder and local SOCKS5/HTTP
//! server) so it can be embedded in other binaries, most notably the GUI,
//! instead of being spawned as an external `juicity-client` process.

pub mod client;
pub mod forwarder;
pub mod local;

/// Install the process-wide default rustls [`CryptoProvider`] (aws-lc-rs).
///
/// Safe to call multiple times; only the first call has an effect.  Embedders
/// that link this crate as a library should call it once before creating any
/// QUIC endpoint.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn install_default_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}
