use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Main configuration structure matching juicity's JSON config format
///
/// # Field grouping
/// - **Client fields**: server, uuid, password, sni, allow_insecure,
///   pinned_certchain_sha256, protect_path, forward
/// - **Server fields**: users, certificate, private_key, fwmark,
///   send_through, dialer_link, disable_outbound_udp443
/// - **Common fields**: listen, congestion_control, log_level
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // ── Client fields ──
    pub server: String,
    pub uuid: String,
    pub password: String,
    pub sni: String,
    pub allow_insecure: bool,
    pub pinned_certchain_sha256: String,
    /// Path to the protect_path socket (compatible with Go version)
    pub protect_path: String,
    pub forward: HashMap<String, String>,

    // ── Server fields ──
    pub users: HashMap<String, String>,
    pub certificate: String,
    pub private_key: String,
    pub fwmark: String,
    pub send_through: String,
    pub dialer_link: String,
    pub disable_outbound_udp443: bool,

    // ── Common fields ──
    pub listen: String,
    pub congestion_control: String,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: String::new(),
            uuid: String::new(),
            password: String::new(),
            sni: String::new(),
            allow_insecure: false,
            pinned_certchain_sha256: String::new(),
            protect_path: String::new(),
            forward: HashMap::new(),
            users: HashMap::new(),
            certificate: String::new(),
            private_key: String::new(),
            fwmark: String::new(),
            send_through: String::new(),
            dialer_link: String::new(),
            disable_outbound_udp443: false,
            listen: String::new(),
            congestion_control: "bbr".to_string(),
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    /// Read config from a JSON file
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Validate config for client run
    pub fn validate_for_client(&self) -> anyhow::Result<()> {
        if self.server.is_empty() {
            anyhow::bail!("'server' is required");
        }
        if !self.server.contains(':') {
            anyhow::bail!("'server' must be in host:port format");
        }
        if self.uuid.is_empty() {
            anyhow::bail!("'uuid' is required");
        }
        // Validate UUID format
        uuid::Uuid::parse_str(&self.uuid)
            .map_err(|e| anyhow::anyhow!("invalid uuid '{}': {}", self.uuid, e))?;
        if self.password.is_empty() {
            anyhow::bail!("'password' is required");
        }
        if self.password.len() < 8 {
            anyhow::bail!("'password' must be at least 8 characters long");
        }
        if self.listen.is_empty() && self.forward.is_empty() {
            anyhow::bail!("'listen' or 'forward' is required");
        }
        if !self.listen.is_empty() && !self.listen.contains(':') {
            anyhow::bail!("'listen' must be in host:port format");
        }
        Ok(())
    }

    /// Validate config for server run
    pub fn validate_for_server(&self) -> anyhow::Result<()> {
        if self.listen.is_empty() {
            anyhow::bail!("'listen' is required");
        }
        if !self.listen.contains(':') {
            anyhow::bail!("'listen' must be in host:port format");
        }
        if self.users.is_empty() {
            anyhow::bail!("'users' is required");
        }
        for (id, pw) in &self.users {
            uuid::Uuid::parse_str(id)
                .map_err(|e| anyhow::anyhow!("invalid user uuid '{}': {}", id, e))?;
            if pw.is_empty() {
                anyhow::bail!("password for user '{}' is required", id);
            }
        }
        if self.certificate.is_empty() {
            anyhow::bail!("'certificate' is required");
        }
        if !std::path::Path::new(&self.certificate).exists() {
            anyhow::bail!("certificate file '{}' not found", self.certificate);
        }
        if self.private_key.is_empty() {
            anyhow::bail!("'private_key' is required");
        }
        if !std::path::Path::new(&self.private_key).exists() {
            anyhow::bail!("private key file '{}' not found", self.private_key);
        }
        Ok(())
    }
}
