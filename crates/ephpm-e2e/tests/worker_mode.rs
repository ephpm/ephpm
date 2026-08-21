//! Worker-mode (persistent-worker engine) Phase-1 acceptance tests.
//!
//! These exercise the Phase-1 exit criteria from `worker-mode-design.md` §9
//! against a running ePHPm instance started in worker mode
//! (`[php] mode = "worker"`, `worker_script = "worker.php"`), serving the
//! reference `examples/worker/worker.php`.
//!
//! Because worker mode is a whole-server switch, this needs a SEPARATE server
//! instance from the default fpm docroot the other e2e tests use. The harness
//! provides its base URL via `EPHPM_WORKER_URL`. `cargo xtask e2e` spawns that
//! node (`tests/worker-docroot`, `[php] mode = "worker"`, 3 workers) and
//! exports the variable — see `xtask/src/e2e_bare.rs`. Tests still self-skip
//! when it is unset so the suite can be run standalone against an existing
//! deployment.
//!
//! Exit criteria covered:
//! - boot-once: a boot counter that increments once per worker, not per request
//! - concurrency: N workers serve N concurrent requests on Linux (ZTS)
//! - fatal -> 500 + recycle + next request succeeds + server never wedges
//! - a `zend_bailout` mid-request never delivers the partial output it had
//!   already produced, and — when the response headers are already on the wire
//!   (`send_response_stream`) — the body is deliberately broken rather than
//!   completed, so the client cannot read a truncated download as a success
//! - worker_max_requests recycle
//! - issue #116: a `do_blocks()`-shaped nested render never recycles a worker,
//!   and renders byte-identically on every request of a worker's life; the same
//!   render taken past the C-stack ceiling costs at most that one worker (500 +
//!   recycle) instead of the process, and is catchable in userland
//! - `exit()` mid-request delivers the request's output and leaves the server
//!   serving
//!
//! The reference worker.php emits, per request:
//!   hello <REQUEST_URI> (boot #<B>, request #<R>)
//! where B is the per-worker boot number and R the per-worker request count.

use std::collections::HashSet;

/// Base URL of the worker-mode ePHPm instance, or `None` to skip.
fn worker_url() -> Option<String> {
    std::env::var("EPHPM_WORKER_URL").ok().filter(|s| !s.is_empty())
}

/// Parse the "boot #B" number out of a reference-script response body.
fn parse_boot(body: &str) -> Option<u32> {
    let start = body.find("boot #")? + "boot #".len();
    let rest = &body[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

async fn get(base: &str, path: &str) -> (u16, String) {
    let url = format!("{base}{path}");
    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    (status, body)
}

/// Boot-once: the framework boots exactly once per worker. Across many
/// sequential requests, the set of distinct "boot #B" values must stay small
/// (bounded by the worker count) and never grow per request — proving zero
/// per-request bootstrap.
#[tokio::test]
async fn boot_happens_once_per_worker_not_per_request() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode boot-once test");
        return;
    };

    let mut boots: HashSet<u32> = HashSet::new();
    for i in 0..50 {
        let (status, body) = get(&base, &format!("/hello-{i}")).await;
        assert_eq!(status, 200, "request {i} must be 200, got {status}: {body}");
        assert!(body.contains("hello /hello-"), "unexpected body: {body}");
        let boot = parse_boot(&body).unwrap_or_else(|| panic!("no boot counter in body: {body}"));
        boots.insert(boot);
    }

    // If ePHPm re-bootstrapped per request, boot would climb every request and
    // we'd see ~50 distinct values. Boot-once means it is bounded by the worker
    // count (a handful), and no single worker's boot exceeds a small number.
    assert!(
        boots.len() <= 32,
        "boot counter looks per-request, not per-worker: {} distinct boot ids",
        boots.len()
    );
    let max_boot = boots.iter().copied().max().unwrap_or(0);
    assert!(max_boot <= 32, "a worker booted {max_boot} times — recycling storm?");
}

/// Concurrency: fire many requests at once; all succeed and the server stays
/// responsive (never wedges). On ZTS this proves multiple workers serve in
/// parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_requests_all_succeed() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode concurrency test");
        return;
    };

    const N: usize = 60;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let base = base.clone();
            tokio::spawn(async move { get(&base, &format!("/c-{i}")).await })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        let (status, body) = h.await.unwrap_or_else(|e| panic!("request {i} panicked: {e}"));
        assert_eq!(status, 200, "concurrent request {i} failed ({status}): {body}");
        assert!(body.starts_with("hello /c-"), "unexpected body for {i}: {body}");
    }
}

/// Fatal fault tolerance (the marquee test): a request that triggers a PHP
/// fatal returns 500, the worker recycles, and the NEXT request succeeds — the
/// server never wedges.
///
/// The trigger is a query flag the worker script must honor. The harness's
/// fixture (`tests/worker-docroot/worker.php`) routes `?__fatal=1` to an
/// uncaught `Error`; a deployment whose script ignores the flag still gets the
/// "server keeps serving" half of this test.
#[tokio::test]
async fn fatal_500s_then_recovers() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode fatal-recovery test");
        return;
    };

    // Trigger a fatal (best-effort: depends on the deployed worker script
    // honoring a `?__fatal=1` query). Accept either a 500 (fatal handled) or a
    // 200 (script ignored the trigger) — but in NO case may the request hang.
    let (fatal_status, fatal_body) = get(&base, "/trigger?__fatal=1").await;
    assert!(
        fatal_status == 500 || fatal_status == 200,
        "fatal trigger returned unexpected status {fatal_status}"
    );
    // If the worker script honored the trigger, the request DIED — a response
    // carrying PHP's fatal-error text must never be a 200. Worker mode used to
    // ship exactly that (the synthesized response kept the default 200), so
    // caches and uptime monitors saw a crashed request as healthy.
    if fatal_body.contains("Fatal error") {
        assert_eq!(
            fatal_status, 500,
            "a request that ended on a PHP fatal returned {fatal_status}, not 500: {fatal_body}"
        );
    }

    // The server must still serve normal requests afterwards.
    for i in 0..10 {
        let (status, body) = get(&base, &format!("/after-fatal-{i}")).await;
        assert_eq!(status, 200, "request after fatal wedged the server: {status} {body}");
        assert!(body.contains("hello /after-fatal-"), "post-fatal body wrong: {body}");
    }
}

/// worker_max_requests recycle: over enough requests a worker crosses its
/// recycle threshold and reboots. We can observe this indirectly: the
/// per-worker "request #R" counter resets after a boot, so across a long run
/// we must see R values reset (not grow monotonically forever), and the boot
/// id set must grow (new boots) — but stay bounded per unit time.
/// A worker request killed by a real `zend_bailout` must not have its partial
/// output delivered.
///
/// `?__bailout=1` echoes a marker and then exhausts the memory limit. The
/// bailout is absorbed by `php_execute_script`'s own `zend_try`, so the worker
/// loop returns with the request still in flight and the capture buffers
/// holding a truncated prefix. The engine used to synthesize a response out of
/// those buffers; for a bare bailout that produced a **200** carrying half a
/// document, and even for this memory case it shipped the partial body.
///
/// The marker assertion is the substantive one and is unconditional — no
/// deployment of any worker script may return output from a request that died.
#[tokio::test]
async fn bailout_never_delivers_partial_output() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode bailout test");
        return;
    };

    let (status, body) = get(&base, "/trigger?__bailout=1").await;

    assert!(
        !body.contains("WORKER-BAILOUT-PARTIAL-OUTPUT"),
        "output echoed before the bailout was delivered to the client \
         ({status}): {body}"
    );
    assert!(
        !body.contains("WORKER-BAILOUT-UNREACHABLE"),
        "the script never got this far ({status}): {body}"
    );

    // A deployment whose worker script ignores the flag answers with the
    // reference "hello" body; that is not this test's subject, so only the
    // marker assertions above apply to it.
    if body.contains("hello /trigger") {
        eprintln!("deployed worker script ignores ?__bailout — status assertion skipped");
    } else {
        assert_eq!(
            status, 500,
            "a request that died in a bailout must be a 500, got {status}: {body}"
        );
    }

    // ...and the pool recovers: the worker is recycled, not wedged.
    for i in 0..10 {
        let (status, body) = get(&base, &format!("/after-bailout-{i}")).await;
        assert_eq!(status, 200, "request after bailout wedged the server: {status} {body}");
    }
}

/// The case where a 500 is no longer available: the worker dies **after**
/// `send_response_stream` has already put `200 OK` and the headers on the wire.
///
/// A status line cannot be retracted, so the only honest signal left is the
/// framing. ePHPm ends the body with an error instead of closing the channel,
/// which under `Transfer-Encoding: chunked` means the terminating `0` chunk is
/// never written — the client's transfer fails. Closing the channel cleanly
/// (what it did before) is byte-for-byte a *successful* truncated download.
///
/// `?__stream_bailout=1` faults inside a userland stream wrapper's second
/// `stream_read()`, i.e. inside the C pump loop, one chunk into the body.
#[tokio::test]
async fn streamed_response_is_broken_not_completed_when_worker_bails() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode stream-bailout test");
        return;
    };

    let url = format!("{base}/s?__stream_bailout=1");
    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    // A deployment whose worker script ignores the flag answers with the
    // reference body; nothing to assert about framing then.
    if resp.status().as_u16() == 200 && !resp.headers().contains_key("transfer-encoding") {
        eprintln!("deployed worker script ignores ?__stream_bailout — skipping");
        return;
    }

    // Reading the body must FAIL. The bytes that did arrive are fine — it is
    // the clean end-of-body that must not happen.
    match resp.bytes().await {
        Ok(body) => panic!(
            "a streamed response whose worker died completed successfully with {} bytes — \
             the client cannot tell this from a finished download",
            body.len()
        ),
        Err(e) => {
            assert!(e.is_body() || e.is_decode(), "expected a body/transfer error, got: {e}");
        }
    }

    // ...and the pool recovers.
    for i in 0..10 {
        let (status, body) = get(&base, &format!("/after-stream-bailout-{i}")).await;
        assert_eq!(status, 200, "request after stream bailout wedged the server: {status} {body}");
    }
}

#[tokio::test]
async fn requests_keep_succeeding_across_recycles() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode recycle test");
        return;
    };

    // Enough requests to cross a lowered worker_max_requests if the harness
    // set one (default is 10000, so this loop alone does not recycle);
    // regardless, every request must be a clean 200.
    for i in 0..200 {
        let (status, body) = get(&base, &format!("/r-{i}")).await;
        assert_eq!(status, 200, "request {i} failed across recycles ({status}): {body}");
        assert!(body.contains("hello /r-"), "recycle-run body wrong at {i}: {body}");
    }
}

/// Extract a `key=value` field from the fixture's blocks trailer comment
/// (`<!-- blocks depth=3 len=… sha1=… boot=… request=… -->`).
fn trailer_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let start = body.rfind(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find(|c: char| c == ' ' || c == '\n').unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Total `ephpm_worker_recycles_total` across every `reason` label, scraped
/// from `/metrics`.
///
/// This — not the worker script's own `boot #N` counter — is the reliable
/// "did a worker die?" signal: under ZTS each worker thread gets its own PHP
/// globals, so a per-worker `static $bootCount` reads 1 on every worker AND on
/// every respawned replacement. Boot ids cannot tell a recycle from a sibling.
async fn worker_recycles(base: &str) -> f64 {
    let (status, body) = get(base, "/metrics").await;
    assert_eq!(status, 200, "/metrics not available on the worker node: {body}");
    body.lines()
        // Skip the `# HELP` / `# TYPE` lines, which also carry the metric name.
        .filter(|l| l.starts_with("ephpm_worker_recycles_total"))
        .filter_map(|l| l.rsplit(' ').next())
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .sum()
}

/// Issue #116 regression: a nested, output-buffered, recursive render — the
/// shape WordPress' `do_blocks()` produces — must not take the resident worker
/// down, and must render identically on every request of a worker's life.
///
/// The fixture route (`?__blocks=N`, `tests/worker-docroot/worker.php`)
/// deliberately combines the three engine behaviours #116 implicated:
/// userland recursion re-entering through internal functions
/// (`preg_replace_callback`/`array_map`), a userland output buffer opened and
/// closed at every nesting level, and a response big enough to grow the SAPI
/// capture buffer through several reallocs — repeated on a worker that never
/// runs `php_request_shutdown` between requests.
///
/// What this proves: the engine survives that workload and produces stable
/// output across a worker's whole life. What it does NOT prove: that a real
/// WordPress block theme renders correctly — WordPress' own per-request
/// registries (`$wp_styles`/`$wp_scripts`, `WP_Style_Engine_CSS_Rules_Store`)
/// are worker-lifetime state that only a framework adapter can reset, and no
/// black-box test in this repo exercises them.
#[tokio::test]
async fn nested_block_render_never_recycles_the_worker() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode block-render test");
        return;
    };

    // Warm every worker, then take the recycle baseline. Other tests in this
    // suite deliberately kill workers (fatal, exit), so the baseline must be
    // read here rather than assumed to be zero.
    for i in 0..30 {
        let (status, body) = get(&base, &format!("/warm-{i}")).await;
        assert_eq!(status, 200, "warmup request {i} failed: {body}");
    }
    let recycles_before = worker_recycles(&base).await;

    // Render the nested workload many times over. A worker that dies mid-render
    // shows up two ways: a non-200 (or a hang) here, or a bump in the recycle
    // counter checked below.
    let mut digest: Option<String> = None;
    for i in 0..120 {
        let (status, body) = get(&base, "/blocks?__blocks=4").await;
        assert_eq!(status, 200, "block render {i} returned {status}: {body}");

        let sha = trailer_field(&body, "sha1")
            .unwrap_or_else(|| panic!("block render {i} produced no sha1 trailer: {body}"))
            .to_string();
        let len: usize = trailer_field(&body, "len")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("block render {i} produced no len trailer"));
        // Guards against the workload being silently defanged (a fixture edit
        // that drops the recursion depth would make the test pass vacuously).
        assert!(len > 10_000, "block render {i} produced only {len} bytes — workload too small");

        match &digest {
            None => digest = Some(sha),
            Some(first) => assert_eq!(
                first, &sha,
                "block render {i} differs from the first render — per-request state leaked \
                 into the renderer"
            ),
        }
    }

    // The render must not have cost a single worker.
    let recycles_after = worker_recycles(&base).await;
    assert!(
        recycles_after <= recycles_before,
        "the nested block render recycled {} worker(s) (issue #116)",
        recycles_after - recycles_before
    );

    // And the pool must still be serving normally.
    for i in 0..30 {
        let (status, body) = get(&base, &format!("/after-blocks-{i}")).await;
        assert_eq!(status, 200, "post-render request {i} failed: {body}");
        assert!(body.contains("hello /after-blocks-"), "post-render body wrong: {body}");
    }
}

/// Issue #116, the crash half: a render nested past the C-stack ceiling must
/// cost at most the worker that ran it — never the process.
///
/// `nested_block_render_never_recycles_the_worker` above proves a *reasonable*
/// nesting depth is fine. This proves the cliff on the other side of it is a
/// cliff and not a crater. ePHPm used to override PHP's `zend_call_stack_init()`
/// with a no-op on Linux, leaving `EG(stack_limit)` NULL and PHP's C-stack
/// overflow guard permanently off; a deep enough `do_blocks()`-shaped render
/// then SIGSEGV'd the worker thread and aborted the whole server. With the guard
/// restored, PHP raises `Error: Maximum call stack size ... reached` — an
/// ordinary uncatchable-at-engine-level fatal, so: 500, worker recycled, pool
/// still serving.
///
/// A regression shows up as a transport error, not a status mismatch: there is
/// no server left to answer. The assertion messages say so, because a bare
/// "connection refused" in CI is otherwise very hard to attribute.
#[tokio::test]
async fn deep_recursion_costs_a_worker_not_the_process() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode deep-recursion test");
        return;
    };

    // Prove the pool is healthy first, so a failure below cannot be blamed on a
    // node that never came up.
    let (status, body) = get(&base, "/pre-deep").await;
    assert_eq!(status, 200, "the worker pool must be healthy before the overflow: {body}");

    let url = format!("{base}/deep?__deep=60000");
    let resp = reqwest::get(&url).await.unwrap_or_else(|e| {
        panic!(
            "GET {url} failed: {e} — a transport error here means the SERVER DIED. \
             PHP's C-stack guard is disabled again, so the overflow was a SIGSEGV \
             rather than a fatal (issue #116)"
        )
    });
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status, 500,
        "an overflowing render must be answered 500, not 200 and not a dropped \
         connection; body: {body}"
    );
    assert!(
        !body.contains("deep-survived"),
        "the fixture must actually exhaust the stack — if it completed, the depth no \
         longer proves anything; body: {body}"
    );

    // The whole point: the pool is still there and still serving.
    for i in 0..10 {
        let (status, body) = get(&base, &format!("/post-deep-{i}")).await;
        assert_eq!(
            status, 200,
            "request {i} after the overflow must succeed — the pool has to replace the \
             worker that died, not lose the process: {body}"
        );
        assert!(body.contains("hello /post-deep-"), "post-overflow body wrong: {body}");
    }
}

/// The overflow arrives as a *catchable* `Error`, so a worker that handles it
/// keeps serving without a recycle at all.
///
/// This is the strongest statement of the fix and the one that maps directly to
/// issue #116's acceptance criterion ("the worker survives and serves the next
/// request"): the fault is back inside PHP's own error model, where an adapter
/// can turn a runaway template into a 500 page and carry on. Nothing in ePHPm's
/// engine can offer that for a real SIGSEGV.
#[tokio::test]
async fn a_caught_overflow_leaves_the_worker_booted() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode caught-overflow test");
        return;
    };

    let recycles_before = worker_recycles(&base).await;

    let (status, body) = get(&base, "/deep-caught?__deep=60000&catch=1").await;
    assert_eq!(status, 200, "a caught overflow is an ordinary handled request: {body}");
    assert!(
        body.contains("deep-caught Error:"),
        "the overflow must surface as a catchable Error (PHP's `Maximum call stack \
         size ... reached`), not as a crash; body: {body}"
    );
    assert!(
        body.contains("Maximum call stack size"),
        "the Error must be PHP's stack-limit one, not some other failure that happens \
         to be catchable; body: {body}"
    );

    // Catching it means the worker never left its loop, so nothing recycled.
    let recycles_after = worker_recycles(&base).await;
    assert!(
        recycles_after <= recycles_before,
        "catching the overflow still cost {} worker(s) — the Error escaped the handler",
        recycles_after - recycles_before
    );
}

/// `exit()` mid-request must deliver that request's output — including bytes
/// still sitting in a userland output buffer, which worker mode has no
/// per-request RSHUTDOWN to flush — and must leave the server serving.
///
/// This is the second finding on issue #116. It pins the CURRENT contract:
/// the response is synthesized from SAPI state and the pool recovers. It does
/// not assert that the worker itself survives — today an `exit()` ends the
/// worker's loop and the pool respawns it.
#[tokio::test]
async fn exit_mid_request_delivers_output_and_server_keeps_serving() {
    let Some(base) = worker_url() else {
        eprintln!("EPHPM_WORKER_URL unset — skipping worker-mode exit test");
        return;
    };

    for round in 0..5 {
        let (status, body) = get(&base, "/exit-route?__exit=1").await;
        assert_eq!(status, 200, "exit round {round} returned {status}: {body}");
        assert!(
            body.contains("exit-route echoed"),
            "exit round {round} lost the echoed output: {body}"
        );
        assert!(
            body.contains("exit-route buffered"),
            "exit round {round} lost output still held in a userland buffer — the engine did \
             not flush it before synthesizing the response: {body}"
        );

        // The pool must keep serving immediately afterwards.
        for i in 0..5 {
            let (status, body) = get(&base, &format!("/after-exit-{round}-{i}")).await;
            assert_eq!(status, 200, "request after exit wedged the server: {status} {body}");
            assert!(body.contains("hello /after-exit-"), "post-exit body wrong: {body}");
        }
    }
}

/// Streaming round-trip (Phase 3): upload a large body and get the same bytes
/// back. Proves `Envelope::bodyStream()` + `send_response_stream()` move the
/// body without the worker buffering it whole. Requires a worker docroot whose
/// script echoes the streamed request body (e.g. `examples/worker/worker-stream.php`);
/// opt in via `EPHPM_WORKER_STREAM_URL` (separate instance from the hello
/// worker). Self-skips when unset.
///
/// Verifies correctness (byte-for-byte echo) here; the flat-memory property is
/// checked out-of-band (worker RSS during a multi-hundred-MB upload) since a
/// black-box HTTP test cannot observe worker memory.
#[tokio::test]
async fn streaming_upload_echoes_back_identically() {
    let Some(base) = std::env::var("EPHPM_WORKER_STREAM_URL").ok().filter(|s| !s.is_empty()) else {
        eprintln!("EPHPM_WORKER_STREAM_URL unset — skipping worker-mode streaming test");
        return;
    };

    // A body comfortably above a small stream threshold, with a non-trivial
    // pattern so a truncation or reorder is caught.
    let payload: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/echo"))
        .body(payload.clone())
        .send()
        .await
        .expect("streaming upload failed");
    assert_eq!(resp.status().as_u16(), 200, "streaming echo not 200");

    let echoed = resp.bytes().await.expect("read echoed body").to_vec();
    assert_eq!(echoed.len(), payload.len(), "echoed length differs from upload");
    assert!(echoed == payload, "echoed body does not match the uploaded body");
}
