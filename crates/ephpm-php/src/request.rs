//! HTTP request to PHP request mapping.
//!
//! Converts an incoming HTTP request into the format expected by PHP's
//! embed SAPI, including populating `$_SERVER` variables.

use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::net::SocketAddr;
use std::path::PathBuf;

/// One PHP middleware mount resolved for a single request.
///
/// Produced by the router from a `[[middleware]] library = "php:<path>"` mount
/// whose `match` glob accepted this request. `script` is already resolved
/// against the request's own document root, so in multi-tenant mode it names
/// the tenant's own file and nothing else.
#[derive(Debug, Clone)]
pub struct PhpMiddleware {
    /// Absolute path to the middleware script.
    pub script: PathBuf,

    /// The mount's `config` table serialised to JSON, surfaced to the script as
    /// `ephpm_middleware_config()`. `None` when the mount declares no `config`.
    pub config_json: Option<String>,
}

/// How the PHP middleware chain ended for one request.
///
/// Mirrors the C `EPHPM_MW_*` codes; used for the
/// `ephpm_middleware_invocations_total{action=...}` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareOutcome {
    /// Every mount ran and fell through to the application script.
    Continue,
    /// A mount short-circuited with `exit()` — the lane's `RESPOND`.
    Respond,
    /// A mount raised a fatal; the application script never ran (fail closed).
    Error,
}

impl MiddlewareOutcome {
    /// Metric label for this outcome.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Respond => "respond",
            Self::Error => "error",
        }
    }
}

/// True for a `$_SERVER` key or HTTP header name whose value must never be
/// rendered into a log line.
///
/// `$_SERVER` carries several cleartext secrets: `DB_PASSWORD` and
/// `DATABASE_URL` (the per-site `pdo_mysql` credential the router injects) and,
/// since #386, `PHP_AUTH_PW` — plus the `Authorization` and `Cookie` headers
/// they are derived from. Nothing in the codebase formats these collections
/// today, which is exactly why the protection was worth making explicit rather
/// than leaving as an unenforced invariant: `PhpRequest` and
/// `WorkerRequestOwned` are both `pub` and sit one `?request` away from a
/// `tracing::debug!` on the request path.
///
/// The match is a substring heuristic over the usual secret-bearing names, so a
/// future injected credential is redacted by default rather than by remembering
/// to add it here.
pub(crate) fn is_secret_name(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "PHP_AUTH_PW",
        "PHP_AUTH_DIGEST",
        "DATABASE_URL",
        "AUTHORIZATION",
        "HTTP_AUTHORIZATION",
        "PROXY-AUTHORIZATION",
        "HTTP_PROXY_AUTHORIZATION",
        "COOKIE",
        "HTTP_COOKIE",
    ];
    const SUBSTRINGS: &[&str] =
        &["PASSWORD", "PASSWD", "SECRET", "TOKEN", "APIKEY", "API_KEY", "API-KEY"];

    let upper = name.to_ascii_uppercase();
    EXACT.contains(&upper.as_str()) || SUBSTRINGS.iter().any(|s| upper.contains(s))
}

/// Format `(name, value)` pairs for `Debug`, replacing secret values with a
/// placeholder. See [`is_secret_name`].
pub(crate) fn redacted_pairs<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<(&'a str, &'a str)> {
    pairs
        .into_iter()
        .map(|(name, value)| (name, if is_secret_name(name) { "<redacted>" } else { value }))
        .collect()
}

/// A PHP request, constructed from an incoming HTTP request.
///
/// Contains all the information needed to set up a PHP execution context
/// via the SAPI callbacks.
pub struct PhpRequest {
    /// HTTP method (GET, POST, etc.)
    pub method: String,

    /// Full request URI including query string (e.g. "/wp-admin/index.php?page=1")
    pub uri: String,

    /// The path component of the URI (e.g. "/wp-admin/index.php")
    pub path: String,

    /// Query string without leading '?' (e.g. "page=1")
    pub query_string: String,

    /// Absolute path to the PHP script to execute.
    pub script_filename: PathBuf,

    /// Document root directory.
    pub document_root: PathBuf,

    /// Request headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,

    /// POST body data.
    pub body: Vec<u8>,

    /// Content-Type header value.
    pub content_type: Option<String>,

    /// Remote client address.
    pub remote_addr: SocketAddr,

    /// Server name (from Host header).
    pub server_name: String,

    /// Server port.
    pub server_port: u16,

    /// Whether the request came over HTTPS.
    pub is_https: bool,

    /// HTTP protocol version string (e.g. "HTTP/1.1").
    pub protocol: String,

    /// Extra environment variables to inject into PHP `$_SERVER`.
    ///
    /// These are added after the standard CGI variables and HTTP headers,
    /// so they can override built-in values if needed. Used for injecting
    /// `EPHPM_REDIS_*` credentials in multi-tenant mode.
    pub env_vars: Vec<(String, String)>,

    /// PHP middleware scripts to run — in chain order — inside this request,
    /// immediately before `script_filename`.
    ///
    /// Empty for every request that has no `php:` mount, which is the default
    /// and costs nothing. See [`PhpMiddleware`].
    pub middleware: Vec<PhpMiddleware>,
}

/// Hand-written so `headers` and `env_vars` go through [`redacted_pairs`]
/// instead of printing the `Authorization` header and the injected
/// `DB_PASSWORD` in full. `body` is summarised by length — it is request data,
/// often large, and routinely carries credentials of its own (a login form
/// post).
impl std::fmt::Debug for PhpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn pairs(v: &[(String, String)]) -> Vec<(&str, &str)> {
            redacted_pairs(v.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        }
        f.debug_struct("PhpRequest")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("path", &self.path)
            .field("query_string", &self.query_string)
            .field("script_filename", &self.script_filename)
            .field("document_root", &self.document_root)
            .field("headers", &pairs(&self.headers))
            .field("body_len", &self.body.len())
            .field("content_type", &self.content_type)
            .field("remote_addr", &self.remote_addr)
            .field("server_name", &self.server_name)
            .field("server_port", &self.server_port)
            .field("is_https", &self.is_https)
            .field("protocol", &self.protocol)
            .field("env_vars", &pairs(&self.env_vars))
            .field("middleware", &self.middleware)
            .finish()
    }
}

impl PhpRequest {
    /// Build the `$_SERVER` variables that `WordPress` and other PHP apps expect.
    ///
    /// Key distinction when fallback rewrites happen (e.g. `/blog/hello` → `/index.php`):
    /// - `REQUEST_URI` = original URI (`/blog/hello`) — what the client asked for
    /// - `SCRIPT_NAME` = resolved script (`/index.php`) — what PHP is executing
    /// - `PHP_SELF` = same as `SCRIPT_NAME`
    #[must_use]
    pub fn server_variables(&self) -> Vec<(String, String)> {
        build_server_variables(
            &self.method,
            &self.uri,
            &self.query_string,
            &self.script_filename,
            &self.document_root,
            &self.path,
            &self.server_name,
            self.server_port,
            &self.protocol,
            self.remote_addr,
            self.is_https,
            &self.headers,
            &self.env_vars,
        )
    }

    /// Build the `$_SERVER` variables in FFI-ready form — same derivation as
    /// [`Self::server_variables`], see [`build_server_variables_c`].
    #[must_use]
    pub fn server_variables_c(&self) -> Vec<CServerVar> {
        build_server_variables_c(
            &self.method,
            &self.uri,
            &self.query_string,
            &self.script_filename,
            &self.document_root,
            &self.path,
            &self.server_name,
            self.server_port,
            &self.protocol,
            self.remote_addr,
            self.is_https,
            &self.headers,
            &self.env_vars,
        )
    }

    /// Extract the cookie string from the request headers.
    #[must_use]
    pub fn cookie_string(&self) -> String {
        cookie_string_from_headers(&self.headers)
    }
}

/// A `$_SERVER` entry in FFI-ready form: NUL-terminated key and value, with
/// static (`&'static CStr`) storage for the strings that never vary between
/// requests, so the per-request invariant half of `$_SERVER` costs zero
/// allocations (issue #133).
pub type CServerVar = (Cow<'static, CStr>, Cow<'static, CStr>);

/// Build the `$_SERVER` variables from borrowed request fields.
///
/// Thin conversion wrapper over [`build_server_variables_c`], which is the
/// single source of truth for `$_SERVER` derivation — kept so tests (and any
/// caller that wants plain strings) can assert on the derivation without
/// touching `CStr`. The hot paths (fpm dispatch in `execute_php`, worker
/// dispatch in `ephpm-server`) call the `_c` variant directly and never build
/// this `String` form.
///
/// Key distinction when fallback rewrites happen (e.g. `/blog/hello` → `/index.php`):
/// - `REQUEST_URI` = original URI (`/blog/hello`) — what the client asked for
/// - `SCRIPT_NAME` = resolved script (`/index.php`) — what PHP is executing
/// - `PHP_SELF` = same as `SCRIPT_NAME`
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_server_variables(
    method: &str,
    uri: &str,
    query_string: &str,
    script_filename: &std::path::Path,
    document_root: &std::path::Path,
    path: &str,
    server_name: &str,
    server_port: u16,
    protocol: &str,
    remote_addr: SocketAddr,
    is_https: bool,
    headers: &[(String, String)],
    env_vars: &[(String, String)],
) -> Vec<(String, String)> {
    build_server_variables_c(
        method,
        uri,
        query_string,
        script_filename,
        document_root,
        path,
        server_name,
        server_port,
        protocol,
        remote_addr,
        is_https,
        headers,
        env_vars,
    )
    .iter()
    .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
    .collect()
}

/// The `REQUEST_METHOD` value as a static C string for the common methods,
/// avoiding a per-request allocation; anything else is copied.
fn method_value(method: &str) -> Option<Cow<'static, CStr>> {
    let known: &'static CStr = match method {
        "GET" => c"GET",
        "POST" => c"POST",
        "HEAD" => c"HEAD",
        "PUT" => c"PUT",
        "DELETE" => c"DELETE",
        "PATCH" => c"PATCH",
        "OPTIONS" => c"OPTIONS",
        _ => return owned(method),
    };
    Some(Cow::Borrowed(known))
}

/// The `SERVER_PROTOCOL` value as a static C string for the protocols hyper
/// actually produces; anything else is copied.
fn protocol_value(protocol: &str) -> Option<Cow<'static, CStr>> {
    let known: &'static CStr = match protocol {
        "HTTP/1.1" => c"HTTP/1.1",
        "HTTP/2.0" => c"HTTP/2.0",
        "HTTP/1.0" => c"HTTP/1.0",
        _ => return owned(protocol),
    };
    Some(Cow::Borrowed(known))
}

/// Copy a string into an owned C string. `None` when it contains an interior
/// NUL — such a pair is dropped by the builder rather than truncated or
/// substituted, so PHP never sees a value that differs from what the client
/// (or the router) actually produced.
fn owned(s: &str) -> Option<Cow<'static, CStr>> {
    CString::new(s).ok().map(Cow::Owned)
}

/// Copy raw bytes into an owned C string. `None` when they contain an interior
/// NUL. Used for the base64-decoded HTTP Basic credentials, which are arbitrary
/// bytes and need not be UTF-8 — PHP strings are byte strings, so a credential
/// in some other encoding reaches PHP unmangled rather than being lossily
/// transcoded.
fn owned_bytes(bytes: Vec<u8>) -> Option<Cow<'static, CStr>> {
    CString::new(bytes).ok().map(Cow::Owned)
}

/// HTTP authentication data derived from a request's `Authorization` header.
///
/// See [`parse_authorization`] for the derivation and how it lines up with
/// php-src.
#[derive(Debug, PartialEq, Eq)]
struct AuthVars<'a> {
    /// The scheme token as the client cased it — becomes `AUTH_TYPE`.
    auth_type: &'a str,

    /// Decoded HTTP Basic credentials: `(user, password)`, where `password` is
    /// `None` for an empty password (php-src leaves `PHP_AUTH_PW` unset in that
    /// case). `None` for the whole pair when this is not a Basic credential, or
    /// when the payload did not yield one.
    ///
    /// The two halves travel together on purpose: a username registered without
    /// its password would read to an application as "user authenticated with an
    /// empty password", so an unrepresentable half suppresses both.
    credentials: Option<(Vec<u8>, Option<Vec<u8>>)>,

    /// The Digest challenge with the `Digest ` prefix removed — becomes
    /// `PHP_AUTH_DIGEST`, matching php-src's `estrdup(auth + 7)`.
    digest: Option<&'a str>,
}

/// The `Authorization` header value for this request: the **first** one,
/// matched case-insensitively.
///
/// "First wins" matches [`cookie_string_from_headers`]; a duplicated
/// `Authorization` header is malformed (RFC 9110 §5.3 forbids list-combining a
/// non-list field) and picking deterministically keeps ePHPm from disagreeing
/// with a front proxy that made the same choice.
fn authorization_header(headers: &[(String, String)]) -> Option<&str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str())
}

/// Value of one base64 character, or `None` for anything else — including
/// whitespace and the `=` pad, both of which php-src's non-strict decoder
/// skips. See [`base64_decode_lenient`].
const fn base64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode base64 the way PHP does it for `Authorization: Basic`.
///
/// This deliberately mirrors `php_base64_decode_ex(..., strict = 0)` — the
/// decoder `php_handle_auth_data()` uses — rather than a conforming RFC 4648
/// decoder, because the point of #386 is to behave like every other SAPI. In
/// non-strict mode php-src **never fails**: bytes outside the alphabet
/// (whitespace, `=`, anything else) are skipped, padding is optional and
/// unvalidated, and a trailing group of a single character contributes nothing.
/// A strict decoder would reject payloads that Apache and PHP-FPM accept, so an
/// app would authenticate there and not here — the exact class of bug this
/// change exists to remove.
fn base64_decode_lenient(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut carry: u8 = 0;
    let mut seen: usize = 0;
    for &c in input {
        let Some(value) = base64_value(c) else { continue };
        match seen % 4 {
            0 => carry = value << 2,
            1 => {
                out.push(carry | (value >> 4));
                carry = (value & 0x0f) << 4;
            }
            2 => {
                out.push(carry | (value >> 2));
                carry = (value & 0x03) << 6;
            }
            _ => out.push(carry | value),
        }
        seen += 1;
    }
    out
}

/// True for an RFC 9110 §5.6.2 `token` — the grammar an auth scheme name obeys.
fn is_http_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Derive the PHP authentication `$_SERVER` variables from an `Authorization`
/// header value.
///
/// Mirrors php-src's `php_handle_auth_data()` (`main/main.c`), which every
/// stock SAPI funnels the header through, with the deviations noted below.
///
/// * The scheme is matched **case-insensitively** for both `Basic ` and
///   `Digest `, including the trailing space — so a bare `Authorization: Basic`
///   with no payload is not a Basic credential (it still yields an `AUTH_TYPE`).
/// * `Basic` payloads are base64-decoded leniently
///   ([`base64_decode_lenient`]) and split on the **first** `:`. No colon means
///   no credentials at all, which is php-src's `strchr` returning NULL.
/// * An empty password yields no `PHP_AUTH_PW`, matching php-src's
///   `if (strlen(pass) > 0)`. An empty *username* is preserved and registered,
///   matching php-src.
/// * `Digest` is only considered when a Basic credential was not produced, and
///   carries the header minus the 7-byte `Digest ` prefix.
///
/// **Deviation — interior NUL.** php-src operates on C strings, so a NUL in the
/// decoded payload truncates the username (or hides the colon entirely). ePHPm
/// registers `$_SERVER` through NUL-terminated FFI as well, but rather than
/// silently handing PHP a *truncated* password — which fails an auth check in a
/// way nobody can debug — an interior NUL in either half suppresses both keys,
/// so the application sees "no credentials" and re-challenges.
///
/// **`AUTH_TYPE` is set for every scheme, not just Basic and Digest.** php-src
/// never sets `AUTH_TYPE` itself: it is a CGI meta-variable (RFC 3875 §4.1.1)
/// that the front-end server supplies, which under nginx/FPM usually means it
/// is absent. ePHPm *is* the front-end, so it reports the scheme the client
/// offered — `Bearer`, `Negotiate` and custom schemes included, which get an
/// `AUTH_TYPE` and nothing else (their credential stays readable in
/// `HTTP_AUTHORIZATION`, which is kept alongside these keys as stock SAPIs do).
///
/// Note that `AUTH_TYPE` here describes *what the client offered*, not a
/// successful authentication: ePHPm has validated nothing at this point. That
/// is why `REMOTE_USER` is deliberately **not** derived — it denotes a user the
/// server itself authenticated, and synthesising it from an unvalidated header
/// would hand an attacker any identity they typed.
fn parse_authorization(raw: &str) -> Option<AuthVars<'_>> {
    let scheme = raw.split(' ').next().unwrap_or_default();
    if !is_http_token(scheme) {
        return None;
    }

    let mut auth = AuthVars { auth_type: scheme, credentials: None, digest: None };

    if let Some(payload) = strip_prefix_ignore_ascii_case(raw, "Basic ") {
        let decoded = base64_decode_lenient(payload.as_bytes());
        if let Some(colon) = decoded.iter().position(|&b| b == b':') {
            let password = &decoded[colon + 1..];
            // Suppress the pair outright if either half cannot cross the FFI
            // boundary intact; see the "interior NUL" note above.
            if !decoded[..colon].contains(&0) && !password.contains(&0) {
                let password = (!password.is_empty()).then(|| password.to_vec());
                auth.credentials = Some((decoded[..colon].to_vec(), password));
            }
        }
    }

    // php-src only reaches its Digest branch when the Basic branch produced
    // nothing, so a header that is somehow both never yields both.
    if auth.credentials.is_none() {
        auth.digest = strip_prefix_ignore_ascii_case(raw, "Digest ");
    }

    Some(auth)
}

/// `str::strip_prefix`, matched case-insensitively over ASCII.
fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let (head, rest) = s.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

/// Build the `$_SERVER` variables from borrowed request fields, directly in
/// the NUL-terminated form the SAPI FFI needs (issue #133).
///
/// This is the single source of truth for `$_SERVER` derivation, shared by
/// the fpm path ([`PhpRequest::server_variables_c`], consumed by
/// `execute_php`) and the worker dispatch path in `ephpm-server`. Both used
/// to build an intermediate `Vec<(String, String)>` and then convert every
/// pair to `CString` a second time — two allocations, two copies, and a NUL
/// scan per string, on every request. Building the C form once halves the
/// allocation traffic, and the entries that never vary between requests
/// (`SERVER_SOFTWARE`, `GATEWAY_INTERFACE`, `REDIRECT_STATUS`, `HTTPS`, the
/// fixed CGI keys, common methods/protocols, canonical header keys) are
/// `&'static CStr` literals that allocate nothing at all.
///
/// A key or value with an interior NUL drops that pair — the same behaviour
/// the fpm path always had. (The worker path previously substituted an empty
/// string for the unrepresentable half, which could register a header key
/// with someone else's value; dropping is strictly safer.)
///
/// Key distinction when fallback rewrites happen (e.g. `/blog/hello` → `/index.php`):
/// - `REQUEST_URI` = original URI (`/blog/hello`) — what the client asked for
/// - `SCRIPT_NAME` = resolved script (`/index.php`) — what PHP is executing
/// - `PHP_SELF` = same as `SCRIPT_NAME`
#[must_use]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn build_server_variables_c(
    method: &str,
    uri: &str,
    query_string: &str,
    script_filename: &std::path::Path,
    document_root: &std::path::Path,
    path: &str,
    server_name: &str,
    server_port: u16,
    protocol: &str,
    remote_addr: SocketAddr,
    is_https: bool,
    headers: &[(String, String)],
    env_vars: &[(String, String)],
) -> Vec<CServerVar> {
    // Derive SCRIPT_NAME from the resolved script_filename relative to
    // document_root. This is correct even after fallback rewrites.
    let script_name = script_filename
        .strip_prefix(document_root)
        .map_or_else(|_| path.to_owned(), |rel| format!("/{}", rel.to_string_lossy()));
    // One CString for SCRIPT_NAME + PHP_SELF; the clone is a plain memcpy.
    let script_name = CString::new(script_name).ok();

    // 16 fixed CGI vars, HTTPS, and up to 3 HTTP-authentication vars (#386).
    let mut vars: Vec<CServerVar> =
        Vec::with_capacity(19 + headers.len() + env_vars.len() + usize::from(is_https));
    let mut push = |key: Cow<'static, CStr>, value: Option<Cow<'static, CStr>>| {
        if let Some(value) = value {
            vars.push((key, value));
        }
    };

    push(Cow::Borrowed(c"REQUEST_METHOD"), method_value(method));
    push(Cow::Borrowed(c"REQUEST_URI"), owned(uri));
    push(Cow::Borrowed(c"SCRIPT_FILENAME"), owned(&script_filename.to_string_lossy()));
    push(Cow::Borrowed(c"SCRIPT_NAME"), script_name.clone().map(Cow::Owned));
    push(Cow::Borrowed(c"DOCUMENT_ROOT"), owned(&document_root.to_string_lossy()));
    push(Cow::Borrowed(c"SERVER_NAME"), owned(server_name));
    push(Cow::Borrowed(c"SERVER_PORT"), owned(&server_port.to_string()));
    push(Cow::Borrowed(c"SERVER_SOFTWARE"), Some(Cow::Borrowed(c"ePHPm/0.1.0")));
    push(Cow::Borrowed(c"SERVER_PROTOCOL"), protocol_value(protocol));
    push(Cow::Borrowed(c"GATEWAY_INTERFACE"), Some(Cow::Borrowed(c"CGI/1.1")));
    push(Cow::Borrowed(c"QUERY_STRING"), owned(query_string));
    push(Cow::Borrowed(c"PHP_SELF"), script_name.map(Cow::Owned));
    push(Cow::Borrowed(c"REMOTE_ADDR"), owned(&remote_addr.ip().to_string()));
    push(Cow::Borrowed(c"REMOTE_PORT"), owned(&remote_addr.port().to_string()));
    push(Cow::Borrowed(c"REDIRECT_STATUS"), Some(Cow::Borrowed(c"200")));

    if is_https {
        push(Cow::Borrowed(c"HTTPS"), Some(Cow::Borrowed(c"on")));
    }

    // HTTP authentication vars derived from the Authorization header (#386).
    //
    // These are registered as ordinary $_SERVER entries through the SAPI's
    // register_server_variables callback, which is what PHP code observes.
    // NOT set: SG(request_info).auth_user / auth_password / auth_digest, the
    // fields a stock SAPI fills so PHP core registers these keys itself. Doing
    // that safely is a C change at both lifecycle sites in ephpm_wrapper.c, and
    // the two lanes differ in a way that matters: the per-request lane gets a
    // php_request_shutdown() per request, so sapi_deactivate_module() efrees and
    // NULLs those fields for it, while the worker lane runs one long-lived PHP
    // request and would need them freed and cleared by hand every iteration.
    // Getting that wrong leaks a credential from one request into the next —
    // across tenants — so it is deliberately left for a change that can be
    // validated against a PHP-linked multi-request test. The practical effect
    // today is limited to C extensions that read SG(request_info) directly.
    //
    // Deliberately pushed HERE — before the header and env-var loops — rather
    // than appended at the end. `ephpm_wrapper.c` registers at most
    // MAX_SERVER_VARS (128) entries and silently drops the overflow, so a
    // request carrying enough headers to reach the cap would otherwise lose
    // exactly the keys an authenticating app is looking for. Their position in
    // this Vec is otherwise irrelevant: $_SERVER is a hash, and no header can
    // collide with these keys (every non-canonical header is prefixed `HTTP_`).
    if let Some(auth) = authorization_header(headers).and_then(parse_authorization) {
        push(Cow::Borrowed(c"AUTH_TYPE"), owned(auth.auth_type));
        // Emitted as a pair or not at all — see AuthVars::credentials.
        if let Some((user, password)) = auth.credentials {
            push(Cow::Borrowed(c"PHP_AUTH_USER"), owned_bytes(user));
            // php-src registers PHP_AUTH_PW only for a non-empty password.
            if let Some(password) = password {
                push(Cow::Borrowed(c"PHP_AUTH_PW"), owned_bytes(password));
            }
        }
        if let Some(digest) = auth.digest {
            push(Cow::Borrowed(c"PHP_AUTH_DIGEST"), owned(digest));
        }
    }

    // Map HTTP headers to $_SERVER variables. The canonical names get static
    // keys; everything else is one byte-pass allocation (see cgi_header_key).
    for (name, value) in headers {
        if let Some(key) = cgi_header_key_c(name) {
            push(key, owned(value));
        }
    }

    // Append extra environment variables (e.g. EPHPM_REDIS_* credentials).
    for (key, value) in env_vars {
        if let Some(key) = owned(key) {
            push(key, owned(value));
        }
    }

    vars
}

/// Extract the cookie string from request headers (first `Cookie` header,
/// case-insensitive; empty string if absent).
///
/// Shared by [`PhpRequest::cookie_string`] and the worker dispatch path so
/// both derive the cookie data identically.
#[must_use]
pub fn cookie_string_from_headers(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// Build the CGI-style `$_SERVER` key for an HTTP header name in one
/// byte pass, directly as a C string.
///
/// Headers `host`, `cookie`, `content-type`, `content-length` map to
/// non-`HTTP_` keys per the CGI spec (and PHP's SAPI conventions) — those
/// come back as `&'static CStr` with no allocation at all; everything else
/// becomes an owned `HTTP_<UPPER-WITH-UNDERSCORES>` built in a single ASCII
/// upper + dash-to-underscore pass over a pre-sized buffer. HTTP header
/// names are ASCII by RFC 7230.
///
/// `None` for a header name with an interior NUL (hyper never produces one;
/// dropping the pair is the fail-safe read).
#[must_use]
pub(crate) fn cgi_header_key_c(name: &str) -> Option<Cow<'static, CStr>> {
    // Special-case the ASCII-canonical spellings first (case-
    // insensitive). Doing this without a to_lowercase alloc is a
    // simple `eq_ignore_ascii_case`.
    if name.eq_ignore_ascii_case("host") {
        return Some(Cow::Borrowed(c"HTTP_HOST"));
    }
    if name.eq_ignore_ascii_case("cookie") {
        return Some(Cow::Borrowed(c"HTTP_COOKIE"));
    }
    if name.eq_ignore_ascii_case("content-type") {
        return Some(Cow::Borrowed(c"CONTENT_TYPE"));
    }
    if name.eq_ignore_ascii_case("content-length") {
        return Some(Cow::Borrowed(c"CONTENT_LENGTH"));
    }

    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(5 + bytes.len() + 1);
    out.extend_from_slice(b"HTTP_");
    for b in bytes {
        out.push(match *b {
            b'-' => b'_',
            b @ b'a'..=b'z' => b - 32,
            b => b,
        });
    }
    CString::new(out).ok().map(Cow::Owned)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use super::*;

    fn make_request() -> PhpRequest {
        PhpRequest {
            method: "GET".into(),
            uri: "/index.php?page=1".into(),
            path: "/index.php".into(),
            query_string: "page=1".into(),
            script_filename: PathBuf::from("/var/www/html/index.php"),
            document_root: PathBuf::from("/var/www/html"),
            headers: vec![
                ("host".into(), "example.com".into()),
                ("accept-encoding".into(), "gzip, deflate".into()),
            ],
            body: Vec::new(),
            content_type: None,
            remote_addr: "192.168.1.1:54321".parse::<SocketAddr>().unwrap(),
            server_name: "example.com".into(),
            server_port: 8080,
            is_https: false,
            protocol: "HTTP/1.1".into(),
            env_vars: Vec::new(),
            middleware: Vec::new(),
        }
    }

    /// Helper to find a server variable by key.
    fn find_var<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn test_server_variables_core_fields() {
        let req = make_request();
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "REQUEST_METHOD"), Some("GET"));
        assert_eq!(find_var(&vars, "REQUEST_URI"), Some("/index.php?page=1"));
        assert_eq!(find_var(&vars, "QUERY_STRING"), Some("page=1"));
        assert_eq!(find_var(&vars, "SERVER_NAME"), Some("example.com"));
        assert_eq!(find_var(&vars, "SERVER_PORT"), Some("8080"));
        assert_eq!(find_var(&vars, "SERVER_SOFTWARE"), Some("ePHPm/0.1.0"));
        assert_eq!(find_var(&vars, "SERVER_PROTOCOL"), Some("HTTP/1.1"));
        assert_eq!(find_var(&vars, "REMOTE_ADDR"), Some("192.168.1.1"));
        assert_eq!(find_var(&vars, "REMOTE_PORT"), Some("54321"));
        // SCRIPT_NAME derived from script_filename relative to document_root
        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
        assert_eq!(find_var(&vars, "PHP_SELF"), Some("/index.php"));
        assert_eq!(find_var(&vars, "GATEWAY_INTERFACE"), Some("CGI/1.1"));
        assert_eq!(find_var(&vars, "REDIRECT_STATUS"), Some("200"));
    }

    #[test]
    fn test_server_variables_script_paths() {
        let req = make_request();
        let vars = req.server_variables();

        let script = find_var(&vars, "SCRIPT_FILENAME").unwrap();
        assert_eq!(PathBuf::from(script), PathBuf::from("/var/www/html/index.php"));

        let docroot = find_var(&vars, "DOCUMENT_ROOT").unwrap();
        assert_eq!(PathBuf::from(docroot), PathBuf::from("/var/www/html"));

        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
    }

    #[test]
    fn test_server_variables_rewritten_request() {
        // Simulate fallback rewrite: /blog/hello → /index.php
        let mut req = make_request();
        req.uri = "/blog/hello?preview=true".into();
        req.path = "/blog/hello".into();
        req.query_string = "preview=true".into();
        // script_filename stays as /var/www/html/index.php (from fallback)
        let vars = req.server_variables();

        // REQUEST_URI keeps original
        assert_eq!(find_var(&vars, "REQUEST_URI"), Some("/blog/hello?preview=true"));
        // SCRIPT_NAME derived from resolved script
        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
        assert_eq!(find_var(&vars, "PHP_SELF"), Some("/index.php"));
    }

    #[test]
    fn test_server_variables_http_header_mapping() {
        let req = make_request();
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "HTTP_ACCEPT_ENCODING"), Some("gzip, deflate"));
    }

    #[test]
    fn test_server_variables_host_header() {
        let req = make_request();
        let vars = req.server_variables();

        // "host" header should map to HTTP_HOST, not HTTP_HTTP_HOST
        assert_eq!(find_var(&vars, "HTTP_HOST"), Some("example.com"));
        assert!(find_var(&vars, "HTTP_HTTP_HOST").is_none());
    }

    #[test]
    fn test_server_variables_content_type_no_http_prefix() {
        let mut req = make_request();
        req.headers.push(("content-type".into(), "application/json".into()));
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "CONTENT_TYPE"), Some("application/json"));
        assert!(find_var(&vars, "HTTP_CONTENT_TYPE").is_none());
    }

    #[test]
    fn test_server_variables_content_length_no_http_prefix() {
        let mut req = make_request();
        req.headers.push(("content-length".into(), "42".into()));
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "CONTENT_LENGTH"), Some("42"));
        assert!(find_var(&vars, "HTTP_CONTENT_LENGTH").is_none());
    }

    #[test]
    fn test_server_variables_https_on() {
        let mut req = make_request();
        req.is_https = true;
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "HTTPS"), Some("on"));
    }

    #[test]
    fn test_server_variables_https_absent_when_false() {
        let req = make_request();
        assert!(!req.is_https);
        let vars = req.server_variables();

        assert!(find_var(&vars, "HTTPS").is_none());
    }

    // ── HTTP authentication ($_SERVER auth vars, issue #386) ─────────────
    //
    // Every assertion here fails against the pre-#386 tree, where the
    // Authorization header reached PHP only as HTTP_AUTHORIZATION and none of
    // PHP_AUTH_USER / PHP_AUTH_PW / AUTH_TYPE / PHP_AUTH_DIGEST was derived.
    //
    // The reference behaviour is php-src's php_handle_auth_data()
    // (main/main.c) plus php_register_server_variables() (main/php_variables.c),
    // which is what every stock SAPI funnels the header through.

    /// Standard-alphabet base64 with padding — the encoder a client uses.
    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Build `$_SERVER` for a request carrying one `Authorization` header.
    fn vars_with_auth(value: &str) -> Vec<(String, String)> {
        let mut req = make_request();
        req.headers.push(("Authorization".into(), value.to_owned()));
        req.server_variables()
    }

    /// The headline case: RFC 7617's own example credential must reach PHP
    /// decoded, exactly as it does under mod_php and PHP-FPM.
    #[test]
    fn basic_auth_populates_php_auth_user_and_pw() {
        let vars = vars_with_auth(&format!("Basic {}", b64(b"Aladdin:open sesame")));

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some("Aladdin"));
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), Some("open sesame"));
        assert_eq!(find_var(&vars, "AUTH_TYPE"), Some("Basic"));
    }

    /// The raw header stays visible too — stock SAPIs keep HTTP_AUTHORIZATION
    /// alongside the derived vars, and frameworks (Laravel's `bearerToken()`,
    /// PSR-7 bridges) read it directly.
    #[test]
    fn raw_authorization_header_is_kept_alongside_the_derived_vars() {
        let encoded = b64(b"Aladdin:open sesame");
        let vars = vars_with_auth(&format!("Basic {encoded}"));

        assert_eq!(
            find_var(&vars, "HTTP_AUTHORIZATION"),
            Some(format!("Basic {encoded}").as_str())
        );
    }

    /// php-src compares the scheme with `zend_binary_strncasecmp`, so a
    /// lowercase `basic` is a valid Basic credential. AUTH_TYPE reports the
    /// scheme as the client cased it, not a normalised spelling.
    #[test]
    fn basic_scheme_is_matched_case_insensitively() {
        let vars = vars_with_auth(&format!("bAsIc {}", b64(b"user:pw")));

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some("user"));
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), Some("pw"));
        assert_eq!(find_var(&vars, "AUTH_TYPE"), Some("bAsIc"));
    }

    /// php-src guards the password registration with `if (strlen(pass) > 0)`,
    /// so `user:` leaves PHP_AUTH_PW *unset* rather than empty. Apps written
    /// against that distinguish "no password sent" from "empty password".
    #[test]
    fn empty_password_leaves_php_auth_pw_unset() {
        let vars = vars_with_auth(&format!("Basic {}", b64(b"user:")));

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some("user"));
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), None);
    }

    /// An empty *username* is registered (php-src estrndup's it unconditionally
    /// once a colon is found) — the asymmetry with the password is deliberate.
    #[test]
    fn empty_username_is_registered_as_an_empty_value() {
        let vars = vars_with_auth(&format!("Basic {}", b64(b":secret")));

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some(""));
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), Some("secret"));
    }

    /// Split on the FIRST colon only — a password containing colons survives
    /// intact. php-src uses `strchr`, which finds the leftmost one.
    #[test]
    fn password_may_contain_colons() {
        let vars = vars_with_auth(&format!("Basic {}", b64(b"user:a:b:c")));

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some("user"));
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), Some("a:b:c"));
    }

    /// No colon in the decoded payload means php-src's `strchr` returns NULL
    /// and it registers neither credential. AUTH_TYPE still describes the
    /// header, and the raw value is still readable.
    #[test]
    fn basic_payload_without_a_colon_yields_no_credentials() {
        let vars = vars_with_auth(&format!("Basic {}", b64(b"justausername")));

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), None);
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), None);
        assert_eq!(find_var(&vars, "AUTH_TYPE"), Some("Basic"));
    }

    /// php_base64_decode is called in NON-strict mode, which skips characters
    /// outside the alphabet and does not require padding. A client whose base64
    /// has stray whitespace or missing `=` authenticates on Apache/FPM, so it
    /// must authenticate here — a strict decoder would silently reject it.
    #[test]
    fn base64_payload_is_decoded_leniently_like_php() {
        // "user:pw" == "dXNlcjpwdw==". Mangle it three ways that php-src
        // tolerates: embedded whitespace, no padding, and both.
        for payload in ["dXNlcjpwdw==", "dXNl cjpw dw==", "dXNlcjpwdw", "dXNl\tcjpwdw"] {
            let vars = vars_with_auth(&format!("Basic {payload}"));
            assert_eq!(
                find_var(&vars, "PHP_AUTH_USER"),
                Some("user"),
                "payload {payload:?} should decode leniently"
            );
            assert_eq!(find_var(&vars, "PHP_AUTH_PW"), Some("pw"), "payload {payload:?}");
        }
    }

    /// Garbage that decodes to nothing usable yields no credentials rather than
    /// an error or a panic.
    #[test]
    fn non_base64_basic_payload_yields_no_credentials() {
        for payload in ["!!!!", "", "@@@@@@@@"] {
            let vars = vars_with_auth(&format!("Basic {payload}"));
            assert_eq!(find_var(&vars, "PHP_AUTH_USER"), None, "payload {payload:?}");
            assert_eq!(find_var(&vars, "PHP_AUTH_PW"), None, "payload {payload:?}");
        }
    }

    /// Digest is not decoded: php-src stores the header minus the 7-byte
    /// "Digest " prefix in PHP_AUTH_DIGEST and sets no user/password.
    #[test]
    fn digest_sets_php_auth_digest_without_the_scheme_prefix() {
        let challenge = r#"username="bob", realm="test", nonce="abc""#;
        let vars = vars_with_auth(&format!("Digest {challenge}"));

        assert_eq!(find_var(&vars, "PHP_AUTH_DIGEST"), Some(challenge));
        assert_eq!(find_var(&vars, "AUTH_TYPE"), Some("Digest"));
        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), None);
        assert_eq!(find_var(&vars, "PHP_AUTH_PW"), None);
    }

    /// Bearer (and any other scheme) gets an AUTH_TYPE and nothing else. The
    /// token itself stays readable via HTTP_AUTHORIZATION, which is where every
    /// framework looks for it.
    #[test]
    fn other_schemes_set_auth_type_only() {
        for scheme in ["Bearer", "Negotiate", "DPoP", "AWS4-HMAC-SHA256"] {
            let vars = vars_with_auth(&format!("{scheme} some-credential"));
            assert_eq!(find_var(&vars, "AUTH_TYPE"), Some(scheme));
            assert_eq!(find_var(&vars, "PHP_AUTH_USER"), None, "scheme {scheme}");
            assert_eq!(find_var(&vars, "PHP_AUTH_PW"), None, "scheme {scheme}");
            assert_eq!(find_var(&vars, "PHP_AUTH_DIGEST"), None, "scheme {scheme}");
        }
    }

    /// `Basic` with no payload is not a Basic credential — php-src's comparison
    /// includes the trailing space, so a 5-character "Basic" never matches.
    #[test]
    fn bare_scheme_with_no_payload_yields_only_auth_type() {
        let vars = vars_with_auth("Basic");

        assert_eq!(find_var(&vars, "AUTH_TYPE"), Some("Basic"));
        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), None);
    }

    /// A scheme that is not an RFC 9110 token is not reported at all, so
    /// AUTH_TYPE can never carry arbitrary client-supplied punctuation.
    #[test]
    fn non_token_scheme_yields_no_auth_type() {
        for value in ["", "  ", "\"quoted\" x", "sch\u{7f}eme x"] {
            let vars = vars_with_auth(value);
            assert_eq!(find_var(&vars, "AUTH_TYPE"), None, "value {value:?}");
        }
    }

    /// A NUL anywhere in the decoded credential suppresses BOTH keys. php-src
    /// would truncate at the NUL; handing PHP a silently-shortened password is
    /// an undebuggable auth failure, so ePHPm emits nothing and the app
    /// re-challenges. The username-side case matches php-src exactly (the NUL
    /// hides the colon from `strchr`).
    #[test]
    fn interior_nul_in_a_credential_suppresses_both_keys() {
        for raw in [b"us\0er:pw".as_slice(), b"user:p\0w".as_slice()] {
            let vars = vars_with_auth(&format!("Basic {}", b64(raw)));
            assert_eq!(find_var(&vars, "PHP_AUTH_USER"), None, "raw {raw:?}");
            assert_eq!(find_var(&vars, "PHP_AUTH_PW"), None, "raw {raw:?}");
            // The header itself still reaches PHP, so nothing is hidden.
            assert!(find_var(&vars, "HTTP_AUTHORIZATION").is_some());
        }
    }

    /// Credentials need not be UTF-8 — PHP strings are byte strings, and a
    /// latin-1 password must reach PHP unmangled rather than being lossily
    /// transcoded. Asserted on the FFI form, which is what PHP actually gets.
    #[test]
    fn non_utf8_credentials_reach_php_unmangled() {
        let mut req = make_request();
        req.headers.push(("Authorization".into(), format!("Basic {}", b64(b"user:p\xffw"))));
        let vars = req.server_variables_c();

        let pw = vars.iter().find(|(k, _)| k.as_ref() == c"PHP_AUTH_PW").map(|(_, v)| v.to_bytes());
        assert_eq!(pw, Some(b"p\xffw".as_slice()));
    }

    /// Duplicate Authorization headers: first wins, matching
    /// `cookie_string_from_headers`.
    #[test]
    fn first_authorization_header_wins() {
        let mut req = make_request();
        req.headers.push(("Authorization".into(), format!("Basic {}", b64(b"first:one"))));
        req.headers.push(("authorization".into(), format!("Basic {}", b64(b"second:two"))));
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some("first"));
    }

    /// No Authorization header means none of these keys exist — an app testing
    /// `isset($_SERVER['PHP_AUTH_USER'])` must see false, not an empty string.
    #[test]
    fn no_authorization_header_derives_nothing() {
        let vars = make_request().server_variables();

        for key in ["AUTH_TYPE", "PHP_AUTH_USER", "PHP_AUTH_PW", "PHP_AUTH_DIGEST"] {
            assert_eq!(find_var(&vars, key), None, "{key} must be absent");
        }
    }

    /// REMOTE_USER is deliberately never derived. It denotes a user the *server*
    /// authenticated; ePHPm validates nothing here, so synthesising it from a
    /// client-supplied header would let anyone assert any identity to an app
    /// that trusts it.
    #[test]
    fn remote_user_is_never_derived_from_the_header() {
        let vars = vars_with_auth(&format!("Basic {}", b64(b"admin:hunter2")));

        assert_eq!(find_var(&vars, "REMOTE_USER"), None);
        assert_eq!(find_var(&vars, "PHP_AUTH_USER"), Some("admin"));
    }

    /// The wrapper registers at most MAX_SERVER_VARS (128) entries and silently
    /// drops the rest, so the auth vars must sit in the fixed prefix rather than
    /// after the unbounded header list. A request with enough headers to blow
    /// the cap must still authenticate.
    #[test]
    fn auth_vars_survive_the_server_var_cap() {
        const MAX_SERVER_VARS: usize = 128; // ephpm_wrapper.c
        let mut req = make_request();
        req.headers.push(("Authorization".into(), format!("Basic {}", b64(b"user:pw"))));
        for i in 0..200 {
            req.headers.push((format!("x-filler-{i}"), "v".into()));
        }
        let vars = req.server_variables();

        let position = |key: &str| vars.iter().position(|(k, _)| k == key);
        for key in ["AUTH_TYPE", "PHP_AUTH_USER", "PHP_AUTH_PW"] {
            let at = position(key).unwrap_or_else(|| panic!("{key} missing"));
            assert!(at < MAX_SERVER_VARS, "{key} at index {at} would be dropped by the C cap");
        }
    }

    /// fpm and worker dispatch must agree on the auth vars too — the lanes
    /// share `build_server_variables_c`, and this pins that they keep doing so.
    #[test]
    fn worker_and_request_paths_agree_on_auth_vars() {
        let mut req = make_request();
        req.headers.push(("Authorization".into(), format!("Basic {}", b64(b"user:pw"))));

        let worker_mode = build_server_variables_c(
            &req.method,
            &req.uri,
            &req.query_string,
            &req.script_filename,
            &req.document_root,
            &req.path,
            &req.server_name,
            req.server_port,
            &req.protocol,
            req.remote_addr,
            req.is_https,
            &req.headers,
            &req.env_vars,
        );

        assert_eq!(worker_mode, req.server_variables_c());
        assert!(
            worker_mode.iter().any(|(k, v)| k.as_ref() == c"PHP_AUTH_PW" && v.as_ref() == c"pw")
        );
    }

    /// The lenient decoder is the security-relevant half of this change, so
    /// pin its behaviour directly against php_base64_decode's non-strict rules.
    #[test]
    fn lenient_base64_matches_php_non_strict_rules() {
        // Well-formed round trip.
        assert_eq!(base64_decode_lenient(b"dXNlcjpwdw=="), b"user:pw");
        // Padding is optional.
        assert_eq!(base64_decode_lenient(b"dXNlcjpwdw"), b"user:pw");
        // Characters outside the alphabet are skipped, not fatal.
        assert_eq!(base64_decode_lenient(b"dXNl\r\n cjpwdw=="), b"user:pw");
        assert_eq!(base64_decode_lenient(b"d*X&Nl%cjpwdw"), b"user:pw");
        // A trailing group of a single character contributes no byte: 9 input
        // characters carry 6 whole bytes, and the 9th is dropped.
        assert_eq!(base64_decode_lenient(b"dXNlcjpwX"), b"user:p");
        // Never fails, even on pure garbage.
        assert_eq!(base64_decode_lenient(b"!!!!"), b"");
        assert_eq!(base64_decode_lenient(b""), b"");
    }

    // ── Secret redaction in Debug output ─────────────────────────────────

    /// `$_SERVER` now carries a cleartext password (PHP_AUTH_PW) next to the
    /// per-site DB_PASSWORD. `PhpRequest` is `pub` and sits beside a
    /// `tracing::debug!` on the request path, so its Debug must not print them.
    #[test]
    fn php_request_debug_redacts_credentials() {
        let mut req = make_request();
        req.headers.push(("Authorization".into(), format!("Basic {}", b64(b"user:hunter2"))));
        req.headers.push(("Cookie".into(), "session=super-secret".into()));
        req.env_vars = vec![
            ("DB_PASSWORD".into(), "db-secret".into()),
            ("DATABASE_URL".into(), "mysql://u:db-secret@h/db".into()),
            ("DB_USER".into(), "site-a".into()),
        ];

        let rendered = format!("{req:?}");

        for secret in ["hunter2", "super-secret", "db-secret", &b64(b"user:hunter2")] {
            assert!(!rendered.contains(secret), "Debug leaked {secret:?}: {rendered}");
        }
        // Non-secret context is still there — redaction must not blind the log.
        assert!(rendered.contains("site-a"), "{rendered}");
        assert!(rendered.contains("example.com"), "{rendered}");
    }

    #[test]
    fn secret_name_matching_covers_the_known_credential_keys() {
        for name in [
            "PHP_AUTH_PW",
            "DB_PASSWORD",
            "DATABASE_URL",
            "EPHPM_REDIS_PASSWORD",
            "HTTP_AUTHORIZATION",
            "authorization",
            "Cookie",
            "X-Api-Key",
        ] {
            assert!(is_secret_name(name), "{name} should be treated as secret");
        }
        for name in ["PHP_AUTH_USER", "AUTH_TYPE", "DB_USER", "REQUEST_URI", "HTTP_HOST"] {
            assert!(!is_secret_name(name), "{name} should not be redacted");
        }
    }

    #[test]
    fn test_cookie_string_found() {
        let mut req = make_request();
        req.headers.push(("Cookie".into(), "session=abc123".into()));
        assert_eq!(req.cookie_string(), "session=abc123");
    }

    #[test]
    fn test_cookie_string_missing() {
        let req = make_request();
        assert_eq!(req.cookie_string(), "");
    }

    #[test]
    fn test_cookie_string_case_insensitive() {
        let mut req = make_request();
        req.headers.push(("COOKIE".into(), "token=xyz".into()));
        assert_eq!(req.cookie_string(), "token=xyz");
    }

    #[test]
    fn test_env_vars_injected_into_server_variables() {
        let mut req = make_request();
        req.env_vars = vec![
            ("EPHPM_REDIS_HOST".into(), "127.0.0.1".into()),
            ("EPHPM_REDIS_PORT".into(), "6379".into()),
            ("EPHPM_REDIS_USERNAME".into(), "example.com".into()),
            ("EPHPM_REDIS_PASSWORD".into(), "abc123".into()),
        ];
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "EPHPM_REDIS_HOST"), Some("127.0.0.1"));
        assert_eq!(find_var(&vars, "EPHPM_REDIS_PORT"), Some("6379"));
        assert_eq!(find_var(&vars, "EPHPM_REDIS_USERNAME"), Some("example.com"));
        assert_eq!(find_var(&vars, "EPHPM_REDIS_PASSWORD"), Some("abc123"));
    }

    #[test]
    fn test_env_vars_empty_by_default() {
        let req = make_request();
        let vars = req.server_variables();
        assert!(find_var(&vars, "EPHPM_REDIS_HOST").is_none());
    }

    /// The worker dispatch path in `ephpm-server` builds `$_SERVER` by calling
    /// [`build_server_variables`] directly from its owned locals rather than
    /// constructing a `PhpRequest`. This test guards that both derivations
    /// produce byte-identical output for the same synthetic request — if this
    /// ever diverges, worker mode and fpm mode would present PHP with
    /// different `$_SERVER`, which is a correctness bug.
    #[test]
    fn test_worker_path_server_variables_match_request_mode() {
        // A request that exercises every interesting branch: HTTPS on, a
        // fallback rewrite (uri != script), custom + canonical headers, and
        // injected env vars.
        let mut req = make_request();
        req.uri = "/blog/hello?preview=true".into();
        req.path = "/blog/hello".into();
        req.query_string = "preview=true".into();
        req.is_https = true;
        req.headers = vec![
            ("host".into(), "example.com".into()),
            ("accept-encoding".into(), "gzip, deflate".into()),
            ("content-type".into(), "application/json".into()),
            ("content-length".into(), "42".into()),
            ("x-custom-header".into(), "value".into()),
            ("cookie".into(), "session=abc123".into()),
        ];
        req.env_vars = vec![
            ("EPHPM_REDIS_HOST".into(), "127.0.0.1".into()),
            ("EPHPM_REDIS_PORT".into(), "6379".into()),
        ];

        // fpm path: via PhpRequest::server_variables_c() (what execute_php
        // registers).
        let request_mode = req.server_variables_c();

        // worker path: the exact call `handle_php_worker` makes, built from
        // borrowed/owned fields with no intermediate PhpRequest.
        let worker_mode = build_server_variables_c(
            &req.method,
            &req.uri,
            &req.query_string,
            &req.script_filename,
            &req.document_root,
            &req.path,
            &req.server_name,
            req.server_port,
            &req.protocol,
            req.remote_addr,
            req.is_https,
            &req.headers,
            &req.env_vars,
        );

        // Byte-identical, including order.
        assert_eq!(worker_mode, request_mode);
    }

    #[test]
    fn test_worker_path_cookie_matches_request_mode() {
        let mut req = make_request();
        req.headers.push(("Cookie".into(), "session=abc123".into()));
        assert_eq!(cookie_string_from_headers(&req.headers), req.cookie_string());
    }

    /// The `String` form must stay a faithful view of the FFI form — it is
    /// what every derivation test in this module asserts on, so if the two
    /// ever diverged those tests would silently stop covering what PHP
    /// actually receives.
    #[test]
    fn test_string_form_matches_c_form() {
        let mut req = make_request();
        req.is_https = true;
        req.env_vars = vec![("DB_USER".into(), "site-a".into())];
        let strings = req.server_variables();
        let c_form = req.server_variables_c();
        assert_eq!(strings.len(), c_form.len());
        for ((sk, sv), (ck, cv)) in strings.iter().zip(c_form.iter()) {
            assert_eq!(sk.as_bytes(), ck.to_bytes());
            assert_eq!(sv.as_bytes(), cv.to_bytes());
        }
    }

    /// Multi-tenant guard for the #133 rework: `$_SERVER` is derived fresh
    /// from each request's own resolved site — there is deliberately **no**
    /// cross-request cache in this module (the only shared storage is
    /// `&'static CStr` literals for values identical for every tenant). Two
    /// consecutive builds for different sites on the same thread must each
    /// carry only their own tenant's docroot, host, and injected `DB_*`
    /// credentials.
    #[test]
    fn test_per_site_values_never_bleed_between_requests() {
        let build = |site: &str, docroot: &str, password: &str| {
            build_server_variables_c(
                "GET",
                "/index.php",
                "",
                &PathBuf::from(format!("{docroot}/index.php")),
                &PathBuf::from(docroot),
                "/index.php",
                site,
                443,
                "HTTP/1.1",
                "192.0.2.1:1234".parse().unwrap(),
                true,
                &[("host".to_string(), site.to_string())],
                &[
                    ("DB_USER".to_string(), site.to_string()),
                    ("DB_PASSWORD".to_string(), password.to_string()),
                ],
            )
        };

        let site_a = build("a.example", "/sites/a.example", "secret-a");
        let site_b = build("b.example", "/sites/b.example", "secret-b");

        let get = |vars: &[CServerVar], key: &CStr| -> Option<String> {
            vars.iter()
                .find(|(k, _)| k.as_ref() == key)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };

        for (vars, site, docroot, password) in [
            (&site_a, "a.example", "/sites/a.example", "secret-a"),
            (&site_b, "b.example", "/sites/b.example", "secret-b"),
        ] {
            assert_eq!(get(vars, c"SERVER_NAME").as_deref(), Some(site));
            assert_eq!(get(vars, c"HTTP_HOST").as_deref(), Some(site));
            assert_eq!(get(vars, c"DOCUMENT_ROOT").as_deref(), Some(docroot));
            assert_eq!(get(vars, c"DB_USER").as_deref(), Some(site));
            assert_eq!(get(vars, c"DB_PASSWORD").as_deref(), Some(password));
        }
        // And nothing of A survives into B's view (or vice versa).
        let b_values: Vec<String> =
            site_b.iter().map(|(_, v)| v.to_string_lossy().into_owned()).collect();
        assert!(!b_values.iter().any(|v| v.contains("a.example") || v.contains("secret-a")));
    }
}
