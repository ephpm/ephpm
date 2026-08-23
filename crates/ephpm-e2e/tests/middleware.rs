//! Native middleware end-to-end — the **dlopen lane**, in a real server.
//!
//! What this suite proves that nothing else does: a real shared-library module,
//! mounted by absolute path in a `[[middleware]]` block, is dlopened at
//! startup, passes the ABI handshake, and actually decides real HTTP requests.
//! Every other middleware test in the tree drives `MiddlewareChain` in-process
//! without a server around it.
//!
//! **Currently skipped by the bare-process harness (v0.8.0):** the `cors`
//! module it built moved OUT of this repo to github.com/ephpm/middleware, so
//! `xtask/src/e2e_bare.rs` no longer produces a cdylib here and drops this
//! suite from the run. Re-point it at a fetched module (`ephpm middleware get
//! cors`) once the CLI has landed and the middleware repo is public; the
//! module's own dlopen coverage lives in that repo's CI meanwhile.
//!
//! The mount is deliberately an explicit **path**, not the bare name `"cors"`:
//! the builtin registry is empty in stock binaries, so a bare name would fail
//! to resolve rather than test dlopen. See `xtask/src/e2e_bare.rs`
//! (`SingleNodeOptions::for_suite`) for the generated config.
//!
//! The module is configured with a single allowed origin rather than `"*"`, so
//! the suite can distinguish "the module ran and made a decision" from
//! "something appended headers unconditionally" — an unlisted origin must come
//! back with no CORS headers at all.
//!
//! Environment:
//! - `EPHPM_URL` — base URL of the node with the middleware mounted
//! - `EPHPM_BINARY` — ephpm binary, for the startup-failure assertions
//! - `EPHPM_MIDDLEWARE_LIB` — the built cdylib that node mounted

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ephpm_e2e::required_env;

/// The origin the mounted CORS module is configured to allow.
const ALLOWED_ORIGIN: &str = "https://allowed.e2e.test";

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("build reqwest client")
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

// ── the module runs, in a real server, on real requests ───────────────────

/// A PHP-dispatched request from the allowed origin comes back with the CORS
/// headers the dlopened module appended — and with PHP's own body intact,
/// proving the chain returned `CONTINUE` rather than short-circuiting.
#[tokio::test]
async fn dlopened_module_appends_headers_to_a_php_response() {
    let base_url = required_env("EPHPM_URL");
    let resp = client()
        .get(format!("{base_url}/index.php"))
        .header("Origin", ALLOWED_ORIGIN)
        .send()
        .await
        .expect("request /index.php");

    assert_eq!(resp.status(), 200, "PHP must still run on a CONTINUE verdict");
    assert_eq!(
        header(&resp, "access-control-allow-origin"),
        Some(ALLOWED_ORIGIN),
        "the dlopened cors module did not append its header — headers: {:?}",
        resp.headers()
    );
    assert_eq!(header(&resp, "vary"), Some("Origin"));

    let body = resp.text().await.expect("read body");
    assert!(!body.is_empty(), "PHP produced no body");
}

/// The `RESPOND` short-circuit over real HTTP: a preflight is answered by the
/// module with 204 and PHP never runs. This is the verdict that has to travel
/// furthest — status, empty body and headers all marshaled out of module
/// memory, through the chain, into the client's response.
#[tokio::test]
async fn dlopened_module_answers_preflight_without_running_php() {
    let base_url = required_env("EPHPM_URL");
    let resp = client()
        .request(reqwest::Method::OPTIONS, format!("{base_url}/index.php"))
        .header("Origin", ALLOWED_ORIGIN)
        .header("Access-Control-Request-Method", "PUT")
        .send()
        .await
        .expect("preflight /index.php");

    assert_eq!(resp.status(), 204, "preflight must be short-circuited by the module");
    assert_eq!(header(&resp, "access-control-allow-origin"), Some(ALLOWED_ORIGIN));
    assert_eq!(header(&resp, "access-control-max-age"), Some("600"), "config reached init");
    assert!(
        header(&resp, "access-control-allow-methods").is_some_and(|v| v.contains("PUT")),
        "preflight must advertise the allowed methods"
    );

    let body = resp.bytes().await.expect("read body");
    assert!(body.is_empty(), "a 204 short-circuit must have no body");
}

/// The decision is real: an origin outside the allow list gets **no** CORS
/// headers. Without this, a middleware that unconditionally stamped headers
/// on every response would pass the tests above.
#[tokio::test]
async fn disallowed_origin_gets_no_cors_headers() {
    let base_url = required_env("EPHPM_URL");
    let resp = client()
        .get(format!("{base_url}/index.php"))
        .header("Origin", "https://not-allowed.e2e.test")
        .send()
        .await
        .expect("request /index.php");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "access-control-allow-origin"),
        None,
        "an unlisted origin must not receive CORS headers"
    );
}

/// A request with no `Origin` at all also passes through untouched — the
/// module's first early-return branch, and the shape of every ordinary
/// same-origin request the server handles.
#[tokio::test]
async fn request_without_origin_is_untouched() {
    let base_url = required_env("EPHPM_URL");
    let resp =
        client().get(format!("{base_url}/index.php")).send().await.expect("request /index.php");

    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "access-control-allow-origin"), None);
    assert_eq!(header(&resp, "vary"), None);
}

/// Documented coverage boundary, pinned: in FPM mode the chain runs only for
/// PHP-dispatched requests, so a static file does NOT pass through middleware.
/// If that ever changes, this test should be updated deliberately rather than
/// discovered by an operator whose CORS policy silently started applying to
/// assets.
#[tokio::test]
async fn static_files_bypass_the_middleware_chain() {
    let base_url = required_env("EPHPM_URL");
    let resp = client()
        .get(format!("{base_url}/test.html"))
        .header("Origin", ALLOWED_ORIGIN)
        .send()
        .await
        .expect("request /test.html");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "access-control-allow-origin"),
        None,
        "static-file responses are documented as bypassing the middleware chain"
    );
}

// ── startup must fail loudly on a broken mount ────────────────────────────

/// Spawn `ephpm serve` with a hand-written config, wait up to `budget`, and
/// return `(exited_within_budget, combined_output)`.
///
/// Used in both directions: a broken `[[middleware]]` mount must abort startup
/// (the fail-fast contract that keeps a server from coming up healthy with an
/// auth module silently missing), and a valid one must not.
fn serve_and_watch(config: &str, label: &str, budget: Duration) -> (bool, String) {
    let binary = PathBuf::from(required_env("EPHPM_BINARY"));
    let dir = tempfile::Builder::new()
        .prefix(&format!("ephpm-mw-{label}-"))
        .tempdir()
        .expect("tempdir");
    let config_path = dir.path().join("ephpm.toml");
    std::fs::write(&config_path, config).expect("write config");

    let mut child = Command::new(&binary)
        .args(["serve", "--config"])
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ephpm");

    // Startup validation is synchronous and happens before the listener binds,
    // so a fail-fast exit is prompt; the budget is generous only to absorb a
    // loaded CI runner.
    let deadline = Instant::now() + budget;
    let exited = loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break true,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                break false;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };

    let out = child.wait_with_output().expect("collect output");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (exited, combined)
}

/// Minimal config with one `[[middleware]]` mount pointing at `library`.
fn config_with_mount(library: &str, port: u16, dir: &str) -> String {
    format!(
        "[server]\n\
         listen = \"127.0.0.1:{port}\"\n\
         document_root = \"{dir}\"\n\
         \n\
         [[middleware]]\n\
         library = \"{library}\"\n\
         order = 10\n"
    )
}

/// A mount pointing at a path that does not exist must abort startup, naming
/// the path — not start up middleware-less.
#[test]
fn missing_library_path_fails_startup() {
    let docroot = std::env::temp_dir().join("ephpm-mw-missing-docroot");
    std::fs::create_dir_all(&docroot).expect("docroot");
    let cfg = config_with_mount(
        "/nonexistent/definitely-not-a-module.so",
        18191,
        &docroot.to_string_lossy(),
    );
    let (exited, output) = serve_and_watch(&cfg, "missing", Duration::from_secs(20));

    assert!(exited, "ephpm must exit rather than serve with a broken middleware mount:\n{output}");
    assert!(
        output.contains("definitely-not-a-module.so"),
        "startup error must name the offending path:\n{output}"
    );
}

/// A mount pointing at a file that exists but is not a loadable object must
/// abort startup with the platform loader's error — the "wrong file / wrong
/// architecture" case an operator hits when copying modules between hosts.
#[test]
fn garbage_library_file_fails_startup() {
    let dir = tempfile::Builder::new().prefix("ephpm-mw-junk-").tempdir().expect("tempdir");
    let junk = dir.path().join("not-a-module.so");
    std::fs::write(&junk, b"\x7fELF definitely not an object file").expect("write junk");

    let cfg = config_with_mount(&junk.to_string_lossy(), 18192, &dir.path().to_string_lossy());
    let (exited, output) = serve_and_watch(&cfg, "junk", Duration::from_secs(20));

    assert!(exited, "ephpm must exit rather than serve with an unloadable module:\n{output}");
    assert!(
        output.contains("not-a-module.so"),
        "startup error must name the offending path:\n{output}"
    );
}

/// The positive control for the two tests above: the SAME spawn path, with the
/// SAME shape of config, but mounting the real module — must NOT exit.
///
/// Without this, both failure tests would still pass if `ephpm serve` were
/// broken in some unrelated way and exited immediately on every config.
#[test]
fn valid_module_mount_does_not_abort_startup() {
    let lib = required_env("EPHPM_MIDDLEWARE_LIB");
    let dir = tempfile::Builder::new().prefix("ephpm-mw-ok-").tempdir().expect("tempdir");
    let cfg = format!(
        "[server]\n\
         listen = \"127.0.0.1:18193\"\n\
         document_root = \"{}\"\n\
         \n\
         [[middleware]]\n\
         library = \"{lib}\"\n\
         order = 10\n\
         config = {{ allow_origins = [\"{ALLOWED_ORIGIN}\"] }}\n",
        dir.path().to_string_lossy()
    );
    // Reuse the failure harness, then invert the expectation: a healthy mount
    // means the process is STILL RUNNING when the budget expires.
    let (exited, output) = serve_and_watch(&cfg, "ok", Duration::from_secs(6));
    assert!(
        !exited,
        "a valid middleware mount must not abort startup — ephpm exited with:\n{output}"
    );
}
