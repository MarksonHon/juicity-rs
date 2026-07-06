use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use juicity_common::consts;
use juicity_common::protocol::UnderlayAuth;
use tokio::sync::Notify;

/// In-flight key type (32 bytes salt)
pub type InFlightKey = [u8; 32];

/// Manages underlay authentication keys that are in-flight (waiting for their
/// corresponding UDP packets).
///
/// Each key has its own [`Notify`] so that [`store`](Self::store) only wakes
/// tasks waiting for the exact key that was inserted — avoiding the thundering
/// herd problem of a single shared Notify.
///
/// # Lock design
///
/// This implementation uses [`std::sync::Mutex<HashMap>`] internally. The lock
/// is only held briefly for lookups, inserts, and removes — no `.await` point
/// is ever held under the lock. The per-key [`Notify`] mechanism handles the
/// actual blocking, so concurrent accesses to different keys do not contend.
pub struct InFlightUnderlayKey {
    ttl: Duration,
    evict_timeout: Duration,
    map: Mutex<HashMap<InFlightKey, InFlightEntry>>,
}

/// Single-map entry combining auth data, insertion timestamp and a per-key
/// [`Notify`] for cache locality.
struct InFlightEntry {
    auth: Option<UnderlayAuth>,
    inserted_at: Instant,
    notify: Arc<Notify>,
}

impl InFlightUnderlayKey {
    /// Create a new `InFlightUnderlayKey` with the given TTL and evict timeout.
    pub fn new(ttl: Duration, evict_timeout: Duration) -> Self {
        Self {
            ttl,
            evict_timeout,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Store an authentication for later retrieval.
    ///
    /// If a task is already waiting for this key (via `evict`), its per-key
    /// [`Notify`] is fired so it can wake up and consume the value immediately.
    ///
    /// If the number of in-flight entries already equals
    /// `MAX_IN_FLIGHT_UNDERLAY_ENTRIES`, expired entries are evicted first.
    /// If the map is still full after eviction, the new entry is silently
    /// dropped to prevent unbounded memory growth during a burst of forged or
    /// unanswered underlay auth packets.
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let mut map = match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Fast path: key already exists — atomically update and notify.
        if let Some(entry) = map.get_mut(&key) {
            entry.auth = Some(auth);
            entry.notify.notify_waiters();
            return;
        }

        // New entry: enforce capacity limit (approximate check).
        if map.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
            let now = Instant::now();
            let ttl = self.ttl;
            map.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
            if map.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
                tracing::warn!(
                    "in-flight underlay auth table is full ({} entries); dropping new entry",
                    map.len()
                );
                return;
            }
        }

        // Insert the new entry.
        use std::collections::hash_map::Entry;
        match map.entry(key) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().auth = Some(auth);
                entry.get().notify.notify_waiters();
            }
            Entry::Vacant(entry) => {
                entry.insert(InFlightEntry {
                    notify: Arc::new(Notify::new()),
                    auth: Some(auth),
                    inserted_at: Instant::now(),
                });
            }
        }
    }

    /// Evict and retrieve an authentication using a per-key [`Notify`] for
    /// zero-latency wakeup.
    ///
    /// If the key is already present, it is removed and returned immediately.
    /// Otherwise a placeholder entry with a dedicated [`Notify`] is inserted,
    /// and the caller waits for that Notify.  When [`store`](Self::store)
    /// eventually fills in the value, only the task(s) waiting for this
    /// *exact* key are woken — eliminating the thundering herd.
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // Obtain (or create) the per-key Notify while holding the lock,
        // then drop the lock before any `.await` to avoid deadlock.
        let notify = {
            let mut map = match self.map.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            match map.get_mut(key) {
                Some(entry) => {
                    if let Some(auth) = entry.auth.take() {
                        // Value already present — consume immediately.
                        // The mutable reference is implicitly dropped before remove.
                        map.remove(key);
                        return Some(auth);
                    }
                    // Entry exists but value not yet stored — clone its Notify
                    // and wait (the mutex lock is released when the guard drops).
                    entry.notify.clone()
                }
                None => {
                    // No entry yet — create a placeholder and wait.
                    let notify = Arc::new(Notify::new());
                    map.insert(
                        *key,
                        InFlightEntry {
                            notify: notify.clone(),
                            auth: None,
                            inserted_at: Instant::now(),
                        },
                    );
                    notify
                }
            }
        };

        // Wait for notification with a short timeout to handle keys that are
        // never stored (e.g. a forged salt that no corresponding UDP packet
        // will complete).
        let deadline = Instant::now() + self.evict_timeout;
        loop {
            tokio::select! {
                _ = notify.notified() => {
                    let mut map = match self.map.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if let Some(entry) = map.get_mut(key) {
                        if let Some(auth) = entry.auth.take() {
                            map.remove(key);
                            return Some(auth);
                        }
                    }
                    if Instant::now() >= deadline {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    let mut map = match self.map.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if let Some(entry) = map.remove(key) {
                        return entry.auth;
                    }
                    return None;
                }
            }
        }
    }

    /// Clean up expired in-flight underlay auth entries.
    ///
    /// Iterates all entries of the internal map and removes entries whose TTL
    /// (time-to-live) has elapsed since insertion.  This is intended to be run
    /// as a **background cleanup task**.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let ttl = self.ttl;
        if let Ok(mut map) = self.map.lock() {
            map.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
        }
    }
}
