//! Request-teardown regression tests (v0.11.3).
//!
//! ePHPm used to run `php_request_shutdown()` lazily, at the top of the same
//! PHP thread's NEXT request, instead of when the request finished. Everything
//! PHP does at request end waited for that next request:
//!
//! - **Sessions hung.** The session module writes the session and releases its
//!   lock in `RSHUTDOWN`. Under the default multi-thread pool a second request
//!   on the same session landed on another thread and blocked on the lock the
//!   idle first thread still held — forever with the files handler (here: the
//!   node's 5s request timeout, a 504), up to the lock TTL with the ephpm KV
//!   handler.
//! - **Global-object destructors** (and extension `RSHUTDOWN`, Xdebug's debugger
//!   session end) ran only when some unrelated request reached that thread.
//!
//! Both fixtures deliberately rely on implicit end-of-request behaviour (no
//! `session_write_close()`, no `unset()`), which is exactly what most
//! applications do.
//!
//! Runs against the long-lived shared node, whose pool size is derived from the
//! host (always >= 2 threads), which is what makes the session hang reachable.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://127.0.0.1:18080`)

use std::io::Read;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use ephpm_e2e::required_env;

fn client() -> reqwest::Client {
    // Comfortably above the node's 5s request timeout, so a hung session shows
    // up as the server's 504 rather than a client-side timeout.
    reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("reqwest client")
}

/// Extract the `PHPSESSID=<sid>` pair from a response's `Set-Cookie` headers.
fn session_cookie(resp: &reqwest::Response) -> String {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("PHPSESSID="))
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned())
        .expect("the first response must set a PHPSESSID cookie")
}

/// The fixture, deployable into a vhost (see [`session_host`]).
const SESSION_AUTOCLOSE_PHP: &str = include_str!("../../../tests/docroot/session_autoclose.php");

/// Vhost the `ephpm` (KV-backed) session case runs under on a multi-tenant node.
const SESSION_TENANT: &str = "teardown-session.test";

/// The `Host` to send for `handler`, deploying the fixture into a vhost first
/// when one is needed.
///
/// On a multi-tenant node (`EPHPM_SITES_DIR` set — the bare-process shared node
/// is one) a request whose `Host` matches no vhost has no KV keyspace, so the
/// KV-backed `ephpm` session handler cannot work on the default document root.
/// That case therefore runs under a real vhost. The `files` handler needs no
/// KV and keeps running on the default document root.
fn session_host(handler: &str) -> Option<&'static str> {
    if handler != "ephpm" {
        return None;
    }
    let sites = std::env::var("EPHPM_SITES_DIR").ok()?;
    let dir = std::path::Path::new(&sites).join(SESSION_TENANT);
    std::fs::create_dir_all(&dir).expect("create session site dir");
    std::fs::write(dir.join("session_autoclose.php"), SESSION_AUTOCLOSE_PHP)
        .expect("write session fixture");
    Some(SESSION_TENANT)
}

/// Sequential requests on one session (never explicitly closed) must each
/// succeed and see the previous request's write.
async fn sequential_session_requests(handler: &str) {
    let base_url = required_env("EPHPM_URL");
    let client = client();
    let url = format!("{base_url}/session_autoclose.php?handler={handler}");
    let host = session_host(handler);
    let get = |cookie: Option<&str>| {
        let mut req = client.get(&url);
        if let Some(host) = host {
            req = req.header(reqwest::header::HOST, host);
        }
        if let Some(cookie) = cookie {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        req.send()
    };

    let first = get(None).await.expect("request 1 failed");
    assert_eq!(first.status().as_u16(), 200, "[{handler}] request 1 status");
    let cookie = session_cookie(&first);
    assert_eq!(first.text().await.expect("body 1").trim(), "1", "[{handler}] request 1");

    // More requests than the smallest derived pool has threads, so at least
    // one of them is guaranteed to land on a thread other than the one that
    // served its predecessor.
    for n in 2..=5 {
        let started = Instant::now();
        let resp = get(Some(&cookie))
            .await
            .unwrap_or_else(|e| panic!("[{handler}] request {n} failed: {e}"));
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        assert_eq!(
            status,
            200,
            "[{handler}] request {n} returned {status} after {:?} (body: {body:?}) — the \
             previous request's session lock was not released when that request ended",
            started.elapsed()
        );
        assert_eq!(
            body.trim(),
            n.to_string(),
            "[{handler}] request {n} must see every previous request's session write"
        );
    }

    if let (Some(host), Ok(sites)) = (host, std::env::var("EPHPM_SITES_DIR")) {
        let _ = std::fs::remove_dir_all(std::path::Path::new(&sites).join(host));
    }
}

#[tokio::test]
async fn files_session_is_released_when_the_request_ends() {
    sequential_session_requests("files").await;
}

#[tokio::test]
async fn ephpm_session_is_released_when_the_request_ends() {
    sequential_session_requests("ephpm").await;
}

const TENANT_A: &str = "teardown-a.test";
const TENANT_B: &str = "teardown-b.test";

/// Site A's page: a global object whose destructor writes a KV key.
const DESTRUCT_KV_PHP: &str = r#"<?php
final class TeardownKv
{
    public function __construct(private string $id) {}
    public function __destruct()
    {
        ephpm_kv_set('teardown-scope:' . $this->id, 'written-by-a');
    }
}
$keep = new TeardownKv($_GET['id'] ?? '');
echo "ok\n";
"#;

/// Deployed in both sites: report whether the key exists in THIS site's KV
/// namespace.
const KV_READ_PHP: &str = r#"<?php
echo var_export(ephpm_kv_get('teardown-scope:' . ($_GET['id'] ?? '')), true), "\n";
"#;

async fn get_with_host(client: &reqwest::Client, base_url: &str, host: &str, path: &str) -> String {
    let resp = client
        .get(format!("{base_url}{path}"))
        .header(reqwest::header::HOST, host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} (Host: {host}) failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "GET {path} (Host: {host}) returned {status}: {body:?}");
    body.trim().to_owned()
}

/// A tenant's end-of-request work runs in ITS OWN site scope.
///
/// Before v0.11.3 the deferred teardown of tenant A's request ran at the top
/// of the next request on the same PHP thread — after that request had already
/// installed ITS site scope (KV keyspace, database, WebSocket scope). A's global
/// destructor therefore wrote into whichever tenant reached the thread next.
/// Here A's destructor writes a KV key; it must land in A's namespace and never
/// in B's, however many of B's requests go through the pool afterwards.
#[tokio::test]
async fn global_destructor_writes_into_its_own_tenant_scope() {
    let base_url = required_env("EPHPM_URL");
    // Needs a multi-tenant node (the bare-process shared node is one). The
    // k8s lane provides no writable sites dir, so skip there.
    let Ok(sites) = std::env::var("EPHPM_SITES_DIR") else {
        eprintln!("skipping: EPHPM_SITES_DIR not set (no multi-tenant node)");
        return;
    };
    let sites = std::path::PathBuf::from(sites);
    for (host, files) in [
        (TENANT_A, &[("destruct_kv.php", DESTRUCT_KV_PHP), ("kv_read.php", KV_READ_PHP)][..]),
        (TENANT_B, &[("kv_read.php", KV_READ_PHP)][..]),
    ] {
        let dir = sites.join(host);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create site dir");
        for (name, body) in files {
            std::fs::write(dir.join(name), body).expect("write site script");
        }
    }

    let client = client();
    let id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let read = format!("/kv_read.php?id={id}");

    assert_eq!(
        get_with_host(&client, &base_url, TENANT_A, &format!("/destruct_kv.php?id={id}")).await,
        "ok"
    );

    // Push more of B's requests through the pool than it has threads (it is
    // clamped to at most 32), so one of them is guaranteed to reach the thread
    // that served A. Under the deferred teardown, A's destructor ran at the
    // top of that request — in B's scope.
    for n in 0..40 {
        let in_b = get_with_host(&client, &base_url, TENANT_B, &read).await;
        assert_eq!(
            in_b, "NULL",
            "tenant A's destructor wrote into tenant B's KV namespace (B request {n}) — \
             A's request teardown ran under B's site scope"
        );
    }

    // The teardown runs just after A's response; allow it a moment.
    let deadline = Instant::now() + Duration::from_secs(5);
    let in_a = loop {
        let v = get_with_host(&client, &base_url, TENANT_A, &read).await;
        if v != "NULL" || Instant::now() >= deadline {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        in_a, "'written-by-a'",
        "tenant A's destructor must write into A's own KV namespace"
    );

    for host in [TENANT_A, TENANT_B] {
        let _ = std::fs::remove_dir_all(sites.join(host));
    }
}

/// A global object's `__destruct` runs right after the response — with no
/// follow-up request — observed over a loopback connection the destructor
/// opens back to this test.
#[tokio::test]
async fn global_destructor_runs_without_a_follow_up_request() {
    let base_url = required_env("EPHPM_URL");
    // The destructor dials 127.0.0.1 on the SERVER's host. That reaches this
    // listener only when the server runs on this host (the bare-process
    // harness); behind a cluster Service URL it cannot, so skip there.
    let host = reqwest::Url::parse(&base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        eprintln!("skipping: EPHPM_URL host {host:?} is not loopback (server not on this host)");
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("listener addr").port();

    let resp = client()
        .get(format!("{base_url}/destruct_notify.php?port={port}"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.expect("body").trim(), "ok");

    // No further request is sent. The destructor must connect on its own,
    // shortly after the response.
    let received = tokio::task::spawn_blocking(move || {
        listener.set_nonblocking(true).expect("nonblocking listener");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking stream");
                    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
                    let mut buf = String::new();
                    let _ = stream.read_to_string(&mut buf);
                    return Some(buf);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    })
    .await
    .expect("accept task");

    assert_eq!(
        received.as_deref().map(str::trim),
        Some("destructed"),
        "the global object's destructor did not run within 5s of the response — request \
         teardown is being deferred to the thread's next request"
    );
}
