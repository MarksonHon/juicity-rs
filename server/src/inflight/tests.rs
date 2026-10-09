use super::*;
use juicity_common::protocol::{Network, ProxyMetadata};
use std::future::Future;
use std::task::Poll;
use uuid::Uuid;

fn auth(key: InFlightKey) -> UnderlayAuth {
    UnderlayAuth {
        iv: key,
        psk: vec![0x42; 32],
        metadata: ProxyMetadata {
            network: Network::Udp,
            hostname: "127.0.0.1".to_owned(),
            port: 53,
            uuid: Uuid::nil(),
        },
        uuid: Uuid::nil(),
    }
}

#[tokio::test]
async fn placeholder_flood_leaves_room_for_auth_and_preserves_waiters() {
    let table = InFlightUnderlayKey::new(Duration::from_secs(60), Duration::from_secs(60));
    let keys: Vec<_> = (0..=consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES)
        .map(|n| {
            let mut key = [0u8; 32];
            key[2..10].copy_from_slice(&(n as u64).to_be_bytes());
            key
        })
        .collect();
    let mut waiters: Vec<_> = keys
        .iter()
        .map(|key| Box::pin(table.evict(key, |_| Some(()))))
        .collect();
    std::future::poll_fn(|cx| {
        for waiter in &mut waiters {
            let _ = waiter.as_mut().poll(cx);
        }
        Poll::Ready(())
    })
    .await;
    assert_eq!(
        table
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting
            .len(),
        consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES
    );

    let new_key = [0xff; 32];
    table.store(new_key, auth(new_key));
    assert!(table.evict(&new_key, |_| Some(())).await.is_some());

    table.store(keys[0], auth(keys[0]));
    // Store notifies before this pending packet is polled again.
    let result = tokio::time::timeout(Duration::from_secs(1), waiters.remove(0))
        .await
        .unwrap();
    assert_eq!(result.unwrap().0.iv, keys[0]);
}

#[tokio::test]
async fn concurrent_verified_packets_consume_auth_only_once() {
    let table = InFlightUnderlayKey::new(Duration::from_secs(1), Duration::from_millis(10));
    let key = [0; 32];
    table.store(key, auth(key));
    let (one, two) = tokio::join!(
        table.evict(&key, |_| Some(())),
        table.evict(&key, |_| Some(())),
    );
    assert_ne!(one.is_some(), two.is_some());
}
