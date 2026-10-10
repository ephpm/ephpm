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

/// Sequential requests on one session (never explicitly closed) must each
/// succeed and see the previous request's write.
async fn sequential_session_requests(handler: &str) {
    let base_url = required_env("EPHPM_URL");
    let client = client();
    let url = format!("{base_url}/session_autoclose.php?handler={handler}");

    let first = client.get(&url).send().await.expect("request 1 failed");
    assert_eq!(first.status().as_u16(), 200, "[{handler}] request 1 status");
    let cookie = session_cookie(&first);
    assert_eq!(first.text().await.expect("body 1").trim(), "1", "[{handler}] request 1");

    // More requests than the smallest derived pool has threads, so at least
    // one of them is guaranteed to land on a thread other than the one that
    // served its predecessor.
    for n in 2..=5 {
        let started = Instant::now();
        let resp = client
            .get(&url)
            .header(reqwest::header::COOKIE, &cookie)
            .send()
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
}

#[tokio::test]
async fn files_session_is_released_when_the_request_ends() {
    sequential_session_requests("files").await;
}

#[tokio::test]
async fn ephpm_session_is_released_when_the_request_ends() {
    sequential_session_requests("ephpm").await;
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
