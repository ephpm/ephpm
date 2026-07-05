//! WordPress worker-mode E2E tests.
//!
//! These run against an ePHPm instance in persistent worker mode serving a
//! real WordPress install through the `ephpm/wordpress-worker` package
//! (`[php] mode = "worker"`, `worker_script` = the package's
//! `bin/ephpm-wp-worker`, `worker_populate_superglobals = true`).
//!
//! Like `worker_mode.rs`, this needs a SEPARATE server instance from the
//! default fpm docroot the other e2e tests use. The harness provides its base
//! URL via `EPHPM_WP_URL`; tests self-skip (pass) when it is unset so they
//! don't break fpm-only CI lanes.
//!
//! Fixture: the image built by the `ephpm/wordpress-worker` package's
//! `e2e/Dockerfile` (WordPress + wp-sqlite-db drop-in, headless
//! `wp_install()` at build time creating admin user `admin` / `password123`).
//! Locally:
//!
//! ```sh
//! # in the wordpress-worker package checkout (stages pkgsrc + builds image):
//! e2e/run.sh          # or: podman build -f e2e/Dockerfile -t wp-e2e e2e/
//! podman run -d -p 8100:8080 ephpm-wp-worker-e2e:latest
//! EPHPM_WP_URL=http://127.0.0.1:8100 cargo test --test wordpress
//! ```
//!
//! Credentials are overridable via `EPHPM_WP_ADMIN_USER` / `EPHPM_WP_ADMIN_PASS`.
//!
//! Regression coverage: the login POST -> 302 redirect path. `wp_redirect()`
//! aborts the worker request via the adapter's `RedirectSignal`, and
//! `redirectResponse()` must preserve headers already emitted through
//! `setcookie()` — WordPress sets its auth cookies *before* redirecting. An
//! earlier adapter bug rebuilt the redirect headers from scratch, producing a
//! cookie-less 302 and an unusable login. The fix starts from
//! `headers_list()` and emits each `Set-Cookie` as its own wire header.
//!
//! KNOWN ENGINE BUG (retried, not masked): on some recycled workers the
//! exit-synthesized response path returns `200` with an EMPTY body and no
//! headers (observed alternating per worker generation). That signature — 200
//! with an empty body where content is required — is retried a few times
//! (each retry lands on / produces a different worker generation). It cannot
//! be confused with the cookie regression, which manifests as a 302 WITHOUT
//! Set-Cookie followed by a deterministic bounce back to wp-login.php; those
//! paths hard-fail immediately with no retry.

use std::collections::BTreeMap;

/// Base URL of the WordPress worker-mode instance, or `None` to skip.
fn wp_url() -> Option<String> {
    std::env::var("EPHPM_WP_URL").ok().filter(|s| !s.is_empty())
}

fn admin_user() -> String {
    std::env::var("EPHPM_WP_ADMIN_USER").unwrap_or_else(|_| "admin".into())
}

fn admin_pass() -> String {
    std::env::var("EPHPM_WP_ADMIN_PASS").unwrap_or_else(|_| "password123".into())
}

/// A client that never follows redirects — the 302 itself is under test.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client")
}

/// Collect the raw `Set-Cookie` header values of a response.
fn set_cookies(resp: &reqwest::Response) -> Vec<String> {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect()
}

/// Fold `Set-Cookie` lines into a jar, honoring deletions (empty / "deleted"
/// values clear the cookie).
fn update_jar(jar: &mut BTreeMap<String, String>, lines: &[String]) {
    for line in lines {
        let pair = line.split(';').next().unwrap_or_default();
        let Some((name, value)) = pair.split_once('=') else { continue };
        let (name, value) = (name.trim().to_owned(), value.trim().to_owned());
        if value.is_empty() || value == "deleted" || value == "+" {
            jar.remove(&name);
        } else {
            jar.insert(name, value);
        }
    }
}

fn cookie_header(jar: &BTreeMap<String, String>) -> String {
    jar.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
}

/// One full login round-trip attempt.
///
/// Returns `Err(reason)` ONLY for the known engine empty-response signature
/// (200 + empty body where content is required) — the caller retries those.
/// Everything that could indicate the cookie regression asserts (panics)
/// immediately.
async fn login_roundtrip_attempt(client: &reqwest::Client, base: &str) -> Result<(), String> {
    let mut jar: BTreeMap<String, String> = BTreeMap::new();

    // ── 1. GET /wp-login.php: form renders, test cookie offered. ────────────
    let login_url = format!("{base}/wp-login.php");
    let resp = client
        .get(&login_url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {login_url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200, "GET /wp-login.php must be 200");
    update_jar(&mut jar, &set_cookies(&resp));
    let form = resp.text().await.expect("read login form body");
    if form.is_empty() {
        return Err("GET /wp-login.php returned 200 with an empty body".into());
    }
    assert!(form.contains("user_login"), "login form missing user_login field: {form}");
    // WordPress refuses the POST without its test cookie; make sure we have
    // one even if the server didn't offer it on the GET.
    jar.entry("wordpress_test_cookie".into()).or_insert_with(|| "WP%20Cookie%20check".into());

    // ── 2. POST credentials: 302 to /wp-admin/ WITH auth cookies. ───────────
    let form_body = format!(
        "log={}&pwd={}&wp-submit=Log+In&redirect_to={}&testcookie=1",
        urlencoding::encode(&admin_user()),
        urlencoding::encode(&admin_pass()),
        urlencoding::encode(&format!("{base}/wp-admin/")),
    );
    let resp = client
        .post(&login_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Cookie", cookie_header(&jar))
        .body(form_body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {login_url} failed: {e}"));

    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let cookies = set_cookies(&resp);
    if status == 200 && cookies.is_empty() {
        let body = resp.text().await.unwrap_or_default();
        if body.is_empty() {
            return Err("login POST returned 200 with an empty body".into());
        }
        panic!("login POST re-rendered the form (bad credentials?): {body}");
    }
    assert_eq!(
        status, 302,
        "login POST must redirect (302), got {status} — wrong credentials or the \
         worker adapter mishandled wp_redirect(); Set-Cookie seen: {cookies:?}"
    );
    assert!(
        location.contains("/wp-admin"),
        "login redirect must target /wp-admin/, got Location: {location:?} \
         (a redirect back to wp-login.php means authentication failed)"
    );

    // The regression under test: the 302 must carry the auth cookies that
    // WordPress setcookie()'d before wp_redirect(). Before the fix the
    // adapter rebuilt the redirect headers from scratch and the 302 had NO
    // Set-Cookie headers at all. NO retry here — a cookie-less 302 with
    // valid credentials is the bug.
    let names: Vec<&str> =
        cookies.iter().filter_map(|c| c.split('=').next().map(str::trim)).collect();
    assert!(
        names.iter().any(|n| n.starts_with("wordpress_logged_in_")),
        "302 must set the wordpress_logged_in_* session cookie; \
         Set-Cookie names on the wire: {names:?}"
    );
    assert!(
        names.iter().any(|n| (n.starts_with("wordpress_sec_")
            || (n.starts_with("wordpress_") && !n.starts_with("wordpress_logged_in_")))
            && *n != "wordpress_test_cookie"),
        "302 must also set the wordpress_[sec_]* auth cookie; names: {names:?}"
    );
    assert!(
        cookies.len() >= 2,
        "WordPress sets several auth cookies — expected multiple distinct \
         Set-Cookie lines on the 302, got {}: {cookies:?}",
        cookies.len()
    );
    update_jar(&mut jar, &cookies);

    // ── 3. Authenticated wp-admin: 200 with the jar, 302-to-login without. ──
    // Control first: WITHOUT cookies /wp-admin/index.php must bounce to
    // wp-login (auth_redirect). This proves the 200 below is cookie-driven.
    let admin_index = format!("{base}/wp-admin/index.php");
    let resp = client
        .get(&admin_index)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {admin_index} (no cookies) failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        302,
        "unauthenticated GET /wp-admin/index.php must redirect to wp-login"
    );

    // With the jar: the dashboard renders — the cookies from the 302 work.
    let resp = client
        .get(&admin_index)
        .header("Cookie", cookie_header(&jar))
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {admin_index} failed: {e}"));
    let status = resp.status().as_u16();
    let bounce = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        status, 200,
        "authenticated GET /wp-admin/index.php must be 200, got {status} \
         (Location: {bounce:?}) — auth cookies from the 302 were not honored"
    );
    let body = resp.text().await.expect("read wp-admin body");
    if body.is_empty() {
        return Err("authenticated GET /wp-admin/index.php returned 200 with an empty body".into());
    }
    assert!(
        body.contains("Dashboard"),
        "authenticated wp-admin response does not look like the dashboard"
    );
    assert!(
        !body.contains("name=\"log\""),
        "wp-admin served the login form — session cookies were dropped"
    );

    // Bare /wp-admin/ (the Location target) must also be served without a
    // bounce back to wp-login. (The adapter routes extensionless paths through
    // the front controller, so this is a 200 page, not necessarily the
    // dashboard chrome.)
    let admin_url = format!("{base}/wp-admin/");
    let resp = client
        .get(&admin_url)
        .header("Cookie", cookie_header(&jar))
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {admin_url} failed: {e}"));
    let status = resp.status().as_u16();
    let bounce = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        status == 200 && !bounce.contains("wp-login"),
        "authenticated GET /wp-admin/ bounced back to login \
         (status {status}, Location: {bounce:?})"
    );

    Ok(())
}

/// Full login round-trip: GET the login form (test cookie), POST valid
/// credentials, assert the 302 to /wp-admin/ CARRIES the auth cookies on the
/// wire (multiple distinct Set-Cookie lines, including the logged-in cookie),
/// then GET wp-admin with the jar and assert an authenticated 200 — not a
/// bounce back to wp-login.php.
#[tokio::test]
async fn wordpress_login_roundtrip() {
    let Some(base) = wp_url() else {
        eprintln!("EPHPM_WP_URL unset — skipping WordPress login round-trip test");
        return;
    };
    let client = client();

    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        match login_roundtrip_attempt(&client, &base).await {
            Ok(()) => return,
            Err(reason) if attempt < ATTEMPTS => {
                // Known engine empty-response signature on recycled workers —
                // see module docs. Retrying reaches a healthy worker.
                eprintln!("attempt {attempt}/{ATTEMPTS} hit engine empty-response: {reason}");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(reason) => {
                panic!("login round-trip still failing after {ATTEMPTS} attempts: {reason}");
            }
        }
    }
}
