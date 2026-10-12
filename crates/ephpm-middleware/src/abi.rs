//! The versioned C ABI shared between the ePHPm host and middleware modules.
//!
//! A middleware module is a shared library exporting four C symbols:
//!
//! ```c
//! int32_t ephpm_middleware_init(uint32_t abi_version,
//!                               const char* config_json,
//!                               const ephpm_host_v1* host);
//! int32_t ephpm_middleware_invoke(const ephpm_request_t* request,
//!                                 ephpm_response_t* response_out);
//! void    ephpm_middleware_shutdown(void);
//! const char* ephpm_middleware_describe(void);   /* nullable */
//!
//! /* Optional, minor 1 — the response phase. A module that does not export
//!    it is unaffected; the host skips it after generating the response. */
//! int32_t ephpm_middleware_invoke_response(const ephpm_request_t* request,
//!                                          const ephpm_response_ctx_t* response,
//!                                          ephpm_response_edit_t* edit_out);
//!
//! /* Optional — the module's accepted top-level `config` keys, as a static
//!    NUL-terminated JSON array of strings. Absent symbol or NULL return =
//!    "this module does not declare its keys" (no host-side check). */
//! const char* ephpm_middleware_config_keys(void);
//! ```
//!
//! The optional symbols are the only thing that flows **module → host** at
//! load time; [`EphpmHostV1`] flows the other way. That is why a module's
//! self-description ([`SYM_DESCRIBE`], [`SYM_CONFIG_KEYS`]) is an exported
//! symbol rather than a host-table field: the host resolves it with `dlsym`,
//! and its absence is simply "not declared", so adding one never needs an ABI
//! bump on either side.
//!
//! The request phase ([`EphpmResponse`]) fails **closed** — a broken gate must
//! not fail open — while the response phase ([`EphpmResponseEdit`]) fails
//! **safe**: an error or panic there leaves the already-authorized response
//! untouched. Two phases, two policies. Response-phase modules are therefore
//! transforms (compression, ETag, header injection), **not** security gates.
//!
//! The host callback table ([`EphpmHostV1`]) is passed BY POINTER at `init`
//! (valid for the process lifetime) rather than having modules `dlsym` host
//! symbols — exporting symbols from the host executable needs `-rdynamic` on
//! Linux and has no clean Windows analogue, while a table pointer is portable
//! everywhere `dlopen`/`LoadLibrary` is.
//!
//! Request data is exposed through accessor function pointers on the host
//! table (not a flat struct), so fields can be added without breaking the
//! ABI: additions append to the end of the table under the same major
//! version. The major byte of [`ABI_V1`] gates compatibility — modules must
//! refuse to init when the host's major is newer than they were built for.
//!
//! All pointers a module writes into [`EphpmResponse`] must remain valid
//! until its `invoke` returns; the host copies everything before unwinding.

use std::os::raw::{c_char, c_int};

/// Current ABI version (`0xMMmmmmmm`: major byte gates compatibility, the
/// lower three bytes are an additive minor level).
///
/// `0x0100_0005` = major **1**, minor **5**.
///
/// - Minor 1 added the optional response phase (the [`SYM_INVOKE_RESPONSE`]
///   symbol and the three appended `response_*` accessors on [`EphpmHostV1`]).
/// - Minor 2 added three appended request accessors — [`request_scheme`],
///   [`request_is_secure`], and [`request_host`] — and gave the pre-existing
///   [`request_body`] slot real (bounded, buffered) semantics.
/// - Minor 3 is the multi-tenancy correctness pass. It **redefines** two
///   pre-existing surfaces rather than only appending to them:
///   * [`request_vhost_id`] now returns the router's **canonical site key**
///     (NULL when the host matched no vhost) instead of the raw `Host`
///     header (issue #390);
///   * the `kv_*` callbacks now resolve **this request's per-site keyspace**
///     on a multi-tenant node instead of always hitting the process-global
///     store (issue #376), with the global store still reachable through the
///     appended `kv_*_global` slots ([`ABI_MINOR_GLOBAL_KV`]).
///
///   The `kv_*` redefinition is a no-op on a single-site node (one tenant, one
///   store). **`request_vhost_id` is not** — a single-site node matches no
///   vhost, so it returns NULL on *every* request there, where it used to
///   return the `Host`. C modules must null-check it. See
///   [`ABI_MINOR_GLOBAL_KV`] for the migration note.
/// - Minor 4 appends one request accessor, [`request_gate_repo`] — the
///   preview access gate's `owner/name` repository, populated only from the
///   operator-owned per-site override and NULL when unset. Purely additive,
///   exactly like the minor-2 request accessors: an older module never reads
///   the slot and a newer module minor-gates it on [`ABI_MINOR_GATE_REPO`].
/// - Minor 5 appends one node-level slot, [`node_is_multi_tenant`] — whether
///   this node has virtual hosting configured — so a module can finally tell
///   "untenanted node" from "multi-site node, unmatched host", which
///   [`request_vhost_id`] alone reports identically as NULL (issue #469).
///   Purely additive; gated on [`ABI_MINOR_NODE_TENANCY`].
///
/// The major byte is unchanged across every minor, so **every** module built
/// against major 1 still loads: growth is additive.
///
/// The optional module-exported [`SYM_CONFIG_KEYS`] symbol (issue #480) is
/// **not** a minor bump: it flows module → host, so a host that predates it
/// simply never looks it up, and a module that does not export it is
/// unchanged. It landed alongside minor 5.
///
/// [`request_scheme`]: EphpmHostV1::request_scheme
/// [`request_is_secure`]: EphpmHostV1::request_is_secure
/// [`request_host`]: EphpmHostV1::request_host
/// [`request_body`]: EphpmHostV1::request_body
/// [`request_vhost_id`]: EphpmHostV1::request_vhost_id
/// [`request_gate_repo`]: EphpmHostV1::request_gate_repo
/// [`node_is_multi_tenant`]: EphpmHostV1::node_is_multi_tenant
pub const ABI_V1: u32 = 0x0100_0005;

/// Major version — the compatibility gate. A module refuses to init when the
/// host's major (`host.abi_version >> 24`) is newer than its own.
pub const ABI_MAJOR: u32 = 1;

/// Minor version — the additive feature level in the low three bytes of
/// [`ABI_V1`]. A module that reaches for a trailing accessor checks the host's
/// advertised minor (`host.abi_version & 0x00FF_FFFF`) is at least the minor
/// that introduced it before calling: an older host's table does not have
/// those trailing fields, and reading past a shorter table is undefined
/// behaviour. See [`ABI_MINOR_RESPONSE_PHASE`] and
/// [`ABI_MINOR_REQUEST_ACCESSORS`].
pub const ABI_MINOR: u32 = 5;

/// The minor version that introduced the response phase — the three
/// `response_*` accessors on [`EphpmHostV1`] and the [`SYM_INVOKE_RESPONSE`]
/// symbol. A module needs `host.abi_version & 0x00FF_FFFF >=` this before
/// using the response-side host callbacks.
pub const ABI_MINOR_RESPONSE_PHASE: u32 = 1;

/// The minor version that introduced the appended request accessors —
/// [`EphpmHostV1::request_scheme`], [`EphpmHostV1::request_is_secure`], and
/// [`EphpmHostV1::request_host`]. A module needs
/// `host.abi_version & 0x00FF_FFFF >=` this before calling any of them: on an
/// older host those trailing table fields are absent and reading them is
/// undefined behaviour. ([`EphpmHostV1::request_body`] is **not** gated by
/// this — its slot has existed since minor 0; only its buffered semantics are
/// new — so a minor-0 module that already called it stays correct.)
pub const ABI_MINOR_REQUEST_ACCESSORS: u32 = 2;

/// The minor version that introduced the appended process-global KV
/// accessors — [`EphpmHostV1::kv_get_global`] and its four siblings — and, in
/// the same pass, **redefined** two pre-existing surfaces for multi-tenant
/// correctness. A module needs `host.abi_version & 0x00FF_FFFF >=` this before
/// calling any `kv_*_global` slot.
///
/// # What changed for an existing module
///
/// ## [`EphpmHostV1::request_vhost_id`] — changes on **every** node (#390)
///
/// It returns the router's canonical site key rather than the raw `Host`, and
/// **NULL** rather than a client-supplied string when no vhost matched.
///
/// The NULL is not a rare edge case. The router's `resolve_site`
/// short-circuits to its no-site branch whenever no `sites_dir` is
/// configured, so on a **single-site node — the most common
/// deployment shape — this slot returns NULL on every single request**, where
/// in minor ≤ 2 it returned the `Host` and was never NULL.
///
/// **A C module MUST null-check it.** `strcmp(host->request_vhost_id(req), …)`
/// was safe against every prior minor and segfaults now. Rust modules get a
/// compile error instead: the safe wrapper returns `Option<&str>`.
///
/// The behaviour is deliberate — no vhost matched means no tenant, and
/// inventing one from a client-supplied header is exactly the bug #390 fixed.
/// [`EphpmHostV1::request_host`] still gives the host as sent, for the cases
/// that genuinely want it.
///
/// Note this also renames keys built from the vhost id, once, at upgrade — on
/// a single-site node to whatever the module substitutes for "no tenant"
/// (the stock modules use `ephpm_middleware::UNMATCHED_VHOST`).
///
/// ## The `kv_*` slots — change only on a multi-tenant node (#376)
///
/// They resolve **this request's** per-vhost keyspace rather than the
/// process-global store. A module that was relying on node-wide `kv_*` state —
/// an edge-wide rate limiter, an operator-owned config key — must move those
/// calls to the matching `kv_*_global` slot. Everything per-tenant should stay
/// where it is and simply becomes isolated, and now shares a keyspace with the
/// tenant's `ephpm_kv_*`.
///
/// This half genuinely *is* a no-op on a single-site node: there is one store,
/// and both the scoped and global slots reach it.
pub const ABI_MINOR_GLOBAL_KV: u32 = 3;

/// The minor version that introduced the appended request accessor
/// [`EphpmHostV1::request_gate_repo`]. A module needs
/// `host.abi_version & 0x00FF_FFFF >=` this before calling it: on an older
/// host the trailing table slot is absent and reading it is undefined
/// behaviour. Purely additive — nothing pre-existing changed in minor 4.
pub const ABI_MINOR_GATE_REPO: u32 = 4;

/// The minor version that introduced the appended node-level slot
/// [`EphpmHostV1::node_is_multi_tenant`] (issue #469). A module needs
/// `host.abi_version & 0x00FF_FFFF >=` this before calling it: on an older host
/// the trailing table slot is absent and reading it is undefined behaviour.
///
/// **The fallback on an older host must be "not multi-tenant"** (serve), never
/// "multi-tenant" (deny). A module built for minor 5 that assumed tenancy
/// against a minor-4 host would treat every request on a single-site node as an
/// unknown tenant and deny 100% of traffic — issue #453 again, reintroduced
/// through the compatibility path. The safe Rust accessors
/// ([`Request::node_is_multi_tenant`](crate::Request::node_is_multi_tenant),
/// [`Host::node_is_multi_tenant`](crate::Host::node_is_multi_tenant)) already
/// return `false` there. Purely additive — nothing pre-existing changed in
/// minor 5.
pub const ABI_MINOR_NODE_TENANCY: u32 = 5;

/// Middleware verdicts for one request.
pub const ACTION_CONTINUE: c_int = 0;
/// Short-circuit: return `status`/`body` to the client; PHP never runs.
pub const ACTION_RESPOND: c_int = 1;
/// Mutate the request (path/header overrides), then continue the chain.
pub const ACTION_REWRITE: c_int = 2;

/// Log levels for [`EphpmHostV1::log`] (match `tracing` levels).
pub const LOG_ERROR: c_int = 1;
/// Warning level.
pub const LOG_WARN: c_int = 2;
/// Info level.
pub const LOG_INFO: c_int = 3;
/// Debug level.
pub const LOG_DEBUG: c_int = 4;

/// Opaque per-request context. Only valid for the duration of one
/// `ephpm_middleware_invoke` call; never store it.
#[repr(C)]
pub struct EphpmRequest {
    _opaque: [u8; 0],
}

/// Opaque handle to the response being transformed, passed to
/// `ephpm_middleware_invoke_response`. Read it through the host table's
/// `response_status` / `response_headers` / `response_body` accessors; every
/// pointer they return is borrowed and valid **only** for the duration of the
/// one `ephpm_middleware_invoke_response` call — never store it. Mutate the
/// response by filling the [`EphpmResponseEdit`] out-struct instead.
#[repr(C)]
pub struct EphpmResponseCtx {
    _opaque: [u8; 0],
}

/// One header name/value pair (NUL-terminated UTF-8).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EphpmHeaderKv {
    /// Header name.
    pub name: *const c_char,
    /// Header value.
    pub value: *const c_char,
}

/// The module's verdict, filled during `invoke`. Zero-initialized by the
/// host before the call (`action = ACTION_CONTINUE`, everything else null).
#[repr(C)]
pub struct EphpmResponse {
    /// One of `ACTION_CONTINUE` / `ACTION_RESPOND` / `ACTION_REWRITE`.
    pub action: c_int,
    /// HTTP status for `ACTION_RESPOND`.
    pub status: u16,
    /// Response body bytes for `ACTION_RESPOND` (nullable).
    pub body: *const u8,
    /// Length of `body`.
    pub body_len: usize,
    /// New request path for `ACTION_REWRITE` (nullable = keep).
    pub rewrite_path: *const c_char,
    /// Header overrides for `ACTION_REWRITE` **and** extra response headers
    /// for `ACTION_RESPOND` (nullable).
    pub header_overrides: *const EphpmHeaderKv,
    /// Number of entries in `header_overrides`.
    pub header_overrides_len: usize,
    /// Headers appended to the eventual client response for
    /// `ACTION_CONTINUE` / `ACTION_REWRITE` (nullable). For `ACTION_RESPOND`
    /// use `header_overrides`.
    pub response_headers: *const EphpmHeaderKv,
    /// Number of entries in `response_headers`.
    pub response_headers_len: usize,
}

/// The response-phase edit, filled by `ephpm_middleware_invoke_response`. The
/// host zero-initializes it before the call (`status = 0`, every pointer
/// null, every length 0) — a module that touches nothing leaves the response
/// exactly as it was.
///
/// Every pointer a module writes here must stay valid **until its
/// `ephpm_middleware_invoke_response` returns**; the host applies the edit
/// (copying out of module memory) before unwinding, exactly like the
/// request-phase [`EphpmResponse`] contract.
///
/// The host applies the fields in a fixed order: `remove_headers` first, then
/// `set_headers` (replace-or-add, case-insensitive), then `status`, then
/// `body`. **When `body` is non-null the host also recomputes
/// `Content-Length`** to the replacement length (dropping any stale value),
/// so a transform can never desync framing.
#[repr(C)]
pub struct EphpmResponseEdit {
    /// New HTTP status, or `0` to keep the existing one.
    pub status: u16,
    /// Replacement response body (`null` = keep the existing body). Valid
    /// until `invoke_response` returns.
    pub body: *const u8,
    /// Length of `body`.
    pub body_len: usize,
    /// Headers to replace-or-add, case-insensitively (nullable). Each name
    /// present in the response is overwritten; absent names are appended.
    pub set_headers: *const EphpmHeaderKv,
    /// Number of entries in `set_headers`.
    pub set_headers_len: usize,
    /// Header names to delete, case-insensitively (nullable). Each is a
    /// NUL-terminated UTF-8 string.
    pub remove_headers: *const *const c_char,
    /// Number of entries in `remove_headers`.
    pub remove_headers_len: usize,
}

/// Version-1 host callback table, passed to `ephpm_middleware_init` and valid
/// for the process lifetime.
///
/// KV operations hit the embedded, gossip-replicated store — the same data
/// PHP sees through `ephpm_kv_*` — which is what makes a cluster-wide rate
/// limiter a single `kv_incr_ttl` call.
///
/// Since minor 3 the `kv_*` slots resolve **the keyspace of the vhost serving
/// the current request**, so "the same data PHP sees" holds per tenant on a
/// multi-tenant node rather than only on a single-site one. The process-global
/// store — where operator-owned state lives, out of reach of any tenant's
/// PHP — is reachable through the appended `kv_*_global` slots. See
/// [`ABI_MINOR_GLOBAL_KV`].
#[repr(C)]
pub struct EphpmHostV1 {
    /// Host ABI version (== [`ABI_V1`] for this table).
    pub abi_version: u32,

    // ── Request accessors (valid only during `invoke`) ───────────────────
    /// HTTP method (`"GET"`, ...).
    pub request_method: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// URL path (no query string).
    pub request_path: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Raw query string (no leading `?`; empty when absent).
    pub request_query: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Client IP after trusted-proxy resolution.
    pub request_remote_ip: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Header lookup by case-insensitive name; NULL when absent. Multi-value
    /// headers return the values pre-joined per HTTP list semantics.
    pub request_header:
        unsafe extern "C" fn(*const EphpmRequest, name: *const c_char) -> *const c_char,
    /// Bounded, read-only view of the buffered request body. Writes the byte
    /// slice base into `*out_ptr` and returns its length; the slice is valid
    /// only for the duration of the request (`invoke`) — never store it.
    ///
    /// The body is buffered by the host ONLY when the operator opts in with
    /// `[server.request] middleware_body_limit > 0`; that knob also caps how
    /// many bytes are exposed here (a longer body is truncated to the limit for
    /// this view — PHP still receives the complete body). When the knob is `0`
    /// (the default) the chain runs before any body byte is read — rejecting
    /// BEFORE the body transfer is the point — and this returns length 0 with
    /// `*out_ptr` set null. It is also 0 on the static-file and response-phase
    /// paths, which carry no request body.
    ///
    /// This slot has existed since minor 0 (it returned 0 then); only the
    /// buffered semantics are new in minor 2, so it needs no minor gate.
    pub request_body: unsafe extern "C" fn(*const EphpmRequest, out_ptr: *mut *const u8) -> usize,
    /// The **canonical site key** of the vhost serving this request, or NULL
    /// when the request matched no known virtual host.
    ///
    /// This is the value the router's one host→tenant derivation produced —
    /// the same identity that selects this request's per-site database file,
    /// KV keyspace and OPcache vhost. It is suffix-stripped, port-stripped,
    /// lowercased and allowlist-validated, so `Host: Site.Example`,
    /// `site.example:8080` and `site.example.` all yield one key.
    ///
    /// **NULL means "no tenant", and a module must not invent one.** ePHPm
    /// does not reject unrecognised hosts by default, so any host that matched
    /// no vhost is an arbitrary client-supplied string; returning NULL is what
    /// lets a gate tell "tenant `foo`" from "someone sent `Host: foo`" and
    /// fail closed instead of keying policy on attacker input.
    ///
    /// **NULL-check this, always.** It is not a rare edge case: a **single-site
    /// node matches no vhost at all**, so this returns NULL on *every* request
    /// there — and that is the most common deployment shape. In minor ≤ 2 the
    /// slot was never NULL, so a C module doing
    /// `strcmp(host->request_vhost_id(req), …)` was safe then and crashes now.
    /// See [`ABI_MINOR_GLOBAL_KV`] for the full migration note.
    ///
    /// Changed in **minor 3** (issue #390). Before that this returned the raw
    /// `Host` header with only the port stripped — un-normalized, never NULL.
    /// Three separate modules had each re-normalized it locally, one of them
    /// with an auth bypass (a `Host` differing only in letter case missed its
    /// credential entry). For the un-normalized/raw view use
    /// [`request_host`](Self::request_host), which is the normalized request
    /// host and is explicitly *not* a tenant identity.
    pub request_vhost_id: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,

    // ── KV store ─────────────────────────────────────────────────────────
    /// Get. Returns 0 with `out`/`out_len` set (free with `kv_free`),
    /// 1 when the key is absent, negative on error.
    pub kv_get: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int,
    /// Set with optional TTL (`ttl_secs <= 0` = no expiry). Returns 0 on
    /// success, negative on error.
    pub kv_set: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
        ttl_secs: i64,
    ) -> c_int,
    /// Set-if-absent. Returns 0 when set, 1 when the key already existed,
    /// negative on error.
    pub kv_set_nx: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
        ttl_secs: i64,
    ) -> c_int,
    /// Atomic increment. Returns 0 with the new value in `out`, negative on
    /// error (e.g. non-numeric existing value).
    pub kv_incr:
        unsafe extern "C" fn(key: *const u8, key_len: usize, by: i64, out: *mut i64) -> c_int,
    /// Free a buffer returned by `kv_get`.
    pub kv_free: unsafe extern "C" fn(ptr: *mut u8, len: usize),

    // ── Logging ──────────────────────────────────────────────────────────
    /// Log through the host's `tracing` subscriber (`LOG_*` levels).
    pub log: unsafe extern "C" fn(level: c_int, msg: *const u8, msg_len: usize),

    // ── Additions (appended for ABI compatibility) ───────────────────────
    /// Atomic increment that applies `ttl_secs` (`<= 0` = no expiry) ONLY
    /// when the increment creates the key. Existing keys keep their current
    /// TTL — fixed-window semantics. Returns 0 with the new value in `out`,
    /// negative on error (e.g. non-numeric existing value).
    ///
    /// Appended after [`EphpmHostV1::log`]: older middleware built against a
    /// table without this field never reads past the offsets it knows, so
    /// growing the table at the end is ABI-compatible.
    pub kv_incr_ttl: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        by: i64,
        ttl_secs: i64,
        out: *mut i64,
    ) -> c_int,

    // ── Response accessors (minor 1; valid only during `invoke_response`) ──
    //
    // Appended after `kv_incr_ttl`: a module built against minor 0 never
    // reads these offsets, so the growth is ABI-compatible. A module that
    // DOES read them must first confirm `abi_version & 0x00FF_FFFF >=
    // ABI_MINOR_RESPONSE_PHASE`, otherwise it would read past a shorter
    // (older-host) table.
    /// Current response status for `invoke_response`.
    pub response_status: unsafe extern "C" fn(*const EphpmResponseCtx) -> u16,
    /// Borrow the response's header list. Writes the array base into
    /// `*out_ptr` and returns its length; the array is valid only for the
    /// duration of the call. Duplicate headers (e.g. `Set-Cookie`) appear as
    /// separate entries.
    pub response_headers:
        unsafe extern "C" fn(*const EphpmResponseCtx, out_ptr: *mut *const EphpmHeaderKv) -> usize,
    /// Borrow the response body. Writes the byte-slice base into `*out_ptr`
    /// and returns its length; valid only for the duration of the call.
    pub response_body:
        unsafe extern "C" fn(*const EphpmResponseCtx, out_ptr: *mut *const u8) -> usize,

    // ── Request accessors (minor 2; valid only during `invoke`) ───────────
    //
    // Appended after `response_body`: a module built against minor 0 or 1
    // never reads these offsets, so the growth is ABI-compatible. A module
    // that DOES read them must first confirm `abi_version & 0x00FF_FFFF >=
    // ABI_MINOR_REQUEST_ACCESSORS`, otherwise it would read past a shorter
    // (older-host) table.
    /// Request URL scheme, authoritative from the connection: `"https"` when
    /// the client's TLS terminated here, `"http"` otherwise. Derived from the
    /// real connection TLS state (after trusted-proxy resolution), NOT from a
    /// client-supplied `X-Forwarded-Proto` header — so a `force_https` gate can
    /// trust it. Never null.
    pub request_scheme: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Whether the request arrived over a secure (HTTPS/TLS) connection — the
    /// boolean form of [`request_scheme`](Self::request_scheme). `1` = secure,
    /// `0` = plaintext.
    pub request_is_secure: unsafe extern "C" fn(*const EphpmRequest) -> c_int,
    /// Normalized request `Host` — port stripped, one trailing FQDN-root dot
    /// stripped, lowercased (the same normalization the router uses to key a
    /// vhost). **Not a tenant identity**: it is what the client sent, not what
    /// the router matched, and it is populated even for a host that names no
    /// vhost. Use [`request_vhost_id`](Self::request_vhost_id) for tenancy
    /// decisions and this only when you genuinely want the request host (a
    /// canonical-host redirect, a log line). Empty (never null) when the
    /// request carried no usable `Host`.
    pub request_host: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,

    // ── Process-global KV (minor 3) ───────────────────────────────────────
    //
    // Appended after `request_host`. A module that reads these must first
    // confirm `abi_version & 0x00FF_FFFF >= ABI_MINOR_GLOBAL_KV`.
    //
    // The same five operations as the `kv_*` slots above, against the
    // **process-wide** store instead of this request's per-site keyspace. Use
    // them only for state that is genuinely node-scoped: operator-owned
    // configuration, which must live where no tenant's PHP can rewrite it, or
    // a deliberately node-wide edge counter. Anything belonging to the tenant
    // being served belongs in the request-scoped `kv_*` slots — which is where
    // PHP's `ephpm_kv_*` puts it, so the two lanes meet there by default.
    //
    // On a single-site node these reach the same store as `kv_*`.
    /// Global-store `kv_get`. Same return contract; free with `kv_free`.
    pub kv_get_global: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int,
    /// Global-store `kv_set`.
    pub kv_set_global: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
        ttl_secs: i64,
    ) -> c_int,
    /// Global-store `kv_set_nx`.
    pub kv_set_nx_global: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
        ttl_secs: i64,
    ) -> c_int,
    /// Global-store `kv_incr`.
    pub kv_incr_global:
        unsafe extern "C" fn(key: *const u8, key_len: usize, by: i64, out: *mut i64) -> c_int,
    /// Global-store `kv_incr_ttl`.
    pub kv_incr_ttl_global: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        by: i64,
        ttl_secs: i64,
        out: *mut i64,
    ) -> c_int,

    // ── Preview access gate (minor 4) ─────────────────────────────────────
    //
    // Appended after `kv_incr_ttl_global`. A module that reads this must first
    // confirm `abi_version & 0x00FF_FFFF >= ABI_MINOR_GATE_REPO`.
    /// The preview access gate's target repository for this request, as an
    /// `owner/name` string, or NULL when this vhost has no gate repository
    /// configured.
    ///
    /// This is a **trusted, router-populated** channel, exactly like
    /// [`request_vhost_id`](Self::request_vhost_id): its value comes only from
    /// the operator-owned per-site override file (`[preview_auth] repo`), never
    /// from anything a client can send. It exists so the OAuth issuer can seal
    /// the repository a preview is *for* into the signed OAuth `state` at login
    /// — login always runs on the target preview host, where the router knows
    /// the site and its repo, whereas the callback lands on the apex host where
    /// it does not. A module must not read a repository from any request header;
    /// that would be client-controllable and defeats the gate.
    ///
    /// **NULL means "no gate repository", and a module must fail closed on it**
    /// when it is operating in a per-preview mode that requires one — inventing
    /// one would authorize against an attacker-chosen target. Rust modules get
    /// `Option<&str>` from [`Request::gate_repo`](crate::Request::gate_repo),
    /// which already null-checks and empty-checks.
    pub request_gate_repo: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,

    // ── Node tenancy (minor 5) ────────────────────────────────────────────
    //
    // Appended after `request_gate_repo`. A module that reads this must first
    // confirm `abi_version & 0x00FF_FFFF >= ABI_MINOR_NODE_TENANCY`, and must
    // treat an older host as "not multi-tenant".
    /// `1` when this node has virtual hosting configured (`[server] sites_dir`),
    /// `0` when it does not.
    ///
    /// This is what lets a module tell apart the two situations in which
    /// [`request_vhost_id`](Self::request_vhost_id) returns NULL:
    ///
    /// * `0` — the node has **no tenancy**. Every request is NULL there; treat
    ///   it as the one untenanted bucket and serve it.
    /// * `1` — the node **is** multi-tenant, so a NULL vhost id means this
    ///   request's `Host` matched no site. A gate that wants "deny unknown
    ///   tenants" can deny exactly this case.
    ///
    /// A **node-level** fact, not request state: it takes no request pointer,
    /// is valid to call from `init` as well as from `invoke`/`invoke_response`,
    /// and does not change for the life of the process (there is no runtime
    /// config reload). It is a function rather than a plain field so it can
    /// stay correct if that ever changes.
    pub node_is_multi_tenant: unsafe extern "C" fn() -> c_int,
}

/// Symbol names the loader looks up.
pub const SYM_INIT: &[u8] = b"ephpm_middleware_init";
/// Per-request entrypoint symbol.
pub const SYM_INVOKE: &[u8] = b"ephpm_middleware_invoke";
/// Shutdown symbol.
pub const SYM_SHUTDOWN: &[u8] = b"ephpm_middleware_shutdown";
/// Optional metadata symbol.
pub const SYM_DESCRIBE: &[u8] = b"ephpm_middleware_describe";
/// Optional response-phase entrypoint symbol (minor 1). A module that does
/// not export it has no response phase and the host skips it in that phase.
pub const SYM_INVOKE_RESPONSE: &[u8] = b"ephpm_middleware_invoke_response";
/// Optional config-key declaration symbol (issue #480).
///
/// Returns a NUL-terminated, UTF-8 JSON **array of strings** naming every
/// top-level `config` key the module accepts — e.g.
/// `["burst","key_headers","per_ip_rps"]` — or NULL. The pointer must stay
/// valid for the life of the library (a static string, or one the module
/// allocates once and never frees), and the function must be callable before
/// `ephpm_middleware_init`: the host reads it first.
///
/// | Module | Host behaviour |
/// |---|---|
/// | does not export the symbol | mounts as before — no check, no warning |
/// | exports it, returns NULL | the same: "this module does not declare its keys" |
/// | returns a valid array | rejects, **before `init`**, a mount whose `config` names any other top-level key — the same error, did-you-mean included, a builtin gives |
/// | returns anything else | refuses the mount: a module that answers the question wrongly is a bug, not a compatibility case |
///
/// `[]` is a real declaration — "accepts no keys" — and is not the same as
/// NULL. Only the top level is described; nested sections and free-form maps
/// are the module's own business.
///
/// A Rust module gets this for free: [`declare!`](crate::declare) exports it
/// from [`Middleware::CONFIG_KEYS`](crate::Middleware::CONFIG_KEYS) (NULL when
/// that is `None`).
pub const SYM_CONFIG_KEYS: &[u8] = b"ephpm_middleware_config_keys";

/// `ephpm_middleware_init` signature.
pub type InitFn = unsafe extern "C" fn(u32, *const c_char, *const EphpmHostV1) -> c_int;
/// `ephpm_middleware_invoke` signature.
pub type InvokeFn = unsafe extern "C" fn(*const EphpmRequest, *mut EphpmResponse) -> c_int;
/// `ephpm_middleware_invoke_response` signature (optional; minor 1). Return
/// `0` to apply the edit, non-zero to signal an error — on which the host
/// leaves the response unchanged (fail-safe: a broken transform must not
/// corrupt a good response).
pub type InvokeResponseFn = unsafe extern "C" fn(
    *const EphpmRequest,
    *const EphpmResponseCtx,
    *mut EphpmResponseEdit,
) -> c_int;
/// `ephpm_middleware_shutdown` signature.
pub type ShutdownFn = unsafe extern "C" fn();
/// `ephpm_middleware_describe` signature.
pub type DescribeFn = unsafe extern "C" fn() -> *const c_char;
/// `ephpm_middleware_config_keys` signature (optional; see [`SYM_CONFIG_KEYS`]).
pub type ConfigKeysFn = unsafe extern "C" fn() -> *const c_char;

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::*;

    /// Minor 5 must be an **append**: every pre-existing slot keeps its
    /// offset, so a module built against minor 4 (or older) reads exactly the
    /// table it was compiled for, and the new slot sits past the end of what
    /// it knows. A field inserted anywhere else would shift `request_gate_repo`
    /// and break every older module silently — this pins it.
    #[test]
    fn node_tenancy_slot_is_appended_after_every_older_slot() {
        let slot = size_of::<usize>();
        assert_eq!(
            offset_of!(EphpmHostV1, node_is_multi_tenant),
            offset_of!(EphpmHostV1, request_gate_repo) + slot,
            "the minor-5 slot must directly follow the minor-4 slot"
        );
        assert_eq!(
            size_of::<EphpmHostV1>(),
            offset_of!(EphpmHostV1, node_is_multi_tenant) + slot,
            "the minor-5 slot must be the last field"
        );
    }

    /// The advertised version, the minor constant and the newest gate agree —
    /// a module gating on `ABI_MINOR_NODE_TENANCY` against this host passes.
    #[test]
    fn advertised_minor_matches_the_newest_gate() {
        assert_eq!(ABI_V1 >> 24, ABI_MAJOR);
        assert_eq!(ABI_V1 & 0x00FF_FFFF, ABI_MINOR);
        assert_eq!(ABI_MINOR, ABI_MINOR_NODE_TENANCY);
    }
}
