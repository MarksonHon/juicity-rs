use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use juicity_common::consts;
use juicity_common::protocol::UnderlayAuth;
use tokio::sync::Notify;

/// In-flight key type (32 bytes salt)
pub type InFlightKey = [u8; 32];

/// Matches UDP packets to auth received on the authenticated QUIC stream.
/// Packets can arrive first, so bounded per-key waiters have separate capacity
/// from authenticated entries. No lock is held across an await.
pub struct InFlightUnderlayKey {
    ttl: Duration,
    evict_timeout: Duration,
    map: Mutex<InFlightState>,
}

#[derive(Default)]
struct InFlightState {
    auth: HashMap<InFlightKey, (UnderlayAuth, Instant)>,
    waiting: HashMap<InFlightKey, (Arc<Notify>, Instant)>,
}

impl InFlightUnderlayKey {
    /// Create a new `InFlightUnderlayKey` with the given TTL and evict timeout.
    pub fn new(ttl: Duration, evict_timeout: Duration) -> Self {
        Self {
            ttl,
            evict_timeout,
            map: Mutex::new(InFlightState::default()),
        }
    }

    /// Store auth and wake packets waiting for this salt. Expired auth is
    /// reclaimed at capacity; unauthenticated waiters cannot exhaust this cap.
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if !map.auth.contains_key(&key) && map.auth.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES
        {
            map.auth
                .retain(|_, (_, inserted_at)| now.duration_since(*inserted_at) <= self.ttl);
            if map.auth.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
                tracing::warn!(
                    "in-flight underlay auth table is full ({} entries); dropping new entry",
                    map.auth.len()
                );
                return;
            }
        }
        map.auth.insert(key, (auth, now));
        if let Some((notify, _)) = map.waiting.remove(&key) {
            notify.notify_waiters();
        }
    }

    /// Wait for auth, then consume it only if `authenticate` succeeds.
    /// The callback runs under the lock so concurrent packets cannot reuse auth.
    /// It must not block or re-enter this table.
    pub async fn evict<T>(
        &self,
        key: &InFlightKey,
        authenticate: impl FnOnce(&UnderlayAuth) -> Option<T>,
    ) -> Option<(UnderlayAuth, T)> {
        let deadline = tokio::time::Instant::now() + self.evict_timeout;
        loop {
            let wait = {
                let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((auth, _)) = map.auth.get(key) {
                    let verified = authenticate(auth)?;
                    let (auth, _) = map.auth.remove(key).unwrap();
                    return Some((auth, verified));
                }
                if !map.waiting.contains_key(key)
                    && map.waiting.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES
                {
                    let now = Instant::now();
                    map.waiting.retain(|_, (notify, inserted_at)| {
                        let keep = now.duration_since(*inserted_at) <= self.evict_timeout;
                        if !keep {
                            notify.notify_waiters();
                        }
                        keep
                    });
                    if map.waiting.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
                        return None;
                    }
                }
                let (notify, _) = map
                    .waiting
                    .entry(*key)
                    .or_insert_with(|| (Arc::new(Notify::new()), Instant::now()));
                // Register before store can notify, even if this future is not polled yet.
                notify.clone().notified_owned()
            };
            if tokio::time::timeout_at(deadline, wait).await.is_err() {
                // Other packets may still share this waiter; cleanup reclaims it.
                return None;
            }
        }
    }

    /// Remove expired auth and waiters, including abandoned packet tasks.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.auth
            .retain(|_, (_, inserted_at)| now.duration_since(*inserted_at) <= self.ttl);
        map.waiting.retain(|_, (notify, inserted_at)| {
            let keep = now.duration_since(*inserted_at) <= self.evict_timeout;
            if !keep {
                notify.notify_waiters();
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests;
