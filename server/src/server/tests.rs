use super::*;
use std::time::Duration;

#[tokio::test]
async fn failing_dns_lookup_does_not_block_next_lookup() {
    let cache = StdMutex::new(IndexMap::new());
    let resolving = StdMutex::new(HashMap::new());
    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            resolve_udp_target("\0", 53, &cache, &resolving),
        )
        .await
        .expect("a failed lookup must not leave the next lookup waiting");
        assert!(result.is_err());
    }
    assert!(resolving
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty());
}

#[test]
fn cancelled_and_timed_out_dns_lookups_release_key_and_wake_waiter() {
    use std::task::Poll;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    started_rx.recv().unwrap();
    runtime.block_on(async {
        let cache = StdMutex::new(IndexMap::new());
        let resolving = StdMutex::new(HashMap::new());
        let mut resolver = Box::pin(resolve_udp_target("localhost", 53, &cache, &resolving));
        let mut waiter = Box::pin(resolve_udp_target("localhost", 53, &cache, &resolving));
        std::future::poll_fn(|cx| {
            assert!(resolver.as_mut().poll(cx).is_pending());
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(resolver);
        assert!(resolving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(resolving.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        let result =
            tokio::time::timeout(consts::DNS_QUERY_TIMEOUT + Duration::from_secs(1), waiter)
                .await
                .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("DNS query timeout"));
        assert!(resolving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        release_tx.send(()).unwrap();
        let addr = tokio::time::timeout(
            Duration::from_secs(1),
            resolve_udp_target("localhost", 53, &cache, &resolving),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 53);
        blocker.await.unwrap();
    });
}

#[tokio::test]
async fn dns_waiter_is_bounded_by_query_timeout() {
    let cache = StdMutex::new(IndexMap::new());
    let resolving = StdMutex::new(HashMap::from([(
        (Arc::from("localhost"), 53),
        Arc::new(tokio::sync::Notify::new()),
    )]));
    let result = tokio::time::timeout(
        consts::DNS_QUERY_TIMEOUT + Duration::from_secs(1),
        resolve_udp_target("localhost", 53, &cache, &resolving),
    )
    .await
    .expect("waiting for another resolver must also time out");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("DNS query timeout"));
}
