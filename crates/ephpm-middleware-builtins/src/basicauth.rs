//! `basic-auth` — ePHPm native middleware gating a whole site behind
//! HTTP Basic authentication (RFC 7617) before PHP runs.
//!
//! The motivating deployment is per-PR preview hosting: a preview of a
//! *private* repository must not be publicly readable, the credential has to
//! appear and disappear with the preview, and an unauthorised request must
//! never reach the interpreter. Because the whole site is gated rather than a
//! route, this belongs in the middleware chain — a `RESPOND` verdict here
//! short-circuits before the request body is read and before PHP dispatch.
//!
//! # Where credentials come from
//!
//! [`MiddlewareMount`](ephpm_config::MiddlewareMount) matches on **path glob
//! only** — there is no host or site matcher — so a per-site credential
//! cannot be expressed as one mount per site. Instead a single global mount
//! resolves credentials at request time, from either source:
//!
//! - **`users`** — a static `{ username = "<hash>" }` table in the mount's
//!   `config`. Applies to every vhost. Right for a single-site deployment.
//! - **`kv_prefix`** — a KV lookup at `<kv_prefix><vhost>` whose value is the
//!   same `{ username: "<hash>" }` JSON object. This is what makes previews
//!   workable: the control plane writes one key when it creates a preview and
//!   deletes it when the preview dies, with no `ephpm.toml` edit and no
//!   restart.
//!
//! When both are configured the KV entry wins for that vhost and `users` is
//! the fallback for vhosts with no entry. A KV entry whose value is exactly
//! `public` marks a managed site as deliberately un-gated, so a fleet mixing
//! public and private previews can keep the fail-closed
//! `missing_credentials = "deny"` default instead of opening every unknown
//! `Host`.
//!
//! The lookup key is the request's vhost identity **normalised** (lowercased,
//! trailing FQDN dot stripped) by [`normalize_vhost`] — `Host` is
//! client-controlled and case-insensitive, and using it raw would let
//! `Host: Site.Example` miss `site.example`'s credential.
//!
//! The KV callbacks on the middleware host table are **process-global**, not
//! scoped to the request's site (issue #376). For an operator-level credential
//! store that is the semantics this middleware wants and relies on
//! deliberately: the control plane writing `basicauth:preview-42.example` must
//! reach the same store the middleware reads, and a tenant's PHP must *not* be
//! able to reach it at all. In multi-tenant (`sites_dir`) mode PHP's
//! `ephpm_kv_*` hits that vhost's own store, so a tenant cannot read or
//! rewrite its own credential — which is the property you want here.
//!
//! The flip side, and a real limitation today: with `sites_dir` set the RESP
//! listener scopes every authenticated connection to one site's store, so
//! there is no `ephpm kv set` path to the process-global store this module
//! reads (issue #384). KV-sourced credentials are therefore usable in
//! single-site mode; multi-tenant deployments need the static `users` table
//! until #384 lands.
//!
//! # Passwords are stored hashed
//!
//! Both sources hold PBKDF2-HMAC-SHA256 hashes in the PHC-style encoding
//! documented in [`crate::password_hash`], never plaintext. A value that does
//! not parse as one is a **startup error** for `users` and a rejected
//! credential set for KV — there is no plaintext fallback to accidentally
//! depend on. Generate them with `ephpm hash-password`.
//!
//! Verification is [`PasswordHash::verify`], i.e. `aws-lc-rs`'s constant-time
//! `pbkdf2::verify`. An **unknown username still performs a full derivation**
//! against a decoy hash, so response latency does not tell an attacker which
//! usernames exist. That property is asserted by
//! `unknown_user_costs_the_same_kdf_work_as_a_known_one` below, which fails if
//! anyone reintroduces an early return on the "no such user" path.
//!
//! Because a KDF that is worth using is slow, successful verifications are
//! cached for `cache_ttl_secs` keyed by
//! `HMAC(per-process key, vhost ‖ user ‖ password ‖ stored hash)`. The stored
//! hash is part of the key, so **rotating or removing a credential takes
//! effect on the next request** — the TTL bounds memory, not revocation.
//! Failures are never cached, so a brute-force attempt pays the full KDF every
//! time. Pair the mount with `ratelimit` (see below) to bound that.
//!
//! # Transport security
//!
//! Basic sends base64, which is not encryption. What to do about a plain-HTTP
//! request is the `require_https` knob, and its default is deliberate — see
//! the table below and the guide.
//!
//! # Composing with `ratelimit`
//!
//! Brute-force protection is a chain decision, not a reimplementation. Mount
//! `ratelimit` at a lower `order` so it runs first:
//!
//! ```toml
//! [[middleware]]
//! library = "ratelimit"
//! order = 10
//! config = { per_ip_rps = 2, burst = 10 }
//!
//! [[middleware]]
//! library = "basic-auth"
//! order = 20
//! config = { kv_prefix = "basicauth:", realm = "Preview" }
//! ```
//!
//! Ordered this way an over-budget client is turned away with `429` before
//! this module spends a KDF on it.
//!
//! # Configuration (`[[middleware]] config = { ... }`)
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `users` (object) | `{}` | static username → PBKDF2 hash table, applied to every vhost |
//! | `kv_prefix` (string) | unset | look up `<kv_prefix><vhost>` in the KV store for a per-site credential table |
//! | `realm` (string) | `"Restricted"` | `WWW-Authenticate` realm. No `"`, `\` or control characters |
//! | `require_https` (string) | `"warn"` | `"enforce"` / `"warn"` / `"off"` — what a non-HTTPS request gets |
//! | `missing_credentials` (string) | `"deny"` | `"deny"` / `"allow"` — what a vhost with no credential table gets |
//! | `forward_user_header` (string) | unset | on success, REWRITE with this request header set to the authenticated username (our SAPI does not populate `PHP_AUTH_USER` — issue #386) |
//! | `cache_ttl_secs` (integer) | `300` | how long a successful verification is remembered; `0` disables the cache |
//! | `cache_max_entries` (integer) | `4096` | hard ceiling on cached verifications |
//!
//! At least one of `users` or `kv_prefix` must be set: a mount that can never
//! authenticate anyone is a configuration error, not a locked door, so it
//! fails startup.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64ct::{Base64, Base64Unpadded, Encoding};
use dashmap::DashMap;
use ephpm_middleware::abi::{LOG_ERROR, LOG_WARN};
use ephpm_middleware::{Host, Middleware, Request, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::password_hash::{DEFAULT_ITERATIONS, PasswordHash};

/// Largest KV credential document this module will parse, in bytes. A
/// credential table is a handful of entries; anything larger is a mistake or
/// an attempt to make every request pay for a big JSON parse.
const MAX_KV_DOCUMENT_BYTES: usize = 64 * 1024;

/// Minimum seconds between `require_https = "warn"` log lines, so a busy
/// plain-HTTP deployment warns rather than floods.
const WARN_INTERVAL_SECS: u64 = 60;

/// What a request that did not arrive over HTTPS gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpsPolicy {
    /// Refuse to issue the challenge: `403`, no `WWW-Authenticate`.
    Enforce,
    /// Issue the challenge anyway, logging a throttled warning.
    Warn,
    /// Issue the challenge, say nothing.
    Off,
}

/// What a vhost with no resolvable credential table gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingPolicy {
    /// `403`. The safe default: an unconfigured site is not an open site.
    Deny,
    /// Continue to PHP unauthenticated. For a fleet that mixes public and
    /// private sites behind one mount.
    Allow,
}

/// Username → password hash for one vhost.
type Credentials = HashMap<String, PasswordHash>;

/// What a vhost's credential source resolved to.
enum Source<'a> {
    /// Authenticate against this table.
    Table(std::borrow::Cow<'a, Credentials>),
    /// The control plane deliberately marked this site public.
    Public,
    /// No credential source for this vhost — `missing_credentials` decides.
    None,
}

/// HTTP Basic authentication policy, built once at `init`.
pub struct BasicAuth {
    realm: String,
    users: Credentials,
    kv_prefix: Option<String>,
    require_https: HttpsPolicy,
    missing_credentials: MissingPolicy,
    forward_user_header: Option<String>,
    cache: VerifyCache,
    /// Burned when the presented username is unknown, so the "no such user"
    /// path costs the same derivation a real one does.
    decoy: PasswordHash,
    /// Unix second of the last throttled warning (0 = never).
    last_warn: AtomicU64,
}

/// Redacting `Debug`: never let a credential table, a cache key or a decoy
/// hash reach a log line or a test failure message. Only the policy knobs and
/// the *shape* of the credential sources are shown.
impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuth")
            .field("realm", &self.realm)
            .field("static_users", &self.users.len())
            .field("kv_prefix", &self.kv_prefix)
            .field("require_https", &self.require_https)
            .field("missing_credentials", &self.missing_credentials)
            .field("forward_user_header", &self.forward_user_header)
            .field("cache_enabled", &self.cache.enabled())
            .finish_non_exhaustive()
    }
}

// ── Verification cache ────────────────────────────────────────────────────

/// Remembers successful verifications so the steady-state request path costs
/// one HMAC instead of a full KDF.
///
/// The cache key commits to the **stored hash** as well as the presented
/// credential, so changing or removing a credential invalidates its entry
/// immediately; the TTL only bounds how long an unused entry occupies memory.
/// Only successes are stored — a wrong password pays the full KDF every time,
/// which is the cost that makes brute force expensive.
struct VerifyCache {
    /// Per-process HMAC key. Cache keys are unpredictable to anyone who has
    /// not compromised the process, so the map's (non-constant-time) lookup
    /// is not an oracle on the credential.
    key: [u8; 32],
    entries: DashMap<[u8; 32], Instant>,
    ttl: Duration,
    max_entries: usize,
}

impl VerifyCache {
    fn new(ttl_secs: u64, max_entries: usize) -> Result<Self, String> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| {
            format!(
                "failed to draw the verification-cache key from OS entropy: {e}. \
                 Refusing to start rather than use a predictable key."
            )
        })?;
        Ok(Self { key, entries: DashMap::new(), ttl: Duration::from_secs(ttl_secs), max_entries })
    }

    fn enabled(&self) -> bool {
        !self.ttl.is_zero() && self.max_entries > 0
    }

    /// Tag identifying one (vhost, user, password, stored hash) tuple.
    ///
    /// Fields are length-prefixed so no two different tuples can serialise to
    /// the same byte string (`("ab", "c")` vs `("a", "bc")`).
    fn tag(&self, vhost: &str, user: &str, password: &str, stored: &PasswordHash) -> [u8; 32] {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        for field in [vhost.as_bytes(), user.as_bytes(), password.as_bytes()] {
            mac.update(&(field.len() as u64).to_le_bytes());
            mac.update(field);
        }
        let phc = stored.to_phc_string();
        mac.update(&(phc.len() as u64).to_le_bytes());
        mac.update(phc.as_bytes());
        mac.finalize().into_bytes().into()
    }

    /// Whether this tuple verified recently. Expired entries are evicted on
    /// the way past.
    fn contains(&self, tag: &[u8; 32]) -> bool {
        match self.entries.get(tag).map(|e| *e.value()) {
            Some(expiry) if expiry > Instant::now() => true,
            Some(_) => {
                self.entries.remove(tag);
                false
            }
            None => false,
        }
    }

    /// Record a success, keeping the map under `max_entries`.
    fn insert(&self, tag: [u8; 32]) {
        if self.entries.len() >= self.max_entries {
            let now = Instant::now();
            self.entries.retain(|_, expiry| *expiry > now);
            if self.entries.len() >= self.max_entries {
                // Full of live entries: skip rather than grow without bound.
                // The only cost is paying the KDF again.
                return;
            }
        }
        self.entries.insert(tag, Instant::now() + self.ttl);
    }
}

// ── Config parsing ────────────────────────────────────────────────────────

/// Read an optional non-empty string field.
fn opt_str(config: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match config.get(key) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        None | Some(serde_json::Value::Null | serde_json::Value::String(_)) => Ok(None),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

/// Read an optional unsigned integer field.
fn opt_u64(config: &serde_json::Value, key: &str, default: u64) -> Result<u64, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(v) => v.as_u64().ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

/// Reject header values and realms that could break out of their header.
fn is_safe_header_text(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| !c.is_control() && c != '"' && c != '\\')
}

/// Parse a `{ username: "<hash>" }` table. `origin` names the source in error
/// messages so a bad `ephpm.toml` and a bad KV document are distinguishable.
fn parse_credentials(value: &serde_json::Value, origin: &str) -> Result<Credentials, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{origin} must be a table of username = \"<hash>\" entries"))?;
    let mut out = Credentials::with_capacity(obj.len());
    for (user, hash) in obj {
        if user.is_empty() {
            return Err(format!("{origin}: username must not be empty"));
        }
        // RFC 7617 splits user-id from password at the FIRST colon, so a
        // username containing one can never be presented.
        if user.contains(':') {
            return Err(format!(
                "{origin}: username `{user}` contains `:`, which HTTP Basic cannot represent"
            ));
        }
        if !is_safe_header_text(user) {
            return Err(format!("{origin}: username `{user}` contains control characters"));
        }
        let hash = hash
            .as_str()
            .ok_or_else(|| format!("{origin}: `{user}` must be a password hash string"))?;
        let parsed = PasswordHash::parse(hash).map_err(|e| format!("{origin}: `{user}`: {e}"))?;
        out.insert(user.clone(), parsed);
    }
    Ok(out)
}

// ── Request handling ──────────────────────────────────────────────────────

/// KV value that marks a site as deliberately public: managed by the control
/// plane, no credential required.
///
/// Spelled as an exact literal rather than "an empty credential table" on
/// purpose. A control plane that writes an empty or truncated document
/// through a *bug* must fail closed; only a deliberate, unmistakable marker
/// opens a site.
const PUBLIC_MARKER: &[u8] = b"public";

/// Canonical vhost key for a credential lookup.
///
/// The middleware ABI's `request_vhost_id` is the router's `SERVER_NAME` —
/// the `Host` header with its port stripped, **but neither lowercased nor
/// stripped of a trailing FQDN-root dot**. Host is case-insensitive and
/// client-controlled, so using it raw would mean `Host: Site.Example` and
/// `Host: site.example` hash to different KV keys: a private site's
/// credential would simply not be found, and under
/// `missing_credentials = "allow"` that is an authentication bypass by
/// changing one letter's case.
///
/// This applies the same normalisation the server's own `normalize_host_key`
/// does to everything else host-keyed (the port is already gone by here).
fn normalize_vhost(vhost: &str) -> String {
    vhost.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase()
}

/// Decode an `Authorization: Basic <base64>` value into `(user, password)`.
///
/// Returns `None` for any other scheme or malformed input. Per RFC 7617 the
/// decoded form is `user-id ":" password` split at the **first** colon, and we
/// advertise `charset="UTF-8"`, so non-UTF-8 credentials are rejected rather
/// than silently reinterpreted.
fn parse_basic(raw: &str) -> Option<(String, String)> {
    let (scheme, encoded) = raw.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let encoded = encoded.trim();
    // Padded is what the RFC specifies and what clients send; accept unpadded
    // too rather than 401 a credential that is otherwise perfectly valid.
    let decoded =
        Base64::decode_vec(encoded).or_else(|_| Base64Unpadded::decode_vec(encoded)).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_owned(), password.to_owned()))
}

impl BasicAuth {
    /// `401` plus the `WWW-Authenticate` challenge (RFC 7617 §2: `realm` and
    /// `charset="UTF-8"`).
    fn challenge(&self) -> Response {
        Response::respond(401, "401 Unauthorized")
            .header(
                "WWW-Authenticate",
                format!("Basic realm=\"{}\", charset=\"UTF-8\"", self.realm),
            )
            // A 401 is per-credential; never let a shared cache keep it.
            .header("Cache-Control", "no-store")
            .header("Content-Type", "text/plain; charset=utf-8")
    }

    /// Refusal that deliberately carries **no** challenge — used where
    /// inviting the client to send credentials would be wrong.
    fn refuse(body: &'static str) -> Response {
        Response::respond(403, body)
            .header("Cache-Control", "no-store")
            .header("Content-Type", "text/plain; charset=utf-8")
    }

    /// Emit `message` at most once per [`WARN_INTERVAL_SECS`].
    fn warn_throttled(&self, host: &Host<'_>, message: &str) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let last = self.last_warn.load(Ordering::Relaxed);
        if now.saturating_sub(last) < WARN_INTERVAL_SECS && last != 0 {
            return;
        }
        if self.last_warn.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
        {
            host.log(LOG_WARN, message);
        }
    }

    /// What this vhost's credential source says: a table to authenticate
    /// against, an explicit "this site is public", or nothing at all.
    ///
    /// The KV entry (when `kv_prefix` is configured) wins; a vhost with no
    /// entry falls back to the static `users` table. A KV document that fails
    /// to parse yields [`Source::None`] and logs — it must never fall through
    /// to the static table, or a corrupt per-site credential would silently
    /// inherit whatever global credential happens to be configured.
    fn credentials_for<'a>(&'a self, vhost: &str, host: &Host<'_>) -> Source<'a> {
        if let Some(prefix) = &self.kv_prefix {
            let key = format!("{prefix}{vhost}");
            if let Some(raw) = host.kv_get(&key) {
                if raw.len() > MAX_KV_DOCUMENT_BYTES {
                    host.log(
                        LOG_ERROR,
                        &format!(
                            "basic-auth: credential document at `{key}` is {} bytes \
                             (limit {MAX_KV_DOCUMENT_BYTES}) — ignoring",
                            raw.len()
                        ),
                    );
                    return Source::None;
                }
                // The one deliberate way to declare a managed site public.
                if raw == PUBLIC_MARKER {
                    return Source::Public;
                }
                let parsed = serde_json::from_slice::<serde_json::Value>(&raw)
                    .map_err(|e| e.to_string())
                    .and_then(|v| parse_credentials(&v, "KV credential document"));
                return match parsed {
                    Ok(creds) if !creds.is_empty() => Source::Table(std::borrow::Cow::Owned(creds)),
                    // An empty table is NOT "public" — see PUBLIC_MARKER.
                    Ok(_) => Source::None,
                    Err(e) => {
                        host.log(
                            LOG_ERROR,
                            &format!("basic-auth: unusable credential document at `{key}`: {e}"),
                        );
                        Source::None
                    }
                };
            }
        }
        if self.users.is_empty() {
            Source::None
        } else {
            Source::Table(std::borrow::Cow::Borrowed(&self.users))
        }
    }

    /// Verify a presented credential against `creds`, doing the same amount of
    /// KDF work whether or not the username exists.
    fn verify(&self, vhost: &str, user: &str, password: &str, creds: &Credentials) -> bool {
        // Deliberately NOT `let Some(stored) = creds.get(user) else { return
        // false }`: returning early here makes response latency an oracle for
        // which usernames exist. The decoy costs one derivation, exactly as a
        // real miss would.
        let Some(stored) = creds.get(user) else {
            let _ = self.decoy.verify(password);
            return false;
        };

        if self.cache.enabled() {
            let tag = self.cache.tag(vhost, user, password, stored);
            if self.cache.contains(&tag) {
                return true;
            }
            if stored.verify(password) {
                self.cache.insert(tag);
                return true;
            }
            return false;
        }
        stored.verify(password)
    }
}

impl Middleware for BasicAuth {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let users = match config.get("users") {
            None | Some(serde_json::Value::Null) => Credentials::new(),
            Some(v) => parse_credentials(v, "`users`")?,
        };
        let kv_prefix = opt_str(config, "kv_prefix")?;
        if users.is_empty() && kv_prefix.is_none() {
            return Err("at least one of `users` or `kv_prefix` is required — a mount with no \
                 credential source can never authenticate anyone"
                .into());
        }

        let realm = opt_str(config, "realm")?.unwrap_or_else(|| "Restricted".to_owned());
        if !is_safe_header_text(&realm) {
            return Err(
                "`realm` must not be empty or contain `\"`, `\\` or control characters".into()
            );
        }

        let require_https = match opt_str(config, "require_https")?.as_deref() {
            None | Some("warn") => HttpsPolicy::Warn,
            Some("enforce") => HttpsPolicy::Enforce,
            Some("off") => HttpsPolicy::Off,
            Some(other) => {
                return Err(format!(
                    "`require_https` must be \"enforce\", \"warn\" or \"off\", got \"{other}\""
                ));
            }
        };
        let missing_credentials = match opt_str(config, "missing_credentials")?.as_deref() {
            None | Some("deny") => MissingPolicy::Deny,
            Some("allow") => MissingPolicy::Allow,
            Some(other) => {
                return Err(format!(
                    "`missing_credentials` must be \"deny\" or \"allow\", got \"{other}\""
                ));
            }
        };

        let forward_user_header = opt_str(config, "forward_user_header")?;
        if let Some(name) = &forward_user_header
            && !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(format!(
                "`forward_user_header` must be a header name (letters, digits, `-`), got `{name}`"
            ));
        }

        let cache_ttl_secs = opt_u64(config, "cache_ttl_secs", 300)?;
        let cache_max_entries = usize::try_from(opt_u64(config, "cache_max_entries", 4096)?)
            .map_err(|_| "`cache_max_entries` is too large".to_owned())?;
        let cache = VerifyCache::new(cache_ttl_secs, cache_max_entries)?;

        // A decoy at the generation cost, drawn from fresh entropy: its
        // password is unknowable, so it can only ever fail to verify.
        let mut filler = [0u8; 32];
        getrandom::fill(&mut filler)
            .map_err(|e| format!("failed to draw the decoy credential from OS entropy: {e}"))?;
        let decoy = PasswordHash::generate(&Base64::encode_string(&filler), DEFAULT_ITERATIONS)?;

        Ok(Self {
            realm,
            users,
            kv_prefix,
            require_https,
            missing_credentials,
            forward_user_header,
            cache,
            decoy,
            last_warn: AtomicU64::new(0),
        })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        let host = req.host();
        // Host is client-controlled and case-insensitive; normalise before it
        // is used as a credential-lookup key. See `normalize_vhost`.
        let vhost = normalize_vhost(req.vhost_id());

        let creds = match self.credentials_for(&vhost, &host) {
            Source::Table(creds) => creds,
            Source::Public => return Response::cont(),
            Source::None => {
                return match self.missing_credentials {
                    MissingPolicy::Allow => Response::cont(),
                    MissingPolicy::Deny => {
                        self.warn_throttled(
                            &host,
                            &format!(
                                "basic-auth: no credentials configured for vhost `{vhost}` — \
                                 denying. Write a credential for it, mark it public, or set \
                                 `missing_credentials = \"allow\"`."
                            ),
                        );
                        Self::refuse("403 Forbidden: this site has no credentials configured")
                    }
                };
            }
        };

        // Basic sends base64, which is not encryption. `is_https()` is the
        // determination PHP sees, i.e. already through trusted-proxy
        // `X-Forwarded-Proto` resolution — see the knob's docs for why the
        // default is to warn rather than refuse.
        if !req.is_https() {
            match self.require_https {
                HttpsPolicy::Enforce => {
                    return Self::refuse(
                        "403 Forbidden: authentication requires HTTPS. If TLS terminates \
                         at a proxy, add that proxy to [server.security] trusted_proxies \
                         so its X-Forwarded-Proto is honoured.",
                    );
                }
                HttpsPolicy::Warn => self.warn_throttled(
                    &host,
                    &format!(
                        "basic-auth: challenging over plain HTTP on vhost `{vhost}` — \
                         credentials are base64, not encrypted. If TLS terminates at a \
                         proxy, add it to [server.security] trusted_proxies; otherwise \
                         enable TLS and set require_https = \"enforce\"."
                    ),
                ),
                HttpsPolicy::Off => {}
            }
        }

        let Some(raw) = req.header("Authorization") else {
            return self.challenge();
        };
        let Some((user, password)) = parse_basic(raw) else {
            return self.challenge();
        };
        if !self.verify(&vhost, &user, &password, &creds) {
            return self.challenge();
        }

        match &self.forward_user_header {
            // REWRITE header overrides *replace* any same-named header the
            // client sent, so a forged `X-Auth-User` cannot survive this.
            Some(name) => Response::rewrite().header(name.as_str(), user),
            None => Response::cont(),
        }
    }

    fn describe() -> &'static str {
        concat!("basic-auth ", env!("CARGO_PKG_VERSION"), " (HTTP Basic authentication)")
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND, ACTION_REWRITE};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;
    use crate::password_hash::{MIN_ITERATIONS, kdf_invocations};

    /// Cheap-but-legal cost for tests; the real default is 100 000.
    fn hash(password: &str) -> String {
        PasswordHash::generate(password, MIN_ITERATIONS).expect("hash").to_phc_string()
    }

    fn basic_header(user: &str, password: &str) -> Vec<(String, String)> {
        let encoded = Base64::encode_string(format!("{user}:{password}").as_bytes());
        vec![("Authorization".to_owned(), format!("Basic {encoded}"))]
    }

    fn mw(config: &serde_json::Value) -> BasicAuth {
        BasicAuth::init(config).expect("init")
    }

    /// Drive one request. `https` is the router's resolved determination.
    fn invoke_on(
        mw: &BasicAuth,
        vhost: &str,
        https: bool,
        headers: &[(String, String)],
    ) -> Response {
        let ctx = RequestCtx::new("GET", "/index.php", "", "203.0.113.9", vhost, headers)
            .with_https(https);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn invoke(mw: &BasicAuth, headers: &[(String, String)]) -> Response {
        invoke_on(mw, "site.example", true, headers)
    }

    fn header_of<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.__headers().iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Wire the crate-shared store and hand it back, so a test can delete a
    /// key directly — the v1 middleware ABI has no delete callback.
    fn wire_kv() -> &'static std::sync::Arc<ephpm_kv::store::Store> {
        crate::test_kv::wire()
    }

    // ── Configuration ────────────────────────────────────────────────────

    #[test]
    fn init_requires_a_credential_source() {
        let err = BasicAuth::init(&serde_json::Value::Null).expect_err("must fail");
        assert!(err.contains("`users` or `kv_prefix`"), "{err}");
        let err = BasicAuth::init(&serde_json::json!({ "users": {} })).expect_err("must fail");
        assert!(err.contains("`users` or `kv_prefix`"), "{err}");
        // Either source alone is enough.
        assert!(BasicAuth::init(&serde_json::json!({ "kv_prefix": "ba:" })).is_ok());
    }

    #[test]
    fn init_rejects_plaintext_passwords() {
        // The whole point: there is no plaintext fallback to lean on.
        let err = BasicAuth::init(&serde_json::json!({ "users": { "bob": "hunter2" } }))
            .expect_err("plaintext must be refused");
        assert!(err.contains("not a password hash"), "{err}");
        assert!(err.contains("hash-password"), "error should say how to make one: {err}");
    }

    #[test]
    fn init_rejects_unrepresentable_and_unsafe_usernames() {
        let h = hash("pw");
        let err = BasicAuth::init(&serde_json::json!({ "users": { "a:b": h.clone() } }))
            .expect_err("must reject");
        assert!(err.contains("cannot represent"), "{err}");
        let err = BasicAuth::init(&serde_json::json!({ "users": { "a\nb": h.clone() } }))
            .expect_err("must reject");
        assert!(err.contains("control characters"), "{err}");
        let err =
            BasicAuth::init(&serde_json::json!({ "users": { "": h } })).expect_err("must reject");
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn init_rejects_a_realm_that_would_break_the_header() {
        let cfg = serde_json::json!({
            "kv_prefix": "ba:",
            "realm": "evil\", charset=\"UTF-8\", x=\"",
        });
        assert!(BasicAuth::init(&cfg).is_err(), "a quote in the realm must be refused");
    }

    #[test]
    fn init_rejects_unknown_policy_values() {
        for (key, value) in
            [("require_https", "yes"), ("missing_credentials", "maybe"), ("require_https", "")]
        {
            let cfg = serde_json::json!({ "kv_prefix": "ba:", key: value });
            // An empty string reads as "unset" and is accepted; anything else
            // must be refused rather than silently defaulted.
            if value.is_empty() {
                assert!(BasicAuth::init(&cfg).is_ok(), "empty {key} should mean unset");
            } else {
                assert!(BasicAuth::init(&cfg).is_err(), "{key} = {value} must be refused");
            }
        }
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let mw = mw(&serde_json::json!({ "kv_prefix": "ba:" }));
        assert_eq!(mw.realm, "Restricted");
        assert_eq!(mw.require_https, HttpsPolicy::Warn);
        assert_eq!(mw.missing_credentials, MissingPolicy::Deny);
        assert!(mw.forward_user_header.is_none());
        assert!(mw.cache.enabled());
    }

    // ── The 401 challenge ────────────────────────────────────────────────

    #[test]
    fn no_credentials_gets_401_with_a_conformant_challenge() {
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        let resp = invoke(&mw, &[]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 401);
        // RFC 7617: scheme, realm, and the charset parameter.
        let challenge = header_of(&resp, "WWW-Authenticate").expect("challenge header");
        assert_eq!(challenge, "Basic realm=\"Restricted\", charset=\"UTF-8\"");
        assert_eq!(header_of(&resp, "Cache-Control"), Some("no-store"));
    }

    #[test]
    fn realm_is_configurable() {
        let mw = mw(&serde_json::json!({
            "users": { "bob": hash("pw") },
            "realm": "PR Preview",
        }));
        assert_eq!(
            header_of(&invoke(&mw, &[]), "WWW-Authenticate"),
            Some("Basic realm=\"PR Preview\", charset=\"UTF-8\"")
        );
    }

    #[test]
    fn wrong_password_and_unknown_user_are_401() {
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        assert_eq!(invoke(&mw, &basic_header("bob", "nope")).__status(), 401);
        assert_eq!(invoke(&mw, &basic_header("eve", "pw")).__status(), 401);
        // Both still carry the challenge, so a client can retry.
        assert!(header_of(&invoke(&mw, &basic_header("eve", "pw")), "WWW-Authenticate").is_some());
    }

    #[test]
    fn correct_password_continues_to_php() {
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("s3cret") } }));
        assert_eq!(invoke(&mw, &basic_header("bob", "s3cret")).__action(), ACTION_CONTINUE);
    }

    #[test]
    fn malformed_authorization_headers_are_401_not_500() {
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        for value in [
            "Basic",
            "Basic ",
            "Basic !!!not-base64!!!",
            "Bearer abc.def.ghi",
            "Basic Ym9i",     // "bob" — no colon
            "Basic //79/A==", // valid base64, invalid UTF-8
            "basic",
        ] {
            let headers = vec![("Authorization".to_owned(), value.to_owned())];
            let resp = invoke(&mw, &headers);
            assert_eq!(resp.__status(), 401, "value {value:?} should 401");
        }
    }

    #[test]
    fn a_password_may_contain_colons() {
        // RFC 7617 splits at the FIRST colon; the rest is all password.
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("a:b:c") } }));
        assert_eq!(invoke(&mw, &basic_header("bob", "a:b:c")).__action(), ACTION_CONTINUE);
    }

    // ── Timing / enumeration ─────────────────────────────────────────────

    /// The property that matters for a login endpoint: an unknown username
    /// must cost the same KDF work a known one does, or response latency
    /// becomes a user-enumeration oracle.
    ///
    /// This fails the moment someone "optimises" the miss path with an early
    /// return, which is the realistic way the protection gets lost.
    #[test]
    fn unknown_user_costs_the_same_kdf_work_as_a_known_one() {
        // Cache off, so both arms are measured on the cold path.
        let mw = mw(&serde_json::json!({
            "users": { "bob": hash("pw") },
            "cache_ttl_secs": 0,
        }));

        let before = kdf_invocations();
        assert_eq!(invoke(&mw, &basic_header("bob", "wrong")).__status(), 401);
        let known_user_cost = kdf_invocations() - before;

        let before = kdf_invocations();
        assert_eq!(invoke(&mw, &basic_header("nosuchuser", "wrong")).__status(), 401);
        let unknown_user_cost = kdf_invocations() - before;

        assert_eq!(
            known_user_cost, unknown_user_cost,
            "an unknown username must perform the same number of derivations as a \
             known one ({known_user_cost} vs {unknown_user_cost}) — otherwise response \
             latency reveals which usernames exist"
        );
        assert_eq!(known_user_cost, 1, "expected exactly one derivation per attempt");
    }

    #[test]
    fn a_missing_authorization_header_costs_no_kdf() {
        // No credential presented means no secret to compare, so there is
        // nothing to make uniform — and spending a KDF would hand an
        // unauthenticated client a CPU amplifier.
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        let before = kdf_invocations();
        assert_eq!(invoke(&mw, &[]).__status(), 401);
        assert_eq!(kdf_invocations(), before);
    }

    // ── Verification cache ───────────────────────────────────────────────

    #[test]
    fn cache_skips_the_kdf_on_repeat_but_never_on_failure() {
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        let creds = basic_header("bob", "pw");

        let before = kdf_invocations();
        assert_eq!(invoke(&mw, &creds).__action(), ACTION_CONTINUE);
        assert_eq!(kdf_invocations(), before + 1, "first hit derives");

        let before = kdf_invocations();
        assert_eq!(invoke(&mw, &creds).__action(), ACTION_CONTINUE);
        assert_eq!(kdf_invocations(), before, "second hit is cached");

        // A wrong password must never be cached away: brute force pays full
        // price on every attempt.
        let wrong = basic_header("bob", "nope");
        for _ in 0..3 {
            let before = kdf_invocations();
            assert_eq!(invoke(&mw, &wrong).__status(), 401);
            assert_eq!(kdf_invocations(), before + 1);
        }
    }

    #[test]
    fn cache_is_scoped_per_vhost() {
        // The same user/password on a different vhost is a different tuple,
        // so one site's success can never satisfy another's check.
        let mw = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        let creds = basic_header("bob", "pw");
        assert_eq!(invoke_on(&mw, "a.example", true, &creds).__action(), ACTION_CONTINUE);
        let before = kdf_invocations();
        assert_eq!(invoke_on(&mw, "b.example", true, &creds).__action(), ACTION_CONTINUE);
        assert_eq!(
            kdf_invocations(),
            before + 1,
            "a second vhost must derive rather than reuse the first vhost's entry"
        );
    }

    #[test]
    fn cache_ttl_zero_disables_it() {
        let mw = mw(&serde_json::json!({
            "users": { "bob": hash("pw") },
            "cache_ttl_secs": 0,
        }));
        assert!(!mw.cache.enabled());
        let creds = basic_header("bob", "pw");
        for _ in 0..2 {
            let before = kdf_invocations();
            assert_eq!(invoke(&mw, &creds).__action(), ACTION_CONTINUE);
            assert_eq!(kdf_invocations(), before + 1);
        }
    }

    #[test]
    fn cache_stays_under_its_ceiling() {
        let mw = mw(&serde_json::json!({
            "users": { "bob": hash("pw") },
            "cache_max_entries": 2,
        }));
        for vhost in ["v1", "v2", "v3", "v4", "v5"] {
            assert_eq!(
                invoke_on(&mw, vhost, true, &basic_header("bob", "pw")).__action(),
                ACTION_CONTINUE
            );
        }
        assert!(mw.cache.entries.len() <= 2, "cache grew past its ceiling");
    }

    #[test]
    fn rotating_the_stored_hash_invalidates_the_cached_success() {
        // The cache key commits to the stored hash, so revocation is
        // immediate — the TTL only bounds memory.
        let old = mw(&serde_json::json!({ "users": { "bob": hash("pw") } }));
        let creds = basic_header("bob", "pw");
        assert_eq!(invoke(&old, &creds).__action(), ACTION_CONTINUE);

        let stored = old.users.get("bob").expect("entry");
        let tag_old = old.cache.tag("site.example", "bob", "pw", stored);
        let rotated = PasswordHash::generate("pw", MIN_ITERATIONS).expect("rehash");
        let tag_new = old.cache.tag("site.example", "bob", "pw", &rotated);
        assert_ne!(tag_old, tag_new, "a rehash must produce a different cache key");
        assert!(old.cache.contains(&tag_old));
        assert!(!old.cache.contains(&tag_new));
    }

    // ── HTTPS policy ─────────────────────────────────────────────────────

    #[test]
    fn enforce_refuses_to_challenge_over_plain_http() {
        let mw = mw(&serde_json::json!({
            "users": { "bob": hash("pw") },
            "require_https": "enforce",
        }));
        let resp = invoke_on(&mw, "site.example", false, &basic_header("bob", "pw"));
        assert_eq!(resp.__status(), 403);
        assert!(
            header_of(&resp, "WWW-Authenticate").is_none(),
            "enforce must NOT invite the client to send credentials in the clear"
        );
        // The message has to say how to fix it behind a TLS-terminating proxy.
        let body = String::from_utf8_lossy(resp.__body()).into_owned();
        assert!(body.contains("trusted_proxies"), "{body}");
        // Over HTTPS the same request succeeds.
        assert_eq!(
            invoke_on(&mw, "site.example", true, &basic_header("bob", "pw")).__action(),
            ACTION_CONTINUE
        );
    }

    #[test]
    fn warn_and_off_still_authenticate_over_plain_http() {
        for policy in ["warn", "off"] {
            let mw = mw(&serde_json::json!({
                "users": { "bob": hash("pw") },
                "require_https": policy,
            }));
            let resp = invoke_on(&mw, "site.example", false, &[]);
            assert_eq!(resp.__status(), 401, "{policy} should still challenge");
            assert!(header_of(&resp, "WWW-Authenticate").is_some(), "{policy}");
            assert_eq!(
                invoke_on(&mw, "site.example", false, &basic_header("bob", "pw")).__action(),
                ACTION_CONTINUE,
                "{policy} should still authenticate"
            );
        }
    }

    // ── Per-vhost credentials from the KV store ──────────────────────────

    #[test]
    fn kv_credentials_resolve_per_vhost() {
        wire_kv();
        let mw = mw(&serde_json::json!({ "kv_prefix": "t1:basicauth:" }));
        let host = Host::new(host_table());
        let doc_a = serde_json::json!({ "alice": hash("alice-pw") }).to_string();
        let doc_b = serde_json::json!({ "bob": hash("bob-pw") }).to_string();
        assert!(host.kv_set("t1:basicauth:a.example", doc_a.as_bytes(), 0));
        assert!(host.kv_set("t1:basicauth:b.example", doc_b.as_bytes(), 0));

        // Each site accepts only its own credential.
        assert_eq!(
            invoke_on(&mw, "a.example", true, &basic_header("alice", "alice-pw")).__action(),
            ACTION_CONTINUE
        );
        assert_eq!(
            invoke_on(&mw, "b.example", true, &basic_header("bob", "bob-pw")).__action(),
            ACTION_CONTINUE
        );
        // Cross-site credentials do not work.
        assert_eq!(
            invoke_on(&mw, "b.example", true, &basic_header("alice", "alice-pw")).__status(),
            401
        );
        assert_eq!(
            invoke_on(&mw, "a.example", true, &basic_header("bob", "bob-pw")).__status(),
            401
        );
    }

    #[test]
    fn deleting_the_kv_entry_revokes_access_immediately() {
        let store = wire_kv();
        let mw = mw(&serde_json::json!({ "kv_prefix": "t2:basicauth:" }));
        let host = Host::new(host_table());
        let doc = serde_json::json!({ "alice": hash("pw") }).to_string();
        assert!(host.kv_set("t2:basicauth:gone.example", doc.as_bytes(), 0));
        let creds = basic_header("alice", "pw");
        assert_eq!(invoke_on(&mw, "gone.example", true, &creds).__action(), ACTION_CONTINUE);

        // Tear the preview down: the very next request is refused despite the
        // cached success, because there is no longer a credential table at all.
        assert!(store.remove("t2:basicauth:gone.example"));
        assert_eq!(invoke_on(&mw, "gone.example", true, &creds).__status(), 403);
    }

    #[test]
    fn rotating_the_kv_credential_invalidates_the_cache_immediately() {
        // The rotation half of the same story: the credential table still
        // exists, but its hash changed, so the cached tag no longer matches
        // and the old password stops working on the next request.
        wire_kv();
        let mw = mw(&serde_json::json!({ "kv_prefix": "t7:basicauth:" }));
        let host = Host::new(host_table());
        let key = "t7:basicauth:rotate.example";
        let old_doc = serde_json::json!({ "alice": hash("old-pw") }).to_string();
        assert!(host.kv_set(key, old_doc.as_bytes(), 0));
        assert_eq!(
            invoke_on(&mw, "rotate.example", true, &basic_header("alice", "old-pw")).__action(),
            ACTION_CONTINUE
        );

        let new_doc = serde_json::json!({ "alice": hash("new-pw") }).to_string();
        assert!(host.kv_set(key, new_doc.as_bytes(), 0));
        assert_eq!(
            invoke_on(&mw, "rotate.example", true, &basic_header("alice", "old-pw")).__status(),
            401,
            "the old password must stop working immediately, not after the cache TTL"
        );
        assert_eq!(
            invoke_on(&mw, "rotate.example", true, &basic_header("alice", "new-pw")).__action(),
            ACTION_CONTINUE
        );
    }

    #[test]
    fn kv_entry_takes_precedence_over_the_static_table() {
        wire_kv();
        let mw = mw(&serde_json::json!({
            "kv_prefix": "t3:basicauth:",
            "users": { "global": hash("global-pw") },
        }));
        let host = Host::new(host_table());
        let doc = serde_json::json!({ "local": hash("local-pw") }).to_string();
        assert!(host.kv_set("t3:basicauth:override.example", doc.as_bytes(), 0));

        // The per-site table replaces the global one; it does not merge.
        assert_eq!(
            invoke_on(&mw, "override.example", true, &basic_header("local", "local-pw")).__action(),
            ACTION_CONTINUE
        );
        assert_eq!(
            invoke_on(&mw, "override.example", true, &basic_header("global", "global-pw"))
                .__status(),
            401
        );
        // A vhost with no KV entry still falls back to the static table.
        assert_eq!(
            invoke_on(&mw, "fallback.example", true, &basic_header("global", "global-pw"))
                .__action(),
            ACTION_CONTINUE
        );
    }

    #[test]
    fn an_unusable_kv_document_denies_and_never_falls_back() {
        wire_kv();
        let mw = mw(&serde_json::json!({
            "kv_prefix": "t4:basicauth:",
            "users": { "global": hash("global-pw") },
        }));
        let host = Host::new(host_table());
        // Plaintext where a hash belongs, and outright junk.
        for (vhost, doc) in [
            ("plain.example", r#"{"bob":"hunter2"}"#),
            ("junk.example", "not json at all"),
            ("empty.example", "{}"),
        ] {
            assert!(host.kv_set(&format!("t4:basicauth:{vhost}"), doc.as_bytes(), 0));
            // Falling back to the global credential here would silently open
            // the site to whoever holds it.
            assert_eq!(
                invoke_on(&mw, vhost, true, &basic_header("global", "global-pw")).__status(),
                403,
                "vhost {vhost} must deny, not inherit the global credential"
            );
        }
    }

    // ── Unconfigured sites ───────────────────────────────────────────────

    #[test]
    fn missing_credentials_deny_is_the_default_and_carries_no_challenge() {
        wire_kv();
        let mw = mw(&serde_json::json!({ "kv_prefix": "t5:basicauth:" }));
        let resp = invoke_on(&mw, "unknown.example", true, &[]);
        assert_eq!(resp.__status(), 403);
        assert!(
            header_of(&resp, "WWW-Authenticate").is_none(),
            "no credential can ever satisfy this, so do not prompt for one"
        );
    }

    /// `Host` is client-controlled and case-insensitive. Without
    /// normalisation `Host: Locked.Example` would miss the credential this
    /// site's key holds — under `missing_credentials = "allow"` that is an
    /// authentication bypass by changing one letter's case.
    #[test]
    fn vhost_lookup_is_case_and_trailing_dot_insensitive() {
        wire_kv();
        let mw = mw(&serde_json::json!({
            "kv_prefix": "t8:basicauth:",
            "missing_credentials": "allow",
        }));
        let host = Host::new(host_table());
        let doc = serde_json::json!({ "alice": hash("pw") }).to_string();
        assert!(host.kv_set("t8:basicauth:locked.example", doc.as_bytes(), 0));

        // Every spelling of the same host must land on the same credential.
        for spelling in ["locked.example", "Locked.Example", "LOCKED.EXAMPLE", "locked.example."] {
            assert_eq!(
                invoke_on(&mw, spelling, true, &[]).__status(),
                401,
                "Host {spelling:?} must still be gated"
            );
            assert_eq!(
                invoke_on(&mw, spelling, true, &basic_header("alice", "pw")).__action(),
                ACTION_CONTINUE,
                "Host {spelling:?} must accept the site's credential"
            );
        }
    }

    #[test]
    fn an_explicit_public_marker_opens_a_managed_site() {
        wire_kv();
        // Default (deny) policy: a fleet can still mix public and private
        // sites without opening every unknown Host.
        let mw = mw(&serde_json::json!({ "kv_prefix": "t9:basicauth:" }));
        let host = Host::new(host_table());
        assert!(host.kv_set("t9:basicauth:open.example", b"public", 0));
        assert_eq!(invoke_on(&mw, "open.example", true, &[]).__action(), ACTION_CONTINUE);
        // An unmanaged host is still refused.
        assert_eq!(invoke_on(&mw, "unmanaged.example", true, &[]).__status(), 403);
    }

    #[test]
    fn an_empty_or_truncated_document_is_not_public() {
        // A control plane bug that writes an empty/partial document must fail
        // CLOSED. Only the exact `public` marker opens a site.
        wire_kv();
        let mw = mw(&serde_json::json!({ "kv_prefix": "t10:basicauth:" }));
        let host = Host::new(host_table());
        for (vhost, doc) in [
            ("empty.example", "{}"),
            ("blank.example", ""),
            ("partial.example", "{\"alice\":"),
            ("nearly.example", "publicX"),
            ("cased.example", "PUBLIC"),
        ] {
            assert!(host.kv_set(&format!("t10:basicauth:{vhost}"), doc.as_bytes(), 0));
            assert_eq!(
                invoke_on(&mw, vhost, true, &[]).__status(),
                403,
                "{vhost} ({doc:?}) must fail closed, not read as public"
            );
        }
    }

    #[test]
    fn missing_credentials_allow_lets_public_sites_through() {
        // The mixed-fleet case: some previews are public, some are not.
        wire_kv();
        let mw = mw(&serde_json::json!({
            "kv_prefix": "t6:basicauth:",
            "missing_credentials": "allow",
        }));
        assert_eq!(invoke_on(&mw, "public.example", true, &[]).__action(), ACTION_CONTINUE);

        let host = Host::new(host_table());
        let doc = serde_json::json!({ "alice": hash("pw") }).to_string();
        assert!(host.kv_set("t6:basicauth:private.example", doc.as_bytes(), 0));
        assert_eq!(invoke_on(&mw, "private.example", true, &[]).__status(), 401);
    }

    // ── Forwarding the authenticated user ────────────────────────────────

    #[test]
    fn forward_user_header_overrides_a_forged_one() {
        let mw = mw(&serde_json::json!({
            "users": { "bob": hash("pw") },
            "forward_user_header": "X-Auth-User",
        }));
        let mut headers = basic_header("bob", "pw");
        // A client claiming to be someone else.
        headers.push(("X-Auth-User".to_owned(), "admin".to_owned()));
        let resp = invoke(&mw, &headers);
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(header_of(&resp, "X-Auth-User"), Some("bob"));
        // REWRITE overrides are applied with `override_header`, which replaces
        // rather than appends — asserted end to end in the server's tests.
        assert_eq!(resp.__headers().len(), 1);
    }

    #[test]
    fn forward_user_header_name_is_validated() {
        let cfg = serde_json::json!({
            "kv_prefix": "ba:",
            "forward_user_header": "X-Auth: injected\r\nX-Evil",
        });
        assert!(BasicAuth::init(&cfg).is_err());
    }
}
