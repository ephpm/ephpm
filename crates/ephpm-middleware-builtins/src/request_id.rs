//! `request-id` — ePHPm native middleware that gives every request a stable
//! correlation id, injects it for PHP, and echoes it on the response.
//!
//! Analogous to Caddy's `request_id`, Kong's `correlation-id`, Traefik's
//! request-id plugins, or nginx's `$request_id`. One id per request ties the
//! access log, the PHP application log, and the client's copy of the header
//! together.
//!
//! # Two phases
//!
//! - **Request phase** ([`Middleware::invoke`]) — resolve the id (honor a
//!   trusted inbound header, otherwise generate a fresh UUIDv4), inject it as a
//!   request header so PHP sees `$_SERVER['HTTP_<HEADER>']`, and stage the same
//!   value as a response header so the dynamic response carries exactly the id
//!   PHP logged.
//! - **Response phase** ([`ResponseMiddleware::invoke_response`]) — guarantee
//!   the header on responses the request phase never touched (the static-file
//!   path runs **no** request phase) and stay idempotent on the PHP path.
//!
//! # Why the request phase also stages the response header
//!
//! The v1 response phase cannot see state its own request phase set — it is
//! handed a request view rebuilt from the *original* inbound headers, and it
//! may run with no preceding `invoke` at all (static files, an upstream
//! short-circuit). So a *generated* id known only to the request phase could
//! not be re-derived in the response phase; regenerating there would echo a
//! different id than PHP received. The request phase therefore carries the id
//! to the response itself (`response_header`), and the response phase only
//! *fills in* the header when it is absent — never overwrites it.
//!
//! Configuration (`[[middleware]] config = { ... }`), all optional:
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `header` (string) | `"X-Request-Id"` | the request/response header name carrying the id |
//! | `trust_inbound` (bool) | `false` | when true, reuse a well-formed inbound `header` value instead of generating; when false, always generate (the inbound value is ignored) |
//!
//! An inbound id is only trusted when it is a short, printable ASCII token
//! (no control characters, no whitespace, ≤ 200 bytes). A trusted value that
//! fails that check is replaced with a generated id rather than reflected — a
//! client must not be able to smuggle CR/LF or oversized junk into logs and
//! downstream headers through a "trusted" correlation id.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ephpm_middleware::{Middleware, Request, Response, ResponseMiddleware, ResponseView};

/// Max length of an inbound id we are willing to reflect.
const MAX_INBOUND_LEN: usize = 200;

/// Resolved request-id policy, built once at `init`.
pub struct RequestId {
    /// Header name carrying the id (as configured; used verbatim on the wire).
    header: String,
    /// Whether a well-formed inbound value is reused instead of generated.
    trust_inbound: bool,
}

/// Read an optional string config key with a default.
fn opt_string(config: &serde_json::Value, key: &str, default: &str) -> Result<String, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default.to_owned()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

/// Read an optional boolean config key with a default.
fn opt_bool(config: &serde_json::Value, key: &str, default: bool) -> Result<bool, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("`{key}` must be a boolean, got {other}")),
    }
}

/// True when `id` is a safe correlation token: non-empty, ≤ [`MAX_INBOUND_LEN`]
/// bytes, and every byte a printable ASCII graphic (no controls, no spaces).
fn is_safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_INBOUND_LEN && id.bytes().all(|b| b.is_ascii_graphic())
}

/// Per-process entropy mixed into every generated id, so two processes (or two
/// restarts) do not produce the same id stream. Seeded once from wall-clock
/// nanos and the address of a stack local.
static SEED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Per-thread SplitMix64 state. Lazily seeded from the process seed, the
    /// thread identity, and a monotonic counter so distinct threads never
    /// walk the same sequence.
    static RNG: Cell<u64> = const { Cell::new(0) };
}

/// SplitMix64 — a tiny, fast, well-distributed 64-bit generator. Not
/// cryptographic; request ids need collision resistance, not unpredictability.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Initialise the process seed exactly once.
fn ensure_seed() {
    if SEED.load(Ordering::Relaxed) == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x1234_5678, |d| d.as_nanos() as u64);
        let local = 0u8;
        let addr = std::ptr::from_ref(&local) as u64;
        let mut s = nanos ^ addr.rotate_left(17) ^ 0xA5A5_5A5A_1234_ABCD;
        if s == 0 {
            s = 0xDEAD_BEEF_CAFE_F00D;
        }
        // Racy is fine: any winner leaves a usable non-zero seed.
        SEED.store(s, Ordering::Relaxed);
    }
}

/// Draw the next two 64-bit words of randomness from the thread-local RNG.
fn next_u128() -> (u64, u64) {
    ensure_seed();
    RNG.with(|cell| {
        let mut state = cell.get();
        if state == 0 {
            static THREAD_COUNTER: AtomicU64 = AtomicU64::new(1);
            let tc = THREAD_COUNTER.fetch_add(1, Ordering::Relaxed);
            state = SEED
                .load(Ordering::Relaxed)
                .wrapping_mul(0x2545_F491_4F6C_DD1D)
                .wrapping_add(tc.rotate_left(32));
            if state == 0 {
                state = 0x1;
            }
        }
        let hi = splitmix64(&mut state);
        let lo = splitmix64(&mut state);
        cell.set(state);
        (hi, lo)
    })
}

/// Generate a random UUIDv4 string (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`).
fn generate_id() -> String {
    let (mut hi, mut lo) = next_u128();
    // Version 4 in the high nibble of byte 6.
    hi = (hi & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    // Variant 10xx in the two high bits of byte 8.
    lo = (lo & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    let b = |v: u64, shift: u32| ((v >> shift) & 0xFF) as u8;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b(hi, 56),
        b(hi, 48),
        b(hi, 40),
        b(hi, 32),
        b(hi, 24),
        b(hi, 16),
        b(hi, 8),
        b(hi, 0),
        b(lo, 56),
        b(lo, 48),
        b(lo, 40),
        b(lo, 32),
        b(lo, 24),
        b(lo, 16),
        b(lo, 8),
        b(lo, 0),
    )
}

impl RequestId {
    /// The id to use for this request: a trusted, well-formed inbound value
    /// when `trust_inbound` is on, otherwise a freshly generated UUIDv4.
    fn resolve(&self, inbound: Option<&str>) -> String {
        if self.trust_inbound
            && let Some(v) = inbound
            && is_safe_id(v)
        {
            return v.to_owned();
        }
        generate_id()
    }
}

impl Middleware for RequestId {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let header = opt_string(config, "header", "X-Request-Id")?;
        if header.is_empty() {
            return Err("`header` must not be empty".into());
        }
        Ok(Self { header, trust_inbound: opt_bool(config, "trust_inbound", false)? })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        let id = self.resolve(req.header(&self.header));
        // Inject the request header (PHP sees $_SERVER['HTTP_...']) AND echo the
        // same value on the response, so the dynamic path carries exactly the
        // id PHP logged. The response phase fills the header in only when it is
        // still absent (e.g. the static-file path, which runs no request phase).
        Response::rewrite()
            .header(self.header.clone(), id.clone())
            .response_header(self.header.clone(), id)
    }
}

impl ResponseMiddleware for RequestId {
    fn invoke_response(&self, req: &Request<'_>, resp: &mut ResponseView<'_>) {
        // Idempotent: if the header is already present (request phase echoed it,
        // or PHP set its own), leave it untouched — never overwrite or duplicate.
        if resp.header(&self.header).is_some() {
            return;
        }
        // No request phase ran for this response (static file / short-circuit),
        // so honor a trusted inbound value or generate a fresh id.
        let id = self.resolve(req.header(&self.header));
        resp.set_header(self.header.clone(), id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request / Response views by hand.

    use ephpm_middleware::abi::ACTION_REWRITE;
    use ephpm_middleware::host::{RequestCtx, ResponseCtx, host_table};

    use super::*;

    fn init(config: serde_json::Value) -> RequestId {
        RequestId::init(&config).expect("init")
    }

    fn hdr(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    fn invoke(mw: &RequestId, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new("GET", "/index.php", "", "203.0.113.9", "example.test", headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    /// Drive the response phase against a fabricated response, returning the
    /// resulting `(status, headers, body)`.
    fn invoke_response(
        mw: &RequestId,
        req_headers: &[(String, String)],
        resp_status: u16,
        resp_headers: Vec<(String, String)>,
        resp_body: &[u8],
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let ctx = RequestCtx::new("GET", "/", "", "203.0.113.9", "example.test", req_headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        let mut rctx = ResponseCtx::new(resp_status, resp_headers, resp_body.to_vec());
        {
            // SAFETY: `rctx` outlives the view; host_table() is 'static.
            let mut view = unsafe { ResponseView::from_raw(rctx.as_ptr(), host_table()) };
            mw.invoke_response(&req, &mut view);
            let (status, body, set, remove) = view.__into_parts();
            for name in remove {
                rctx.remove_header(&name);
            }
            for (n, v) in set {
                rctx.set_header(&n, &v);
            }
            if let Some(s) = status {
                rctx.set_status(s);
            }
            if let Some(b) = body {
                rctx.replace_body(b);
            }
        }
        rctx.into_parts()
    }

    fn get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn looks_like_uuid(v: &str) -> bool {
        v.len() == 36 && v.as_bytes()[14] == b'4' && v.chars().filter(|c| *c == '-').count() == 4
    }

    // ── request phase ─────────────────────────────────────────────────────

    #[test]
    fn generates_and_injects_when_absent() {
        let mw = init(serde_json::Value::Null);
        let resp = invoke(&mw, &[]);
        assert_eq!(resp.__action(), ACTION_REWRITE);
        // Request header override (PHP-visible).
        let req_id = get(resp.__headers(), "X-Request-Id").expect("request header");
        assert!(looks_like_uuid(req_id), "{req_id}");
        // Response echo — same value.
        let resp_id = get(resp.__response_headers(), "X-Request-Id").expect("response header");
        assert_eq!(req_id, resp_id, "PHP and the client must see the same id");
    }

    #[test]
    fn ignores_inbound_when_not_trusted() {
        let mw = init(serde_json::Value::Null);
        let resp = invoke(&mw, &[hdr("X-Request-Id", "client-supplied-123")]);
        let id = get(resp.__headers(), "X-Request-Id").unwrap();
        assert_ne!(id, "client-supplied-123");
        assert!(looks_like_uuid(id), "{id}");
    }

    #[test]
    fn honors_trusted_inbound() {
        let mw = init(serde_json::json!({ "trust_inbound": true }));
        let resp = invoke(&mw, &[hdr("X-Request-Id", "abc-123-DEF")]);
        assert_eq!(get(resp.__headers(), "X-Request-Id"), Some("abc-123-DEF"));
        assert_eq!(get(resp.__response_headers(), "X-Request-Id"), Some("abc-123-DEF"));
    }

    #[test]
    fn trusted_but_unsafe_inbound_is_regenerated() {
        let mw = init(serde_json::json!({ "trust_inbound": true }));
        // CR/LF injection attempt — must not be reflected.
        let resp = invoke(&mw, &[hdr("X-Request-Id", "bad\r\nInjected: 1")]);
        let id = get(resp.__headers(), "X-Request-Id").unwrap();
        assert!(looks_like_uuid(id), "{id}");
    }

    #[test]
    fn custom_header_name() {
        let mw = init(serde_json::json!({ "header": "X-Correlation-Id", "trust_inbound": true }));
        let resp = invoke(&mw, &[hdr("X-Correlation-Id", "corr-1")]);
        assert_eq!(get(resp.__headers(), "X-Correlation-Id"), Some("corr-1"));
    }

    #[test]
    fn generated_ids_are_unique() {
        let mw = init(serde_json::Value::Null);
        let a = get(invoke(&mw, &[]).__headers(), "X-Request-Id").unwrap().to_owned();
        let b = get(invoke(&mw, &[]).__headers(), "X-Request-Id").unwrap().to_owned();
        assert_ne!(a, b);
    }

    // ── response phase ────────────────────────────────────────────────────

    #[test]
    fn response_phase_adds_header_when_absent() {
        // Static-file path: no request phase ran, response has no id yet.
        let mw = init(serde_json::Value::Null);
        let (_status, headers, _body) = invoke_response(&mw, &[], 200, vec![], b"body");
        let id = get(&headers, "X-Request-Id").expect("id added");
        assert!(looks_like_uuid(id), "{id}");
    }

    #[test]
    fn response_phase_is_idempotent_when_present() {
        // PHP path: the request phase already echoed the id — do not overwrite.
        let mw = init(serde_json::Value::Null);
        let (_status, headers, _body) =
            invoke_response(&mw, &[], 200, vec![hdr("X-Request-Id", "existing-id-42")], b"body");
        assert_eq!(get(&headers, "X-Request-Id"), Some("existing-id-42"));
        // Exactly one occurrence — no duplicate.
        assert_eq!(
            headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("X-Request-Id")).count(),
            1
        );
    }

    #[test]
    fn response_phase_honors_trusted_inbound_on_static_path() {
        let mw = init(serde_json::json!({ "trust_inbound": true }));
        let (_status, headers, _body) =
            invoke_response(&mw, &[hdr("X-Request-Id", "inbound-77")], 200, vec![], b"body");
        assert_eq!(get(&headers, "X-Request-Id"), Some("inbound-77"));
    }

    // ── config validation ─────────────────────────────────────────────────

    #[test]
    fn bad_config_fails_init() {
        assert!(RequestId::init(&serde_json::json!({ "header": "" })).is_err());
        assert!(RequestId::init(&serde_json::json!({ "header": 5 })).is_err());
        assert!(RequestId::init(&serde_json::json!({ "trust_inbound": "yes" })).is_err());
    }
}
