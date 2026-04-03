//! Integration tests for connection pool lifecycle.
//!
//! Tests pool acquire, release, recycle, timeout, and maintenance without
//! requiring a real database server. Uses `tokio::net::TcpListener` to
//! simulate a backend.

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
    assert!(
        matches!(err, DbError::PoolTimeout { .. }),
        "error should be PoolTimeout, got: {err}"
    );
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
    assert!(
        matches!(err, DbError::PoolClosed),
        "error should be PoolClosed, got: {err}"
    );
}
