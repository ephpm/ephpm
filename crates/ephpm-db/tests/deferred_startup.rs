//! Regression tests for issue #226: the proxy must not require its upstream
//! to exist at startup.
//!
//! The bug: `MySqlProxy::new()` connected to the upstream *first* and bound
//! the listen socket only afterwards, with a bounded ~40 s retry budget.
//! Chaining `[db.mysql]` in front of ePHPm's own `[db.sqlite]` litewire
//! listener — which the same startup function binds a few statements later —
//! was therefore impossible: the proxy spent its whole budget against a
//! listener that did not exist yet, gave up permanently, and left PHP with
//! `[2002] Connection refused` for the life of the process. The general form
//! of the same bug is any deployment whose database is slower to start than
//! ePHPm.
//!
//! Everything here runs against an in-process mock `MySQL` server, so the
//! tests need no container and run in ordinary CI. The mock is deliberately
//! started *late* — that is the whole point.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_db::ResetStrategy;
use ephpm_db::health::{ProxyHealth, RetryBudget};
use ephpm_db::mysql::RwSplitParams;
use ephpm_db::pool::PoolConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ── MySQL packet framing (test-local, deliberately independent of the crate) ──

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

async fn read_packet(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((header[3], payload))
}

async fn write_packet(stream: &mut TcpStream, seq: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len()).expect("test packet fits in 24 bits");
    let bytes = len.to_le_bytes();
    stream.write_all(&[bytes[0], bytes[1], bytes[2], seq]).await?;
    stream.write_all(payload).await
}

fn ok_packet() -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00]
}

/// A `HandshakeV10` greeting the proxy's `parse_server_greeting` accepts.
fn greeting() -> Vec<u8> {
    let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    let mut p = Vec::with_capacity(80);
    p.push(10); // protocol version
    p.extend_from_slice(b"8.0.0-mock\0");
    p.extend_from_slice(&1_u32.to_le_bytes()); // connection id
    p.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // auth-plugin-data part 1
    p.push(0); // filler
    p.extend_from_slice(&caps.to_le_bytes()[..2]);
    p.push(33); // charset
    p.extend_from_slice(&0x0002_u16.to_le_bytes()); // status: AUTOCOMMIT
    p.extend_from_slice(&caps.to_le_bytes()[2..]);
    p.push(21); // auth-plugin-data length (8 + 12 + null)
    p.extend_from_slice(&[0u8; 10]); // reserved
    p.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
    p.push(0); // null-terminate part 2
    p.extend_from_slice(b"mysql_native_password\0");
    p
}

fn handshake_response() -> Vec<u8> {
    let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    let mut b = Vec::with_capacity(64);
    b.extend_from_slice(&caps.to_le_bytes());
    b.extend_from_slice(&16_777_215_u32.to_le_bytes());
    b.push(33);
    b.extend_from_slice(&[0u8; 23]);
    b.extend_from_slice(b"root\0");
    b.push(0); // lenenc-encoded empty auth response
    b.extend_from_slice(b"mysql_native_password\0");
    b
}

// ── A mock MySQL backend that can be started whenever the test wants ─────────

/// Reserve a loopback port and release it, so a backend can be bound there
/// later. Between the release and the late `start_mock_backend_on`, the
/// address is exactly what the bug needs: routable but refusing connections.
async fn reserve_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("reserve port");
    let addr = listener.local_addr().expect("reserved addr");
    drop(listener);
    addr
}

/// Bind a mock `MySQL` server on `addr`. Returns the count of accepted
/// backend connections.
async fn start_mock_backend_on(addr: SocketAddr) -> Arc<AtomicUsize> {
    let listener = TcpListener::bind(addr).await.expect("bind mock backend");
    let connects = Arc::new(AtomicUsize::new(0));
    let connects_task = Arc::clone(&connects);
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            connects_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(mock_session(sock));
        }
    });
    connects
}

/// Greet, accept any auth, then answer everything with `OK`.
async fn mock_session(mut sock: TcpStream) {
    let _ = sock.set_nodelay(true);
    if write_packet(&mut sock, 0, &greeting()).await.is_err() {
        return;
    }
    if read_packet(&mut sock).await.is_err() {
        return;
    }
    if write_packet(&mut sock, 2, &ok_packet()).await.is_err() {
        return;
    }
    loop {
        if read_packet(&mut sock).await.is_err() {
            return;
        }
        if write_packet(&mut sock, 1, &ok_packet()).await.is_err() {
            return;
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        min_connections: 1,
        max_connections: 5,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        health_check_interval: Duration::from_secs(30),
    }
}

/// Start a proxy on a free port with the deferred (production) startup path.
async fn spawn_proxy(upstream: SocketAddr) -> (String, Arc<ProxyHealth>) {
    let listen = reserve_addr().await.to_string();
    let health = ProxyHealth::new("mysql", listen.clone(), upstream.to_string());
    ephpm_db::mysql::spawn_deferred(
        &format!("mysql://root@{upstream}/test"),
        &listen,
        None,
        test_pool_config(),
        ResetStrategy::Smart,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
        Arc::clone(&health),
    )
    .await
    .expect("spawn_deferred must succeed without a reachable upstream");
    (listen, health)
}

/// Poll until `health` reports its first upstream handshake, or time out.
async fn await_first_connect(health: &ProxyHealth, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if health.ever_connected() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Complete one client handshake through the proxy.
async fn client_handshake(sock: &mut TcpStream) -> Result<(), String> {
    let (_, payload) = read_packet(sock).await.map_err(|e| format!("read greeting: {e}"))?;
    if payload.first() != Some(&10) {
        return Err(format!("not a HandshakeV10: {:?}", payload.first()));
    }
    write_packet(sock, 1, &handshake_response())
        .await
        .map_err(|e| format!("write handshake response: {e}"))?;
    let (_, ok) = read_packet(sock).await.map_err(|e| format!("read auth result: {e}"))?;
    if ok.first() == Some(&0x00) { Ok(()) } else { Err(format!("auth not OK: {:?}", ok.first())) }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The core of #226. Before the fix this call could not even be made: startup
/// connected first and bound second, so with nothing listening upstream there
/// was no proxy and no socket. Now the socket exists immediately.
#[tokio::test]
async fn listener_is_bound_before_the_upstream_exists() {
    let upstream = reserve_addr().await; // nothing is listening there
    let (listen, health) = spawn_proxy(upstream).await;

    let client = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&listen))
        .await
        .expect("connect to the proxy must not hang");
    assert!(client.is_ok(), "the proxy listen socket must be bound with no upstream: {client:?}");

    assert!(!health.ever_connected(), "no upstream exists, so no handshake can have happened");
    assert!(!health.is_up(), "the live gauge must report the upstream down");
}

/// An upstream that appears after ePHPm has started — the local litewire
/// listener in the chained config, or any database slower to boot than the
/// server — must be picked up by the retry loop.
#[tokio::test]
async fn upstream_that_appears_late_is_picked_up() {
    let upstream = reserve_addr().await;
    let (listen, health) = spawn_proxy(upstream).await;

    // The proxy is now burning connect attempts against a dead address.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!health.ever_connected(), "sanity: nothing to connect to yet");

    let backend_connects = start_mock_backend_on(upstream).await;

    assert!(
        await_first_connect(&health, Duration::from_secs(15)).await,
        "the proxy must reach an upstream that appeared after startup"
    );
    assert!(health.is_up());
    assert!(backend_connects.load(Ordering::SeqCst) >= 1, "the backend must have been dialed");

    // And it must actually serve: full client handshake through the proxy.
    let mut client = TcpStream::connect(&listen).await.expect("connect to proxy");
    client_handshake(&mut client).await.expect("proxy must serve clients once upstream is up");
}

/// A client that connects during the upstream-connect window must not get
/// `ECONNREFUSED`. The socket is bound, so it queues in the kernel accept
/// backlog and is served once the proxy starts accepting — which is what
/// makes PHP's first request survive a cold start rather than 500.
#[tokio::test]
async fn client_connected_before_the_upstream_is_served_from_the_backlog() {
    let upstream = reserve_addr().await;
    let (listen, health) = spawn_proxy(upstream).await;

    // Connect while the upstream is still dead and hold the socket.
    let mut early_client =
        TcpStream::connect(&listen).await.expect("connect must succeed into the backlog");

    tokio::time::sleep(Duration::from_millis(300)).await;
    start_mock_backend_on(upstream).await;
    assert!(await_first_connect(&health, Duration::from_secs(15)).await);

    // The connection made before any upstream existed now gets its greeting.
    tokio::time::timeout(Duration::from_secs(10), client_handshake(&mut early_client))
        .await
        .expect("the queued client must be served, not time out")
        .expect("the queued client must complete its handshake");
}

/// Retry is unbounded on the deferred path: an upstream down for longer than
/// the old ~40 s budget must still be picked up. Before the fix the proxy
/// gave up permanently and only a process restart could recover it.
#[tokio::test]
async fn retry_survives_longer_than_the_old_bounded_budget() {
    // The schedule itself is what makes this true; asserting it directly is
    // faster and more honest than sleeping for 40 s in CI.
    assert!(!RetryBudget::Unbounded.is_final_attempt(10), "the old budget stopped at 10 attempts");
    assert!(!RetryBudget::Unbounded.is_final_attempt(u32::MAX));
    assert!(RetryBudget::Bounded(10).is_final_attempt(10));
    assert!(!RetryBudget::Bounded(10).is_final_attempt(9));

    // 250ms doubling caps at 30 s, so steady state is one connect attempt per
    // 30 s — cheap enough to run forever, frequent enough to recover fast.
    assert_eq!(RetryBudget::Unbounded.max_backoff_ms(), 30_000);
    assert_eq!(RetryBudget::Bounded(10).max_backoff_ms(), 8_000);
}

/// A listen address that cannot be bound is a configuration error and must
/// fail startup — not log and leave a dead port, which is the failure shape
/// #226 is about.
#[tokio::test]
async fn bind_failure_is_an_error_not_a_warning() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.expect("occupy a port");
    let addr = occupied.local_addr().unwrap().to_string();
    let health = ProxyHealth::new("mysql", addr.clone(), "127.0.0.1:1".to_string());

    let result = ephpm_db::mysql::spawn_deferred(
        "mysql://root@127.0.0.1:1/test",
        &addr,
        None,
        test_pool_config(),
        ResetStrategy::Smart,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
        health,
    )
    .await;

    assert!(result.is_err(), "binding an occupied port must return an error");
}

/// A malformed URL is likewise fatal at startup, before anything is bound.
#[tokio::test]
async fn malformed_url_is_an_error() {
    let listen = reserve_addr().await.to_string();
    let health = ProxyHealth::new("mysql", listen.clone(), "unparsed".to_string());
    let result = ephpm_db::mysql::spawn_deferred(
        "not-a-database-url",
        &listen,
        None,
        test_pool_config(),
        ResetStrategy::Smart,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
        health,
    )
    .await;

    assert!(result.is_err(), "a malformed [db.mysql] url must fail startup");
}
