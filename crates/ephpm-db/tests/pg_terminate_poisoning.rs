//! Regression tests for pooled-backend poisoning on the `PostgreSQL` proxy path.
//!
//! The `PostgreSQL` equivalent of the `COM_QUIT` bug: PDO sends `Terminate`
//! (`'X'`) when it tears down a handle, and the proxy relayed it to the pooled
//! backend. Two code paths carried it — the simple bidirectional relay used
//! when `reset_strategy = "never"`, and the *pinned* session that every
//! extended-query client (which is to say, every real `PDO_pgsql` client) ends
//! up on after its first `Parse`.
//!
//! `Smart` masked the damage: `DISCARD ALL` failed against the just-closed
//! socket, so the connection was discarded rather than re-parked and the pool
//! self-healed by burning a connection per request. `Never` had no such luck —
//! it recycled the corpse.
//!
//! Everything runs against an in-process mock `PostgreSQL` server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ephpm_db::ResetStrategy;
use ephpm_db::pool::PoolConfig;
use ephpm_db::postgres::{PgProxy, PgRwSplitParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

// ── PG message framing (test-local) ──────────────────────────────────────────

const MSG_QUERY: u8 = b'Q';
const MSG_TERMINATE: u8 = b'X';
const MSG_SYNC: u8 = b'S';
const MSG_PARSE: u8 = b'P';
const MSG_BIND: u8 = b'B';
const MSG_EXECUTE: u8 = b'E';
const MSG_ROW_DESCRIPTION: u8 = b'T';
const MSG_DATA_ROW: u8 = b'D';
const MSG_COMMAND_COMPLETE: u8 = b'C';
const MSG_READY_FOR_QUERY: u8 = b'Z';
const MSG_AUTH: u8 = b'R';
const MSG_PARAMETER_STATUS: u8 = b'S';
const MSG_BACKEND_KEY_DATA: u8 = b'K';

async fn read_message(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).await?;
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let payload_len = usize::try_from(len - 4).unwrap_or(0);
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    Ok((header[0], payload))
}

async fn write_message(stream: &mut TcpStream, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = i32::try_from(payload.len() + 4).expect("test message fits in i32");
    stream.write_all(&[tag]).await?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await
}

// ── Mock PostgreSQL backend ──────────────────────────────────────────────────

struct MockPgBackend {
    addr: SocketAddr,
    /// Backend TCP connections accepted — how many times the pool had to dial.
    connects: Arc<AtomicUsize>,
    /// `Terminate` messages that reached the backend. Must stay at zero.
    terminates: Arc<AtomicUsize>,
    kill: broadcast::Sender<()>,
}

impl MockPgBackend {
    fn url(&self) -> String {
        format!("postgres://postgres@{}/test", self.addr)
    }

    fn kill_live_connections(&self) {
        let _ = self.kill.send(());
    }
}

async fn start_mock_backend() -> MockPgBackend {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock PG backend");
    let addr = listener.local_addr().expect("mock PG addr");
    let connects = Arc::new(AtomicUsize::new(0));
    let terminates = Arc::new(AtomicUsize::new(0));
    let (kill, _) = broadcast::channel(16);

    let connects_task = Arc::clone(&connects);
    let terminates_task = Arc::clone(&terminates);
    let kill_task = kill.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            let id = connects_task.fetch_add(1, Ordering::SeqCst) + 1;
            let terminates = Arc::clone(&terminates_task);
            let mut killed = kill_task.subscribe();
            tokio::spawn(async move {
                tokio::select! {
                    () = mock_session(sock, id, terminates) => {}
                    _ = killed.recv() => { /* drop the socket */ }
                }
            });
        }
    });

    MockPgBackend { addr, connects, terminates, kill }
}

/// One backend connection: startup, then answer queries and **close on
/// `Terminate`**, exactly as a real server does.
async fn mock_session(mut sock: TcpStream, id: usize, terminates: Arc<AtomicUsize>) {
    let _ = sock.set_nodelay(true);

    // StartupMessage has no tag byte: [len: 4BE][payload].
    let Ok(len) = sock.read_i32().await else { return };
    let payload_len = usize::try_from(len - 4).unwrap_or(0);
    let mut payload = vec![0u8; payload_len];
    if sock.read_exact(&mut payload).await.is_err() {
        return;
    }

    if write_message(&mut sock, MSG_AUTH, &0_i32.to_be_bytes()).await.is_err() {
        return;
    }
    if write_message(&mut sock, MSG_PARAMETER_STATUS, b"server_version\0mock\0").await.is_err() {
        return;
    }
    if write_message(&mut sock, MSG_BACKEND_KEY_DATA, &[0u8; 8]).await.is_err() {
        return;
    }
    if write_message(&mut sock, MSG_READY_FOR_QUERY, b"I").await.is_err() {
        return;
    }

    loop {
        let Ok((tag, _)) = read_message(&mut sock).await else { return };
        let ok = match tag {
            MSG_TERMINATE => {
                terminates.fetch_add(1, Ordering::SeqCst);
                return; // a real server closes the socket here
            }
            // A simple Query, or the Sync that flushes an extended-protocol
            // batch, both produce one complete response ending in ReadyForQuery.
            MSG_QUERY | MSG_SYNC => write_row_set(&mut sock, id).await.is_ok(),
            // Parse / Bind / Describe / Execute produce no output until Sync.
            _ => true,
        };
        if !ok {
            return;
        }
    }
}

/// A one-column, one-row result carrying this backend connection's id.
async fn write_row_set(sock: &mut TcpStream, id: usize) -> std::io::Result<()> {
    let mut desc = Vec::with_capacity(32);
    desc.extend_from_slice(&1_i16.to_be_bytes()); // field count
    desc.extend_from_slice(b"backend_id\0");
    desc.extend_from_slice(&0_i32.to_be_bytes()); // table oid
    desc.extend_from_slice(&0_i16.to_be_bytes()); // column attnum
    desc.extend_from_slice(&25_i32.to_be_bytes()); // type oid: text
    desc.extend_from_slice(&(-1_i16).to_be_bytes()); // type length
    desc.extend_from_slice(&(-1_i32).to_be_bytes()); // type modifier
    desc.extend_from_slice(&0_i16.to_be_bytes()); // format: text
    write_message(sock, MSG_ROW_DESCRIPTION, &desc).await?;

    let value = id.to_string();
    let mut row = Vec::with_capacity(value.len() + 6);
    row.extend_from_slice(&1_i16.to_be_bytes()); // field count
    row.extend_from_slice(&i32::try_from(value.len()).expect("short value").to_be_bytes());
    row.extend_from_slice(value.as_bytes());
    write_message(sock, MSG_DATA_ROW, &row).await?;

    write_message(sock, MSG_COMMAND_COMPLETE, b"SELECT 1\0").await?;
    write_message(sock, MSG_READY_FOR_QUERY, b"I").await
}

// ── Client: one PHP-shaped request ───────────────────────────────────────────

/// Whether the client speaks the simple or the extended query protocol.
///
/// `PDO_pgsql` uses the extended protocol, which pins the session to one
/// backend for its whole lifetime — a different code path in the proxy, and
/// the one that mattered in production.
#[derive(Clone, Copy)]
enum Protocol {
    Simple,
    Extended,
}

/// Gap between sequential sessions.
///
/// The client returns as soon as it has written `Terminate`, while the proxy
/// still has to finish its relay and park the backend. Without a gap the next
/// session can win that race and dial fresh, never touching the connection the
/// previous session left behind — which would hide the defect under test. Far
/// below the pool's 500ms checkout-validation threshold, so the ping never
/// runs and cannot mask anything either.
const REQUEST_PACING: Duration = Duration::from_millis(20);

async fn client_session(proxy: &str, protocol: Protocol) -> Result<usize, String> {
    let result =
        match tokio::time::timeout(Duration::from_secs(5), client_session_inner(proxy, protocol))
            .await
        {
            Ok(result) => result,
            Err(_) => Err("session timed out".to_string()),
        };
    tokio::time::sleep(REQUEST_PACING).await;
    result
}

async fn client_session_inner(proxy: &str, protocol: Protocol) -> Result<usize, String> {
    let mut s = TcpStream::connect(proxy).await.map_err(|e| format!("connect: {e}"))?;
    let _ = s.set_nodelay(true);

    // StartupMessage.
    let mut startup = Vec::with_capacity(64);
    startup.extend_from_slice(&0x0003_0000_i32.to_be_bytes());
    startup.extend_from_slice(b"user\0postgres\0");
    startup.extend_from_slice(b"database\0test\0");
    startup.push(0);
    let total = i32::try_from(startup.len() + 4).expect("startup fits in i32");
    s.write_all(&total.to_be_bytes()).await.map_err(|e| format!("startup len: {e}"))?;
    s.write_all(&startup).await.map_err(|e| format!("startup: {e}"))?;

    // Drain the handshake up to ReadyForQuery.
    loop {
        let (tag, _) = read_message(&mut s).await.map_err(|e| format!("handshake: {e}"))?;
        if tag == MSG_READY_FOR_QUERY {
            break;
        }
    }

    match protocol {
        Protocol::Simple => {
            write_message(&mut s, MSG_QUERY, b"SELECT backend_id\0")
                .await
                .map_err(|e| format!("query: {e}"))?;
        }
        Protocol::Extended => {
            let mut parse = Vec::with_capacity(32);
            parse.push(0); // unnamed statement
            parse.extend_from_slice(b"SELECT backend_id\0");
            parse.extend_from_slice(&0_i16.to_be_bytes()); // no parameter types
            write_message(&mut s, MSG_PARSE, &parse).await.map_err(|e| format!("parse: {e}"))?;

            let mut bind = Vec::with_capacity(16);
            bind.push(0); // unnamed portal
            bind.push(0); // unnamed statement
            bind.extend_from_slice(&0_i16.to_be_bytes()); // format codes
            bind.extend_from_slice(&0_i16.to_be_bytes()); // parameter values
            bind.extend_from_slice(&0_i16.to_be_bytes()); // result format codes
            write_message(&mut s, MSG_BIND, &bind).await.map_err(|e| format!("bind: {e}"))?;

            let mut execute = Vec::with_capacity(8);
            execute.push(0); // unnamed portal
            execute.extend_from_slice(&0_i32.to_be_bytes()); // no row limit
            write_message(&mut s, MSG_EXECUTE, &execute)
                .await
                .map_err(|e| format!("execute: {e}"))?;

            write_message(&mut s, MSG_SYNC, b"").await.map_err(|e| format!("sync: {e}"))?;
        }
    }

    let id = read_row_set(&mut s).await?;

    // PDO sends this when it destroys the handle at request end.
    write_message(&mut s, MSG_TERMINATE, b"").await.map_err(|e| format!("terminate: {e}"))?;
    drop(s);
    Ok(id)
}

async fn read_row_set(s: &mut TcpStream) -> Result<usize, String> {
    let mut value: Option<usize> = None;
    loop {
        let (tag, payload) = read_message(s).await.map_err(|e| format!("response: {e}"))?;
        match tag {
            MSG_DATA_ROW => {
                // [field count: 2BE][len: 4BE][bytes]
                let len = usize::try_from(i32::from_be_bytes([
                    *payload.get(2).ok_or("short DataRow")?,
                    *payload.get(3).ok_or("short DataRow")?,
                    *payload.get(4).ok_or("short DataRow")?,
                    *payload.get(5).ok_or("short DataRow")?,
                ]))
                .map_err(|e| format!("negative DataRow field length: {e}"))?;
                let bytes = payload.get(6..6 + len).ok_or("truncated DataRow")?;
                let text =
                    std::str::from_utf8(bytes).map_err(|e| format!("DataRow not utf8: {e}"))?;
                value = Some(text.parse().map_err(|e| format!("DataRow value {text:?}: {e}"))?);
            }
            b'E' => return Err("backend returned ErrorResponse".to_string()),
            MSG_READY_FOR_QUERY => {
                return value.ok_or_else(|| "no DataRow in response".to_string());
            }
            _ => {}
        }
    }
}

// ── Proxy harness ────────────────────────────────────────────────────────────

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        min_connections: 1,
        max_connections: 4,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        // Long enough that the background health check never runs.
        health_check_interval: Duration::from_secs(3600),
    }
}

/// Start a proxy in front of `backend`; no background maintenance task, so
/// every recovery is attributable to the request path itself.
async fn start_proxy(backend: &MockPgBackend, reset_strategy: ResetStrategy) -> String {
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind for port discovery");
    let listen = probe.local_addr().expect("probe addr").to_string();
    drop(probe);

    let proxy = PgProxy::new(
        &backend.url(),
        &listen,
        test_pool_config(),
        reset_strategy,
        vec![],
        PgRwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
        // Instrumented like production, so these pool-lifecycle tests also
        // exercise the query-stats tap point on every relayed statement.
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
    )
    .await
    .expect("build PgProxy against mock backend");

    tokio::spawn(async move {
        if let Err(e) = proxy.run().await {
            eprintln!("mock PG proxy stopped: {e}");
        }
    });

    for _ in 0..100 {
        if TcpStream::connect(&listen).await.is_ok() {
            return listen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("PG proxy never became ready at {listen}");
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The MySQL probe, ported to `PostgreSQL`: 20 sequential connect / query /
/// disconnect cycles over the simple bidirectional relay.
#[tokio::test(flavor = "multi_thread")]
async fn twenty_sequential_sessions_all_succeed() {
    for strategy in [ResetStrategy::Never, ResetStrategy::Always, ResetStrategy::Smart] {
        let backend = start_mock_backend().await;
        let proxy = start_proxy(&backend, strategy).await;

        let mut ok = 0usize;
        let mut failures = Vec::new();
        for i in 1..=20 {
            match client_session(&proxy, Protocol::Simple).await {
                Ok(_) => ok += 1,
                Err(e) => failures.push(format!("#{i}: {e}")),
            }
        }
        assert_eq!(ok, 20, "expected 20/20 under {strategy:?}; failures: {failures:?}");
        assert_eq!(
            backend.terminates.load(Ordering::SeqCst),
            0,
            "Terminate reached a pooled backend under {strategy:?}"
        );
    }
}

/// `reset_strategy = "never"` takes the plain bidirectional relay, which had no
/// reset on the return path to accidentally notice the socket was dead. This
/// is the configuration that poisoned permanently.
#[tokio::test(flavor = "multi_thread")]
async fn terminate_is_not_forwarded_and_the_backend_is_reused() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Never).await;

    let first = client_session(&proxy, Protocol::Simple).await.expect("first session");
    let mut served_by = vec![first];
    for _ in 0..19 {
        served_by.push(client_session(&proxy, Protocol::Simple).await.expect("subsequent session"));
    }

    assert!(
        served_by.iter().all(|id| *id == first),
        "every session should reuse one pooled backend, got ids {served_by:?}"
    );
    assert_eq!(
        backend.connects.load(Ordering::SeqCst),
        1,
        "the pool should have dialled the backend exactly once for 20 client sessions"
    );
    assert_eq!(backend.terminates.load(Ordering::SeqCst), 0, "Terminate was relayed");
}

/// The `PDO_pgsql` shape. The first extended-protocol message pins the session
/// to one backend and splices the rest of it verbatim — which is how
/// `Terminate` reached the backend even under the default `Smart` strategy.
/// `Smart` hid it by burning a connection per request instead of poisoning
/// one, so the symptom was cost rather than failure.
#[tokio::test(flavor = "multi_thread")]
async fn extended_protocol_session_does_not_forward_terminate() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Smart).await;

    for i in 1..=20 {
        let result = client_session(&proxy, Protocol::Extended).await;
        assert!(result.is_ok(), "extended-protocol session #{i} failed: {result:?}");
    }

    assert_eq!(
        backend.terminates.load(Ordering::SeqCst),
        0,
        "Terminate must not be relayed on the pinned session path"
    );
    assert_eq!(
        backend.connects.load(Ordering::SeqCst),
        1,
        "the pinned session path should reuse one backend, not burn one per request"
    );
}

/// A backend that dies while idle must be caught at checkout.
#[tokio::test(flavor = "multi_thread")]
async fn dead_idle_connection_is_replaced_at_checkout() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Never).await;

    let first = client_session(&proxy, Protocol::Simple).await.expect("first session");
    backend.kill_live_connections();
    // Longer than the pool's 500ms checkout-validation threshold.
    tokio::time::sleep(Duration::from_millis(750)).await;

    let second = client_session(&proxy, Protocol::Simple)
        .await
        .expect("session after the backend died must succeed on a fresh dial");
    assert_ne!(first, second, "expected a freshly dialled backend connection");
    assert!(backend.connects.load(Ordering::SeqCst) >= 2, "the pool never re-dialled");
}

/// A backend that dies inside the validation window has to be caught by the
/// relay. What must never happen is the corpse going back into the pool.
#[tokio::test(flavor = "multi_thread")]
async fn broken_backend_is_discarded_not_reparked() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Never).await;

    client_session(&proxy, Protocol::Simple).await.expect("first session");
    backend.kill_live_connections();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Draws the dead socket; nothing can be promised about this one.
    let _ = client_session(&proxy, Protocol::Simple).await;

    for i in 1..=5 {
        let result = client_session(&proxy, Protocol::Simple).await;
        assert!(result.is_ok(), "recovery session #{i} failed: {result:?} — corpse was re-parked");
    }
}
