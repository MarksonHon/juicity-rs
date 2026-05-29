use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use juicity_common::consts;
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
    owner: u64,
}

struct InFlightInner {
    entries: HashMap<InFlightKey, InFlightEntry>,
    order: VecDeque<InFlightKey>,
    owners: HashMap<u64, HashSet<InFlightKey>>,
}

impl InFlightInner {
    fn remove_key(&mut self, key: &InFlightKey) -> Option<InFlightEntry> {
        let entry = self.entries.remove(key)?;
        if let Some(keys) = self.owners.get_mut(&entry.owner) {
            keys.remove(key);
            if keys.is_empty() {
                self.owners.remove(&entry.owner);
            }
        }
        Some(entry)
    }

    fn evict_oldest(&mut self) -> bool {
        while let Some(oldest_key) = self.order.pop_front() {
            if self.remove_key(&oldest_key).is_some() {
                return true;
            }
        }
        false
    }
}

impl InFlightUnderlayKey {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(InFlightInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                owners: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    #[inline]
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        self.store_for_owner(0, key, auth);
    }

    /// Store an authentication for later retrieval.
    ///
    /// If the number of in-flight entries already equals `MAX_IN_FLIGHT_UNDERLAY_ENTRIES`,
    /// expired entries are evicted first. If the map is still full after eviction,
    /// evict the oldest entry and insert the new one. This favors fresher auth data
    /// and promptly releases stale connection metadata.
    pub fn store_for_owner(&self, owner: u64, key: InFlightKey, auth: UnderlayAuth) {
        let mut inner = self.inner.lock().unwrap();
        if inner.entries.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
            // Eagerly evict expired entries before considering whether to drop.
            let now = Instant::now();
            let ttl = self.ttl;
            let expired_keys: Vec<InFlightKey> = inner
                .entries
                .iter()
                .filter_map(|(k, e)| {
                    if now.duration_since(e.inserted_at) > ttl {
                        Some(*k)
                    } else {
                        None
                    }
                })
                .collect();
            for k in expired_keys {
                inner.remove_key(&k);
            }
            if inner.entries.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
                if inner.evict_oldest() {
                    tracing::debug!(
                        "in-flight underlay auth table full ({} entries); evicted oldest entry",
                        inner.entries.len() + 1
                    );
                }
            }
        }

        // Replace semantics: remove old owner index if the key already exists.
        inner.remove_key(&key);
        inner.entries.insert(
            key,
            InFlightEntry {
                auth,
                inserted_at: Instant::now(),
                owner,
            },
        );
        inner.owners.entry(owner).or_default().insert(key);
        inner.order.push_back(key);
        // Notify any waiting evict() call that a new key is available
        self.notify.notify_waiters();
    }

    /// Evict and retrieve an authentication using Notify for zero-latency wakeup.
    /// Uses a loop with notified() to avoid the 100ms sleep penalty.
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // First attempt without waiting
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner.remove_key(key) {
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
                    if let Some(entry) = guard.remove_key(key) {
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
                    return guard.remove_key(key).map(|e| e.auth);
                }
            }
        }
    }

    /// Remove all in-flight auth entries associated with one connection owner.
    pub fn remove_owner(&self, owner: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(keys) = inner.owners.remove(&owner) {
            for key in keys {
                inner.entries.remove(&key);
            }
        }
    }

    /// Clean up expired keys.
    pub fn cleanup(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let ttl = self.ttl;
        let expired_keys: Vec<InFlightKey> = inner
            .entries
            .iter()
            .filter_map(|(k, e)| {
                if now.duration_since(e.inserted_at) > ttl {
                    Some(*k)
                } else {
                    None
                }
            })
            .collect();
        for k in expired_keys {
            inner.remove_key(&k);
        }
    }
}
