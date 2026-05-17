use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use juicity_common::protocol::UnderlayAuth;
use tokio::sync::Notify;

/// In-flight key type (32 bytes salt)
pub type InFlightKey = [u8; 32];

/// Manages underlay authentication keys that are in-flight (waiting for their corresponding UDP packets)
pub struct InFlightUnderlayKey {
    ttl: Duration,
    inner: Mutex<InFlightInner>,
    notify: Notify,
}

struct InFlightInner {
    keys: HashMap<InFlightKey, UnderlayAuth>,
    timestamps: HashMap<InFlightKey, Instant>,
}

impl InFlightUnderlayKey {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(InFlightInner {
                keys: HashMap::new(),
                timestamps: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    /// Store an authentication for later retrieval
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let mut inner = self.inner.lock().unwrap();
        inner.keys.insert(key, auth);
        inner.timestamps.insert(key, Instant::now());
        // Notify any waiting evict() call that a new key is available
        self.notify.notify_waiters();
    }

    /// Evict and retrieve an authentication (async version using Notify)
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // First attempt without waiting
        {
            let mut inner = self.inner.lock().unwrap();
            let auth = inner.keys.remove(key);
            inner.timestamps.remove(key);
            if auth.is_some() {
                return auth;
            }
        }

        // If not found yet, wait for notification (with timeout)
        // (In the Go implementation, it waits on a condition variable)
        tokio::select! {
            _ = self.notify.notified() => {
                // Woken up - check if our key arrived
                let mut guard = self.inner.lock().unwrap();
                let auth = guard.keys.remove(key);
                guard.timestamps.remove(key);
                auth
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // Timeout - try one last time
                let mut guard = self.inner.lock().unwrap();
                let auth = guard.keys.remove(key);
                guard.timestamps.remove(key);
                auth
            }
        }
    }

    /// Clean up expired keys
    pub fn cleanup(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let expired: Vec<InFlightKey> = inner
            .timestamps
            .iter()
            .filter(|(_, ts)| now.duration_since(**ts) > self.ttl)
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            inner.keys.remove(&key);
            inner.timestamps.remove(&key);
        }
    }
}
