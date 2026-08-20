//! Native WebSocket e2e tests (`[server.websocket]`).
//!
//! Runs against its own isolated node (`ISOLATED_CONFIG_SUITES` in
//! `xtask/src/e2e_bare.rs`) because enabling WebSockets changes how **every**
//! upgrade request routes. That node is multi-tenant, so this suite gets two
//! real virtual hosts — `ws-a.test` and `ws-b.test` — on one server, which is
//! what lets the cross-site test be an end-to-end assertion rather than a
//! unit-level one.
//!
//! Environment:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://127.0.0.1:18100`)
//! - `EPHPM_SITES_DIR` — writable sites directory on the ephpm host
//!
//! The suite runs single-threaded (see `suite_is_serial`): its tests share the
//! deployed vhosts and each vhost's KV keyspace.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ephpm_e2e::required_env;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Vhosts this suite deploys. Both are in the isolated node's `trusted_hosts`.
const SITE_A: &str = "ws-a.test";
const SITE_B: &str = "ws-b.test";

/// Query-string token `websocket.php` demands during `connect`. The auth-
/// rejection test is what makes this more than decoration.
const TOKEN: &str = "letmein";

/// How long any single frame may take to arrive. Generous: a `message` event
/// is a full PHP execution, and CI runners are not fast.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);

type Ws = WebSocketStream<TcpStream>;

// ── fixtures ────────────────────────────────────────────────────────────────

/// The WebSocket entrypoint. One script, every event — userland routes on
/// `$_SERVER['WS_EVENT']`, which is the whole point of the design.
const WEBSOCKET_PHP: &str = r#"<?php
// E2E WebSocket entrypoint. Deployed to two vhosts so the suite can prove
// site isolation with a real server.
$event = $_SERVER['WS_EVENT'] ?? '';
$conn  = $_SERVER['WS_CONNECTION_ID'] ?? '';

if ($event === 'connect') {
    parse_str($_SERVER['QUERY_STRING'] ?? '', $q);
    // Authentication happens HERE — the only point at which an upgrade can
    // still be refused. A non-2xx response means no socket is ever created.
    if (($q['token'] ?? '') !== 'letmein') {
        http_response_code(403);
        echo 'denied';
        return;
    }
    // The HTTP-pushes-to-socket pattern: stash the connection id under an
    // application identity so an ordinary request handler can find it later.
    if (!empty($q['client'])) {
        ephpm_kv_set('ws:client:' . $q['client'], $conn);
    }
    http_response_code(200);
    echo 'ok';
    return;
}

if ($event === 'disconnect') {
    // Best-effort cleanup. The connection is already out of the registry by
    // the time this runs.
    ephpm_kv_incr('ws:disconnects');
    return;
}

if ($event === 'message') {
    $body  = file_get_contents('php://input');
    $parts = explode(':', $body, 3);
    switch ($parts[0]) {
        case 'echo':
            // Implicit form: no connection id, acts on the connection that
            // fired this event.
            ephpm_ws_send('echo:' . ($parts[1] ?? ''));
            break;
        case 'whoami':
            ephpm_ws_send('id:' . $conn);
            break;
        case 'opcode':
            ephpm_ws_send('opcode:' . ($_SERVER['WS_OPCODE'] ?? '?'));
            break;
        case 'join':
            $ok = ephpm_ws_subscribe($parts[1]);
            ephpm_ws_send('joined:' . ($ok ? '1' : '0'));
            break;
        case 'leave':
            $ok = ephpm_ws_unsubscribe($parts[1]);
            ephpm_ws_send('left:' . ($ok ? '1' : '0'));
            break;
        case 'say':
            $n = ephpm_ws_broadcast($parts[1], 'chat:' . ($parts[2] ?? ''));
            ephpm_ws_send('sent:' . $n);
            break;
        case 'bye':
            ephpm_ws_close(4001);
            break;
        default:
            ephpm_ws_send('unknown');
    }
}
"#;

/// An ordinary HTTP handler that pushes to a live socket — the killer feature,
/// and the exact pattern documented in `site/content/guides/websockets.md`.
const PUSH_PHP: &str = r#"<?php
// Look the connection up by application identity (the id `websocket.php`
// stashed during `connect`), or take a raw id for the cross-site test.
$id = $_GET['id'] ?? null;
if ($id === null && isset($_GET['client'])) {
    $id = ephpm_kv_get('ws:client:' . $_GET['client']);
}
if (!$id) {
    http_response_code(404);
    echo 'no-connection';
    return;
}
$ok = ephpm_ws_connection_send($id, $_GET['msg'] ?? 'ping');
echo $ok ? 'sent' : 'not-sent';
"#;

/// Floods a connection from an ordinary HTTP request and reports how many
/// sends were accepted vs refused. This is the slow-consumer probe: with the
/// client not reading, the bounded outbound queue must start refusing (and
/// shed the connection) instead of buffering without limit.
const FLOOD_PHP: &str = r#"<?php
$id   = $_GET['id'] ?? '';
$n    = (int)($_GET['n'] ?? 0);
$size = (int)($_GET['size'] ?? 1024);
$payload = str_repeat('x', $size);
$ok = 0;
$failed = 0;
for ($i = 0; $i < $n; $i++) {
    if (ephpm_ws_connection_send($id, $payload)) {
        $ok++;
    } else {
        $failed++;
    }
}
echo $ok . ':' . $failed;
"#;

/// The implicit form from an ordinary HTTP request must throw, not no-op.
const PUSH_IMPLICIT_PHP: &str = r#"<?php
try {
    ephpm_ws_send('this has no current connection');
    echo 'no-exception';
} catch (Throwable $e) {
    echo 'threw';
}
"#;

/// A vhost that is deliberately WebSocket-incapable: no entrypoint at all.
const INDEX_PHP: &str = "<?php echo 'index'; ?>";

/// Reads the `disconnect`-event counter out of this vhost's KV keyspace.
const DISCONNECTS_PHP: &str = "<?php echo (int)(ephpm_kv_get('ws:disconnects') ?? 0);";

fn sites_dir() -> PathBuf {
    PathBuf::from(required_env("EPHPM_SITES_DIR"))
}

/// Deploy both vhosts. Idempotent — the suite is serial, so every test can
/// call this and get the same state.
///
/// The actual filesystem work happens **once** per test binary. Re-deploying
/// per test is not merely wasted work — it republishes files the server may be
/// compiling at that instant, and this node runs with OPcache timestamp
/// validation OFF (the `serve`-mode default), so a bad compile is cached for
/// the life of the process. See [`write`].
async fn deploy_sites() {
    static DEPLOYED: std::sync::Once = std::sync::Once::new();
    DEPLOYED.call_once(|| {
        for host in [SITE_A, SITE_B] {
            let dir = sites_dir().join(host);
            std::fs::create_dir_all(&dir)
                .unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
            write(&dir, "websocket.php", WEBSOCKET_PHP);
            write(&dir, "push.php", PUSH_PHP);
            write(&dir, "flood.php", FLOOD_PHP);
            write(&dir, "push_implicit.php", PUSH_IMPLICIT_PHP);
            write(&dir, "index.php", INDEX_PHP);
            write(&dir, "disconnects.php", DISCONNECTS_PHP);
        }
    });
    // Lazy vhost discovery has a short negative-lookup cache; poll until both
    // hosts actually resolve rather than racing it.
    for host in [SITE_A, SITE_B] {
        wait_for_site(host).await;
    }
}

/// Publish a fixture file **atomically**: write a sibling temp file, then
/// `rename` it into place.
///
/// A plain `fs::write` truncates first, so there is a window in which the path
/// exists but is empty. That window is not theoretical here: connections leak
/// out of every test (the client just drops the socket), and the server
/// dispatches each one's `disconnect` event asynchronously — so PHP can be
/// compiling `websocket.php` at the exact moment the next test rewrites it.
///
/// With OPcache timestamp validation off (this node runs `ephpm serve`, where
/// that is the default), compiling the empty file caches an **inert script for
/// the rest of the process**: upgrades still succeed with 200, and no event
/// ever emits a frame again. That is precisely how this suite failed on a
/// loaded CI runner while passing locally — every later test that needed
/// `websocket.php` to send something timed out, while the one test that only
/// touches `push_implicit.php` kept passing.
///
/// `rename` is atomic within a filesystem, so a concurrent compile sees either
/// the old complete file or the new complete file — never an empty one.
fn write(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, body).unwrap_or_else(|e| panic!("write {}: {e}", tmp.display()));
    std::fs::rename(&tmp, &path)
        .unwrap_or_else(|e| panic!("rename {} -> {}: {e}", tmp.display(), path.display()));
}

async fn wait_for_site(host: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let resp = http_get(host, "/index.php").await;
        if resp.0 == 200 && resp.1 == "index" {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "vhost {host} never became discoverable");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// `(status, body)` for a plain HTTP request to one vhost.
async fn http_get(host: &str, path: &str) -> (u16, String) {
    let base = required_env("EPHPM_URL");
    let resp = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header("Host", host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} (Host: {host}) failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    (status, body)
}

/// `host:port` of the server under test, for a raw TCP connect.
fn server_addr() -> String {
    let base = required_env("EPHPM_URL");
    let rest = base.strip_prefix("http://").unwrap_or(&base);
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.contains(':') { authority.to_string() } else { format!("{authority}:80") }
}

/// Open a WebSocket to `host`'s entrypoint.
///
/// Connects the TCP socket to the real server address but does the handshake
/// with `Host: <host>` — the request names the vhost, the socket names the
/// machine. That is what makes two virtual hosts testable on one loopback
/// listener.
async fn connect(host: &str, query: &str) -> Result<Ws, u16> {
    let stream = TcpStream::connect(server_addr())
        .await
        .unwrap_or_else(|e| panic!("tcp connect to {}: {e}", server_addr()));
    let url = format!("ws://{host}/chat?{query}");
    match tokio_tungstenite::client_async(url, stream).await {
        Ok((ws, _resp)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("websocket handshake to {host} failed unexpectedly: {e}"),
    }
}

/// Open a WebSocket that is expected to succeed.
async fn connect_ok(host: &str, query: &str) -> Ws {
    connect(host, query)
        .await
        .unwrap_or_else(|status| panic!("expected an accepted upgrade, got HTTP {status}"))
}

/// Next data frame, skipping the server's keepalive ping/pong traffic.
async fn recv(ws: &mut Ws) -> String {
    let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("timed out waiting for a websocket frame")
            .expect("websocket stream ended while waiting for a frame")
            .expect("websocket read error");
        match next {
            Message::Text(text) => return text,
            Message::Binary(bytes) => return String::from_utf8_lossy(&bytes).into_owned(),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(frame) => panic!("connection closed while waiting for a frame: {frame:?}"),
        }
    }
}

/// Assert nothing arrives within `window`. Used for the negative cases, where
/// "no frame" is the whole assertion.
async fn expect_silence(ws: &mut Ws, window: Duration) {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            // Timed out: nothing arrived, which is what we wanted.
            Err(_) => return,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => {}
            Ok(other) => panic!("expected silence, got {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, text: &str) {
    ws.send(Message::Text(text.to_string())).await.expect("websocket write");
}

// ── tests ───────────────────────────────────────────────────────────────────

/// The basic loop: a frame in, one PHP execution, a frame out.
#[tokio::test]
async fn echo_round_trips_through_php() {
    deploy_sites().await;
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;

    send(&mut ws, "echo:hello").await;
    assert_eq!(recv(&mut ws).await, "echo:hello");

    // A second message on the same connection: each is its own execution, and
    // they stay in order.
    send(&mut ws, "echo:again").await;
    assert_eq!(recv(&mut ws).await, "echo:again");
}

/// `WS_CONNECTION_ID` reaches PHP, and looks like the documented capability
/// token rather than something guessable.
#[tokio::test]
async fn the_connection_id_is_exposed_and_unguessable_shaped() {
    deploy_sites().await;
    let mut first = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    let mut second = connect_ok(SITE_A, &format!("token={TOKEN}")).await;

    send(&mut first, "whoami").await;
    send(&mut second, "whoami").await;
    let id_a = recv(&mut first).await.strip_prefix("id:").expect("id: prefix").to_string();
    let id_b = recv(&mut second).await.strip_prefix("id:").expect("id: prefix").to_string();

    assert_eq!(id_a.len(), 32, "connection id should be 128 bits of hex: {id_a}");
    assert!(
        id_a.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "connection id should be lowercase hex: {id_a}"
    );
    assert_ne!(id_a, id_b);
    let shared = id_a.chars().zip(id_b.chars()).take_while(|(x, y)| x == y).count();
    assert!(shared < 8, "two ids share a {shared}-char prefix — not random: {id_a} / {id_b}");
}

/// `WS_OPCODE` distinguishes text from binary.
#[tokio::test]
async fn binary_frames_are_reported_as_binary() {
    deploy_sites().await;
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;

    send(&mut ws, "opcode").await;
    assert_eq!(recv(&mut ws).await, "opcode:text");

    ws.send(Message::Binary(b"opcode".to_vec())).await.expect("binary write");
    assert_eq!(recv(&mut ws).await, "opcode:binary");
}

/// Channels: two clients, one broadcasts, the other receives.
#[tokio::test]
async fn a_channel_broadcast_reaches_the_other_subscriber() {
    deploy_sites().await;
    let mut listener = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    let mut speaker = connect_ok(SITE_A, &format!("token={TOKEN}")).await;

    send(&mut listener, "join:room").await;
    assert_eq!(recv(&mut listener).await, "joined:1");
    send(&mut speaker, "join:room").await;
    assert_eq!(recv(&mut speaker).await, "joined:1");

    send(&mut speaker, "say:room:hello everyone").await;

    // The listener sees only the broadcast.
    assert_eq!(recv(&mut listener).await, "chat:hello everyone");
    // The speaker is a member too, so it sees the broadcast and then its own
    // receipt — the broadcast is queued before the reply, so the order is
    // deterministic.
    assert_eq!(recv(&mut speaker).await, "chat:hello everyone");
    assert_eq!(recv(&mut speaker).await, "sent:2");

    // Leaving stops delivery.
    send(&mut listener, "leave:room").await;
    assert_eq!(recv(&mut listener).await, "left:1");
    send(&mut speaker, "say:room:second").await;
    assert_eq!(recv(&mut speaker).await, "chat:second");
    assert_eq!(recv(&mut speaker).await, "sent:1");
    expect_silence(&mut listener, Duration::from_secs(2)).await;
}

/// Auth at `connect`: a non-2xx response refuses the upgrade outright, and the
/// script's own status reaches the client.
#[tokio::test]
async fn a_rejecting_connect_handler_refuses_the_upgrade() {
    deploy_sites().await;

    let status = connect(SITE_A, "token=wrong").await.expect_err("upgrade should be refused");
    assert_eq!(status, 403, "the connect handler's own status must reach the client");

    // And the same client with the right token still gets in — so the refusal
    // was the handler's decision, not a broken path.
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    send(&mut ws, "echo:ok").await;
    assert_eq!(recv(&mut ws).await, "echo:ok");
}

/// The headline feature: an ordinary HTTP request pushes a frame to a live
/// socket. This is the exact pattern in `guides/websockets.md` — `connect`
/// stashes the id in the embedded KV under an application identity, and a
/// later request looks it up and sends.
#[tokio::test]
async fn an_http_request_pushes_to_a_connected_socket() {
    deploy_sites().await;
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}&client=alice")).await;

    let (status, body) = http_get(SITE_A, "/push.php?client=alice&msg=you+have+mail").await;
    assert_eq!(status, 200);
    assert_eq!(body, "sent", "the HTTP handler should have found alice's connection");

    assert_eq!(recv(&mut ws).await, "you have mail");
}

/// The implicit form has no current connection during an HTTP request, and
/// must say so rather than silently doing nothing.
#[tokio::test]
async fn the_implicit_form_throws_from_an_http_request() {
    deploy_sites().await;

    let (status, body) = http_get(SITE_A, "/push_implicit.php").await;
    assert_eq!(status, 200);
    assert_eq!(body, "threw", "ephpm_ws_send() outside an event must throw, never no-op");
}

/// Multi-tenant isolation, end to end: site B holds site A's exact connection
/// id and still cannot reach it.
#[tokio::test]
async fn one_site_cannot_push_to_another_sites_connection() {
    deploy_sites().await;
    let mut victim = connect_ok(SITE_A, &format!("token={TOKEN}")).await;

    send(&mut victim, "whoami").await;
    let id = recv(&mut victim).await.strip_prefix("id:").expect("id: prefix").to_string();

    // Site B is handed the raw id — leaked, logged, guessed; it does not
    // matter how it got there.
    let (status, body) = http_get(SITE_B, &format!("/push.php?id={id}&msg=pwned")).await;
    assert_eq!(status, 200);
    assert_eq!(body, "not-sent", "site B must not be able to reach site A's connection");
    expect_silence(&mut victim, Duration::from_secs(2)).await;

    // ...and site A can, with the same id, so the failure above was the site
    // boundary and not a dead connection.
    let (status, body) = http_get(SITE_A, &format!("/push.php?id={id}&msg=legitimate")).await;
    assert_eq!(status, 200);
    assert_eq!(body, "sent");
    assert_eq!(recv(&mut victim).await, "legitimate");
}

/// Channels are site-scoped too: the same channel name on two vhosts is two
/// different channels.
#[tokio::test]
async fn identically_named_channels_do_not_bridge_sites() {
    deploy_sites().await;
    let mut on_a = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    let mut on_b = connect_ok(SITE_B, &format!("token={TOKEN}")).await;

    send(&mut on_a, "join:shared").await;
    assert_eq!(recv(&mut on_a).await, "joined:1");
    send(&mut on_b, "join:shared").await;
    assert_eq!(recv(&mut on_b).await, "joined:1");

    send(&mut on_b, "say:shared:from B").await;
    assert_eq!(recv(&mut on_b).await, "chat:from B");
    assert_eq!(recv(&mut on_b).await, "sent:1", "only B's own connection is in B's channel");
    expect_silence(&mut on_a, Duration::from_secs(2)).await;
}

/// `ephpm_ws_close()` closes the socket with the code userland asked for.
#[tokio::test]
async fn userland_can_close_a_connection_with_a_code() {
    deploy_sites().await;
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;

    send(&mut ws, "bye").await;
    let closed = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => return frame,
                Some(Ok(_)) => {}
                other => panic!("expected a close frame, got {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for the close frame");

    let frame = closed.expect("close frame should carry a code");
    assert_eq!(u16::from(frame.code), 4001);
}

/// The strict 404 rule: an upgrade to a vhost with no entrypoint is a 404 and
/// never falls through to `index.php`, the fallback chain, or a static file.
#[tokio::test]
async fn an_upgrade_to_a_vhost_without_an_entrypoint_is_404() {
    deploy_sites().await;
    let host = "ws-a.test";
    let entrypoint = sites_dir().join(host).join("websocket.php");

    // Same vhost, entrypoint removed. `index.php` is still there and would
    // happily answer a plain GET — the upgrade must not reach it.
    std::fs::remove_file(&entrypoint).expect("remove the entrypoint");
    // The router stats the entrypoint per upgrade, so no cache flush is
    // needed; but a plain GET must still work, proving the vhost is live.
    let (status, body) = http_get(host, "/index.php").await;
    assert_eq!((status, body.as_str()), (200, "index"));

    let status = connect(host, &format!("token={TOKEN}")).await.expect_err("should be refused");
    assert_eq!(status, 404, "an upgrade with no entrypoint must 404, not fall through");

    // Restore for whatever runs next (the suite is serial, so this is enough).
    write(&sites_dir().join(host), "websocket.php", WEBSOCKET_PHP);
    let mut ws = connect_ok(host, &format!("token={TOKEN}")).await;
    send(&mut ws, "echo:restored").await;
    assert_eq!(recv(&mut ws).await, "echo:restored");
}

/// A malformed handshake is routed to the WebSocket path (so it gets a
/// diagnostic 400) rather than falling through to the fallback chain.
#[tokio::test]
async fn a_wrong_websocket_version_gets_a_400_not_a_fallthrough() {
    deploy_sites().await;

    let base = required_env("EPHPM_URL");
    let resp = reqwest::Client::new()
        .get(format!("{base}/chat"))
        .header("Host", SITE_A)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "8")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status().as_u16(), 400);
    assert_eq!(
        resp.headers().get("sec-websocket-version").and_then(|v| v.to_str().ok()),
        Some("13"),
        "a 400 should advertise the version we do speak"
    );
}

/// Disconnect is dispatched: the entrypoint's `disconnect` branch bumps a KV
/// counter, and a later HTTP request can see it.
#[tokio::test]
async fn closing_a_socket_dispatches_the_disconnect_event() {
    deploy_sites().await;

    // `push.php` reports 404/no-connection for a missing key, which is a
    // convenient way to read KV without another fixture — so instead, count
    // disconnects by connecting, closing, and polling for the increment.
    let before = disconnect_count().await;

    {
        let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
        send(&mut ws, "echo:bye").await;
        assert_eq!(recv(&mut ws).await, "echo:bye");
        ws.close(None).await.expect("client close");
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if disconnect_count().await > before {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the disconnect event never ran (counter stayed at {before})"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Reads `ws:disconnects` out of the vhost's KV keyspace. The fixture is
/// deployed once with the rest (never rewritten inside the poll loop below —
/// republishing a script the server may be compiling is exactly the hazard
/// [`write`] documents).
async fn disconnect_count() -> i64 {
    let (status, body) = http_get(SITE_A, "/disconnects.php").await;
    assert_eq!(status, 200, "disconnects.php should serve: {body}");
    body.trim().parse().unwrap_or_else(|e| panic!("unparseable counter {body:?}: {e}"))
}

/// The bounded outbound queue, end to end: a consumer that stops reading is
/// shed with close code 1013 (`CLOSE_QUEUE_OVERFLOW`), and sends into the full
/// queue are *refused* — never buffered without limit.
///
/// This node runs `send_queue = 4` (see the websocket block in
/// `xtask/src/e2e_bare.rs`), so the server-side buffer for the connection is
/// four frames; everything beyond that plus whatever the kernel socket
/// buffers absorb must come back as a failed send. The flood is 1000 frames
/// of ~60 KB (≈ 60 MB) — far past any plausible socket buffering, so if every
/// send were accepted the server would demonstrably be buffering unboundedly.
#[tokio::test]
async fn a_slow_consumer_is_shed_with_1013_not_buffered() {
    deploy_sites().await;
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    send(&mut ws, "whoami").await;
    let id = recv(&mut ws).await.strip_prefix("id:").expect("id: prefix").to_string();

    // Stop reading `ws` and flood it from an ordinary HTTP request.
    const FRAMES: i64 = 1000;
    let (status, body) =
        http_get(SITE_A, &format!("/flood.php?id={id}&n={FRAMES}&size=60000")).await;
    assert_eq!(status, 200, "flood.php should serve: {body}");
    let (ok, failed) = body
        .trim()
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
        .unwrap_or_else(|| panic!("unparseable flood report {body:?}"));

    assert_eq!(ok + failed, FRAMES, "every send must be accounted for: {body}");
    assert!(
        failed >= 1,
        "with the client not reading, sends must start failing at the bound — \
         all {FRAMES} were accepted, which means the server buffered ~60 MB"
    );
    assert!(ok < FRAMES, "the queue bound must refuse part of the flood: {body}");

    // Now drain: whatever was already in flight arrives, then the shed close.
    // The overflow schedules a 1013 that beats the queued backlog (the session
    // task's select is biased towards the close), so the count of data frames
    // that still arrive must stay below what was accepted, let alone the flood.
    let mut received: i64 = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let close_code = loop {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("timed out waiting for the shed close frame")
            .expect("stream ended without a close frame");
        match next.expect("websocket read error while draining") {
            Message::Text(_) | Message::Binary(_) => received += 1,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(frame) => {
                break u16::from(frame.expect("close frame should carry a code").code);
            }
        }
    };
    assert_eq!(close_code, 1013, "queue overflow must shed with 1013 (try again later)");
    assert!(
        received < FRAMES,
        "client received {received} of {FRAMES} frames — nothing was dropped, so the \
         server must have buffered the whole flood"
    );

    // The shed was scoped to that connection: a fresh one works immediately.
    let mut fresh = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    send(&mut fresh, "echo:still-alive").await;
    assert_eq!(recv(&mut fresh).await, "echo:still-alive");
}

/// The other half of the bound, and the distinction that makes it coherent: a
/// payload over `[server.websocket] max_message_size` is refused *without*
/// shedding the connection.
///
/// An oversized frame is the calling script's bug, not back-pressure from a
/// slow peer — so it must not cost the client its socket. Asserted right next
/// to the 1013 test above because the two failure modes share one return
/// value (`false` from `ephpm_ws_connection_send`) and only the connection's
/// fate tells them apart.
#[tokio::test]
async fn an_oversized_payload_is_refused_without_shedding_the_connection() {
    deploy_sites().await;
    let mut ws = connect_ok(SITE_A, &format!("token={TOKEN}")).await;
    send(&mut ws, "whoami").await;
    let id = recv(&mut ws).await.strip_prefix("id:").expect("id: prefix").to_string();

    // The node runs `max_message_size = 65536`; 70000 is over it.
    let (status, body) = http_get(SITE_A, &format!("/flood.php?id={id}&n=1&size=70000")).await;
    assert_eq!(status, 200, "flood.php should serve: {body}");
    assert_eq!(body.trim(), "0:1", "an over-cap payload must be refused, not queued");

    // Nothing was delivered, and — the point — the connection is still alive.
    expect_silence(&mut ws, Duration::from_secs(2)).await;
    send(&mut ws, "echo:survived").await;
    assert_eq!(recv(&mut ws).await, "echo:survived");
}
