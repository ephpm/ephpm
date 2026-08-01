//! Path-traversal tests driven over a **raw TCP socket**.
//!
//! # Why this suite exists at all
//!
//! `GET /../b/index.php` used to join onto `<docroot>/../b/index.php` and
//! *execute* it — cross-tenant PHP execution under `sites_dir` vhosting. The
//! fix lives in `ephpm-server`'s router: `percent_decode_path()` rejects any
//! `..` segment (after decoding, and treating `\` as a separator too), and
//! `Router::php_script_contained()` re-checks the resolved script against the
//! canonicalized site root.
//!
//! Every other E2E suite drives the server with `reqwest`, which normalizes
//! `../` away **client-side** before the bytes ever hit the wire — see the
//! note in `hidden_files.rs::dot_dot_not_treated_as_hidden`. That means an
//! ordinary HTTP client *structurally cannot express the attack*, and the
//! existing suite could never catch a regression here.
//!
//! So this file writes the request line itself onto a [`TcpStream`]. Nothing
//! between the test and hyper touches the path.
//!
//! # How this suite avoids passing vacuously
//!
//! A traversal test trivially "passes" if the server is down, the URL is
//! wrong, or the target simply does not exist (404 is not 200). Three
//! guardrails:
//!
//! 1. Every attack test first calls [`probe_control`], which asserts that
//!    `GET /index.php` over the same raw socket returns **200** with PHP-
//!    generated output. A dead or misconfigured server fails there, loudly,
//!    before any traversal assertion runs.
//! 2. The attack assertions require **exactly 400**, not merely "not 200".
//!    404 (target missing), 403 (blocked later, by the second layer) and 500
//!    (`open_basedir` refusing the include) all fail. Only an active reject by
//!    `percent_decode_path` passes.
//! 3. [`dot_dot_back_into_document_root_rejected`] attacks a path whose target
//!    is `index.php` itself — the very file the control just executed. It
//!    cannot be 400 for want of a target.
//!
//! # Environment variables
//!
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`).
//!   Set by both the bare-process harness and the Kind job, so this suite
//!   needs no classification in `xtask/src/e2e_bare.rs` — it runs against the
//!   shared single node like every other unclassified suite.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use ephpm_e2e::required_env;

/// Emitted by `tests/docroot/index.php`. Its presence proves PHP ran.
const CONTROL_MARKER: &str = "Hello from ePHPm!";

/// Emitted by `tests/php/traversal_canary.php`, which lives OUTSIDE the
/// document root the bare-process harness serves. Seeing this in any response
/// body means a request escaped the document root and executed PHP.
const CANARY_MARKER: &str = "EPHPM_TRAVERSAL_CANARY_a41f7c2e";

/// Relative request target for the out-of-docroot canary, as seen from
/// `tests/docroot` (the bare-process harness's document root).
///
/// In the Kind/container harness the document root is `/var/www/html` and this
/// file is not present — which does not weaken anything, because the reject
/// happens in `percent_decode_path` *before* the path ever reaches the
/// filesystem. The assertion is 400 either way.
const CANARY_TARGET: &str = "php/traversal_canary.php";

/// A parsed HTTP/1.1 response read off a raw socket.
struct RawResponse {
    status: u16,
    raw: String,
}

impl RawResponse {
    /// Whole response text (status line, headers, body). Body matching uses
    /// `contains` on this rather than a parsed body because hyper may frame
    /// the PHP response as `Transfer-Encoding: chunked`, which would
    /// interleave chunk sizes with the payload.
    fn contains(&self, needle: &str) -> bool {
        self.raw.contains(needle)
    }
}

/// `host[:port]` to connect to, and the value to send in the `Host` header.
///
/// `EPHPM_URL` is `http://127.0.0.1:18100` under the bare harness and
/// `http://ephpm:8080` under Kind. `Router::check_trusted_host` compares both
/// the full header and the port-stripped form, so sending the authority
/// verbatim is trusted in both.
fn authority() -> String {
    let base = required_env("EPHPM_URL");
    let rest = base.strip_prefix("http://").unwrap_or_else(|| {
        panic!("EPHPM_URL must be a plain http:// URL for raw-socket tests, got {base:?}")
    });
    let authority = rest.split('/').next().unwrap_or(rest).trim_end_matches('/');
    assert!(!authority.is_empty(), "EPHPM_URL {base:?} has no authority");
    if authority.contains(':') { authority.to_string() } else { format!("{authority}:80") }
}

/// Send a hand-written request line and read the whole response.
///
/// `target` is written to the wire **verbatim** — no parsing, no
/// normalization, no percent-encoding. That is the entire point of this
/// module.
///
/// Returns `None` when the peer closed without sending a byte. That is still
/// a refusal (nothing was executed), but it is reported distinctly so a test
/// can say so rather than silently counting it as a pass.
fn raw_get(authority: &str, target: &str) -> Option<RawResponse> {
    let mut stream = TcpStream::connect(authority)
        .unwrap_or_else(|e| panic!("connect to {authority} failed: {e}"));
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("set read timeout");
    stream.set_write_timeout(Some(Duration::from_secs(10))).expect("set write timeout");

    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\
         User-Agent: ephpm-e2e-raw\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .unwrap_or_else(|e| panic!("write {target:?} to {authority} failed: {e}"));
    stream.flush().unwrap_or_else(|e| panic!("flush {target:?} failed: {e}"));

    let mut buf = Vec::new();
    // `Connection: close` means the server hangs up when it is done, so read
    // to EOF. A read timeout surfaces as an error and is treated as EOF —
    // whatever arrived so far is what we assert on.
    let _ = stream.read_to_end(&mut buf);
    if buf.is_empty() {
        return None;
    }

    let raw = String::from_utf8_lossy(&buf).into_owned();
    let status_line = raw.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status in response to {target:?}: {status_line:?}"));

    Some(RawResponse { status, raw })
}

/// Assert the server is alive and *does* execute an in-docroot PHP script.
///
/// Called at the top of every attack test. Without this, a traversal
/// assertion can pass simply because nothing is listening or the docroot is
/// empty — the failure mode this suite is least able to afford.
fn probe_control(authority: &str) {
    let resp = raw_get(authority, "/index.php")
        .unwrap_or_else(|| panic!("control request GET /index.php got no response at all"));
    assert_eq!(
        resp.status, 200,
        "control: raw GET /index.php must return 200 (is the fixture node up, and is \
         tests/docroot/index.php present?) — got {}",
        resp.status
    );
    assert!(
        resp.contains(CONTROL_MARKER),
        "control: raw GET /index.php returned 200 but no PHP output — expected {CONTROL_MARKER:?}. \
         PHP is not actually executing, so every traversal assertion below would be vacuous."
    );
}

/// Assert a hand-written traversal target is rejected with 400 and leaks
/// nothing.
///
/// 400 specifically, not "any non-200": `percent_decode_path` returning `None`
/// is what the router maps to 400. A 403 would mean the first layer regressed
/// and only `php_script_contained` caught it; a 404 would mean the path was
/// resolved against the filesystem before being rejected; a 500 would mean PHP
/// was reached and something else (e.g. `open_basedir`) stopped it. All three
/// are regressions worth failing on.
fn assert_traversal_rejected(authority: &str, target: &str, why: &str) {
    let Some(resp) = raw_get(authority, target) else {
        // A peer that hangs up without a response has certainly not executed
        // anything, so this is not a failure — but say so, so it is never
        // mistaken for a clean 400.
        eprintln!("note: {target:?} ({why}) — server closed the connection with no response");
        return;
    };
    assert_eq!(
        resp.status, 400,
        "{why}: raw GET {target:?} must be rejected with 400 by percent_decode_path, got {}\n\
         --- response ---\n{}\n--- end ---",
        resp.status, resp.raw
    );
    assert!(
        !resp.contains(CANARY_MARKER),
        "{why}: raw GET {target:?} leaked the out-of-docroot canary {CANARY_MARKER:?} — \
         PHP outside the document root was executed"
    );
    assert!(
        !resp.contains(CONTROL_MARKER),
        "{why}: raw GET {target:?} executed PHP (saw {CONTROL_MARKER:?}) instead of being rejected"
    );
}

/// The headline case: a literal `../` in the request line, reaching a real
/// `.php` file that sits outside the document root.
///
/// Under the bare-process harness the document root is `<repo>/tests/docroot`,
/// so this resolves to `<repo>/tests/php/traversal_canary.php` — a file that
/// exists and would execute. `reqwest` cannot send this; a socket can.
#[test]
fn literal_dot_dot_to_php_outside_document_root_rejected() {
    let authority = authority();
    probe_control(&authority);

    assert_traversal_rejected(
        &authority,
        &format!("/../{CANARY_TARGET}"),
        "literal ../ escaping the document root",
    );
    assert_traversal_rejected(
        &authority,
        &format!("/subdir/../../{CANARY_TARGET}"),
        "literal ../../ from a real subdirectory",
    );
    assert_traversal_rejected(&authority, "/../../../../etc/passwd", "deep literal traversal");
}

/// The non-vacuity anchor.
///
/// `/subdir/../index.php` resolves *back into* the document root, onto the
/// exact file `probe_control` just executed. `php_script_contained` would
/// happily allow it (it canonicalizes inside the root), so a 400 here can only
/// come from `percent_decode_path`'s `..`-segment rejection. And it cannot be
/// a 400-for-want-of-a-target: the target demonstrably exists and runs.
#[test]
fn dot_dot_back_into_document_root_rejected() {
    let authority = authority();
    probe_control(&authority);

    assert_traversal_rejected(
        &authority,
        "/subdir/../index.php",
        "dot-segment resolving back inside the document root",
    );
}

/// `%2e%2e` — the ordering case.
///
/// `percent_decode_path` decodes first and *then* runs the `..` check, so an
/// encoded dot pair must be caught. A regression that checked the raw string
/// would let these through.
#[test]
fn percent_encoded_dot_dot_rejected() {
    let authority = authority();
    probe_control(&authority);

    assert_traversal_rejected(
        &authority,
        &format!("/%2e%2e/{CANARY_TARGET}"),
        "lowercase %2e%2e escaping the document root",
    );
    assert_traversal_rejected(
        &authority,
        &format!("/%2E%2E/{CANARY_TARGET}"),
        "uppercase %2E%2E escaping the document root",
    );
    assert_traversal_rejected(
        &authority,
        "/subdir/%2e%2e/index.php",
        "encoded dot-segment resolving back inside the document root",
    );
    assert_traversal_rejected(&authority, "/subdir/.%2e/index.php", "half-encoded dot pair");
}

/// An encoded separator must not be able to manufacture a new path segment.
///
/// `percent_decode_path` rejects `%2f` / `%5c` outright rather than decoding
/// them, so `..%2f` never becomes a traversal in the first place.
#[test]
fn percent_encoded_separator_rejected() {
    let authority = authority();
    probe_control(&authority);

    assert_traversal_rejected(
        &authority,
        &format!("/..%2f{CANARY_TARGET}"),
        "encoded forward slash after a dot pair",
    );
    assert_traversal_rejected(
        &authority,
        "/subdir%2f..%2findex.php",
        "fully encoded separators around a dot segment",
    );
    assert_traversal_rejected(
        &authority,
        &format!("/..%5c{CANARY_TARGET}"),
        "encoded backslash after a dot pair",
    );
}

/// Backslash is a separator too.
///
/// `has_dot_dot_segment` splits on both `/` and `\` because a URI path is
/// joined onto the document root before it reaches the filesystem, and Windows
/// treats `\` as a real separator — so `/a\..\b` traverses exactly like
/// `/a/../b`. The assertion is the same on every platform: the router must not
/// resolve it, regardless of what the host filesystem would have done.
#[test]
fn backslash_dot_dot_rejected() {
    let authority = authority();
    probe_control(&authority);

    assert_traversal_rejected(
        &authority,
        "/subdir\\..\\index.php",
        "backslash dot-segment resolving back inside the document root",
    );
    assert_traversal_rejected(
        &authority,
        "/a\\..\\..\\php\\traversal_canary.php",
        "backslash traversal escaping the document root",
    );
}

/// The over-broad-fix guard.
///
/// If a future tightening of `percent_decode_path` starts rejecting ordinary
/// paths, every test above would still pass (everything would be 400) while
/// the server was completely broken. This is the test that fails in that case.
#[test]
fn ordinary_php_and_static_requests_still_served() {
    let authority = authority();

    // In-docroot PHP over the same raw socket the attacks use.
    probe_control(&authority);

    // A path with a single dot segment (`.`, not `..`) is legal and must not
    // be swept up by the `..` check.
    let resp = raw_get(&authority, "/./index.php").expect("GET /./index.php got no response");
    assert_ne!(
        resp.status, 400,
        "a single-dot segment is not traversal and must not be rejected as such"
    );

    // Nested static file, no dot segments at all.
    let resp = raw_get(&authority, "/subdir/index.html").expect("GET /subdir/index.html hung up");
    assert_eq!(
        resp.status, 200,
        "ordinary nested static request must still be served, got {}\n--- response ---\n{}",
        resp.status, resp.raw
    );

    // Percent-decoding itself must still work — `%2E` is a legitimate `.`
    // inside a filename, and is the case `http_edge.rs` covers via reqwest.
    let resp = raw_get(&authority, "/test%2Ehtml").expect("GET /test%2Ehtml hung up");
    assert_eq!(
        resp.status, 200,
        "percent-decoding must still resolve /test%2Ehtml to /test.html, got {}",
        resp.status
    );
}
