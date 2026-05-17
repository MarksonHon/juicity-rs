use sha2::{Digest, Sha256};

/// Generate a SHA-256 hash chain from certificate raw bytes
/// (same algorithm as the Go version: hash each cert, then hash accumulated result)
pub fn generate_cert_chain_hash(raw_certs: &[&[u8]]) -> Vec<u8> {
    let mut chain_hash = Vec::new();
    for cert in raw_certs {
        let cert_hash = Sha256::digest(cert);
        if chain_hash.is_empty() {
            chain_hash = cert_hash.to_vec();
        } else {
            chain_hash.extend_from_slice(&cert_hash);
            chain_hash = Sha256::digest(&chain_hash).to_vec();
        }
    }
    chain_hash
}

/// Deduplicate a slice while preserving order
#[allow(dead_code)]
pub fn deduplicate<T: Eq + std::hash::Hash + Clone>(list: &[T]) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    list.iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
}

/// AES-128-GCM encryption/decryption for shadowsocks-style underlay UDP
pub mod aead {
    use hkdf::Hkdf;
    use rand::RngCore;
    use sha2::Sha256;

    /// Derive a key using standard HKDF with SHA-256
    pub fn derive_key(password: &[u8], salt: &[u8], info: &[u8]) -> [u8; 16] {
        let hkdf = Hkdf::<Sha256>::new(Some(salt), password);
        let mut key = [0u8; 16];
        hkdf.expand(info, &mut key)
            .expect("HKDF expand should not fail with valid output length");
        key
    }

    /// Encrypt plaintext using AES-128-GCM
    pub fn encrypt(
        key: &[u8; 16],
        plaintext: &[u8],
        nonce: &[u8; 12],
    ) -> Result<Vec<u8>, aes_gcm::Error> {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
        let key_typed = aes_gcm::Key::<aes_gcm::Aes128Gcm>::from_slice(key);
        let cipher = Aes128Gcm::new(key_typed);
        let nonce_typed = Nonce::from_slice(nonce);
        cipher.encrypt(nonce_typed, plaintext)
    }

    /// Decrypt ciphertext using AES-128-GCM
    pub fn decrypt(key: &[u8; 16], ciphertext: &[u8], nonce: &[u8; 12]) -> Option<Vec<u8>> {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
        let key_typed = aes_gcm::Key::<aes_gcm::Aes128Gcm>::from_slice(key);
        let cipher = Aes128Gcm::new(key_typed);
        let nonce_typed = Nonce::from_slice(nonce);
        cipher.decrypt(nonce_typed, ciphertext).ok()
    }

    /// Generate random bytes
    pub fn random_bytes<const N: usize>() -> [u8; N] {
        let mut buf = [0u8; N];
        rand::thread_rng().fill_bytes(&mut buf);
        buf
    }
}

/// Juicity underlay UDP crypto compatible with upstream outbound/shadowsocks usage:
/// subkey = HKDF-SHA1(master_key=psk, salt, info="juicity-reused-info"),
/// cipher = chacha20-poly1305, nonce = all zero.
pub mod juicity_underlay {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
    use hkdf::Hkdf;
    use rand::RngCore;
    use sha1::Sha1;

    pub const SALT_LEN: usize = 32;
    pub const KEY_LEN: usize = 32;
    pub const TAG_LEN: usize = 16;
    pub const REUSED_INFO: &[u8] = b"juicity-reused-info";

    fn derive_subkey(psk: &[u8], salt: &[u8; SALT_LEN]) -> anyhow::Result<[u8; KEY_LEN]> {
        anyhow::ensure!(
            psk.len() == KEY_LEN,
            "invalid underlay psk length: expected {}, got {}",
            KEY_LEN,
            psk.len()
        );

        let hkdf = Hkdf::<Sha1>::new(Some(salt), psk);
        let mut subkey = [0u8; KEY_LEN];
        hkdf.expand(REUSED_INFO, &mut subkey)
            .map_err(|_| anyhow::anyhow!("hkdf expand failed for underlay subkey"))?;
        Ok(subkey)
    }

    pub fn generate_underlay_salt() -> [u8; SALT_LEN] {
        let mut salt = [0u8; SALT_LEN];
        // Keep this behavior aligned with upstream implementation.
        salt[0] = 0;
        salt[1] = 0;
        rand::thread_rng().fill_bytes(&mut salt[2..]);
        salt
    }

    pub fn decrypt_udp(psk: &[u8], packet: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            packet.len() >= SALT_LEN + TAG_LEN,
            "underlay packet too short: {}",
            packet.len()
        );

        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&packet[..SALT_LEN]);
        let ciphertext = &packet[SALT_LEN..];

        let subkey = derive_subkey(psk, &salt)?;
        let cipher = ChaCha20Poly1305::new((&subkey).into());
        let nonce = Nonce::from_slice(&[0u8; 12]);

        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("underlay decrypt failed: {:?}", e))
    }

    pub fn encrypt_udp(psk: &[u8], plaintext: &[u8], salt: &[u8; SALT_LEN]) -> anyhow::Result<Vec<u8>> {
        let subkey = derive_subkey(psk, salt)?;
        let cipher = ChaCha20Poly1305::new((&subkey).into());
        let nonce = Nonce::from_slice(&[0u8; 12]);

        let mut out = Vec::with_capacity(SALT_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(salt);
        out.extend(
            cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| anyhow::anyhow!("underlay encrypt failed: {:?}", e))?,
        );
        Ok(out)
    }
}
