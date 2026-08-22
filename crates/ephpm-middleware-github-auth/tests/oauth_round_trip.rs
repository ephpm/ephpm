//! The OAuth round trip, driven against a stub GitHub over real sockets.
//!
//! A real `github.com` round trip cannot be completed unattended — it needs a
//! human at a browser and a registered GitHub App. Everything *this side* of
//! that can be exercised for real, and is: a loopback HTTP server stands in
//! for `github.com` and `api.github.com`, and the module's own
//! `reqwest`/`rustls` client talks to it over an actual TCP connection with
//! actual request and response bytes. The token exchange, the `Authorization`
//! header, the JSON parsing, the access decision, the `Set-Cookie` and the
//! `Location` are all the shipping code paths.
//!
//! What the stub cannot tell you: that GitHub's real responses match these
//! shapes, and that a real App's redirect URL is registered correctly. Those
//! need a human with a GitHub App — see the README's verification table.
//!
//! The stub also **counts every request it receives**, which is how the most
//! important assertion in this file is made:
//! [`a_request_with_a_session_makes_no_network_call`] shows the counter does
//! not move.
//!
//! The crate's `[lib] name` is `github_auth` (that is what the loadable
//! artefact is called), so it is imported under that name here.
#![allow(unsafe_code, reason = "the test builds the FFI Request view by hand")]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
use ephpm_middleware::host::{RequestCtx, host_table};
use ephpm_middleware::{Middleware as _, Request, Response};
use github_auth::{GithubAuth, query_param};

const VHOST: &str = "pr-1.preview.test";
const SESSION_SECRET: &str = "0123456789abcdef0123456789abcdef";
const CLIENT_SECRET: &str = "stub-client-secret";
const GOOD_CODE: &str = "goodcode";
const ACCESS_TOKEN: &str = "gho_stubtoken";

// ── the stub GitHub ───────────────────────────────────────────────────────

/// What the stub saw, so tests can assert on traffic rather than on outcomes
/// alone.
#[derive(Default)]
struct Seen {
    /// `"METHOD PATH"` for every request served.
    requests: Vec<String>,
    /// Bodies of the token-exchange POSTs.
    token_bodies: Vec<String>,
    /// `Authorization` header values seen on API calls.
    authorizations: Vec<String>,
}

struct Stub {
    base: String,
    seen: Arc<Mutex<Seen>>,
    count: Arc<AtomicUsize>,
}

impl Stub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let count = Arc::new(AtomicUsize::new(0));

        let (seen_t, count_t) = (Arc::clone(&seen), Arc::clone(&count));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // One request per connection; every response carries
                // `Connection: close`, so the client opens a fresh one.
                handle(stream, &seen_t, &count_t);
            }
        });
        Self { base, seen, count }
    }

    fn requests(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().expect("stub state")
    }
}

fn handle(mut stream: TcpStream, seen: &Arc<Mutex<Seen>>, count: &Arc<AtomicUsize>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) =
        (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());

    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("authorization:") {
            // Split the ORIGINAL line — the token is case-sensitive.
            authorization.clear();
            authorization.push_str(h.split_once(':').map_or("", |(_, v)| v.trim()));
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    count.fetch_add(1, Ordering::SeqCst);
    {
        let mut s = seen.lock().expect("stub state");
        s.requests.push(format!("{method} {path}"));
        if path.starts_with("/login/oauth/access_token") {
            s.token_bodies.push(body.clone());
        } else if !authorization.is_empty() {
            s.authorizations.push(authorization.clone());
        }
    }

    let (status, json) = respond(&method, &path, &body, &authorization);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{json}",
        json.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// The stub's GitHub. Deliberately literal about the two behaviours that
/// catch real bugs: GitHub reports OAuth failures with **HTTP 200** and an
/// `error` field, and it answers "exists but not for you" with **404**.
fn respond(method: &str, path: &str, body: &str, authorization: &str) -> (&'static str, String) {
    match (method, path) {
        ("POST", "/login/oauth/access_token") => {
            if !body.contains(&format!("code={GOOD_CODE}")) {
                return ("200 OK", r#"{"error":"bad_verification_code"}"#.to_owned());
            }
            if !body.contains("client_secret=stub-client-secret") {
                return ("200 OK", r#"{"error":"incorrect_client_credentials"}"#.to_owned());
            }
            ("200 OK", format!(r#"{{"access_token":"{ACCESS_TOKEN}","token_type":"bearer"}}"#))
        }
        // Everything below requires the token the exchange handed out.
        _ if authorization != format!("Bearer {ACCESS_TOKEN}") => {
            ("401 Unauthorized", r#"{"message":"Bad credentials"}"#.to_owned())
        }
        ("GET", "/user") => ("200 OK", r#"{"login":"octocat","id":583231}"#.to_owned()),
        ("GET", "/repos/acme/web") => (
            "200 OK",
            r#"{"full_name":"acme/web","permissions":{"pull":true,"push":false}}"#.to_owned(),
        ),
        // Visible, but read is explicitly denied.
        ("GET", "/repos/acme/noread") => ("200 OK", r#"{"permissions":{"pull":false}}"#.to_owned()),
        ("GET", "/user/memberships/orgs/acme") => ("200 OK", r#"{"state":"active"}"#.to_owned()),
        ("GET", "/user/memberships/orgs/other") => ("200 OK", r#"{"state":"pending"}"#.to_owned()),
        ("GET", "/orgs/acme/teams/platform/memberships/octocat") => {
            ("200 OK", r#"{"state":"active"}"#.to_owned())
        }
        // Everything else — including `/repos/acme/secret` and
        // `/orgs/acme/teams/secret-team/memberships/octocat` — is GitHub's
        // deliberate "exists but not for you": a 404, not a 403.
        _ => ("404 Not Found", r#"{"message":"Not Found"}"#.to_owned()),
    }
}

// ── driving the module ────────────────────────────────────────────────────

fn gate(stub: &Stub, extra: serde_json::Value) -> GithubAuth {
    let mut cfg = serde_json::json!({
        "client_id": "Iv1.stub",
        "client_secret": CLIENT_SECRET,
        "session_secret": SESSION_SECRET,
        "repo": "acme/web",
        "github_base": stub.base,
        "github_api_base": stub.base,
        // The derived redirect_uri would be https://<vhost>/…; pin it so the
        // exchange body is deterministic.
        "redirect_uri": format!("https://{VHOST}/_ephpm/auth/github/callback"),
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        cfg[k] = v;
    }
    GithubAuth::init(&cfg).expect("init")
}

fn call(mw: &GithubAuth, path: &str, query: &str, headers: &[(String, String)]) -> Response {
    let ctx = RequestCtx::new("GET", path, query, "203.0.113.9", VHOST, headers);
    // SAFETY: `ctx` outlives the borrow and `host_table()` is 'static — the
    // exact contract `Request::from_raw` documents.
    let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
    mw.invoke(&req)
}

fn header(resp: &Response, name: &str) -> Option<String> {
    resp.__headers().iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
}

fn set_cookies(resp: &Response) -> Vec<String> {
    resp.__headers()
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, v)| v.clone())
        .collect()
}

/// Run the login half and return `(state parameter, state cookie pair)`.
fn begin_login(mw: &GithubAuth, from: &str) -> (String, String) {
    let resp = call(mw, from, "", &[]);
    assert_eq!(resp.__status(), 302);
    let location = header(&resp, "Location").expect("Location");
    let state = query_param(location.split_once('?').expect("query").1, "state").expect("state");
    let cookie = set_cookies(&resp)
        .into_iter()
        .find(|c| c.starts_with("ephpm_session_oauth="))
        .expect("state cookie");
    (state, cookie.split(';').next().expect("pair").to_owned())
}

/// Complete a callback with the given code.
fn finish_login(mw: &GithubAuth, state: &str, cookie: &str, code: &str) -> Response {
    call(
        mw,
        "/_ephpm/auth/github/callback",
        &format!("code={code}&state={state}"),
        &[("Cookie".to_owned(), cookie.to_owned())],
    )
}

/// The session cookie value out of a successful callback.
fn session_value(resp: &Response) -> String {
    set_cookies(resp)
        .into_iter()
        .find(|c| c.starts_with("ephpm_session="))
        .expect("session cookie")
        .split(';')
        .next()
        .expect("pair")
        .to_owned()
}

// ── tests ─────────────────────────────────────────────────────────────────

#[test]
fn happy_path_exchanges_the_code_checks_access_and_issues_a_session() {
    let stub = Stub::start();
    let mw = gate(&stub, serde_json::json!({}));

    let (state, cookie) = begin_login(&mw, "/wp-admin/index.php");
    assert_eq!(stub.requests(), 0, "starting a login must not touch GitHub");

    let resp = finish_login(&mw, &state, &cookie, GOOD_CODE);
    assert_eq!(resp.__action(), ACTION_RESPOND);
    assert_eq!(resp.__status(), 302);
    assert_eq!(
        header(&resp, "Location").as_deref(),
        Some("/wp-admin/index.php"),
        "the user lands where they were going"
    );

    // Exactly the three documented calls, in order.
    let seen = stub.seen();
    assert_eq!(
        seen.requests,
        vec![
            "POST /login/oauth/access_token".to_owned(),
            "GET /user".to_owned(),
            "GET /repos/acme/web".to_owned(),
        ]
    );
    // The exchange carried the client credentials and the redirect_uri.
    let body = seen.token_bodies.first().expect("token body");
    assert!(body.contains("client_id=Iv1.stub"));
    assert!(body.contains("client_secret=stub-client-secret"));
    assert!(body.contains(&format!("code={GOOD_CODE}")));
    assert!(body.contains("redirect_uri="));
    // Both API calls were bearer-authenticated with the exchanged token.
    assert_eq!(seen.authorizations.len(), 2);
    assert!(seen.authorizations.iter().all(|a| a == &format!("Bearer {ACCESS_TOKEN}")));
    drop(seen);

    // The session cookie is a three-part HS256 JWT with the right attributes.
    let session = session_value(&resp);
    let value = session.strip_prefix("ephpm_session=").expect("prefix");
    assert_eq!(value.split('.').count(), 3);
    let raw = set_cookies(&resp).into_iter().find(|c| c.starts_with("ephpm_session=")).expect("c");
    assert!(raw.contains("HttpOnly") && raw.contains("Secure") && raw.contains("SameSite=Lax"));
    assert!(raw.contains("Max-Age=28800"));
    assert!(!raw.contains("Domain="), "host-only, so one tenant's session cannot reach another");

    // The state cookie is cleared on the way out.
    assert!(
        set_cookies(&resp).iter().any(|c| c.starts_with("ephpm_session_oauth=;")
            || c.starts_with("ephpm_session_oauth=; ")
            || (c.starts_with("ephpm_session_oauth=") && c.contains("Max-Age=0"))),
        "the one-shot state cookie must be deleted: {:?}",
        set_cookies(&resp)
    );

    // And the payload says what it should.
    let payload = value.split('.').nth(1).expect("payload");
    let json: serde_json::Value =
        serde_json::from_slice(&base64_url_decode(payload).expect("payload is base64url"))
            .expect("payload is JSON");
    assert_eq!(json["sub"], "octocat");
    assert_eq!(json["gh_id"], 583_231);
    assert_eq!(json["site"], VHOST);
    assert_eq!(json["via"], "github");
    assert_eq!(json["check"], "repo acme/web");
}

#[test]
fn a_request_with_a_session_makes_no_network_call() {
    let stub = Stub::start();
    let mw = gate(&stub, serde_json::json!({}));

    let (state, cookie) = begin_login(&mw, "/");
    let session = session_value(&finish_login(&mw, &state, &cookie, GOOD_CODE));
    let after_login = stub.requests();
    assert_eq!(after_login, 3);

    // Fifty ordinary requests carrying the session. This is the constraint
    // the whole design exists to satisfy: the counter must not move.
    let headers = vec![("Cookie".to_owned(), session)];
    for i in 0..50 {
        let resp = call(&mw, "/index.php", &format!("i={i}"), &headers);
        assert_eq!(resp.__action(), ACTION_CONTINUE, "request {i} must reach PHP");
    }
    assert_eq!(
        stub.requests(),
        after_login,
        "a request with a session cookie must never reach GitHub"
    );
}

#[test]
fn a_bad_code_is_rejected_and_issues_nothing() {
    let stub = Stub::start();
    let mw = gate(&stub, serde_json::json!({}));
    let (state, cookie) = begin_login(&mw, "/");

    let resp = finish_login(&mw, &state, &cookie, "wrongcode");
    // GitHub answers HTTP 200 with an `error` field; treating that as success
    // is the classic OAuth implementation bug.
    assert_eq!(resp.__status(), 400);
    assert!(
        !set_cookies(&resp).iter().any(|c| c.starts_with("ephpm_session=")),
        "a failed exchange must not issue a session"
    );
    assert_eq!(
        stub.seen().requests,
        vec!["POST /login/oauth/access_token".to_owned()],
        "the flow stops at the exchange — no identity or access call is made"
    );
}

#[test]
fn a_user_without_repo_access_is_denied() {
    let stub = Stub::start();
    let mw = gate(&stub, serde_json::json!({ "repo": "acme/secret" }));
    let (state, cookie) = begin_login(&mw, "/");

    let resp = finish_login(&mw, &state, &cookie, GOOD_CODE);
    assert_eq!(resp.__status(), 403);
    assert!(!set_cookies(&resp).iter().any(|c| c.starts_with("ephpm_session=")));
    // The 404 GitHub returns for "exists but not yours" is a denial, not an
    // error — the flow completed and answered no.
    assert_eq!(stub.seen().requests.len(), 3);
}

#[test]
fn a_repo_with_read_explicitly_denied_is_denied() {
    let stub = Stub::start();
    let mw = gate(&stub, serde_json::json!({ "repo": "acme/noread" }));
    let (state, cookie) = begin_login(&mw, "/");
    assert_eq!(finish_login(&mw, &state, &cookie, GOOD_CODE).__status(), 403);
}

#[test]
fn org_membership_must_be_active_not_pending() {
    let stub = Stub::start();

    let mw = gate(&stub, serde_json::json!({ "check": "org", "org": "acme", "repo": null }));
    let (state, cookie) = begin_login(&mw, "/");
    assert_eq!(finish_login(&mw, &state, &cookie, GOOD_CODE).__status(), 302);

    // `pending` is an unaccepted invitation — not a member.
    let mw = gate(&stub, serde_json::json!({ "check": "org", "org": "other", "repo": null }));
    let (state, cookie) = begin_login(&mw, "/");
    assert_eq!(finish_login(&mw, &state, &cookie, GOOD_CODE).__status(), 403);
}

#[test]
fn team_membership_is_checked_against_the_logged_in_user() {
    let stub = Stub::start();

    let mw = gate(
        &stub,
        serde_json::json!({ "check": "team", "org": "acme", "team": "platform", "repo": null }),
    );
    let (state, cookie) = begin_login(&mw, "/");
    assert_eq!(finish_login(&mw, &state, &cookie, GOOD_CODE).__status(), 302);
    // The path is built from `/user`'s login, not from anything the client said.
    assert!(
        stub.seen()
            .requests
            .iter()
            .any(|r| r == "GET /orgs/acme/teams/platform/memberships/octocat")
    );

    let mw = gate(
        &stub,
        serde_json::json!({ "check": "team", "org": "acme", "team": "secret-team", "repo": null }),
    );
    let (state, cookie) = begin_login(&mw, "/");
    assert_eq!(finish_login(&mw, &state, &cookie, GOOD_CODE).__status(), 403);
}

#[test]
fn state_is_a_signed_value_not_a_one_time_server_side_record() {
    // Replaying a completed login's state with a fresh code still requires the
    // state to verify — it does, because it is stateless and not yet expired.
    // What it CANNOT do is complete without the cookie, which is the property
    // that matters and which `callback_without_a_state_cookie_is_rejected`
    // pins. This test records the honest limitation rather than implying a
    // stronger one: a stateless state token is replayable within its 10-minute
    // window by whoever holds the cookie, i.e. by the user themselves.
    let stub = Stub::start();
    let mw = gate(&stub, serde_json::json!({}));
    let (state, cookie) = begin_login(&mw, "/");

    assert_eq!(finish_login(&mw, &state, &cookie, GOOD_CODE).__status(), 302);
    // Second use: the state still verifies (it is a signed value with a TTL),
    // and GitHub is what refuses a reused authorization code.
    assert_eq!(finish_login(&mw, &state, &cookie, "wrongcode").__status(), 400);
}

#[test]
fn github_being_unreachable_is_a_502_not_a_pass() {
    // Point the gate at a closed port: the exchange cannot complete.
    let stub = Stub::start();
    let mw = gate(
        &stub,
        serde_json::json!({
            "github_base": "http://127.0.0.1:1",
            "github_api_base": "http://127.0.0.1:1",
            "http_timeout_secs": 2,
        }),
    );
    let (state, cookie) = begin_login(&mw, "/");
    let resp = finish_login(&mw, &state, &cookie, GOOD_CODE);
    assert_eq!(resp.__status(), 502, "an unreachable identity provider must fail closed");
    assert!(!set_cookies(&resp).iter().any(|c| c.starts_with("ephpm_session=")));
}

/// Minimal unpadded base64url decoder for asserting on the issued payload.
fn base64_url_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = u32::try_from(TABLE.iter().position(|t| *t == c)?).ok()?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}
