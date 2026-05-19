use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use juicity_common::protocol::UnderlayAuth;
use tokio::sync::Notify;

/// In-flight key type (32 bytes salt)
pub type InFlightKey = [u8; 32];

/// Manages underlay authentication keys that are in-flight (waiting for their corresponding UDP packets)
///
/// # Sync blocking safety
///
/// This struct uses `std::sync::Mutex` (not `tokio::sync::Mutex`) intentionally:
/// - All critical sections are extremely short (HashMap insert/remove, ~ns level)
/// - No `.await` points are held while the lock is acquired
/// - Using `tokio::sync::Mutex` would add unnecessary overhead for these micro-operations
/// - The lock is never held across an await point, so it cannot cause deadlock
///
/// The `evict()` method does hold the lock across `.await` boundaries in the `tokio::select!`
/// loop, but each individual lock acquisition is released before the `.await` (the lock guard
/// is dropped at the end of each scope), so this is safe.
pub struct InFlightUnderlayKey {
    ttl: Duration,
    inner: Mutex<InFlightInner>,
    notify: Notify,
}

/// Single-map entry combining auth data and insertion timestamp for cache locality.
struct InFlightEntry {
    auth: UnderlayAuth,
    inserted_at: Instant,
}

struct InFlightInner {
    entries: HashMap<InFlightKey, InFlightEntry>,
}

impl InFlightUnderlayKey {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(InFlightInner {
                entries: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    /// Store an authentication for later retrieval
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.insert(key, InFlightEntry { auth, inserted_at: Instant::now() });
        // Notify any waiting evict() call that a new key is available
        self.notify.notify_waiters();
    }

    /// Evict and retrieve an authentication using Notify for zero-latency wakeup.
    /// Uses a loop with notified() to avoid the 100ms sleep penalty.
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // First attempt without waiting
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner.entries.remove(key) {
                return Some(entry.auth);
            }
        }

        // If not found yet, wait for notification with a short timeout
        // to handle the case where the key never arrives.
        // We use a loop to re-check after notification, since notify_waiters()
        // wakes ALL waiters and our key might not be the one that arrived.
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let wait = self.notify.notified();
            tokio::select! {
                _ = wait => {
                    // Woken up - check if our key arrived
                    let mut guard = self.inner.lock().unwrap();
                    if let Some(entry) = guard.entries.remove(key) {
                        return Some(entry.auth);
                    }
                    // Not our key, loop back to wait again (if within deadline)
                    if Instant::now() >= deadline {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    // Timeout - try one last time
                    let mut guard = self.inner.lock().unwrap();
                    return guard.entries.remove(key).map(|e| e.auth);
                }
            }
        }
    }

    /// Clean up expired keys.
    pub fn cleanup(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let ttl = self.ttl;
        inner.entries.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
    }
}
