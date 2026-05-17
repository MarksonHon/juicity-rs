use std::time::Duration;

/// Default dial timeout
pub const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Default NAT timeout for UDP (3 minutes, compatible with Go version)
pub const DEFAULT_NAT_TIMEOUT: Duration = Duration::from_secs(180);
/// DNS query timeout (17 seconds, RFC 5452)
pub const DNS_QUERY_TIMEOUT: Duration = Duration::from_secs(17);
/// Ethernet MTU
pub const ETHERNET_MTU: usize = 1500;
/// Authentication timeout
pub const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(10);
/// In-flight underlay TTL
pub const IN_FLIGHT_UNDERLAY_TTL: Duration = Duration::from_secs(10);
/// Default congestion control window
pub const DEFAULT_CWND: u64 = 10;
/// Max open incoming streams
pub const MAX_OPEN_INCOMING_STREAMS: u64 = 100;
/// Keep-alive period for QUIC
pub const KEEP_ALIVE_PERIOD: Duration = Duration::from_secs(10);

/// Default NAT timeout in seconds (Go-compatible: 3 minutes)
pub const DEFAULT_NAT_TIMEOUT_SECS: u64 = 180;
/// DNS query timeout in seconds (Go-compatible: 17 seconds, RFC 5452)
pub const DNS_QUERY_TIMEOUT_SECS: u64 = 17;
/// Ethernet MTU (Go-compatible)
pub const ETHERNET_MTU_VAL: usize = 1500;

/// JUICIY protocol version 0
pub const JUICIY_VERSION_0: u8 = 0;
/// Underlay salt length
pub const UNDERLAY_SALT_LEN: usize = 32;
