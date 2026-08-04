//! Regression tests for pooled-backend poisoning on the `MySQL` proxy path.
//!
//! The bug these pin: PHP has no idea the proxy pools anything. At the end of
//! every request PDO tears down its handle and mysqlnd sends `COM_QUIT`. The
//! proxy relayed that to the *pooled* backend, the backend closed, the dead
//! socket was parked as a healthy idle slot, and every later checkout drew a
//! corpse. Field probe on v0.6.0, 20 sequential reads at ordinary pacing:
//! `ok=2 failed=18`, first failure at request #3, permanent.
//!
//! Everything here runs against an in-process mock `MySQL` server, so the
//! tests need no container and run in ordinary CI. The mock closes its socket
//! on `COM_QUIT` exactly as a real server does — that is the whole mechanism.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ephpm_db::ResetStrategy;
use ephpm_db::mysql::{MySqlProxy, RwSplitParams};
use ephpm_db::pool::PoolConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

// ── MySQL packet framing (test-local, deliberately independent of the crate) ──

const COM_QUIT: u8 = 0x01;
const COM_QUERY: u8 = 0x03;

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

fn eof_packet() -> Vec<u8> {
    vec![0xFE, 0x00, 0x00, 0x02, 0x00]
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

/// A protocol-41 column definition packet.
fn column_def(name: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(64);
    for field in [b"def".as_slice(), b"", b"", b"", name.as_bytes(), name.as_bytes()] {
        d.push(u8::try_from(field.len()).expect("short field"));
        d.extend_from_slice(field);
    }
    d.push(0x0c); // length of the fixed-field block
    d.extend_from_slice(&33_u16.to_le_bytes()); // charset
    d.extend_from_slice(&255_u32.to_le_bytes()); // column length
    d.push(0xFD); // MYSQL_TYPE_VAR_STRING
    d.extend_from_slice(&0_u16.to_le_bytes()); // flags
    d.push(0); // decimals
    d.extend_from_slice(&[0, 0]); // filler
    d
}

// ── Mock MySQL backend ───────────────────────────────────────────────────────

/// Counters and controls for the in-process mock `MySQL` backend.
struct MockBackend {
    addr: SocketAddr,
    /// Backend TCP connections accepted — how many times the pool had to dial.
    connects: Arc<AtomicUsize>,
    /// `COM_QUIT` packets that reached the backend. Must stay at zero: session
    /// termination belongs to the client-facing socket, never to a pooled one.
    quits: Arc<AtomicUsize>,
    /// Drops every live backend connection, simulating a server that closes
    /// connections while they sit idle in the pool.
    kill: broadcast::Sender<()>,
}

impl MockBackend {
    fn url(&self) -> String {
        format!("mysql://root@{}/test", self.addr)
    }

    fn kill_live_connections(&self) {
        let _ = self.kill.send(());
    }
}

async fn start_mock_backend() -> MockBackend {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock backend");
    let addr = listener.local_addr().expect("mock backend addr");
    let connects = Arc::new(AtomicUsize::new(0));
    let quits = Arc::new(AtomicUsize::new(0));
    let (kill, _) = broadcast::channel(16);

    let connects_task = Arc::clone(&connects);
    let quits_task = Arc::clone(&quits);
    let kill_task = kill.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            let id = connects_task.fetch_add(1, Ordering::SeqCst) + 1;
            let quits = Arc::clone(&quits_task);
            let mut killed = kill_task.subscribe();
            tokio::spawn(async move {
                tokio::select! {
                    () = mock_session(sock, id, quits) => {}
                    _ = killed.recv() => { /* drop the socket */ }
                }
            });
        }
    });

    MockBackend { addr, connects, quits, kill }
}

/// One backend connection, behaving like a real `MySQL` server: greet,
/// authenticate, answer commands, and **close on `COM_QUIT`**.
async fn mock_session(mut sock: TcpStream, id: usize, quits: Arc<AtomicUsize>) {
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
        let Ok((_, payload)) = read_packet(&mut sock).await else { return };
        let ok = match payload.first().copied() {
            Some(COM_QUIT) => {
                quits.fetch_add(1, Ordering::SeqCst);
                return; // a real server closes the socket here
            }
            Some(COM_QUERY) => write_result_set(&mut sock, id).await.is_ok(),
            // COM_PING, COM_RESET_CONNECTION and anything else: plain OK.
            _ => write_packet(&mut sock, 1, &ok_packet()).await.is_ok(),
        };
        if !ok {
            return;
        }
    }
}

/// A one-column, one-row result set carrying this backend connection's id, so
/// the client can tell which pooled connection served it.
async fn write_result_set(sock: &mut TcpStream, id: usize) -> std::io::Result<()> {
    write_packet(sock, 1, &[0x01]).await?; // column count
    write_packet(sock, 2, &column_def("backend_id")).await?;
    write_packet(sock, 3, &eof_packet()).await?;
    let value = id.to_string();
    let mut row = Vec::with_capacity(value.len() + 1);
    row.push(u8::try_from(value.len()).expect("short value"));
    row.extend_from_slice(value.as_bytes());
    write_packet(sock, 4, &row).await?;
    write_packet(sock, 5, &eof_packet()).await
}

// ── Client: one PHP-shaped request ───────────────────────────────────────────

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

/// Gap between sequential sessions.
///
/// The field probe ran "at ordinary pacing" — a real site's requests do not
/// arrive back-to-back within microseconds of each other. It matters here for
/// a mechanical reason too: the client returns as soon as it has written
/// `COM_QUIT`, while the proxy still has to finish its relay and park the
/// backend. Without a gap the next session can win that race, dial a fresh
/// connection, and never touch the connection the previous session left
/// behind — which would hide exactly the defect under test.
///
/// Far below the pool's 500ms checkout-validation threshold, so the ping never
/// runs and cannot mask anything either.
const REQUEST_PACING: Duration = Duration::from_millis(20);

/// Connect, run one `SELECT`, send `COM_QUIT`, disconnect — the exact shape of
/// a PHP request that opens a PDO handle and lets it fall out of scope.
///
/// Returns the id of the backend connection that served the query.
async fn client_session(proxy: &str) -> Result<usize, String> {
    let result =
        match tokio::time::timeout(Duration::from_secs(5), client_session_inner(proxy)).await {
            Ok(result) => result,
            Err(_) => Err("session timed out".to_string()),
        };
    tokio::time::sleep(REQUEST_PACING).await;
    result
}

async fn client_session_inner(proxy: &str) -> Result<usize, String> {
    let mut s = TcpStream::connect(proxy).await.map_err(|e| format!("connect: {e}"))?;
    let _ = s.set_nodelay(true);
    read_packet(&mut s).await.map_err(|e| format!("greeting: {e}"))?;
    write_packet(&mut s, 1, &handshake_response())
        .await
        .map_err(|e| format!("handshake response: {e}"))?;
    let (_, ok) = read_packet(&mut s).await.map_err(|e| format!("auth ok: {e}"))?;
    if ok.first() != Some(&0x00) {
        return Err("proxy did not accept the handshake".to_string());
    }

    let mut query = vec![COM_QUERY];
    query.extend_from_slice(b"SELECT backend_id");
    write_packet(&mut s, 0, &query).await.map_err(|e| format!("query: {e}"))?;
    let id = read_result_set(&mut s).await?;

    // mysqlnd sends this when PDO destroys the handle at request end.
    write_packet(&mut s, 0, &[COM_QUIT]).await.map_err(|e| format!("quit: {e}"))?;
    drop(s);
    Ok(id)
}

async fn read_result_set(s: &mut TcpStream) -> Result<usize, String> {
    let (_, header) = read_packet(s).await.map_err(|e| format!("result header: {e}"))?;
    match header.first().copied() {
        Some(0xFF) => return Err("backend returned ERR".to_string()),
        Some(0x00) => return Err("expected a result set, got OK".to_string()),
        None => return Err("empty result header".to_string()),
        Some(_) => {}
    }
    let columns = usize::from(header[0]);
    for _ in 0..columns {
        read_packet(s).await.map_err(|e| format!("column definition: {e}"))?;
    }
    read_packet(s).await.map_err(|e| format!("column EOF: {e}"))?;

    let (_, row) = read_packet(s).await.map_err(|e| format!("row: {e}"))?;
    if row.first() == Some(&0xFE) {
        return Err("result set had no rows".to_string());
    }
    let len = usize::from(row[0]);
    let value = std::str::from_utf8(row.get(1..1 + len).ok_or("short row")?)
        .map_err(|e| format!("row is not utf8: {e}"))?;
    let id = value.parse::<usize>().map_err(|e| format!("row value {value:?}: {e}"))?;
    read_packet(s).await.map_err(|e| format!("terminating EOF: {e}"))?;
    Ok(id)
}

// ── Proxy harness ────────────────────────────────────────────────────────────

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        min_connections: 1,
        max_connections: 4,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        // Long enough that the background health check never runs. The bug
        // must be fixed at checkout time, not papered over by a 30s reaper —
        // that is the difference between "broken for one request" and "broken
        // for thirty seconds".
        health_check_interval: Duration::from_secs(3600),
    }
}

/// Start a proxy in front of `backend` and return its listen address.
///
/// Deliberately does **not** start the pool maintenance task: no background
/// warmer, no background health check. Every recovery observed in these tests
/// is therefore attributable to the request path itself.
async fn start_proxy(backend: &MockBackend, reset_strategy: ResetStrategy) -> String {
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind for port discovery");
    let listen = probe.local_addr().expect("probe addr").to_string();
    drop(probe);

    let proxy = MySqlProxy::new(
        &backend.url(),
        &listen,
        None,
        test_pool_config(),
        reset_strategy,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
    )
    .await
    .expect("build MySqlProxy against mock backend");

    tokio::spawn(async move {
        if let Err(e) = proxy.run().await {
            eprintln!("mock proxy stopped: {e}");
        }
    });

    for _ in 0..100 {
        if TcpStream::connect(&listen).await.is_ok() {
            return listen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("proxy never became ready at {listen}");
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The field probe, ported verbatim: 20 sequential connect / query /
/// disconnect cycles at ordinary pacing. v0.6.0 scored `ok=2 failed=18` with
/// the first failure at request #3 and no recovery.
#[tokio::test(flavor = "multi_thread")]
async fn twenty_sequential_sessions_all_succeed() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Smart).await;

    let mut ok = 0usize;
    let mut failures = Vec::new();
    for i in 1..=20 {
        match client_session(&proxy).await {
            Ok(_) => ok += 1,
            Err(e) => failures.push(format!("#{i}: {e}")),
        }
    }

    assert_eq!(ok, 20, "expected 20/20 sessions to succeed; failures: {failures:?}");
    assert_eq!(
        backend.quits.load(Ordering::SeqCst),
        0,
        "COM_QUIT must never be relayed to a pooled backend"
    );
}

/// Same probe under the other two reset strategies. `Never` is the one that
/// poisons permanently — it recycles without a reset, so nothing on the return
/// path ever notices the socket is dead.
#[tokio::test(flavor = "multi_thread")]
async fn twenty_sequential_sessions_all_succeed_for_every_reset_strategy() {
    for strategy in [ResetStrategy::Never, ResetStrategy::Always] {
        let backend = start_mock_backend().await;
        let proxy = start_proxy(&backend, strategy).await;

        for i in 1..=20 {
            let result = client_session(&proxy).await;
            assert!(result.is_ok(), "session #{i} failed under {strategy:?}: {result:?}");
        }
        assert_eq!(
            backend.quits.load(Ordering::SeqCst),
            0,
            "COM_QUIT reached the backend under {strategy:?}"
        );
    }
}

/// The backend connection must survive the client's disconnect and be handed
/// to the next request. Proven two ways: the backend accepted exactly one TCP
/// connection, and every session was served by the same connection id.
#[tokio::test(flavor = "multi_thread")]
async fn com_quit_is_not_forwarded_and_the_backend_is_reused() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Smart).await;

    let first = client_session(&proxy).await.expect("first session");
    let mut served_by = vec![first];
    for _ in 0..19 {
        served_by.push(client_session(&proxy).await.expect("subsequent session"));
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
    assert_eq!(backend.quits.load(Ordering::SeqCst), 0, "COM_QUIT was relayed to the backend");
}

/// A backend that dies while its connection sits idle must be detected at
/// checkout, not handed to the caller. The idle gap here exceeds the pool's
/// validation threshold, so the ping runs and the pool dials fresh.
#[tokio::test(flavor = "multi_thread")]
async fn dead_idle_connection_is_replaced_at_checkout() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Smart).await;

    let first = client_session(&proxy).await.expect("first session");
    backend.kill_live_connections();
    // Longer than the pool's checkout-validation threshold (500ms) and far
    // shorter than health_check_interval, so only the checkout ping can save
    // this request.
    tokio::time::sleep(Duration::from_millis(750)).await;

    let second = client_session(&proxy)
        .await
        .expect("session after the backend died must succeed on a fresh dial");
    assert_ne!(first, second, "expected a freshly dialled backend connection");
    assert!(backend.connects.load(Ordering::SeqCst) >= 2, "the pool never re-dialled");
}

/// A backend that dies inside the validation window cannot be caught by the
/// ping — the relay itself has to notice. What must never happen is the dead
/// socket going *back* into the pool, which is what made the original failure
/// permanent rather than transient.
#[tokio::test(flavor = "multi_thread")]
async fn broken_backend_is_discarded_not_reparked() {
    let backend = start_mock_backend().await;
    let proxy = start_proxy(&backend, ResetStrategy::Never).await;

    client_session(&proxy).await.expect("first session");
    backend.kill_live_connections();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // This request draws the dead socket. Whether it succeeds is not the
    // point — nothing can be promised about a connection the peer closed a
    // moment ago.
    let _ = client_session(&proxy).await;

    // Recovery must be immediate and permanent. Under the original code the
    // corpse was recycled on the error path, so every request from here on
    // failed until the 30s background reaper ran.
    for i in 1..=5 {
        let result = client_session(&proxy).await;
        assert!(result.is_ok(), "recovery session #{i} failed: {result:?} — corpse was re-parked");
    }
}
