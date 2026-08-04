//! Integration tests for connection pool lifecycle.
//!
//! Tests pool acquire, release, recycle, timeout, and maintenance without
//! requiring a real database server. Uses `tokio::net::TcpListener` to
//! simulate a backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_db::error::DbError;
use ephpm_db::pool::{Pool, PoolConfig};
use tokio::net::{TcpListener, TcpStream};

/// Helper: create a pool config with short timeouts for testing.
fn test_config(max: u32) -> PoolConfig {
    PoolConfig {
        min_connections: 0,
        max_connections: max,
        idle_timeout: Duration::from_secs(5),
        max_lifetime: Duration::from_secs(60),
        pool_timeout: Duration::from_millis(200),
        health_check_interval: Duration::from_secs(300),
    }
}

/// Helper: bind a listener and build a pool that connects to it.
async fn pool_with_backend(max: u32) -> (Pool, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let connect = move || -> ephpm_db::pool::BoxFuture<Result<TcpStream, DbError>> {
        Box::pin(async move {
            let stream = TcpStream::connect(addr).await?;
            Ok(stream)
        })
    };
    let reset = |s: TcpStream| -> ephpm_db::pool::BoxFuture<Result<TcpStream, DbError>> {
        Box::pin(async { Ok(s) })
    };
    let ping = |s: TcpStream| -> ephpm_db::pool::BoxFuture<Result<(TcpStream, bool), DbError>> {
        Box::pin(async { Ok((s, true)) })
    };

    let pool = Pool::new(test_config(max), connect, reset, ping);
    (pool, listener)
}

#[tokio::test]
async fn acquire_creates_connection() {
    let (pool, listener) = pool_with_backend(2).await;

    // Accept in background so the pool's connect succeeds.
    let accept = tokio::spawn(async move {
        let _ = listener.accept().await.unwrap();
    });

    let checkout = pool.acquire().await;
    assert!(checkout.is_ok(), "acquire should succeed");
    accept.await.unwrap();
}

#[tokio::test]
async fn pool_timeout_when_exhausted() {
    let (pool, listener) = pool_with_backend(1).await;

    // Accept one backend connection.
    let accept = tokio::spawn(async move {
        let (_s, _) = listener.accept().await.unwrap();
        // Hold the backend alive.
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let checkout = pool.acquire().await.unwrap();

    // Second acquire should time out.
    let start = Instant::now();
    let result = pool.acquire().await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "second acquire should fail");
    let err = result.err().unwrap();
    assert!(matches!(err, DbError::PoolTimeout { .. }), "error should be PoolTimeout, got: {err}");
    assert!(elapsed >= Duration::from_millis(150), "should have waited near pool_timeout");

    drop(checkout);
    accept.abort();
}

#[tokio::test]
async fn recycle_reuses_connection() {
    // Use max_connections=2 so the semaphore has room: one permit may be
    // parked in the idle slot while acquire grabs another.
    let (pool, listener) = pool_with_backend(2).await;

    // Accept connections in the background as needed.
    let accept = tokio::spawn(async move {
        let mut count = 0u32;
        loop {
            match listener.accept().await {
                Ok(_) => count += 1,
                Err(_) => break,
            }
            if count >= 3 {
                break;
            }
        }
        count
    });

    // Acquire, return, acquire again — the second acquire should find the
    // connection in the idle queue instead of opening a new one.
    let mut checkout = pool.acquire().await.unwrap();
    let stream = checkout.take_stream();
    checkout.return_to_pool(stream);

    // Small delay for the recycle to settle.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Second acquire should succeed (either from idle or new connection).
    let checkout2 = pool.acquire().await;
    assert!(checkout2.is_ok(), "second acquire after recycle should succeed");
    drop(checkout2);

    accept.abort();
}

#[tokio::test]
async fn close_rejects_new_acquires() {
    let (pool, _listener) = pool_with_backend(2).await;
    pool.close();

    let result = pool.acquire().await;
    assert!(result.is_err(), "acquire after close should fail");
    let err = result.err().unwrap();
    assert!(matches!(err, DbError::PoolClosed), "error should be PoolClosed, got: {err}");
}

// ── Checkout-time validation ─────────────────────────────────────────────────

/// Build a pool whose `ping` verdict and dial count are observable.
///
/// `alive` decides what the ping closure reports; `pings` and `connects` count
/// how often each closure ran.
fn instrumented_pool(
    addr: std::net::SocketAddr,
    alive: bool,
) -> (Pool, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let connects = Arc::new(AtomicUsize::new(0));
    let pings = Arc::new(AtomicUsize::new(0));

    let connects_c = Arc::clone(&connects);
    let connect = move || -> ephpm_db::pool::BoxFuture<Result<TcpStream, DbError>> {
        let counter = Arc::clone(&connects_c);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(TcpStream::connect(addr).await?)
        })
    };
    let reset = |s: TcpStream| -> ephpm_db::pool::BoxFuture<Result<TcpStream, DbError>> {
        Box::pin(async { Ok(s) })
    };
    let pings_c = Arc::clone(&pings);
    let ping =
        move |s: TcpStream| -> ephpm_db::pool::BoxFuture<Result<(TcpStream, bool), DbError>> {
            let counter = Arc::clone(&pings_c);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok((s, alive))
            })
        };

    (Pool::new(test_config(4), connect, reset, ping), connects, pings)
}

/// A connection returned and immediately re-acquired cannot have died in the
/// interim for any reason a ping would catch, so the checkout must not pay for
/// one. This is what keeps validation off the hot path under load.
#[tokio::test]
async fn acquire_skips_validation_for_a_freshly_returned_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });

    let (pool, connects, pings) = instrumented_pool(addr, true);

    let mut checkout = pool.acquire().await.unwrap();
    let stream = checkout.take_stream();
    checkout.return_to_pool(stream);

    let second = pool.acquire().await;
    assert!(second.is_ok(), "re-acquire should succeed");
    assert_eq!(pings.load(Ordering::SeqCst), 0, "no ping should run inside the validation window");
    assert_eq!(connects.load(Ordering::SeqCst), 1, "the idle connection should have been reused");

    accept.abort();
}

/// Past the idle threshold the connection is pinged, and a ping that reports
/// "dead" must cause the pool to discard the slot and dial fresh rather than
/// hand out a socket it already knows is gone.
#[tokio::test]
async fn acquire_discards_an_idle_connection_that_fails_validation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });

    let (pool, connects, pings) = instrumented_pool(addr, false);

    let mut checkout = pool.acquire().await.unwrap();
    let stream = checkout.take_stream();
    checkout.return_to_pool(stream);
    assert_eq!(pool.idle_len(), 1, "connection should be parked");

    // Wait past the pool's 500ms checkout-validation threshold.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let second = pool.acquire().await;
    assert!(second.is_ok(), "acquire should recover by dialling a fresh connection");
    assert_eq!(pings.load(Ordering::SeqCst), 1, "the idle connection should have been pinged");
    assert_eq!(connects.load(Ordering::SeqCst), 2, "a fresh connection should have been dialled");
    assert_eq!(pool.idle_len(), 0, "the failed connection must not be left in the idle queue");

    accept.abort();
}

/// A live connection that has been idle a while is pinged and then handed out
/// — validation must not throw away healthy connections.
#[tokio::test]
async fn acquire_keeps_an_idle_connection_that_passes_validation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });

    let (pool, connects, pings) = instrumented_pool(addr, true);

    let mut checkout = pool.acquire().await.unwrap();
    let stream = checkout.take_stream();
    checkout.return_to_pool(stream);

    tokio::time::sleep(Duration::from_millis(600)).await;

    let second = pool.acquire().await;
    assert!(second.is_ok(), "acquire should succeed");
    assert_eq!(pings.load(Ordering::SeqCst), 1, "the idle connection should have been pinged");
    assert_eq!(connects.load(Ordering::SeqCst), 1, "no redial should have been needed");

    accept.abort();
}
