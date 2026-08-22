/*
 * C wrapper for PHP embed SAPI — custom SAPI callbacks + request lifecycle.
 *
 * PHP uses setjmp/longjmp for error handling via zend_try/zend_catch macros.
 * These macros cannot be used from Rust (they expand to setjmp which must be
 * called from C). This wrapper provides:
 *
 *   1. Custom SAPI callbacks (ub_write, read_post, read_cookies, etc.) that
 *      capture PHP output and bridge HTTP request data into PHP.
 *
 *   2. Per-request lifecycle management (request_shutdown → set info →
 *      request_startup → execute → capture response).
 *
 *   3. Safe script execution with zend_try/zend_catch bailout protection.
 *
 * The embed SAPI lifecycle:
 *   php_embed_init()          — module startup + initial request startup
 *   ephpm_install_sapi()      — override default callbacks with ours
 *   ephpm_finalize_init()     — mark initial request active (HTTP mode)
 *   ephpm_execute_request()×N — reuse request: update SAPI → execute → capture
 *   php_embed_shutdown()      — request shutdown + module shutdown
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <setjmp.h>
#include <ctype.h>

/* MSVC has no POSIX strtok_r; its strtok_s has the same 3-arg semantics. */
#ifdef _MSC_VER
static char *ephpm_strtok_r(char *str, const char *delim, char **saveptr) {
    return strtok_s(str, delim, saveptr);
}
#define strtok_r ephpm_strtok_r
#endif
#include "sapi/embed/php_embed.h"
#include "main/php.h"
#include "main/SAPI.h"
#include "main/php_main.h"
#include "main/php_variables.h"
#include "main/php_streams.h"
#include "main/php_output.h"
#include "Zend/zend.h"
#include "Zend/zend_ini.h"
#include "Zend/zend_constants.h"
#include "Zend/zend_stream.h"
#include "Zend/zend_call_stack.h"
#include "Zend/zend_exceptions.h"
#include "Zend/zend_globals.h"
#include "Zend/zend_smart_str.h"
#include "main/php_version.h"
#include "ext/session/php_session.h"

#if PHP_VERSION_ID < 80400
#include <ctype.h>
/* PHP 8.4 added the public `sapi_read_post_data()` helper; 8.3 and earlier
 * keep the identical logic inline inside `sapi_activate()`, with no callable
 * entry point. Our request-reuse model needs to drive it explicitly (to set
 * SG(request_info).post_entry for sapi_handle_post() and to read the raw body
 * into request_body for php://input), so on pre-8.4 we replicate 8.4's
 * sapi_read_post_data() verbatim. It uses only globals/APIs present since
 * well before 8.3 (SG(known_post_content_types), post_entry, content_type_dup,
 * sapi_module.default_post_reader), so it compiles and behaves identically. */
static void ephpm_sapi_read_post_data_compat(void) {
    sapi_post_entry *post_entry;
    uint32_t content_type_length = (uint32_t)strlen(SG(request_info).content_type);
    char *content_type = estrndup(SG(request_info).content_type, content_type_length);
    char *p;
    char oldchar = 0;
    void (*post_reader_func)(void) = NULL;

    /* Lowercase the content type and trim trailing descriptive data so only
     * the bare "type/subtype" remains for the handler lookup. */
    for (p = content_type; p < content_type + content_type_length; p++) {
        switch (*p) {
            case ';':
            case ',':
            case ' ':
                content_type_length = p - content_type;
                oldchar = *p;
                *p = 0;
                break;
            default:
                *p = tolower((unsigned char)*p);
                break;
        }
    }

    /* Find an appropriate POST content handler (e.g. rfc1867 for multipart). */
    if ((post_entry = zend_hash_str_find_ptr(&SG(known_post_content_types), content_type,
            content_type_length)) != NULL) {
        SG(request_info).post_entry = post_entry;
        post_reader_func = post_entry->post_reader;
    } else {
        SG(request_info).post_entry = NULL;
        if (UNEXPECTED(!sapi_module.default_post_reader)) {
            SG(request_info).content_type_dup = NULL;
            sapi_module.sapi_error(E_WARNING, "Unsupported content type:  '%s'", content_type);
            efree(content_type);
            return;
        }
    }
    if (oldchar) {
        *(p - 1) = oldchar;
    }

    SG(request_info).content_type_dup = content_type;

    if (post_reader_func) {
        post_reader_func();
    }
    if (sapi_module.default_post_reader) {
        sapi_module.default_post_reader();
    }
}
#endif /* PHP_VERSION_ID < 80400 */

/* ===== Per-thread state =====
 *
 * With ZTS, multiple threads execute PHP concurrently. All per-request
 * state must be thread-local to avoid races. Every shipped platform is ZTS
 * — including Windows, whose php8embed.lib is a ZTS build (#326). On a
 * hypothetical non-ZTS build, thread-local storage is harmless (a single
 * thread executes PHP).
 *
 * EPHPM_TLS picks the right keyword per compiler:
 *   - GCC / Clang: __thread (the long-standing extension; works pre-C11)
 *   - MSVC:        __declspec(thread) (MSVC's equivalent; __thread isn't
 *                  a keyword in MSVC and produces "undeclared identifier"
 *                  for every variable trying to use it)
 *
 * C11's _Thread_local would work on both, but we'd need /std:c11 (or
 * /std:c17) on MSVC and -std=c11 on GCC to enable it. The macro keeps
 * the build-flag surface unchanged.
 */
#if defined(_MSC_VER)
# define EPHPM_TLS __declspec(thread)
#else
# define EPHPM_TLS __thread
#endif

static EPHPM_TLS char *output_buf = NULL;
static EPHPM_TLS size_t output_len = 0;
static EPHPM_TLS size_t output_cap = 0;

/* Response header buffer — "Name: Value\n" lines after script execution */

static EPHPM_TLS char *headers_buf = NULL;
static EPHPM_TLS size_t headers_buf_len = 0;
static EPHPM_TLS size_t headers_buf_cap = 0;

/* Saved response status */

static EPHPM_TLS int response_status_code = 200;

/* Request info — pointers into Rust-owned CStrings, valid only during execution */

static EPHPM_TLS const char *req_method = NULL;
static EPHPM_TLS const char *req_uri = NULL;
static EPHPM_TLS const char *req_query_string = NULL;
static EPHPM_TLS const char *req_content_type = NULL;
static EPHPM_TLS const char *req_cookie_data = NULL;
static EPHPM_TLS const char *req_post_data = NULL;
static EPHPM_TLS size_t req_post_data_len = 0;
static EPHPM_TLS size_t req_post_data_offset = 0;
static EPHPM_TLS const char *req_path_translated = NULL;

/* Worker mode: when set, this thread's request body is streamed from Rust via
 * g_worker_ops.body_read rather than served from the in-memory req_post_data
 * buffer. read_post and the bodyStream() php_stream both pull from the same
 * incremental reader, so the body is consumed exactly once (whichever reads
 * first wins — see the Envelope docs). Reset per iteration. */
static EPHPM_TLS int req_body_streaming = 0;

/* Worker mode: non-zero from take_request() returning a request until the
 * matching send_response()/send_response_stream() completes. Lets
 * ephpm_worker_run() detect a script that ended mid-request (exit()/die(),
 * wp_die(), a loop break) and synthesize the response from SAPI state instead
 * of dropping it. */
static EPHPM_TLS int req_in_flight = 0;

/* Worker mode: bumped once per take_request(). A bodyStream() resource
 * captures the generation at open; reads from a stale generation return EOF
 * so a resource stashed across iterations can never read the NEXT request's
 * body (cross-request isolation). */
static EPHPM_TLS unsigned long req_generation = 0;

/* Non-NULL sentinel for SG(server_context). sapi_activate() only parses the
 * POST body when server_context is set; the value itself is never dereferenced
 * by our SAPI, so a single shared marker address suffices. */
static int ephpm_server_context_marker = 0;

#ifdef EPHPM_NATIVE_EXEC_TIMER
/* Configured max_execution_time (seconds), set once from Rust after
 * php_embed_init() via ephpm_set_max_execution_time(). This is the arm value
 * both request paths use.
 *
 * Why not read it from the ini? The embed SAPI resets the max_execution_time
 * ini entry to 0 (unlimited) at request runtime, so INI_INT("max_execution_time")
 * and EG(timeout_seconds) are 0 by the time a request executes — relying on
 * PHP's own ini->timer arming would leave the timer disarmed. Arming explicitly
 * from this process-wide configured value is immune to that reset (and to a
 * previous request's set_time_limit(0), which alters the ini entry at RUNTIME
 * stage). set_time_limit() during a request still re-arms live on top of it. */
static long g_configured_max_exec_secs = 0;
#endif

/* Record the configured max_execution_time. No-op on builds without per-thread
 * execution timers. Called once from Rust after php_embed_init(), before the
 * tokio runtime starts, so no locking is needed. */
void ephpm_set_max_execution_time(long secs)
{
#ifdef EPHPM_NATIVE_EXEC_TIMER
    g_configured_max_exec_secs = secs;
#else
    (void)secs;
#endif
}

#ifdef EPHPM_NATIVE_EXEC_TIMER
/* Arm PHP's per-thread execution timer for a request AND mirror the effective
 * limit into the max_execution_time ini entry, so userland ini_get() reports
 * the value that is actually being enforced instead of the 0 the embed SAPI
 * leaves behind (#279). Called at the start of every request on both the fpm
 * and worker paths.
 *
 * Two steps, order-sensitive:
 *
 *   1. zend_set_timeout(secs, reset_signals=1) is the AUTHORITATIVE arm: it
 *      (re)installs the SIGRTMIN disposition and starts the countdown from the
 *      configured baseline. Doing this first means the timer state does not
 *      depend on what step 2's ini handler does.
 *
 *   2. zend_alter_ini_entry_chars(...STAGE_RUNTIME) updates the ini entry so
 *      ini_get('max_execution_time') reflects reality. This is exactly how PHP's
 *      own set_time_limit() writes the value. Altering the entry at RUNTIME
 *      stage re-enters max_execution_time's OnUpdateTimeout handler, which sets
 *      EG(timeout_seconds) and re-arms via zend_set_timeout(secs, 0). That
 *      re-entrant re-arm is harmless and does NOT fight step 1 or leak a timer:
 *      it targets the same per-thread POSIX timer with the same value
 *      (timer_settime replaces the setting in place — no new timer_create), the
 *      SIGRTMIN handler is already installed by step 1, and it merely restarts
 *      the identical countdown microseconds later. secs==0 leaves the entry at
 *      "0" (genuinely unlimited) and OnUpdateTimeout disarms — preserving #277's
 *      "0 means only the server backstop applies" semantics.
 *
 * A subsequent userland set_time_limit(N) during the request still overrides
 * both the timer and ini_get, live, on top of this baseline (its own
 * zend_alter_ini_entry_chars call runs after this one returns).
 *
 * No zend_try guard: zend_set_timeout and ini alteration do not zend_bailout
 * (they return FAILURE on error rather than longjmp'ing), matching the bare
 * zend_set_timeout calls this replaces and the request_ini replay above. */
static void ephpm_arm_exec_timer(void)
{
    zend_set_timeout(g_configured_max_exec_secs, 1);

    char buf[32];
    int n = snprintf(buf, sizeof(buf), "%ld", g_configured_max_exec_secs);
    if (n > 0 && (size_t)n < sizeof(buf)) {
        zend_string *key = zend_string_init(ZEND_STRL("max_execution_time"), 0);
        zend_alter_ini_entry_chars(key, buf, (size_t)n, ZEND_INI_USER,
                                   ZEND_INI_STAGE_RUNTIME);
        zend_string_release(key);
    }
}
#endif

/* Worker mode — lazy Envelope backing store (Phase 1 fast path).
 *
 * `take_request` stashes borrowed pointers to the Rust-owned request data
 * here without materializing any of the four PHP arrays (serverVars,
 * headers, cookies, query). The Envelope's accessor methods build each
 * array on first call, cache it as a property, and return the cached
 * value on subsequent calls in the same request.
 *
 * Lifetime: the pointers borrow from `CurrentRequest` in
 * `crates/ephpm-php/src/worker_bridge.rs`, which the Rust side keeps alive
 * from the moment `worker_take_request` publishes it until the matching
 * `send_response` completes on the same worker thread — the same "valid
 * until execute returns" contract `ephpm_request_set_info` relies on
 * (:653-672). The C wrapper never dereferences these pointers outside a
 * matched take_request/send_response pair.
 *
 * Cross-iteration isolation: an Envelope stashed by userland across the
 * loop iteration would still hold the old `req_generation` on its
 * `generation` property; each accessor compares against the current
 * `req_generation` and returns an empty array if it does not match, so a
 * stashed Envelope can never read the next request's data. */
static EPHPM_TLS size_t             req_lazy_server_count = 0;
static EPHPM_TLS const char *const *req_lazy_server_keys  = NULL;
static EPHPM_TLS const char *const *req_lazy_server_vals  = NULL;
static EPHPM_TLS size_t             req_lazy_header_count = 0;
static EPHPM_TLS const char *const *req_lazy_header_keys  = NULL;
static EPHPM_TLS const char *const *req_lazy_header_vals  = NULL;
static EPHPM_TLS const char        *req_lazy_cookie_data  = NULL;
static EPHPM_TLS const char        *req_lazy_query_string = NULL;

/* Pull the next chunk of a streaming worker-mode request body (defined with
 * the worker-mode block below, but used by the read_post SAPI callback above
 * it). Returns bytes written into buf (0 = EOF, negative = error). */
static long ephpm_worker_body_read(char *buf, size_t cap);

/* Server variables */

#define MAX_SERVER_VARS 128

static EPHPM_TLS struct {
    const char *key;
    const char *value;
} server_vars[MAX_SERVER_VARS];

static EPHPM_TLS int server_var_count = 0;

/* Per-request INI overrides (e.g. open_basedir for vhost isolation).
 *
 * These must be (re)applied AFTER php_request_startup() on every request:
 * php_request_shutdown() runs zend_ini_deactivate(), which restores every
 * entry modified during the request to its original value, so an override
 * applied before the per-request shutdown/startup cycle is wiped before the
 * script runs. We buffer the key/value pairs here (owning copies, since the
 * caller frees its strings once ephpm_request_set_ini returns) and replay
 * them inside ephpm_execute_request once the fresh request is live. */
#define MAX_REQUEST_INI 16

static EPHPM_TLS char *request_ini_keys[MAX_REQUEST_INI];
static EPHPM_TLS char *request_ini_vals[MAX_REQUEST_INI];
static EPHPM_TLS size_t request_ini_count = 0;

/* Track whether a PHP request is currently active on this thread */
static EPHPM_TLS int request_active = 0;

/* PHP middleware lane (`[[middleware]] library = "php:<path>"`, EXPERIMENTAL).
 *
 * These scripts run inside the SAME PHP request as the application script,
 * immediately before it — the position PHP's own `auto_prepend_file` occupies,
 * except there can be several, each scoped by its mount's `match` glob and
 * carrying its own `config`.
 *
 * Running them in-request rather than as their own ephpm_execute_request() is
 * the whole design: superglobals, open_basedir, sys_temp_dir/session.save_path,
 * the OPcache vhost, the per-site DB session, the execution timer and the crash
 * guard are all already correct for this request, so a middleware file inherits
 * every isolation and safety property the app script has instead of needing a
 * parallel set. The marginal cost is one extra (OPcache-cached)
 * php_execute_script() per matching mount.
 *
 * Pointers are borrowed from Rust and must stay valid until
 * ephpm_execute_request() returns — same contract as server_vars above. */
#define MAX_REQUEST_MIDDLEWARE 16

static EPHPM_TLS const char *req_middleware_paths[MAX_REQUEST_MIDDLEWARE];
static EPHPM_TLS const char *req_middleware_configs[MAX_REQUEST_MIDDLEWARE];
static EPHPM_TLS size_t req_middleware_count = 0;

/* Index of the middleware file currently executing, -1 when none. Drives
 * ephpm_middleware_config(), which therefore returns NULL when called from
 * anywhere other than a middleware file. */
static EPHPM_TLS int req_middleware_active = -1;

/* How the chain ended for the request just executed, read back by Rust for the
 * `ephpm_middleware_invocations_total{action=...}` label. */
#define EPHPM_MW_CONTINUE 0
#define EPHPM_MW_EXIT     1
#define EPHPM_MW_FATAL    2
static EPHPM_TLS int req_middleware_outcome = EPHPM_MW_CONTINUE;

/* How many mounts actually executed. Equals req_middleware_count when the whole
 * chain continued; otherwise the 1-based position of the mount that ended it,
 * so the metric can attribute `respond`/`error` to the right mount instead of
 * smearing it across every mount that matched. */
static EPHPM_TLS int req_middleware_ran = 0;

/* Duplicate a C string with plain malloc (must outlive the Zend per-request
 * allocator, so estrdup is unsuitable). Returns NULL on OOM or NULL input. */
static char *ephpm_strdup_malloc(const char *s)
{
    if (!s) {
        return NULL;
    }
    size_t n = strlen(s) + 1;
    char *p = (char *)malloc(n);
    if (p) {
        memcpy(p, s, n);
    }
    return p;
}

/* Apply every buffered per-request INI override at PHP_INI_STAGE_ACTIVATE.
 *
 * Called TWICE per request, and both calls are load-bearing:
 *
 *   1. BEFORE php_request_startup() — php_request_startup() runs
 *      sapi_activate(), which parses a multipart/form-data body through
 *      rfc1867 and therefore reads `upload_tmp_dir` (and PHP's temp-dir
 *      fallback) while writing the uploaded files to disk. That is *before*
 *      the post-startup replay below, so a per-vhost `upload_tmp_dir` set
 *      only afterwards arrives too late: uploads land in the shared system
 *      temp dir, outside the vhost's `open_basedir`, and
 *      move_uploaded_file() then fails (and the upload temp file is exposed
 *      to other tenants). At this point the previous request has already
 *      been shut down, so zend_ini_deactivate() has run and the value we set
 *      here survives into the request that is about to start.
 *
 *   2. AFTER php_request_startup() — request startup re-activates the INI
 *      table, so entries must be (re)asserted for the script itself; this is
 *      what makes `open_basedir` / `session.save_path` effective for the
 *      executed script. Applying twice is idempotent.
 *
 * PHP_INI_SYSTEM + STAGE_ACTIVATE is used for the reasons documented on
 * ephpm_request_set_ini(): STAGE_ACTIVATE skips OnUpdateBaseDir's
 * "runtime updates may only tighten" check, which a peer vhost path would
 * otherwise fail. */
static void ephpm_request_ini_apply(void)
{
    for (size_t i = 0; i < request_ini_count; i++) {
        zend_string *zkey = zend_string_init(request_ini_keys[i],
                                             strlen(request_ini_keys[i]), 0);
        zend_string *zval = zend_string_init(request_ini_vals[i],
                                             strlen(request_ini_vals[i]), 0);
        zend_alter_ini_entry(zkey, zval, PHP_INI_SYSTEM, PHP_INI_STAGE_ACTIVATE);
        zend_string_release(zval);
        zend_string_release(zkey);
    }
}

/* Release all buffered per-request INI overrides. */
static void ephpm_request_ini_reset(void)
{
    for (size_t i = 0; i < request_ini_count; i++) {
        free(request_ini_keys[i]);
        free(request_ini_vals[i]);
        request_ini_keys[i] = NULL;
        request_ini_vals[i] = NULL;
    }
    request_ini_count = 0;
}

/* ===================================================================
 * SAPI Callbacks
 *
 * These are installed into PHP's sapi_module_struct by
 * ephpm_install_sapi(). PHP calls them during request processing.
 * =================================================================== */

/*
 * ub_write — Called by PHP for all output (echo, print, template rendering).
 * Appends data to our output buffer instead of writing to stdout.
 */
static size_t ephpm_sapi_ub_write(const char *str, size_t str_length)
{
    if (output_len + str_length > output_cap) {
        size_t new_cap = (output_cap == 0) ? 8192 : output_cap;
        while (new_cap < output_len + str_length)
            new_cap *= 2;
        char *new_buf = realloc(output_buf, new_cap);
        if (!new_buf) return 0;
        output_buf = new_buf;
        output_cap = new_cap;
    }
    memcpy(output_buf + output_len, str, str_length);
    output_len += str_length;
    return str_length;
}

/*
 * flush — Called by PHP to flush the output buffer.
 * No-op: we buffer the entire response and send it at once.
 */
static void ephpm_sapi_flush(void *server_context)
{
    (void)server_context;
}

/*
 * send_headers — Called by PHP before the first output to finalize headers.
 * We capture headers separately, so just return success.
 */
static int ephpm_sapi_send_headers(sapi_headers_struct *sapi_headers)
{
    (void)sapi_headers;
    return SAPI_HEADER_SENT_SUCCESSFULLY;
}

/*
 * read_post — Called by PHP to read POST request body data.
 * Returns up to count_bytes from the POST body.
 */
static size_t ephpm_sapi_read_post(char *buffer, size_t count_bytes)
{
    /* Worker mode streaming: pull incrementally from Rust so PHP's own POST
     * reader (which drives $_POST / multipart parsing) never forces the whole
     * body into memory. read_post and bodyStream() share this reader, so the
     * body is consumed exactly once. */
    if (req_body_streaming) {
        long n = ephpm_worker_body_read(buffer, count_bytes);
        if (n <= 0) {
            return 0;
        }
        req_post_data_offset += (size_t)n;
        return (size_t)n;
    }

    if (!req_post_data || req_post_data_offset >= req_post_data_len)
        return 0;

    size_t remaining = req_post_data_len - req_post_data_offset;
    size_t to_copy = remaining < count_bytes ? remaining : count_bytes;
    memcpy(buffer, req_post_data + req_post_data_offset, to_copy);
    req_post_data_offset += to_copy;
    return to_copy;
}

/*
 * read_cookies — Called by PHP to get the raw Cookie header string.
 * Returns the cookie string set by Rust before execution.
 */
static char *ephpm_sapi_read_cookies(void)
{
    return (char *)req_cookie_data;
}

/*
 * register_server_variables — Called by PHP during request startup
 * to populate $_SERVER. We iterate over the server variables that
 * Rust added via ephpm_request_add_server_var().
 */
static void ephpm_sapi_register_server_variables(zval *track_vars_array)
{
    for (int i = 0; i < server_var_count; i++) {
        php_register_variable_safe(
            (char *)server_vars[i].key,
            (char *)server_vars[i].value,
            strlen(server_vars[i].value),
            track_vars_array
        );
    }
}

/*
 * log_message — Called by PHP to log error messages.
 * Routes to stderr for now. Future: call back to Rust tracing.
 */
static void ephpm_sapi_log_message(const char *message, int syslog_type_int)
{
    (void)syslog_type_int;
    fprintf(stderr, "[PHP] %s\n", message);
}

/* ===================================================================
 * Internal helpers
 * =================================================================== */

/*
 * Capture response headers from PHP's SAPI globals into our buffer.
 * Must be called after script execution, while the request is still active.
 *
 * Headers are stored as "Name: Value\n" lines for Rust to parse.
 */
static void headers_buf_append(const char *data, size_t len)
{
    while (headers_buf_len + len > headers_buf_cap) {
        size_t new_cap = headers_buf_cap ? headers_buf_cap * 2 : 1024;
        char *new_buf = realloc(headers_buf, new_cap);
        if (!new_buf) return;
        headers_buf = new_buf;
        headers_buf_cap = new_cap;
    }
    memcpy(headers_buf + headers_buf_len, data, len);
    headers_buf_len += len;
}

static void capture_response_headers(void)
{
    headers_buf_len = 0;
    int has_content_type = 0;

    zend_llist_position pos;
    sapi_header_struct *h = (sapi_header_struct *)
        zend_llist_get_first_ex(&SG(sapi_headers).headers, &pos);

    while (h) {
        headers_buf_append(h->header, h->header_len);
        headers_buf_append("\n", 1);

        if (!has_content_type &&
            h->header_len > 13 &&
            strncasecmp(h->header, "Content-Type:", 13) == 0) {
            has_content_type = 1;
        }

        h = (sapi_header_struct *)
            zend_llist_get_next_ex(&SG(sapi_headers).headers, &pos);
    }

    /* In the reuse model, sapi_send_headers() may not fire (output goes
     * directly through ub_write), so the default Content-Type never gets
     * added to the headers list. Synthesize it from SG(sapi_headers).mimetype
     * or fall back to SG(default_mimetype) + SG(default_charset). */
    if (!has_content_type) {
        if (SG(sapi_headers).mimetype) {
            const char *prefix = "Content-Type: ";
            headers_buf_append(prefix, strlen(prefix));
            headers_buf_append(SG(sapi_headers).mimetype,
                               strlen(SG(sapi_headers).mimetype));
            headers_buf_append("\n", 1);
        } else {
            const char *mime = SG(default_mimetype);
            const char *charset = SG(default_charset);
            if (!mime || !*mime) mime = "text/html";
            char ct_buf[256];
            int ct_len;
            if (charset && *charset) {
                ct_len = snprintf(ct_buf, sizeof(ct_buf),
                    "Content-Type: %s; charset=%s\n", mime, charset);
            } else {
                ct_len = snprintf(ct_buf, sizeof(ct_buf),
                    "Content-Type: %s\n", mime);
            }
            if (ct_len > 0 && (size_t)ct_len < sizeof(ct_buf)) {
                headers_buf_append(ct_buf, (size_t)ct_len);
            }
        }
    }
}

/* ===================================================================
 * Public API — called from Rust via FFI
 * =================================================================== */

/*
 * Finalize PHP embed initialization for HTTP serve mode.
 *
 * Mark the embed SAPI's initial request as active so
 * ephpm_execute_request() properly shuts it down before starting
 * its own request lifecycle on the first HTTP request.
 *
 * Must be called once after php_embed_init() and ephpm_install_sapi().
 */
void ephpm_finalize_init(void)
{
    request_active = 1;
}

/* ===================================================================
 * ZTS thread lifecycle
 *
 * With ZTS PHP, each worker thread must be registered with the TSRM
 * (Thread Safe Resource Manager) before accessing any PHP globals.
 * TSRM allocates per-thread copies of all global resource tables
 * (executor globals, SAPI globals, etc.).
 *
 * ephpm_thread_init()     — register this thread with TSRM + start request
 * ephpm_thread_shutdown() — shut down request + unregister from TSRM
 * =================================================================== */

#ifdef ZTS
#include "TSRM/TSRM.h"

/*
 * Initialize the current thread for PHP execution under ZTS.
 *
 * 1. Calls ts_resource(0) to register the thread with TSRM and allocate
 *    thread-local copies of all PHP global tables.
 * 2. Starts a PHP request (php_request_startup) so this thread has a
 *    valid execution context.
 *
 * Must be called once per thread, before any PHP execution.
 * Returns 0 on success, -1 on failure.
 */
int ephpm_thread_init(void)
{
    /* Register this thread with TSRM. ts_resource(0) is idempotent —
     * if the thread is already registered, it returns the existing slot. */
    ts_resource(0);

    /* Override SAPI callbacks on this thread's SAPI globals.
     * In ZTS mode, sapi_module is a global struct but the callbacks
     * are shared. SG() macros access per-thread SAPI globals. */

    /* Start a request on this thread so PHP globals are initialized. */
    int ret = php_request_startup();
    if (ret != SUCCESS) {
        return -1;
    }

    /* php_request_startup() -> zend_activate() -> zend_call_stack_init() has
     * already computed EG(stack_limit) from THIS thread's stack bounds, so the
     * C-stack overflow guard is armed for every request this thread serves
     * (worker mode runs its whole loop inside this one request). Nothing to do
     * here: zend.max_allowed_stack_size is left at PHP's default of 0, which
     * means "auto-detect from the real stack", not "disabled" (that is -1). */

    request_active = 1;
    return 0;
}

/*
 * Shut down PHP on the current thread.
 *
 * Performs request shutdown and unregisters the thread from TSRM,
 * freeing its thread-local PHP globals.
 */
void ephpm_thread_shutdown(void)
{
    if (request_active) {
        php_request_shutdown(NULL);
        request_active = 0;
    }

    /* Free thread-local buffers */
    if (output_buf) {
        free(output_buf);
        output_buf = NULL;
        output_len = 0;
        output_cap = 0;
    }
    if (headers_buf) {
        free(headers_buf);
        headers_buf = NULL;
        headers_buf_len = 0;
        headers_buf_cap = 0;
    }

    /* Unregister from TSRM */
    ts_free_thread();
}

#else /* !ZTS — NTS stubs */

int ephpm_thread_init(void) { return 0; }
void ephpm_thread_shutdown(void) {}

#endif /* ZTS */

/* ===================================================================
 * Signal handling overrides
 *
 * PHP 8.1+ installs process-wide signal handlers (via zend_signal_init)
 * and uses SIGPROF (via setitimer/ITIMER_PROF) for max_execution_time.
 * This is fundamentally incompatible with multi-threaded embedders:
 *
 *   - SIGPROF is process-wide and gets delivered to any thread
 *   - PHP's handler (zend_signal_handler_defer) accesses per-request
 *     globals that only exist on the PHP thread
 *   - Tokio worker threads have no PHP state → NULL deref → SIGSEGV
 *
 * Since we link libphp.a statically, we override PHP's zend_signal_*
 * functions with no-ops. The linker prefers our definitions over the
 * archive's. ePHPm manages timeouts at the HTTP server level instead.
 *
 * Trade-off: pcntl_signal() won't work (PHP userland signal handling).
 * This is acceptable — pcntl is a CLI extension and web requests should
 * not handle signals. FrankenPHP has the same limitation.
 *
 * Future: we could add a thread-safe signal forwarding layer that
 * delivers signals only to the target PHP thread.
 * =================================================================== */

/*
 * The --wrap linker flag renames calls: zend_signal_init → __wrap_zend_signal_init.
 * The original libphp.a symbols become __real_zend_signal_init (unused).
 */

void __wrap_zend_signal_startup(void)
{
    /* no-op — skip PHP's process-wide signal handler installation */
}

void __wrap_zend_signal_init(void)
{
    /* no-op — skip per-request signal handler setup + SIGPROF unblock */
}

void __wrap_zend_signal_deactivate(void)
{
    /* no-op — nothing to tear down */
}

void __wrap_zend_signal_activate(void)
{
    /* no-op — nothing to set up */
}

void __wrap_zend_signal_handler_unblock(void)
{
    /* no-op — no deferred signals to dispatch */
}

/*
 * Timeout arming.
 *
 * On an SDK built WITHOUT --enable-zend-max-execution-timers (macOS, older
 * builds), zend_set_timeout() arms a process-wide setitimer(ITIMER_PROF) and
 * installs a SIGPROF handler. That signal is delivered to whichever thread the
 * kernel picks — including a tokio worker with no PHP state — and PHP's handler
 * then dereferences per-request globals that don't exist there → SIGSEGV. So on
 * those builds we --wrap both functions to no-ops (see crates/ephpm/build.rs)
 * and enforce the request deadline at the HTTP layer instead.
 *
 * On an SDK built WITH per-thread execution timers (EPHPM_NATIVE_EXEC_TIMER,
 * detected from php_config.h), zend_set_timeout() instead calls
 * zend_max_execution_timer_settime() → timer_settime() on a per-thread POSIX
 * timer whose SIGRTMIN is delivered only to the owning PHP thread. That is
 * thread-safe, so we do NOT --wrap it — PHP arms/disarms its own timer per
 * request (and set_time_limit() re-arms it live). These no-op stubs are then
 * unreferenced and must not be compiled.
 */
#ifndef EPHPM_NATIVE_EXEC_TIMER
void __wrap_zend_set_timeout(long seconds, int reset_signals)
{
    (void)seconds;
    (void)reset_signals;
    /* no-op — ePHPm manages request timeouts at the HTTP server level */
}

void __wrap_zend_unset_timeout(void)
{
    /* no-op */
}
#endif /* !EPHPM_NATIVE_EXEC_TIMER */

/*
 * NOTE (issue #116): there is deliberately no __wrap_zend_call_stack_init here.
 *
 * PHP's zend_call_stack_init() runs from zend_activate() on every request
 * startup and computes EG(stack_limit) from the CURRENT thread's real stack
 * bounds. That value is what powers PHP 8.3+'s C-stack overflow guard: the VM
 * and zend_call_function check zend_call_stack_overflowed(EG(stack_limit)) and
 * raise the catchable `Error: Maximum call stack size of N bytes ... reached`
 * instead of walking off the end of the stack.
 *
 * ePHPm used to override this function with a no-op (via --wrap, Linux only),
 * which left EG(stack_limit) NULL and the guard permanently off. Recursion that
 * re-enters the VM through an internal function then SIGSEGV'd the process.
 * The override is gone; the guard is on, on every platform, and every thread
 * that runs PHP is given a stock-PHP-sized stack (ephpm_php::PHP_THREAD_STACK)
 * so the depth it allows matches php-fpm.
 */

/*
 * CLI-mode flag. When set (via ephpm_enable_cli_mode() before php_embed_init),
 * the SAPI reports itself as "cli" so php_sapi_name() / the PHP_SAPI constant
 * return "cli" — exactly what stock php-cli reports. This makes `ephpm php` a
 * drop-in for `php`: the near-universal `if (PHP_SAPI !== 'cli') die(...)` guard
 * and tools like wp-cli that gate on the SAPI name behave identically.
 *
 * `ephpm php` and `ephpm serve` are SEPARATE process invocations, so setting
 * this in the CLI process cannot affect the server (which never calls the
 * enabling function and keeps reporting "ephpm").
 */
static int g_cli_mode = 0;

/*
 * Switch the SAPI into CLI identity. MUST be called before php_embed_init()
 * so ephpm_pre_init() (which runs during init) picks the "cli" name up — the
 * name is copied into OPcache's startup verdict and cannot be changed later.
 */
void ephpm_enable_cli_mode(void)
{
    g_cli_mode = 1;
}

/* SAPI name/pretty-name pair for the active mode. */
#define EPHPM_SAPI_NAME        (g_cli_mode ? "cli" : "ephpm")
#define EPHPM_SAPI_PRETTY_NAME (g_cli_mode ? "Command Line Interface" : "ePHPm Embedded Server")

/*
 * Override the default embed SAPI callbacks with our implementations.
 * Must be called once after php_embed_init().
 */
void ephpm_install_sapi(void)
{
    sapi_module.ub_write = ephpm_sapi_ub_write;
    sapi_module.flush = ephpm_sapi_flush;
    sapi_module.send_headers = ephpm_sapi_send_headers;
    sapi_module.read_post = ephpm_sapi_read_post;
    sapi_module.read_cookies = ephpm_sapi_read_cookies;
    sapi_module.register_server_variables = ephpm_sapi_register_server_variables;
    sapi_module.log_message = ephpm_sapi_log_message;

    /* Update SAPI name visible to phpinfo() and $_SERVER['SERVER_SOFTWARE'].
     * "cli" in the `ephpm php` process, "ephpm" in the server. */
    sapi_module.name = (char *)EPHPM_SAPI_NAME;
    sapi_module.pretty_name = (char *)EPHPM_SAPI_PRETTY_NAME;

    /* php-cli renders phpinfo() (and `php -i`) as plain text, not HTML. Match
     * that in CLI mode so `ephpm php -i` / userland phpinfo() output is a
     * drop-in for `php -i`. The server keeps the default (HTML) behaviour. */
    if (g_cli_mode) {
        sapi_module.phpinfo_as_text = 1;
    }
}

/*
 * Reset per-request state. Call before setting up a new request.
 */
void ephpm_request_clear(void)
{
    output_len = 0;
    headers_buf_len = 0;
    response_status_code = 200;
    req_method = NULL;
    req_uri = NULL;
    req_query_string = NULL;
    req_content_type = NULL;
    req_cookie_data = NULL;
    req_post_data = NULL;
    req_post_data_len = 0;
    req_post_data_offset = 0;
    req_path_translated = NULL;
    server_var_count = 0;
    req_middleware_count = 0;
    req_middleware_active = -1;
    req_middleware_outcome = EPHPM_MW_CONTINUE;
    req_middleware_ran = 0;
}

/*
 * Set core request info fields. Pointers must remain valid until
 * ephpm_execute_request() returns.
 */
void ephpm_request_set_info(
    const char *method,
    const char *uri,
    const char *query_string,
    const char *content_type,
    const char *cookie,
    const char *post_data,
    size_t post_data_len,
    const char *path_translated)
{
    req_method = method;
    req_uri = uri;
    req_query_string = query_string;
    req_content_type = content_type;
    req_cookie_data = cookie;
    req_post_data = post_data;
    req_post_data_len = post_data_len;
    req_post_data_offset = 0;
    req_path_translated = path_translated;
}

/*
 * Add a $_SERVER variable. Call before ephpm_execute_request().
 * Pointers must remain valid until ephpm_execute_request() returns.
 */
void ephpm_request_add_server_var(const char *key, const char *value)
{
    if (server_var_count < MAX_SERVER_VARS) {
        server_vars[server_var_count].key = key;
        server_vars[server_var_count].value = value;
        server_var_count++;
    }
}

/*
 * Queue one PHP middleware script for this request, in chain order.
 *
 * `path` is an absolute filesystem path already resolved (and confined to the
 * request's document root) by the router; `config_json` is the mount's `config`
 * table serialised to JSON, or NULL when the mount declares none. Both pointers
 * are borrowed and must outlive ephpm_execute_request().
 *
 * Mounts beyond MAX_REQUEST_MIDDLEWARE are dropped rather than growing an
 * unbounded per-request array; the Rust side enforces the same cap at startup
 * so this can only fire if the two ever disagree.
 *
 * Call before ephpm_execute_request().
 */
void ephpm_request_add_middleware(const char *path, const char *config_json)
{
    if (!path || req_middleware_count >= MAX_REQUEST_MIDDLEWARE) {
        return;
    }
    req_middleware_paths[req_middleware_count] = path;
    req_middleware_configs[req_middleware_count] = config_json;
    req_middleware_count++;
}

/*
 * How the middleware chain ended for the request just executed:
 * EPHPM_MW_CONTINUE / EPHPM_MW_EXIT / EPHPM_MW_FATAL.
 */
int ephpm_middleware_outcome(void)
{
    return req_middleware_outcome;
}

/*
 * How many queued middleware mounts actually executed for the request just
 * executed. The last one is the mount the outcome above belongs to.
 */
int ephpm_middleware_ran(void)
{
    return req_middleware_ran;
}

/*
 * Set a PHP INI directive for the current request.
 *
 * Uses PHP_INI_SYSTEM + PHP_INI_STAGE_ACTIVATE. Not RUNTIME: the
 * OnUpdateBaseDir handler rejects RUNTIME updates that aren't a strict
 * subset of the prior value (open_basedir can only be tightened at
 * runtime). We reuse a single embed request across HTTP requests, so on
 * the second and later vhost calls a sibling site's path fails the
 * "subset of current open_basedir" check, the update is dropped, the
 * stale value blocks the new script from loading, and the request 500s.
 * STAGE_ACTIVATE — the bucket PHP itself uses during request_startup —
 * skips the tightening check, which is the behavior we want here.
 *
 * Buffer the override rather than applying it now: ephpm_execute_request()
 * tears down the active request (php_request_shutdown -> zend_ini_deactivate)
 * before starting a fresh one, which would immediately undo an entry applied
 * here. The buffered entries are replayed once the new request is live.
 *
 * Call before ephpm_execute_request().
 */
void ephpm_request_set_ini(const char *key, const char *value)
{
    if (request_ini_count >= MAX_REQUEST_INI) {
        return;
    }
    char *kd = ephpm_strdup_malloc(key);
    char *vd = ephpm_strdup_malloc(value);
    if (!kd || !vd) {
        free(kd);
        free(vd);
        return;
    }
    request_ini_keys[request_ini_count] = kd;
    request_ini_vals[request_ini_count] = vd;
    request_ini_count++;
}

/* Return codes for ephpm_execute_request(). Must match the match arms in
 * crates/ephpm-php/src/lib.rs::execute_php. */
#define EPHPM_EXEC_OK            0
#define EPHPM_EXEC_STARTUP_FAIL (-1)
#define EPHPM_EXEC_SCRIPT_EXIT  (-2)
#define EPHPM_EXEC_BAILOUT      (-3)
/* A max_execution_time timeout. Like BAILOUT it unwinds via zend_bailout and
 * forces a 500, but UNLIKE a memory/resource bailout the captured response is a
 * legitimate, complete-enough php-fpm-style error page (the "Maximum execution
 * time exceeded" fatal, any output already produced, and shutdown-function /
 * flushed-buffer output) — so it is delivered, not discarded. */
#define EPHPM_EXEC_TIMEOUT      (-4)

/*
 * Did a zend_bailout() happen since the last ephpm_bailout_reset()?
 *
 * This is the ONLY reliable bailout signal available to us, and the reason is
 * worth spelling out: php_execute_script() wraps the whole compile+execute in
 * its OWN zend_try/zend_end_try. zend_end_try does not re-raise — it restores
 * EG(bailout) and falls through — so a bailout raised anywhere inside the
 * script is fully absorbed there and NEVER reaches a SETJMP we installed
 * around the call. Our guard only ever fires for a bailout raised outside
 * php_execute_script.
 *
 * PG(last_error_type) covers the fatals that go through zend_error()
 * (E_ERROR, uncaught Throwable via zend_exception_error, parse errors), but a
 * bare zend_bailout() — which a C extension, a resource limit, or OPcache can
 * raise directly — sets no error type at all. Before this check such a request
 * came back as a clean HTTP 200 carrying a truncated body.
 *
 * _zend_bailout() sets CG(unclean_shutdown) = 1 immediately before its
 * LONGJMP, unconditionally and regardless of which zend_try catches it, and
 * nothing else in the engine sets that flag. init_compiler() (via
 * zend_activate() <- php_request_startup()) clears it per request; we clear it
 * explicitly as well so the signal can never be a leftover from a previous
 * request on this thread.
 */
#define ephpm_bailout_reset()    (CG(unclean_shutdown) = 0)
#define ephpm_bailout_observed() (CG(unclean_shutdown) != 0)

/*
 * Run one middleware script and classify how it ended.
 *
 * This is php-src's own `zend_execute_scripts()` loop body (Zend/zend.c),
 * inlined for ONE file, with exactly one change: an unwind-exit exception is
 * recognised and reported instead of being handed to zend_exception_error().
 *
 * That change is the whole reason this is not simply `php_execute_script()`.
 * PHP 8 implements exit()/die() by throwing an unwind-exit exception, and both
 * `zend_execute_scripts()` and `php_execute_script()` funnel EVERY pending
 * exception through `zend_exception_error()`, which clears `EG(exception)` and
 * — for an unwind exit — returns SUCCESS. By the time either of them returns,
 * "the script called exit()" and "the script ran to completion" are
 * indistinguishable. Measured, not assumed: the first cut of this lane used
 * php_execute_script() and its exit()-short-circuit tests failed with the
 * application script's output appended to the middleware's.
 *
 * Everything else is deliberately identical to upstream, including adding the
 * resolved path to EG(included_files) (so a middleware file that the app also
 * `require_once`s is not compiled twice) and the op_array teardown order.
 * zend_compile_file() is OPcache's hook, so the file is cached in SHM like any
 * other script — the marginal cost of a mount is a cache lookup plus its own
 * opcodes, not a compile.
 *
 * Returns EPHPM_MW_CONTINUE / EPHPM_MW_EXIT / EPHPM_MW_FATAL. A compile failure
 * (missing file, parse error) raises E_COMPILE_ERROR, which bails out through
 * the caller's SETJMP rather than returning here — also fail-closed.
 */
static int ephpm_run_one_middleware(const char *path)
{
    zend_file_handle fh;
    zend_op_array *op_array;
    int outcome = EPHPM_MW_CONTINUE;

    zend_stream_init_filename(&fh, path);
    op_array = zend_compile_file(&fh, ZEND_REQUIRE);
    if (fh.opened_path) {
        zend_hash_add_empty_element(&EG(included_files), fh.opened_path);
    }
    zend_destroy_file_handle(&fh);

    if (!op_array) {
        /* A syntax error is a thrown ParseError in PHP 7+, NOT a bailout:
         * zend_compile_file returns NULL with EG(exception) still pending and
         * nothing recorded in PG(last_error_type). Reporting it here is what
         * turns it into the 500 the caller's last_error_type check produces —
         * and, just as importantly, clears the exception so it cannot surface
         * inside a shutdown function later in this request. (A file that is
         * simply missing takes the other route: ZEND_REQUIRE raises
         * E_COMPILE_ERROR, which bails out before reaching here.) */
        if (EG(exception)) {
            zend_exception_error(EG(exception), E_ERROR);
        }
        return EPHPM_MW_FATAL;
    }

    zend_execute(op_array, NULL);
    zend_exception_restore();

    if (UNEXPECTED(EG(exception))) {
        if (zend_is_unwind_exit(EG(exception))) {
            /* exit()/die() — the lane's ACTION_RESPOND. */
            zend_clear_exception();
            outcome = EPHPM_MW_EXIT;
        } else {
            if (Z_TYPE(EG(user_exception_handler)) != IS_UNDEF) {
                zend_user_exception_handler();
            }
            if (EG(exception)) {
                /* Reports the uncaught Throwable as a fatal exactly as
                 * zend_execute_scripts would, and clears it. */
                zend_exception_error(EG(exception), E_ERROR);
            }
            outcome = EPHPM_MW_FATAL;
        }
    }

    zend_destroy_static_vars(op_array);
    destroy_op_array(op_array);
    efree_size(op_array, sizeof(zend_op_array));
    return outcome;
}

/*
 * Run the queued PHP middleware scripts, in order, inside the live request.
 *
 * The working directory is deliberately NOT changed first, which is the one
 * place this lane diverges from PHP's `auto_prepend_file` (php_execute_script
 * chdirs to the primary script's directory, then runs prepend + primary inside
 * that). Doing the same here — one getcwd + one chdir + one restore — measured
 * **~55 us per request on Windows**, against ~6 us for actually executing a
 * mount. That is an order of magnitude of pure overhead for a guarantee PHP
 * mostly provides anyway: a relative `include` resolves against the *including
 * script's own directory* before it ever consults the cwd, so
 * `include 'helper.php'` from a middleware file still finds the file beside it.
 * What does change is a bare relative path handed to a filesystem call
 * (`fopen('data.txt')`), which resolves against the server's working directory
 * — so the guide tells middleware authors to use `__DIR__`. The application
 * script is unaffected: php_execute_script still does its own chdir for it.
 *
 * Three ways a mount ends the chain, all fail-closed:
 *
 *   1. exit()/die() — ACTION_RESPOND. Remaining mounts and the app script are
 *      skipped; whatever the middleware emitted becomes the response.
 *
 *   2. An uncaught Throwable or a fatal that does not bail out. Without the
 *      explicit check the app script would run anyway — a middleware that
 *      throws would fail OPEN, which for an auth mount is precisely the bug
 *      that must not exist.
 *
 *   3. A zend_bailout (missing file, parse error, OOM, resource limit).
 *      Nothing here catches it: it longjmps to the SETJMP in
 *      ephpm_execute_request, which skips the app script and 500s.
 */
static int ephpm_run_middleware_chain(void)
{
    const int fatal_error_mask = E_ERROR | E_CORE_ERROR | E_COMPILE_ERROR
                                 | E_USER_ERROR | E_RECOVERABLE_ERROR | E_PARSE;
    int outcome = EPHPM_MW_CONTINUE;

    /* The overwhelmingly common case: no mounts. A server with no `php:`
     * middleware pays one predictable branch for this lane existing. */
    if (req_middleware_count == 0) {
        return EPHPM_MW_CONTINUE;
    }

    for (size_t i = 0; i < req_middleware_count; i++) {
        req_middleware_active = (int)i;
        outcome = ephpm_run_one_middleware(req_middleware_paths[i]);
        req_middleware_active = -1;
        req_middleware_ran = (int)i + 1;

        /* A bailout absorbed elsewhere, or a fatal reported with E_DONT_BAIL,
         * both mean "do not continue" even when the call above returned
         * CONTINUE. */
        if (outcome == EPHPM_MW_CONTINUE
            && (ephpm_bailout_observed() || (PG(last_error_type) & fatal_error_mask))) {
            outcome = EPHPM_MW_FATAL;
        }
        if (outcome != EPHPM_MW_CONTINUE) {
            break;
        }
    }

    return outcome;
}

/*
 * Execute a PHP request.
 *
 * Reuses the active request started by php_embed_init() — we update the
 * SAPI request info fields and execute the script without a full
 * request shutdown/startup cycle. This is necessary because:
 *
 *   - php_request_startup() calls zend_signal_init() and other thread-
 *     sensitive functions that crash on tokio's spawn_blocking threads
 *   - The embed SAPI's initial request provides a valid execution
 *     context that we can reuse for all HTTP requests
 *
 * With ZTS, each spawn_blocking thread has its own TSRM context and
 * __thread-local per-request state, so concurrent reuse is safe.
 *
 * Returns:
 *   EPHPM_EXEC_OK          (0)  the script ran to completion
 *   EPHPM_EXEC_STARTUP_FAIL(-1) php_request_startup failed (only on cold start)
 *   EPHPM_EXEC_SCRIPT_EXIT (-2) the script chose to stop (exit()/die()); the
 *                               captured response is complete and trustworthy
 *   EPHPM_EXEC_BAILOUT     (-3) a zend_bailout() unwound the script; the
 *                               captured response is TRUNCATED and must never
 *                               be completed as a success (see below)
 */
int ephpm_execute_request(const char *filename)
{
    /* ---- Per-request lifecycle (php-fpm-style isolation) ----
     * Tear down the previous request and start a fresh one. Without this,
     * a single request was reused for the whole life of the thread, so
     * user functions/classes/constants and the global symbol table leaked
     * across requests — vanilla WordPress rendered only the first request
     * per worker thread ($wp_did_header / WP_USE_THEMES persisted).
     *
     * php_request_shutdown() runs zend_deactivate() -> shutdown_executor(),
     * which destroys user symbols, constants, statics, and included_files;
     * php_request_startup() then provides a clean executor. The signal /
     * timeout / stack functions that made php_request_startup() crash on
     * tokio spawn_blocking threads are already no-op'd via --wrap, so this
     * is safe. OPcache's compiled bytecode lives in SHM and survives the
     * cycle, so the opcode cache (and JIT buffer) are preserved — this is
     * exactly the classic php-fpm + opcache model. */
    if (request_active) {
        php_request_shutdown(NULL);
        request_active = 0;
    }

    /* Reset output and response buffers (thread-local C buffers). The
     * C-side POST read cursor resets too, so our read_post callback serves
     * the request body from the start. */
    output_len = 0;
    headers_buf_len = 0;
    req_post_data_offset = 0;

    /* Populate SG(request_info) BEFORE php_request_startup().
     *
     * PHP builds the superglobals ($_GET/$_POST/$_SERVER/$_COOKIE/$_FILES/
     * $_REQUEST) during request startup and auto-globals creation, using our
     * installed SAPI callbacks (treat_data, read_post, read_cookies,
     * register_server_variables). Those callbacks read these request_info
     * fields, so the fields must be set first.
     *
     * The old single-request reuse model could NOT call php_request_startup()
     * per request, so it set request_info afterwards and hand-rebuilt the
     * superglobals. Now that the per-request lifecycle calls
     * php_request_startup() every request (above), that manual rebuild became
     * actively harmful: it destroyed the PG(http_globals) arrays startup had
     * just created and re-ran sapi_module.treat_data over them, which faulted
     * inside php_default_treat_data (use-after-free → SIGSEGV) on tokio
     * spawn_blocking threads under load. Letting php_request_startup() own
     * superglobal construction is the correct, crash-free php-fpm model. */
    SG(request_info).request_method = (char *)req_method;
    SG(request_info).request_uri = (char *)req_uri;
    SG(request_info).query_string = (char *)req_query_string;
    SG(request_info).content_type = req_content_type;
    SG(request_info).cookie_data = (char *)req_cookie_data;
    SG(request_info).content_length = (long)req_post_data_len;
    SG(request_info).path_translated = (char *)req_path_translated;
    SG(request_info).proto_num = 1001; /* HTTP/1.1 */

    /* sapi_activate() (run inside php_request_startup) only reads and parses
     * the POST body into $_POST when SG(server_context) is non-NULL — that is
     * the gate cli_server/cgi use to distinguish a real request from CLI. Our
     * SAPI callbacks key off the thread-local request buffers rather than this
     * pointer, so a stable non-NULL sentinel is all that's needed to enable
     * native POST parsing. Without it $_POST stays empty (php://input still
     * works because read_post is driven separately). */
    SG(server_context) = &ephpm_server_context_marker;

    /* Apply the buffered per-request INI overrides BEFORE request startup so
     * that directives consumed *during* startup see the per-vhost values.
     * The critical one is `upload_tmp_dir`: php_request_startup() ->
     * sapi_activate() runs the rfc1867 multipart parser, which writes
     * uploaded files to disk using upload_tmp_dir at that moment. Without
     * this early pass, uploads land in the shared system temp dir — outside
     * the vhost's open_basedir — so move_uploaded_file() fails and the temp
     * file is readable by other tenants. See ephpm_request_ini_apply(). */
    ephpm_request_ini_apply();

    if (php_request_startup() != SUCCESS) {
        return EPHPM_EXEC_STARTUP_FAIL;
    }
    request_active = 1;

    /* php_request_startup() has just re-armed PHP's C-stack overflow guard for
     * THIS thread (zend_activate -> zend_call_stack_init -> EG(stack_limit)).
     * Leave it armed — that is what turns runaway recursion into a catchable
     * `Error: Maximum call stack size ... reached` instead of a SIGSEGV that
     * takes the whole process with it (#116). */

    /* Reset per-request response status. php_request_startup()/sapi_activate()
     * does NOT reset SG(sapi_headers).http_response_code on this embed reuse
     * path, so without this an explicit status from a prior request on the
     * same worker thread (e.g. http_response_code(201), or a 500 from a fatal)
     * leaks into the next request and a 200-expecting handler returns the
     * stale code. headers_sent / no_headers are reset for the same reason. */
    SG(sapi_headers).http_response_code = 200;
    SG(headers_sent) = 0;
    SG(request_info).no_headers = 0;

    /* Replay per-request INI overrides now that the fresh request is live.
     * Applied at STAGE_ACTIVATE (the bucket request_startup itself uses), so
     * open_basedir for vhost isolation takes effect for this request without
     * tripping the runtime "can only tighten" check, and is restored by the
     * next request's php_request_shutdown(). (The same set was already
     * applied before startup for directives rfc1867/sapi_activate consume —
     * see ephpm_request_ini_apply(); re-applying is idempotent.) */
    ephpm_request_ini_apply();

    /* Reset PHP's last-error tracking so we can tell whether THIS script
     * raised a fatal (vs. a value carried over from a prior request). */
    PG(last_error_type) = 0;
    /* Same for the bailout flag — see ephpm_bailout_observed() above. */
    ephpm_bailout_reset();

#ifdef EPHPM_NATIVE_EXEC_TIMER
    /* Arm PHP's per-thread execution timer for THIS request. php_request_startup
     * above already created the timer (init_executor) but armed it from
     * EG(timeout_seconds), which the embed SAPI has reset to 0 (unlimited) — so
     * we arm explicitly from the configured value here, right before executing
     * the script. reset_signals = 1 also (re)installs the SIGRTMIN disposition.
     * A value of 0 means "no limit" and disarms, matching PHP semantics.
     * ephpm_arm_exec_timer() also mirrors the value into the ini entry so
     * ini_get('max_execution_time') reports the enforced limit, not 0 (#279). */
    ephpm_arm_exec_timer();
#endif

    /* Execute the script with bailout protection.
     * PHP's zend_try/zend_catch uses setjmp/longjmp. */
    int result = EPHPM_EXEC_OK;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        /* PHP middleware lane: mounts matching this request run first, in the
         * same request, immediately before the application script. A mount that
         * exits short-circuits (the lane's RESPOND); a mount that fatals aborts
         * the request rather than falling through to the app — fail closed. */
        req_middleware_outcome = ephpm_run_middleware_chain();

        if (req_middleware_outcome == EPHPM_MW_EXIT) {
            result = EPHPM_EXEC_SCRIPT_EXIT;
        } else if (req_middleware_outcome == EPHPM_MW_CONTINUE) {
            zend_file_handle file_handle;
            zend_stream_init_filename(&file_handle, filename);
            php_execute_script(&file_handle);

            /* PHP 8.x: exit()/die() throws an unwind exit exception instead
             * of calling zend_bailout(). Treat it like the old bailout path,
             * but DO NOT mark it as a fatal bailout — exit() is intentional
             * and should preserve whatever status the script set. */
            if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
                zend_clear_exception();
                result = EPHPM_EXEC_SCRIPT_EXIT;
            }
        }
        /* EPHPM_MW_FATAL falls through with result still EPHPM_EXEC_OK: the
         * bailout / last_error_type checks below turn it into the 500 the app
         * script's own fatal would have produced. */
    } else {
        /* A zend_bailout() raised OUTSIDE php_execute_script's own zend_try
         * (rare — that guard absorbs everything raised by the script itself).
         * The CG(unclean_shutdown) check below covers both cases.
         *
         * The middleware chain, unlike the app script, is NOT wrapped in
         * php_execute_script's zend_try, so a bailout inside a mount (a missing
         * file, a parse error, OOM) lands HERE — with the app script skipped,
         * which is the fail-closed answer.
         *
         * `req_middleware_active >= 0` says the jump came from inside a mount
         * rather than from anywhere else in the request, so the outcome is only
         * rewritten when it really belongs to the chain. Clearing the cursor
         * also stops a later ephpm_middleware_config() on this thread from
         * reading a stale index. */
        if (req_middleware_active >= 0) {
            req_middleware_ran = req_middleware_active + 1;
            req_middleware_outcome = EPHPM_MW_FATAL;
            req_middleware_active = -1;
        }
        result = EPHPM_EXEC_BAILOUT;
    }
    EG(bailout) = __orig_bailout;

    /* Run user shutdown functions and flush every open output buffer
     * BEFORE capturing the response. Request teardown is lazy (top of the
     * NEXT ephpm_execute_request), so without this, content still sitting
     * in userland ob_ buffers at script end — and anything printed by
     * register_shutdown_function() — never reached the ub_write capture:
     * `ob_start(); echo "hello";` returned content-length: 0, and
     * WordPress 7.0 (which finalizes its template-enhancement buffer on
     * the `shutdown` action) rendered EVERY page as 0 bytes. Mirrors the
     * worker-mode unwind-exit flush (see the php_output_end_all block in
     * the worker loop).
     *
     * Shutdown functions run even after exit()/fatals — that is php-fpm
     * behavior and WordPress' fatal handler depends on it. Both calls run
     * under a bailout guard (shutdown functions and ob handlers execute
     * userland code that can exit()/fatal); a bailout forfeits whatever
     * remained, and we deliver what the capture has — same policy as
     * worker mode.
     *
     * php_free_shutdown_functions() empties the list afterwards so the
     * lazy php_request_shutdown() at the top of the next request cannot
     * run the same shutdown functions a second time. */
    zend_try {
        php_call_shutdown_functions();
    } zend_catch {
        /* exit()/fatal inside a shutdown function — deliver what we have. */
    } zend_end_try();
    php_free_shutdown_functions();
    zend_try {
        php_output_end_all();
    } zend_catch {
        /* A throwing ob handler forfeits its buffer. */
    } zend_end_try();

    /* Capture response data while the request is still active */
    capture_response_headers();
    response_status_code = SG(sapi_headers).http_response_code;

    /* Decide whether to override status with 500. There are three paths:
     *
     *   1. zend_bailout() — out of memory, a resource limit, OPcache, or a
     *      C extension calling zend_bailout() directly. Detected via
     *      CG(unclean_shutdown); see the ephpm_bailout_observed() comment for
     *      why the SETJMP above cannot see it. Checked AFTER the shutdown
     *      functions / ob flush so a bailout in either of those (which also
     *      truncates the response) counts too.
     *
     *   2. PHP 8.x uncaught Throwable: zend_exception_error() calls
     *      zend_error_va(... | E_DONT_BAIL ...) which prints the fatal
     *      message and lets php_execute_script return normally. SETJMP
     *      sees nothing, so we MUST also check PG(last_error_type) to
     *      catch this case. Without it, "Fatal error: Uncaught Error:
     *      Call to undefined function ..." comes back as 200 OK.
     *
     *   3. zend_error(E_ERROR, ...) — sets last_error_type AND bails out, so
     *      both signals fire. 500 either way.
     *
     * A bailout forces 500 UNCONDITIONALLY: it is never something a script
     * asks for (PHP 8's exit() throws an unwind-exit exception instead), so a
     * status the script set earlier describes a response it never finished
     * producing. The last_error_type path keeps its narrower rule — only
     * override a default 200 — because a framework's own error handler
     * legitimately sets its status before the engine records the fatal. */
    int fatal_error_mask = E_ERROR | E_CORE_ERROR | E_COMPILE_ERROR
                           | E_USER_ERROR | E_RECOVERABLE_ERROR | E_PARSE;
    if (ephpm_bailout_observed()) {
        response_status_code = 500;
        /* Distinguish a max_execution_time timeout from a memory/resource
         * bailout. zend_timeout() raises E_ERROR with a message that always
         * begins "Maximum execution time of" (Zend/zend_execute_API.c). A
         * timeout's captured output is a legitimate error page and is
         * delivered; every other bailout's is truncated garbage and discarded.
         * EG(timed_out) is not reliable here (zend_timeout may clear it before
         * the bailout is observed), so key off the recorded error message. */
        if (PG(last_error_message)
            && strncmp(ZSTR_VAL(PG(last_error_message)),
                       "Maximum execution time of", 25)
                   == 0) {
            result = EPHPM_EXEC_TIMEOUT;
        } else {
            result = EPHPM_EXEC_BAILOUT;
        }
    } else if ((PG(last_error_type) & fatal_error_mask)
               && response_status_code == 200) {
        response_status_code = 500;
    }

    /* Release this request's buffered INI overrides; they have already been
     * applied to the live request above and must not leak into the next one
     * (which buffers its own set before calling back in). The request itself
     * is torn down lazily at the top of the next ephpm_execute_request(), or
     * by php_embed_shutdown() at process exit. */
    ephpm_request_ini_reset();

    return result;
}

/*
 * Get the captured output buffer.
 * Returns a pointer to the buffer and sets *out_len to the length.
 */
const char *ephpm_get_output_buf(size_t *out_len)
{
    *out_len = output_len;
    return output_buf;
}

/*
 * Get the HTTP response status code.
 */
int ephpm_get_response_code(void)
{
    return response_status_code;
}

/*
 * Get the captured response headers buffer.
 * Headers are stored as "Name: Value\n" lines.
 * Returns a pointer to the buffer and sets *out_len to the length.
 */
const char *ephpm_get_response_headers(size_t *out_len)
{
    *out_len = headers_buf_len;
    return headers_buf;
}

/* ===================================================================
 * OPcache clustered invalidation (design: opcache-clustering.md, phase 1)
 *
 * ephpm_opcache_invalidate_under(docroot) walks opcache_get_status(true)['scripts']
 * and calls opcache_invalidate($path, true) for every cached script whose
 * full_path starts with the vhost's docroot prefix. Returns the number of
 * scripts invalidated, or one of the EPHPM_OPCACHE_* failure codes below.
 *
 * Implementation: direct C-level calls into the OPcache extension via
 * zend_call_known_function. Earlier revisions of this branch evaluated a
 * userland snippet with zend_eval_string_ex, but that path has a footgun —
 * zend_eval_string_ex with a retval WRAPS the code as `return <code>;`, so
 * any accidental leading `return` produces `return return (...);` → ParseError
 * (found the hard way on the two-node kind demo). Direct calls eliminate the
 * eval entirely: look up opcache_get_status / opcache_invalidate in
 * EG(function_table) and invoke them the same way a compiled call site would.
 *
 * Must be called on a TSRM-registered thread WITH an active PHP request
 * (i.e. at the start of ephpm_execute_request(), after php_request_startup).
 * Callers gate the invocation with a Rust-side per-vhost version comparison
 * so the actual invalidation runs only when a deploy has advanced the
 * cluster-wide version key, not on every request.
 * =================================================================== */

/* Failure contract (must match crates/ephpm-php/src/lib.rs::opcache_invalidate_under):
 *   -1: OPcache extension unavailable — opcache_get_status or opcache_invalidate
 *       missing from the function table, or opcache_get_status returned a
 *       shape we don't recognise (non-array / no 'scripts' HashTable).
 *   -2: SETJMP bailout inside one of the direct calls (OOM / OPcache in
 *       bad state).
 *   -3: A userland exception surfaced inside one of the direct calls; the
 *       class/message is stashed for ephpm_opcache_last_exception().
 * Non-negative values are the number of scripts invalidated. */
#define EPHPM_OPCACHE_UNAVAILABLE (-1L)
#define EPHPM_OPCACHE_BAILOUT     (-2L)
#define EPHPM_OPCACHE_EXCEPTION   (-3L)

/* Last exception observed by ephpm_opcache_invalidate_under ("Class: msg").
 * Thread-local, valid until the next call on the same thread. Cleared at the
 * top of every call. */
static EPHPM_TLS char opcache_exc_buf[256];

const char *ephpm_opcache_last_exception(void)
{
    return opcache_exc_buf;
}

/* If a userland exception is pending on the executor, format its class name
 * and message into opcache_exc_buf and clear it. A leaked pending exception
 * would surface inside the next unrelated script — same reason the old
 * snippet-based path did this after zend_eval_string_ex. */
static void ephpm_opcache_capture_exception(void)
{
    if (!EG(exception)) {
        return;
    }
    if (zend_is_unwind_exit(EG(exception))) {
        /* exit()/die() unwind — leave it in place for the outer request loop. */
        return;
    }
    zend_object *ex = EG(exception);
    const char *cls = ZSTR_VAL(ex->ce->name);
    zval rv;
    zval *msg = zend_read_property_ex(
        ex->ce, ex, ZSTR_KNOWN(ZEND_STR_MESSAGE), 1, &rv);
    const char *msg_str =
        (msg && Z_TYPE_P(msg) == IS_STRING) ? Z_STRVAL_P(msg) : "?";
    snprintf(opcache_exc_buf, sizeof(opcache_exc_buf), "%s: %s", cls, msg_str);
    zend_clear_exception();
}

/*
 * Invalidate every cached OPcache script whose path starts with `docroot`.
 *
 * Returns the number of scripts invalidated (>= 0), or one of the
 * EPHPM_OPCACHE_* codes above. Must be called from a TSRM-registered thread
 * with an active PHP request.
 */
long ephpm_opcache_invalidate_under(const char *docroot)
{
    opcache_exc_buf[0] = '\0';
    if (!docroot || docroot[0] == '\0') {
        return EPHPM_OPCACHE_UNAVAILABLE;
    }

    /* Look up opcache_get_status / opcache_invalidate directly in the executor
     * function table. If OPcache is not loaded (or was disabled at runtime and
     * unregistered its functions), we get NULL and bail. */
    zend_function *fn_status = zend_hash_str_find_ptr(
        EG(function_table), "opcache_get_status", sizeof("opcache_get_status") - 1);
    zend_function *fn_invalidate = zend_hash_str_find_ptr(
        EG(function_table), "opcache_invalidate", sizeof("opcache_invalidate") - 1);
    if (!fn_status || !fn_invalidate) {
        return EPHPM_OPCACHE_UNAVAILABLE;
    }

    size_t prefix_len = strlen(docroot);
    long count = 0;

    /* SETJMP guard: a bailout inside opcache_get_status / opcache_invalidate
     * (OOM / OPcache-in-bad-state) must not unwind through Rust. */
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;
    EG(bailout) = &__bailout;

    zval status_ret;
    ZVAL_UNDEF(&status_ret);

    if (SETJMP(__bailout) == 0) {
        /* status_ret = opcache_get_status(true); — pass a single boolean-true
         * argument so the returned array includes the per-script table. */
        zval status_args[1];
        ZVAL_TRUE(&status_args[0]);
        zend_call_known_function(
            fn_status, NULL, NULL, &status_ret, 1, status_args, NULL);

        if (EG(exception)) {
            ephpm_opcache_capture_exception();
            count = EPHPM_OPCACHE_EXCEPTION;
            goto done;
        }

        if (Z_TYPE(status_ret) != IS_ARRAY) {
            /* opcache.enable=0 returns false; anything non-array means OPcache
             * is not cooperating. Nothing to invalidate. */
            count = EPHPM_OPCACHE_UNAVAILABLE;
            goto done;
        }

        /* Fetch the 'scripts' sub-array. Missing / empty is a success with
         * zero invalidations (cold cache, or freshly reset). */
        zval *scripts_zv = zend_hash_str_find(
            Z_ARRVAL(status_ret), "scripts", sizeof("scripts") - 1);
        if (!scripts_zv || Z_TYPE_P(scripts_zv) != IS_ARRAY) {
            count = 0;
            goto done;
        }

        /* Iterate the scripts HashTable. Keys are the absolute script paths
         * (OPcache stores full_path as the key). For each key whose prefix
         * matches the vhost docroot, invoke opcache_invalidate($path, true). */
        HashTable *scripts_ht = Z_ARRVAL_P(scripts_zv);
        zend_string *key;
        zend_ulong idx;
        zval *val;
        (void)idx;
        (void)val;
        ZEND_HASH_FOREACH_KEY_VAL(scripts_ht, idx, key, val) {
            if (!key) {
                /* integer key — never a script path; skip. */
                continue;
            }
            if (ZSTR_LEN(key) < prefix_len) {
                continue;
            }
            if (memcmp(ZSTR_VAL(key), docroot, prefix_len) != 0) {
                continue;
            }

            /* opcache_invalidate($path, true) — force=true so an unchanged
             * mtime doesn't block the drop (deploys often preserve timestamps). */
            zval inv_args[2];
            ZVAL_STR_COPY(&inv_args[0], key);
            ZVAL_TRUE(&inv_args[1]);

            zval inv_ret;
            ZVAL_UNDEF(&inv_ret);
            zend_call_known_function(
                fn_invalidate, NULL, NULL, &inv_ret, 2, inv_args, NULL);

            zval_ptr_dtor(&inv_args[0]);
            zval_ptr_dtor(&inv_ret);

            if (EG(exception)) {
                ephpm_opcache_capture_exception();
                count = EPHPM_OPCACHE_EXCEPTION;
                goto done;
            }
            count++;
        } ZEND_HASH_FOREACH_END();
    } else {
        /* zend_bailout() longjmped. Report distinctly. */
        count = EPHPM_OPCACHE_BAILOUT;
    }

done:
    EG(bailout) = __orig_bailout;

    /* Belt-and-braces: even on the happy path, ensure nothing we called left
     * a pending exception behind that would surface in the next script. */
    if (EG(exception) && !zend_is_unwind_exit(EG(exception))) {
        ephpm_opcache_capture_exception();
    }

    zval_ptr_dtor(&status_ret);
    return count;
}

/*
 * Read the OPcache JIT buffer stats: opcache_get_status(false)['jit']
 * ['buffer_size' / 'buffer_free'], in bytes.
 *
 * Returns 0 on success (outputs written), or the EPHPM_OPCACHE_* failure
 * codes above (-1 unavailable / unrecognised shape, -2 bailout, -3 userland
 * exception — stashed for ephpm_opcache_last_exception()). When the JIT is
 * disabled the call still succeeds and reports buffer_size = 0.
 *
 * Same calling contract as ephpm_opcache_invalidate_under: TSRM-registered
 * thread, inside the thread's active request context (the router calls it on
 * the PHP dispatch thread, and worker mode from the worker's own long-lived
 * request). opcache_get_status(false) excludes the per-script table, so a
 * call is cheap — callers additionally rate-limit (ephpm-php jit_metrics).
 */
long ephpm_opcache_jit_stats(unsigned long long *buffer_size,
                             unsigned long long *buffer_free)
{
    opcache_exc_buf[0] = '\0';
    if (!buffer_size || !buffer_free) {
        return EPHPM_OPCACHE_UNAVAILABLE;
    }

    zend_function *fn_status = zend_hash_str_find_ptr(
        EG(function_table), "opcache_get_status", sizeof("opcache_get_status") - 1);
    if (!fn_status) {
        return EPHPM_OPCACHE_UNAVAILABLE;
    }

    long rc = EPHPM_OPCACHE_UNAVAILABLE;

    /* SETJMP guard: a bailout inside opcache_get_status must not unwind
     * through Rust (same shape as the invalidator above). */
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;
    EG(bailout) = &__bailout;

    zval status_ret;
    ZVAL_UNDEF(&status_ret);

    if (SETJMP(__bailout) == 0) {
        /* status_ret = opcache_get_status(false); — no per-script table. */
        zval status_args[1];
        ZVAL_FALSE(&status_args[0]);
        zend_call_known_function(
            fn_status, NULL, NULL, &status_ret, 1, status_args, NULL);

        if (EG(exception)) {
            ephpm_opcache_capture_exception();
            rc = EPHPM_OPCACHE_EXCEPTION;
            goto done;
        }

        if (Z_TYPE(status_ret) != IS_ARRAY) {
            /* opcache.enable=0 returns false. */
            rc = EPHPM_OPCACHE_UNAVAILABLE;
            goto done;
        }

        zval *jit_zv = zend_hash_str_find(
            Z_ARRVAL(status_ret), "jit", sizeof("jit") - 1);
        if (!jit_zv || Z_TYPE_P(jit_zv) != IS_ARRAY) {
            rc = EPHPM_OPCACHE_UNAVAILABLE;
            goto done;
        }

        zval *size_zv = zend_hash_str_find(
            Z_ARRVAL_P(jit_zv), "buffer_size", sizeof("buffer_size") - 1);
        zval *free_zv = zend_hash_str_find(
            Z_ARRVAL_P(jit_zv), "buffer_free", sizeof("buffer_free") - 1);
        if (!size_zv || Z_TYPE_P(size_zv) != IS_LONG
            || !free_zv || Z_TYPE_P(free_zv) != IS_LONG) {
            rc = EPHPM_OPCACHE_UNAVAILABLE;
            goto done;
        }

        /* zend_long is signed; the engine never reports negative sizes, but
         * clamp defensively rather than wrap on cast. */
        *buffer_size = Z_LVAL_P(size_zv) < 0
            ? 0ULL : (unsigned long long)Z_LVAL_P(size_zv);
        *buffer_free = Z_LVAL_P(free_zv) < 0
            ? 0ULL : (unsigned long long)Z_LVAL_P(free_zv);
        rc = 0;
    } else {
        rc = EPHPM_OPCACHE_BAILOUT;
    }

done:
    EG(bailout) = __orig_bailout;
    if (EG(exception) && !zend_is_unwind_exit(EG(exception))) {
        ephpm_opcache_capture_exception();
    }
    zval_ptr_dtor(&status_ret);
    return rc;
}

/* ===================================================================
 * Worker mode — persistent-worker engine (design: worker-mode-design.md)
 *
 * Registers Ephpm\Worker\take_request() / send_response() and the
 * Ephpm\Worker\Envelope class in PHP userland. Inverts control: PHP boots
 * the framework once (via ephpm_worker_run) then loops calling take_request()
 * (blocks in Rust until the next HTTP request) and send_response().
 *
 * Everything here runs on the worker's own long-lived TSRM request context.
 * ephpm_worker_reset_request() resets per-iteration SAPI state WITHOUT the
 * php_request_shutdown/startup that would destroy the booted framework.
 * =================================================================== */

/* Borrowed view of the next HTTP request, filled by the Rust take_request
 * callback. All pointers are owned by the Rust-side channel message and stay
 * valid until the matching send_response() runs — the same "valid until
 * execute returns" contract ephpm_request_set_info relies on. The C side
 * copies every field into zend_strings when building the Envelope, so PHP
 * never retains a borrowed pointer. Server vars and headers are packed as
 * count + a flat array of (key,value) C-string pointer pairs. */
typedef struct {
    const char *method;
    const char *uri;              /* REQUEST_URI (path + query) */
    const char *query_string;     /* without leading '?' */
    const char *cookie_data;      /* raw Cookie header value */
    const char *content_type;     /* may be NULL */
    const char *body;             /* raw request body (may be NULL) */
    size_t      body_len;
    /* Phase 3: when non-zero, the body is streamed via g_worker_ops.body_read
     * (body/body_len are unset). When zero, the whole body is in `body`. */
    int         body_streaming;

    size_t      server_var_count;
    const char *const *server_var_keys;
    const char *const *server_var_vals;

    size_t      header_count;
    const char *const *header_keys;
    const char *const *header_vals;
} EphpmWorkerRequest;

/* Function pointer table into Rust. Mirrors EphpmWorkerOps in
 * crates/ephpm-php/src/worker_bridge.rs — keep the two in lockstep. */
typedef struct {
    /* Block until the next request. On return: 1 = request available (req
     * filled), 0 = graceful shutdown (worker returns from its loop). */
    int (*take_request)(EphpmWorkerRequest *req);
    /* Hand back the response. headers packed as "Name: Value\n" lines. */
    void (*send_response)(int status,
                          const char *headers, size_t headers_len,
                          const char *body, size_t body_len);

    /* ── Phase 3: streaming bodies ──────────────────────────────────
     * Streaming request read (design §9). Pull up to `cap` bytes of the
     * incremental request body into `buf`. Returns the number of bytes
     * written (0 = clean EOF, negative = error). Blocks until at least one
     * byte is available or EOF. Backed by a bounded channel the hyper task
     * feeds; the worker thread blocks here. When the request was dispatched
     * fully-buffered (no streaming reader), this serves from the in-memory
     * body so the same read path works both ways. */
    long (*body_read)(char *buf, size_t cap);

    /* Begin a streaming response: status + packed headers, no body yet. The
     * hyper handler builds a streamed response body from the chunks that
     * follow. */
    void (*response_begin)(int status,
                           const char *headers, size_t headers_len);
    /* Push one response body chunk. Blocks on backpressure (bounded channel).
     * Returns 0 on success, negative if the client/receiver went away (the
     * worker should stop producing). */
    long (*response_chunk)(const char *buf, size_t len);
    /* Finish the streaming response (close the body channel). */
    void (*response_end)(void);
} EphpmWorkerOps;

static EphpmWorkerOps g_worker_ops = {0};

/* Whether the runtime asked us to populate native superglobals per request
 * (worker.populate_superglobals — WordPress adapter). Set once before boot. */
static int g_worker_populate_superglobals = 0;

/* The Envelope class entry, registered in MINIT. */
static zend_class_entry *ephpm_worker_envelope_ce = NULL;

/*
 * Per-iteration reset (design §3.5). Called at the top of take_request(),
 * on the worker's own TSRM context, inside the long-lived request.
 * Deliberately does NOT call php_request_shutdown/startup — that would tear
 * down the booted framework. Touches the SAME SAPI globals the hardened fpm
 * reuse path touches (ephpm_wrapper.c:823-825, :844), minus the lifecycle
 * calls; that symmetry is the safety argument.
 */
void ephpm_worker_reset_request(void)
{
    /* Thread-local C capture buffers. */
    output_len = 0;
    headers_buf_len = 0;

    /* Drop headers emitted by the previous response so they don't accumulate
     * (fpm gets this free from php_request_shutdown). */
    zend_llist_clean(&SG(sapi_headers).headers);
    if (SG(sapi_headers).mimetype) {
        efree(SG(sapi_headers).mimetype);
        SG(sapi_headers).mimetype = NULL;
    }

    /* Proven leak fix on the reuse path (:823-825): without this a prior
     * request's status / headers_sent / no_headers leaks into the next. */
    SG(sapi_headers).http_response_code = 200;
    SG(headers_sent) = 0;
    SG(request_info).no_headers = 0;

    /* Per-iteration fatal detection (:844). */
    PG(last_error_type) = 0;

    /* Per-iteration POST cursor + streaming flag. */
    req_post_data_offset = 0;
    req_body_streaming = 0;
    req_in_flight = 0;

    /* Per-iteration lazy-envelope backing pointers. take_request() will
     * re-populate these before returning the new Envelope; nulling them here
     * makes any accidental accessor call in the gap between iterations
     * (there should be none — the reset runs synchronously inside
     * take_request) return an empty array via the generation-mismatch path
     * rather than reading stale pointers. */
    req_lazy_server_count = 0;
    req_lazy_server_keys  = NULL;
    req_lazy_server_vals  = NULL;
    req_lazy_header_count = 0;
    req_lazy_header_keys  = NULL;
    req_lazy_header_vals  = NULL;
    req_lazy_cookie_data  = NULL;
    req_lazy_query_string = NULL;

    response_status_code = 200;
}

/* Pull the next chunk of a streaming request body from Rust. Serves both the
 * read_post SAPI callback ($_POST/multipart) and the bodyStream() php_stream,
 * so the incremental body is consumed exactly once regardless of which the
 * framework reaches for. Blocks until data is available or EOF. */
static long ephpm_worker_body_read(char *buf, size_t cap)
{
    if (!g_worker_ops.body_read || cap == 0) {
        return 0;
    }
    return g_worker_ops.body_read(buf, cap);
}

/* ── bodyStream(): a real readable php:// stream over the incremental body ──
 * A php_stream whose read op pulls from ephpm_worker_body_read (backed by the
 * bounded hyper->worker channel). Non-seekable, read-only, no writes. This is
 * the Phase-3 zero-prebuffer request path: a multi-GB upload flows through in
 * fixed-size reads with flat worker memory. */
static ssize_t ephpm_body_stream_read(php_stream *stream, char *buf, size_t count)
{
    /* Generation guard: a stream resource stashed across iterations must not
     * read the NEXT request's body from the shared thread-local reader. */
    const unsigned long *gen = (const unsigned long *)stream->abstract;
    if (!gen || *gen != req_generation) {
        stream->eof = 1;
        return 0;
    }
    long n = ephpm_worker_body_read(buf, count);
    if (n < 0) {
        return -1;
    }
    if (n == 0) {
        stream->eof = 1;
        return 0;
    }
    return (ssize_t)n;
}

static int ephpm_body_stream_close(php_stream *stream, int close_handle)
{
    (void)close_handle;
    /* Only the generation marker is owned on the C side — the Rust reader is
     * freed when the worker finishes the request. */
    if (stream->abstract) {
        efree(stream->abstract);
        stream->abstract = NULL;
    }
    return 0;
}

static int ephpm_body_stream_flush(php_stream *stream)
{
    (void)stream;
    return 0;
}

static const php_stream_ops ephpm_body_stream_ops = {
    NULL,                        /* write (read-only) */
    ephpm_body_stream_read,      /* read */
    ephpm_body_stream_close,     /* close */
    ephpm_body_stream_flush,     /* flush */
    "ephpm-request-body",        /* label */
    NULL,                        /* seek (non-seekable) */
    NULL,                        /* cast */
    NULL,                        /* stat */
    NULL                         /* set_option */
};

/* Build a php_stream reading the incremental request body. The abstract
 * pointer carries the request generation the stream was opened under. */
static php_stream *ephpm_worker_open_body_stream(void)
{
    unsigned long *gen = emalloc(sizeof(*gen));
    *gen = req_generation;
    php_stream *stream = php_stream_alloc(&ephpm_body_stream_ops, gen, NULL, "rb");
    if (!stream) {
        efree(gen);
    }
    return stream;
}

/* Build a PHP array of (key => value) string pairs from a packed C list.
 * Repeated keys (duplicate request headers, e.g. X-Forwarded-For sent twice)
 * are joined per RFC 9110 §5.3 list semantics rather than overwritten; a
 * repeated Cookie header joins with the cookie-pair separator instead. */
static void ephpm_worker_fill_str_array(zval *arr, size_t count,
                                        const char *const *keys,
                                        const char *const *vals)
{
    /* Pre-size the hashtable to avoid rehash growth on the hot path. */
    array_init_size(arr, count);
    for (size_t i = 0; i < count; i++) {
        if (!keys[i]) {
            continue;
        }
        const char *v = vals[i] ? vals[i] : "";
        size_t klen = strlen(keys[i]);
        zval *existing = zend_hash_str_find(Z_ARRVAL_P(arr), keys[i], klen);
        if (existing && Z_TYPE_P(existing) == IS_STRING) {
            const char *sep =
                (zend_binary_strcasecmp(keys[i], klen, "cookie", sizeof("cookie") - 1) == 0)
                    ? "; " : ", ";
            zend_string *joined = zend_strpprintf(0, "%s%s%s", Z_STRVAL_P(existing), sep, v);
            add_assoc_str_ex(arr, keys[i], klen, joined);
        } else {
            add_assoc_stringl_ex(arr, keys[i], klen, (char *)v, strlen(v));
        }
    }
}

/* Parse "a=1; b=2" cookie header into an associative array. */
static void ephpm_worker_parse_cookies(zval *arr, const char *cookie)
{
    array_init(arr);
    if (!cookie || !*cookie) {
        return;
    }
    char *dup = estrdup(cookie);
    char *saveptr = NULL;
    char *pair = strtok_r(dup, ";", &saveptr);
    while (pair) {
        while (*pair == ' ') pair++;
        char *eq = strchr(pair, '=');
        if (eq) {
            *eq = '\0';
            add_assoc_string(arr, pair, eq + 1);
        }
        pair = strtok_r(NULL, ";", &saveptr);
    }
    efree(dup);
}

/* Parse "a=1&b=2" query string into an associative array (no url-decoding —
 * Phase 1 keeps it framework-neutral; adapters do their own decoding). */
static void ephpm_worker_parse_query(zval *arr, const char *qs)
{
    array_init(arr);
    if (!qs || !*qs) {
        return;
    }
    char *dup = estrdup(qs);
    char *saveptr = NULL;
    char *pair = strtok_r(dup, "&", &saveptr);
    while (pair) {
        char *eq = strchr(pair, '=');
        if (eq) {
            *eq = '\0';
            add_assoc_string(arr, pair, eq + 1);
        } else if (*pair) {
            add_assoc_string(arr, pair, "");
        }
        pair = strtok_r(NULL, "&", &saveptr);
    }
    efree(dup);
}

/* Store an array as a private property on the Envelope $this object. */
static void ephpm_worker_set_prop_array(zval *obj, const char *name, zval *arr)
{
    zend_update_property(ephpm_worker_envelope_ce, Z_OBJ_P(obj), name,
                         strlen(name), arr);
    zval_ptr_dtor(arr);
}

static void ephpm_worker_set_prop_stringl(zval *obj, const char *name,
                                          const char *val, size_t len)
{
    zend_update_property_stringl(ephpm_worker_envelope_ce, Z_OBJ_P(obj), name,
                                 strlen(name), val ? val : "", val ? len : 0);
}

/* PHP_FUNCTION: \Ephpm\Worker\take_request(): ?\Ephpm\Worker\Envelope
 *
 * Runs the per-iteration reset, blocks in Rust for the next request, and
 * returns an Envelope object (null on graceful shutdown). */
PHP_FUNCTION(ephpm_worker_take_request)
{
    ZEND_PARSE_PARAMETERS_NONE();

    if (!g_worker_ops.take_request) {
        RETURN_NULL();
    }

    /* Reset SAPI-scoped state from the previous iteration BEFORE we block, so
     * the previous response's headers/status/output are already gone. */
    ephpm_worker_reset_request();

#ifdef EPHPM_NATIVE_EXEC_TIMER
    /* Disarm the per-thread execution timer while the worker is idle. The timer
     * is wall-clock (CLOCK_BOOTTIME), so an armed timer would keep counting
     * while we block waiting for the next request and could fire during the
     * idle wait or the next request's early setup. It is re-armed below, once a
     * request is actually in hand. */
    zend_unset_timeout();
#endif

    EphpmWorkerRequest req;
    memset(&req, 0, sizeof(req));
    int have = g_worker_ops.take_request(&req);
    if (!have) {
        /* Graceful shutdown — worker.php's while-loop ends, ephpm_worker_run
         * returns, the pool respawns or drains. */
        RETURN_NULL();
    }

    /* A request is now in flight (until send_response/send_response_stream
     * completes); new generation for bodyStream() isolation. */
    req_in_flight = 1;
    req_generation++;

#ifdef EPHPM_NATIVE_EXEC_TIMER
    /* Re-arm PHP's per-thread execution timer fresh for THIS request, resetting
     * both the countdown and EG(timeout_seconds) to the configured value
     * (reset_signals = 1). Using the process-wide configured baseline — not
     * INI_INT / EG(timeout_seconds) — is what prevents a previous request's
     * set_time_limit(0) (which set EG(timeout_seconds)=0 and altered the ini
     * entry at RUNTIME stage) from leaking into this one. set_time_limit()
     * during this request still re-arms live on top of this baseline.
     * ephpm_arm_exec_timer() also mirrors the value back into the ini entry, so
     * a prior request's set_time_limit(0) does not leave ini_get() reporting 0
     * on this fresh request either (#279). */
    ephpm_arm_exec_timer();
#endif

    /* Point the SAPI request-info + POST buffers at this request so php://input
     * and any framework that reads them see the right body. These are the same
     * thread-local fields the fpm read_post/read_cookies callbacks use. */
    req_method = req.method;
    req_uri = req.uri;
    req_query_string = req.query_string;
    req_content_type = req.content_type;
    req_cookie_data = req.cookie_data;
    req_post_data = req.body;
    req_post_data_len = req.body_len;
    req_post_data_offset = 0;
    /* Phase 3: route read_post / bodyStream() through the incremental Rust
     * reader when the request was dispatched streaming (large upload). */
    req_body_streaming = req.body_streaming ? 1 : 0;

    SG(request_info).request_method = (char *)req.method;
    SG(request_info).request_uri = (char *)req.uri;
    SG(request_info).query_string = (char *)req.query_string;
    SG(request_info).content_type = req.content_type;
    SG(request_info).cookie_data = (char *)req.cookie_data;
    /* For streaming requests body_len carries the declared Content-Length (so
     * PHP's post reader knows how much to expect); the bytes arrive via
     * body_read. For buffered requests it is the actual body length. */
    SG(request_info).content_length = (zend_long)req.body_len;

    /* Optionally rebuild native superglobals through the normal, quiescent
     * treat_data path (WordPress). We NEVER hand-rebuild PG(http_globals) —
     * that re-triggers the php_default_treat_data UAF (design §3.4). Instead
     * we let the registered SAPI callbacks repopulate $_SERVER/$_COOKIE/$_GET
     * via php_hash_environment(), which is safe at this quiescent point. */
    if (g_worker_populate_superglobals) {
        /* Reset server-var registration to this request's set. */
        server_var_count = 0;
        for (size_t i = 0; i < req.server_var_count && i < MAX_SERVER_VARS; i++) {
            ephpm_request_add_server_var(req.server_var_keys[i], req.server_var_vals[i]);
        }
        zend_try {
            php_hash_environment();
        } zend_catch {
            /* Non-fatal: the envelope below still gives the framework the data. */
        } zend_end_try();
    }

    /* Build the Envelope object.
     *
     * Phase 1 lazy fast path (roadmap `worker-dispatch-fastpath.md` §Phase 1):
     * we do NOT materialize serverVars/headers/cookies/query here — they are
     * built by the Envelope accessor methods on first call and cached as
     * properties. Most handlers touch one or two of them, so eagerly building
     * five arrays per request (5 array_init + N allocations per array) is
     * pure waste on the hot path.
     *
     * The pointer arrays that back the lazy build borrow from CurrentRequest
     * on the Rust side; they stay valid until this thread's matching
     * send_response completes. We stamp the current req_generation onto the
     * Envelope so a userland-stashed envelope from a prior iteration can
     * detect that its data pointers no longer refer to it and return empty
     * instead of reading the next request's data. */
    object_init_ex(return_value, ephpm_worker_envelope_ce);

    req_lazy_server_count = req.server_var_count;
    req_lazy_server_keys  = req.server_var_keys;
    req_lazy_server_vals  = req.server_var_vals;
    req_lazy_header_count = req.header_count;
    req_lazy_header_keys  = req.header_keys;
    req_lazy_header_vals  = req.header_vals;
    req_lazy_cookie_data  = req.cookie_data;
    req_lazy_query_string = req.query_string;

    zend_update_property_long(ephpm_worker_envelope_ce, Z_OBJ_P(return_value),
                              "generation", strlen("generation"),
                              (zend_long)req_generation);

    /* Body. Buffered request: store the whole body string (Phase-1 back-compat;
     * rawBody() and bodyStream() both serve from it). Streaming request: store
     * an empty rawBody and a "streaming" marker — bodyStream() opens a real
     * php:// stream over the incremental reader, and rawBody() reads that
     * stream to a string on demand (which re-buffers; adapters that care about
     * memory use bodyStream()). */
    ZEND_ASSERT(ephpm_worker_envelope_ce != NULL);
    zend_update_property_bool(ephpm_worker_envelope_ce, Z_OBJ_P(return_value),
                              "streaming", strlen("streaming"),
                              req.body_streaming ? 1 : 0);
    if (req.body_streaming) {
        ephpm_worker_set_prop_stringl(return_value, "rawBody", "", 0);
    } else {
        ephpm_worker_set_prop_stringl(return_value, "rawBody", req.body, req.body_len);
    }
}

/* Append one "Name: Value\n" line to the packed header buffer. */
static void ephpm_worker_pack_header_line(smart_str *out, zend_string *key, zval *val)
{
    zend_string *vstr = zval_get_string(val);
    smart_str_appendl(out, ZSTR_VAL(key), ZSTR_LEN(key));
    smart_str_appendl(out, ": ", 2);
    smart_str_appendl(out, ZSTR_VAL(vstr), ZSTR_LEN(vstr));
    smart_str_appendc(out, '\n');
    zend_string_release(vstr);
}

/* Pack a PHP headers array into "Name: Value\n" lines. A list value packs one
 * line per element — the multi-value header contract (e.g.
 * ['Set-Cookie' => [$c1, $c2]] emits two Set-Cookie lines, which the Rust side
 * forwards as two distinct wire headers). Caller frees the smart_str. */
static void ephpm_worker_pack_headers(smart_str *out, zval *headers_arr)
{
    zend_string *hkey;
    zval *hval;
    ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(headers_arr), hkey, hval) {
        if (!hkey) {
            continue; /* skip numeric keys */
        }
        ZVAL_DEREF(hval);
        if (Z_TYPE_P(hval) == IS_ARRAY) {
            zval *item;
            ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(hval), item) {
                ZVAL_DEREF(item);
                ephpm_worker_pack_header_line(out, hkey, item);
            } ZEND_HASH_FOREACH_END();
        } else {
            ephpm_worker_pack_header_line(out, hkey, hval);
        }
    } ZEND_HASH_FOREACH_END();
    smart_str_0(out);
}

/* PHP_FUNCTION: \Ephpm\Worker\send_response(int, array, string): void
 *
 * Concatenates any captured output_buf (echo path) with the explicit $body,
 * packs the $headers array into "Name: Value\n" lines (list values become one
 * line per element), and hands both to the Rust send_response callback (which
 * fulfils the parked oneshot). */
PHP_FUNCTION(ephpm_worker_send_response)
{
    zend_long status;
    zval *headers_arr;
    char *body;
    size_t body_len;

    ZEND_PARSE_PARAMETERS_START(3, 3)
        Z_PARAM_LONG(status)
        Z_PARAM_ARRAY(headers_arr)
        Z_PARAM_STRING(body, body_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_worker_ops.send_response) {
        return;
    }

    smart_str hbuf = {0};
    ephpm_worker_pack_headers(&hbuf, headers_arr);

    /* Concatenate captured echo output (if any) + explicit $body. */
    const char *hdr_ptr = hbuf.s ? ZSTR_VAL(hbuf.s) : "";
    size_t hdr_len = hbuf.s ? ZSTR_LEN(hbuf.s) : 0;

    if (output_len > 0) {
        smart_str bbuf = {0};
        smart_str_appendl(&bbuf, output_buf, output_len);
        smart_str_appendl(&bbuf, body, body_len);
        smart_str_0(&bbuf);
        g_worker_ops.send_response((int)status, hdr_ptr, hdr_len,
                                   ZSTR_VAL(bbuf.s), ZSTR_LEN(bbuf.s));
        smart_str_free(&bbuf);
    } else {
        g_worker_ops.send_response((int)status, hdr_ptr, hdr_len, body, body_len);
    }

    smart_str_free(&hbuf);

    /* Clear the captured output so it does not bleed into the next response
     * (the reset at the top of the next take_request also clears it, but this
     * keeps the accounting local). */
    output_len = 0;
    req_in_flight = 0;
}

/* PHP_FUNCTION: \Ephpm\Worker\send_response_stream(int $status, array $headers,
 *                                                  $bodyResource): void
 *
 * Phase-3 streaming response. Rather than handing back a full body string, the
 * framework passes a readable stream/resource; we pump it to the HTTP layer in
 * fixed-size chunks so bytes reach the client before PHP has produced them all
 * (flat worker memory for multi-GB downloads).
 *
 * Any captured echo output (ub_write) is flushed as the first chunk so the
 * echo path still works. Backpressure: response_chunk blocks on the bounded
 * hyper channel; if the client goes away it returns negative and we stop. */
PHP_FUNCTION(ephpm_worker_send_response_stream)
{
    zend_long status;
    zval *headers_arr;
    zval *body_res;

    ZEND_PARSE_PARAMETERS_START(3, 3)
        Z_PARAM_LONG(status)
        Z_PARAM_ARRAY(headers_arr)
        Z_PARAM_RESOURCE(body_res)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_worker_ops.response_begin || !g_worker_ops.response_chunk ||
        !g_worker_ops.response_end) {
        /* Streaming ops not installed — nothing we can do; drop the request so
         * the parked oneshot resolves via the supervisor's 500 net. */
        return;
    }

    php_stream *stream;
    php_stream_from_zval_no_verify(stream, body_res);
    if (!stream) {
        return;
    }

    smart_str hbuf = {0};
    ephpm_worker_pack_headers(&hbuf, headers_arr);
    const char *hdr_ptr = hbuf.s ? ZSTR_VAL(hbuf.s) : "";
    size_t hdr_len = hbuf.s ? ZSTR_LEN(hbuf.s) : 0;

    g_worker_ops.response_begin((int)status, hdr_ptr, hdr_len);
    smart_str_free(&hbuf);

    /* Flush any buffered echo output first. */
    if (output_len > 0) {
        (void)g_worker_ops.response_chunk(output_buf, output_len);
        output_len = 0;
    }

    /* Pump the stream to the client in fixed-size chunks. */
    char chunk[65536];
    for (;;) {
        ssize_t n = php_stream_read(stream, chunk, sizeof(chunk));
        if (n <= 0) {
            break;
        }
        if (g_worker_ops.response_chunk(chunk, (size_t)n) < 0) {
            /* Receiver/client gone — stop producing. */
            break;
        }
    }

    g_worker_ops.response_end();
    req_in_flight = 0;

    /* Release the borrowed request backing storage (the Rust send_response
     * path does this too, but response_end delivers via a different channel). */
}

/* ── Envelope methods ─────────────────────────────────────────────
 * Each returns the property populated by take_request. Framework-neutral;
 * adapters build their own Request from these. */

static void ephpm_worker_return_prop(INTERNAL_FUNCTION_PARAMETERS, const char *name)
{
    ZEND_PARSE_PARAMETERS_NONE();
    zval rv;
    zval *prop = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(ZEND_THIS),
                                    name, strlen(name), 1, &rv);
    RETURN_COPY(prop);
}

/* Is this Envelope still associated with the in-flight request? An envelope
 * stashed by userland across the loop iteration (foot-gun documented in the
 * guide) fails this check; accessors then return empty arrays instead of
 * reading the next request's data. */
static int ephpm_worker_envelope_is_current(zval *this_obj)
{
    zval rv;
    zval *gen = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(this_obj),
                                   "generation", strlen("generation"), 1, &rv);
    if (!gen || Z_TYPE_P(gen) != IS_LONG) {
        return 0;
    }
    return (unsigned long)Z_LVAL_P(gen) == req_generation;
}

/* Fetch the cached array property for `name`, or NULL if not yet built.
 * The property is set to IS_NULL by ZE by default (dynamic properties: reads
 * before a write hand back an uninitialized-property warning + IS_NULL).
 * Treat IS_ARRAY as "cached", anything else as "needs building". */
static zval *ephpm_worker_get_cached_array(zval *this_obj, const char *name, size_t name_len, zval *rv)
{
    zval *prop = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(this_obj),
                                    name, name_len, 1, rv);
    if (prop && Z_TYPE_P(prop) == IS_ARRAY) {
        return prop;
    }
    return NULL;
}

/* Lazy accessor bodies. Each: (1) check the cache; (2) validate generation
 * (empty array if stale — never expose the next request's data); (3) build
 * the array from the stashed pointers; (4) cache it as a property; (5)
 * return it. */
PHP_METHOD(Ephpm_Worker_Envelope, serverVars)
{
    ZEND_PARSE_PARAMETERS_NONE();
    zval rv;
    zval *cached = ephpm_worker_get_cached_array(ZEND_THIS, "serverVars",
                                                 strlen("serverVars"), &rv);
    if (cached) {
        RETURN_COPY(cached);
    }
    zval arr;
    if (ephpm_worker_envelope_is_current(ZEND_THIS)) {
        ephpm_worker_fill_str_array(&arr, req_lazy_server_count,
                                    req_lazy_server_keys, req_lazy_server_vals);
    } else {
        array_init(&arr);
    }
    ephpm_worker_set_prop_array(ZEND_THIS, "serverVars", &arr);
    zval *back = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(ZEND_THIS),
                                    "serverVars", strlen("serverVars"), 1, &rv);
    RETURN_COPY(back);
}

PHP_METHOD(Ephpm_Worker_Envelope, headers)
{
    ZEND_PARSE_PARAMETERS_NONE();
    zval rv;
    zval *cached = ephpm_worker_get_cached_array(ZEND_THIS, "headers",
                                                 strlen("headers"), &rv);
    if (cached) {
        RETURN_COPY(cached);
    }
    zval arr;
    if (ephpm_worker_envelope_is_current(ZEND_THIS)) {
        ephpm_worker_fill_str_array(&arr, req_lazy_header_count,
                                    req_lazy_header_keys, req_lazy_header_vals);
    } else {
        array_init(&arr);
    }
    ephpm_worker_set_prop_array(ZEND_THIS, "headers", &arr);
    zval *back = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(ZEND_THIS),
                                    "headers", strlen("headers"), 1, &rv);
    RETURN_COPY(back);
}

PHP_METHOD(Ephpm_Worker_Envelope, cookies)
{
    ZEND_PARSE_PARAMETERS_NONE();
    zval rv;
    zval *cached = ephpm_worker_get_cached_array(ZEND_THIS, "cookies",
                                                 strlen("cookies"), &rv);
    if (cached) {
        RETURN_COPY(cached);
    }
    zval arr;
    if (ephpm_worker_envelope_is_current(ZEND_THIS)) {
        ephpm_worker_parse_cookies(&arr, req_lazy_cookie_data);
    } else {
        array_init(&arr);
    }
    ephpm_worker_set_prop_array(ZEND_THIS, "cookies", &arr);
    zval *back = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(ZEND_THIS),
                                    "cookies", strlen("cookies"), 1, &rv);
    RETURN_COPY(back);
}

PHP_METHOD(Ephpm_Worker_Envelope, query)
{
    ZEND_PARSE_PARAMETERS_NONE();
    zval rv;
    zval *cached = ephpm_worker_get_cached_array(ZEND_THIS, "query",
                                                 strlen("query"), &rv);
    if (cached) {
        RETURN_COPY(cached);
    }
    zval arr;
    if (ephpm_worker_envelope_is_current(ZEND_THIS)) {
        ephpm_worker_parse_query(&arr, req_lazy_query_string);
    } else {
        array_init(&arr);
    }
    ephpm_worker_set_prop_array(ZEND_THIS, "query", &arr);
    zval *back = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(ZEND_THIS),
                                    "query", strlen("query"), 1, &rv);
    RETURN_COPY(back);
}

/* Whether this envelope's body is streamed (Phase 3) rather than buffered. */
static int ephpm_worker_envelope_is_streaming(zval *this_obj)
{
    zval rv;
    zval *prop = zend_read_property(ephpm_worker_envelope_ce, Z_OBJ_P(this_obj),
                                    "streaming", strlen("streaming"), 1, &rv);
    return prop && zend_is_true(prop);
}

/* rawBody(): string — php://input equivalent.
 *
 * Buffered request: returns the stored body string (Phase-1 behavior).
 * Streaming request: drains the incremental reader into a string. This
 * re-buffers the whole body, defeating the streaming memory win — it exists
 * only for back-compat (a framework that insists on the raw string). Adapters
 * that care about memory use bodyStream() instead. Consuming the body once is
 * shared with bodyStream()/read_post, so calling both is a foot-gun. */
PHP_METHOD(Ephpm_Worker_Envelope, rawBody)
{
    ZEND_PARSE_PARAMETERS_NONE();

    if (!ephpm_worker_envelope_is_streaming(ZEND_THIS)) {
        ephpm_worker_return_prop(INTERNAL_FUNCTION_PARAM_PASSTHRU, "rawBody");
        return;
    }

    /* Drain the streaming reader into a smart_str. */
    smart_str buf = {0};
    char chunk[65536];
    for (;;) {
        long n = ephpm_worker_body_read(chunk, sizeof(chunk));
        if (n <= 0) {
            break;
        }
        smart_str_appendl(&buf, chunk, (size_t)n);
    }
    smart_str_0(&buf);
    if (buf.s) {
        RETVAL_STR(buf.s);          /* transfers ownership */
    } else {
        RETVAL_EMPTY_STRING();
    }
}

/* bodyStream(): resource — a real readable php:// stream over the incremental
 * request body (Phase 3). Reading it pulls fixed-size chunks from Rust without
 * pre-buffering, so a multi-GB upload flows through with flat worker memory.
 * For buffered requests it still works (reads from the in-memory body). */
PHP_METHOD(Ephpm_Worker_Envelope, bodyStream)
{
    ZEND_PARSE_PARAMETERS_NONE();

    php_stream *stream = ephpm_worker_open_body_stream();
    if (!stream) {
        RETURN_FALSE;
    }
    php_stream_to_zval(stream, return_value);
}

/* parsedBody(): ?array — Phase 1 returns null (form/multipart parsing is a
 * framework/adapter concern). Adapters that want native $_POST/$_FILES enable
 * worker.populate_superglobals, which drives PHP's own POST reader through the
 * (streaming) read_post callback — so form/multipart parsing still works and,
 * for large multipart uploads, PHP's rfc1867 handler spools file parts to
 * temp files rather than into memory. Note: PHP's POST reader and bodyStream()
 * share ONE incremental reader, so for a streaming request reading the body
 * both ways drains it once — pick one. */
PHP_METHOD(Ephpm_Worker_Envelope, parsedBody)
{
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_NULL();
}

/* files(): array — Phase 1 returns empty (uploads land in Phase 3). */
PHP_METHOD(Ephpm_Worker_Envelope, files)
{
    ZEND_PARSE_PARAMETERS_NONE();
    array_init(return_value);
}

/* ── arginfo ─────────────────────────────────────────────────── */

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_worker_take_request, 0, 0, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_worker_send_response, 0, 0, 3)
    ZEND_ARG_INFO(0, status)
    ZEND_ARG_INFO(0, headers)
    ZEND_ARG_INFO(0, body)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_worker_send_response_stream, 0, 0, 3)
    ZEND_ARG_INFO(0, status)
    ZEND_ARG_INFO(0, headers)
    ZEND_ARG_INFO(0, body)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_worker_envelope_noargs, 0, 0, 0)
ZEND_END_ARG_INFO()

/* Namespaced free functions: PHP stores them lowercased with a backslash
 * separator, so the entry name must be the fully-qualified "ephpm\\worker\\..."
 * for `\Ephpm\Worker\take_request()` to resolve. */
static const zend_function_entry ephpm_worker_functions[] = {
    ZEND_NS_NAMED_FE("Ephpm\\Worker", take_request,
                     ZEND_FN(ephpm_worker_take_request),
                     arginfo_ephpm_worker_take_request)
    ZEND_NS_NAMED_FE("Ephpm\\Worker", send_response,
                     ZEND_FN(ephpm_worker_send_response),
                     arginfo_ephpm_worker_send_response)
    ZEND_NS_NAMED_FE("Ephpm\\Worker", send_response_stream,
                     ZEND_FN(ephpm_worker_send_response_stream),
                     arginfo_ephpm_worker_send_response_stream)
    PHP_FE_END
};

static const zend_function_entry ephpm_worker_envelope_methods[] = {
    PHP_ME(Ephpm_Worker_Envelope, serverVars,  arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, headers,     arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, cookies,     arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, query,       arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, parsedBody,  arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, files,       arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, bodyStream,  arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_ME(Ephpm_Worker_Envelope, rawBody,     arginfo_ephpm_worker_envelope_noargs, ZEND_ACC_PUBLIC)
    PHP_FE_END
};

/* MINIT for the worker module. Registering the Envelope class here (rather
 * than directly in the embed startup shim) is REQUIRED: zend_register_internal_class
 * -> do_register_internal_class reads EG(current_module) while registering the
 * class's method table, and EG(current_module) is only non-NULL inside a real
 * module MINIT (the engine sets it around each module's MINIT). Registering the
 * class from the bare shim, where EG(current_module) is NULL, segfaults. The
 * module's own `functions` table registers the namespaced free functions with
 * the same correct module context. */
static PHP_MINIT_FUNCTION(ephpm_worker)
{
    (void)type;
    (void)module_number;

    zend_class_entry ce;
    INIT_NS_CLASS_ENTRY(ce, "Ephpm\\Worker", "Envelope", ephpm_worker_envelope_methods);
    ephpm_worker_envelope_ce = zend_register_internal_class(&ce);
    if (ephpm_worker_envelope_ce) {
        /* Store the marshaled request fields as DYNAMIC properties set at
         * runtime in take_request (zend_update_property creates them on the
         * instance). We deliberately do NOT pre-declare typed/default
         * properties: an internal class's default_properties_table must hold
         * non-refcounted zvals, and a string/array default there trips
         * "Internal zvals cannot be refcounted" at startup. Allowing dynamic
         * properties keeps the Envelope a plain data carrier without that
         * constraint. Not final, so adapters may subclass if useful. */
        ephpm_worker_envelope_ce->ce_flags |= ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES;
    }

    return SUCCESS;
}

/* Minimal module entry whose MINIT registers Ephpm\Worker\* + the Envelope
 * class with a valid EG(current_module) context. Passed to php_module_startup
 * as its `additional_module` from ephpm_module_startup, so it is started inside
 * zend_startup — the frozen window every ZTS worker later copies. */
static zend_module_entry ephpm_worker_module_entry = {
    STANDARD_MODULE_HEADER,
    "ephpm_worker",              /* name */
    ephpm_worker_functions,      /* functions (namespaced free functions) */
    PHP_MINIT(ephpm_worker),     /* MINIT: registers the Envelope class */
    NULL,                        /* MSHUTDOWN */
    NULL,                        /* RINIT */
    NULL,                        /* RSHUTDOWN */
    NULL,                        /* MINFO */
    "3.0",                       /* version */
    STANDARD_MODULE_PROPERTIES
};

/*
 * Set the worker ops function pointer table. Called after php_embed_init(),
 * before any worker boots. Mirrors ephpm_set_kv_ops.
 */
void ephpm_set_worker_ops(const EphpmWorkerOps *ops)
{
    if (ops) {
        g_worker_ops = *ops;
    }
}

/* Toggle native superglobal population (worker.populate_superglobals). */
void ephpm_worker_set_populate_superglobals(int enable)
{
    g_worker_populate_superglobals = enable ? 1 : 0;
}

/*
 * Boot a worker: run the worker script under bailout protection, exactly like
 * ephpm_execute_request's SETJMP structure. The script sits in a
 * while (take_request()) loop, so this call returns only when that loop ends
 * (graceful shutdown, worker_max_requests recycle, or a fatal bailout).
 *
 * Runs on the worker's own long-lived TSRM request (started by
 * ephpm_thread_init) — we do NOT start/stop a request here.
 *
 * Returns:
 *    0  the loop ended cleanly (shutdown / recycle)
 *    1  the script ended while a request was still in flight (exit()/die()
 *       mid-request — e.g. WordPress wp_die()/admin-ajax — or a loop break);
 *       the response was synthesized from SAPI state and delivered
 *    2  same, but the request ended on a PHP fatal (uncaught Throwable /
 *       E_ERROR). The synthesized response was forced to 500 unless the script
 *       had already chosen a status
 *   -2  a zend_bailout() killed the worker (recycle; the Rust supervisor
 *       fulfils any parked oneshot with a 500 and ABORTS an already-begun
 *       streaming response rather than letting it end cleanly)
 */
int ephpm_worker_run(const char *script)
{
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    /* See ephpm_bailout_observed(): php_execute_script's own zend_try absorbs
     * every bailout raised inside the worker script, so this SETJMP fires only
     * for one raised outside it. CG(unclean_shutdown) catches both. */
    ephpm_bailout_reset();

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        /* Worker entrypoints are routinely composer bin proxies / CLI-style
         * scripts with a "#!/usr/bin/env php" shebang. The CLI SAPI skips
         * that line; without this flag the embed compiler treats it as output
         * BEFORE the first statement — a fatal compile error for any script
         * opening with a namespace/declare statement (composer proxies do). */
        CG(skip_shebang) = 1;

#ifdef EPHPM_NATIVE_EXEC_TIMER
        /* Disarm PHP's per-thread timer for the boot-once section: framework
         * bootstrap is not a request and must not be time-limited. take_request()
         * arms a fresh limit (g_configured_max_exec_secs) for each real request. */
        zend_unset_timeout();
#endif

        zend_file_handle file_handle;
        zend_stream_init_filename(&file_handle, script);
        php_execute_script(&file_handle);

        /* exit()/die() throws an unwind-exit exception rather than bailing;
         * treat it as a clean loop end (the framework asked to stop). */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
        }

        /* Branch 1 — a bailout absorbed by php_execute_script's own zend_try.
         * Whatever the capture buffers hold is truncated, so we must NOT
         * synthesize a response from them: a bare zend_bailout() sets no error
         * type, so the hit_fatal check further down would see 200 and ship the
         * partial body as a success. Return -2 with the oneshot still parked —
         * the Rust supervisor 500s it, or aborts the body stream if the
         * headers already went out (worker_pool.rs / clear_in_flight_streams).
         *
         * Branch 2 — the script ended with a request still in flight
         * (exit()/die() mid-request, or a break out of the loop without
         * send_response). Deliver what the request actually produced — SAPI
         * status, headers emitted via header()/setcookie(), and the captured
         * echo output — instead of letting the parked oneshot die with the
         * thread (which would turn every wp_die()/admin-ajax exit into a bogus
         * 500). That is safe only because unwind-exit is clean stack
         * unwinding, not a bailout, so SAPI globals and the capture buffers
         * are intact — which is exactly why branch 1 has to come first. */
        if (ephpm_bailout_observed()) {
            result = -2;
        } else if (req_in_flight && g_worker_ops.send_response) {
            /* Unwind-exit skips the script's own ob_end_* calls, and worker
             * mode has no per-request RSHUTDOWN to flush buffers — content
             * still sitting in userland output buffers (WordPress wraps whole
             * pages in ob_start) would otherwise never reach the ub_write
             * capture and the synthesized response would have an empty body.
             * Flush-and-end every buffer under a bailout guard (ob handlers
             * run userland code). */
            zend_try {
                php_output_end_all();
            } zend_catch {
                /* A throwing ob handler forfeits its buffer; deliver what the
                 * capture has. */
            } zend_end_try();

            smart_str hbuf = {0};
            zend_llist_position pos;
            sapi_header_struct *h =
                zend_llist_get_first_ex(&SG(sapi_headers).headers, &pos);
            while (h) {
                smart_str_appendl(&hbuf, h->header, h->header_len);
                smart_str_appendc(&hbuf, '\n');
                h = zend_llist_get_next_ex(&SG(sapi_headers).headers, &pos);
            }
            smart_str_0(&hbuf);

            int status = SG(sapi_headers).http_response_code;
            if (status <= 0) {
                status = 200;
            }

            /* Fatal -> 500, mirroring the fpm path (:997-1002).
             *
             * A PHP 8 uncaught Throwable does NOT reach the SETJMP above:
             * zend_exception_error() prints "Fatal error: Uncaught ..." via
             * zend_error_va(... | E_DONT_BAIL ...) and php_execute_script's
             * own zend_try swallows the bailout, so the script simply
             * "returns" with a request still in flight and lands here. Without
             * this check the synthesized response carries the DEFAULT 200 and
             * ships the fatal-error text as a successful body — caches and
             * uptime monitors then treat a crashed request as healthy.
             *
             * Only override when the script did not set an explicit status
             * itself, so a deliberate exit() after http_response_code(201) (or
             * a framework's own 500) keeps what it chose. Same mask as fpm. */
            const int fatal_error_mask = E_ERROR | E_CORE_ERROR | E_COMPILE_ERROR
                                         | E_USER_ERROR | E_RECOVERABLE_ERROR | E_PARSE;
            int hit_fatal = (PG(last_error_type) & fatal_error_mask) != 0;
            if (hit_fatal && status == 200) {
                status = 500;
            }

            g_worker_ops.send_response(status,
                                       hbuf.s ? ZSTR_VAL(hbuf.s) : "",
                                       hbuf.s ? ZSTR_LEN(hbuf.s) : 0,
                                       output_buf ? output_buf : "",
                                       output_len);
            smart_str_free(&hbuf);
            output_len = 0;
            req_in_flight = 0;
            /* 2 = the request died on a fatal (response synthesized as 500);
             * 1 = the script deliberately exit()ed mid-request. Both deliver a
             * response and both recycle; the distinction is observability. */
            result = hit_fatal ? 2 : 1;
        }
    } else {
        /* zend_bailout() — a fatal unwound past the current iteration's
         * send_response. The Rust supervisor checks the parked oneshot and
         * 500s the in-flight request; the worker is recycled. */
        result = -2;
    }
    EG(bailout) = __orig_bailout;

    return result;
}

/* ===================================================================
 * KV store native PHP functions
 *
 * These register as ephpm_kv_get(), ephpm_kv_set(), etc. in PHP userland.
 * They call into Rust via the function pointer table set by
 * ephpm_set_kv_ops().
 * =================================================================== */

typedef struct {
    int  (*get)(const char *key);
    void (*get_result)(const char **ptr, size_t *len);
    int  (*set)(const char *key, const char *val, size_t val_len, long long ttl_ms);
    int  (*set_nx)(const char *key, const char *val, size_t val_len, long long ttl_ms);
    long (*del)(const char *key);
    int  (*exists)(const char *key);
    int  (*incr_by)(const char *key, long long delta, long long *result);
    int  (*expire)(const char *key, long long ttl_ms);
    long long (*pttl)(const char *key);
    int  (*flush_all)(void);
    /* Blocking versioned wait. Returns 0 = timeout, 1 = changed with a
     * value (in the get_result buffer), 2 = changed but key absent.
     * Must stay LAST-appended: the layout mirrors kv_bridge.rs. */
    int  (*wait)(const char *key, long long last_version, long long timeout_ms,
                 long long *new_version);
} EphpmKvOps;

static EphpmKvOps g_kv_ops = {0};

/* ── PHP_FUNCTION implementations ─────────────────────────────── */

PHP_FUNCTION(ephpm_kv_get)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.get) { RETURN_NULL(); }
    if (!g_kv_ops.get(key)) { RETURN_NULL(); }

    const char *ptr; size_t len;
    g_kv_ops.get_result(&ptr, &len);
    RETURN_STRINGL(ptr, len);
}

PHP_FUNCTION(ephpm_kv_set)
{
    char *key; size_t key_len;
    char *val; size_t val_len;
    zend_long ttl = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(key, key_len)
        Z_PARAM_STRING(val, val_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(ttl)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.set) { RETURN_FALSE; }
    long long ttl_ms = ttl > 0 ? ttl * 1000LL : 0;
    RETURN_BOOL(g_kv_ops.set(key, val, val_len, ttl_ms));
}

PHP_FUNCTION(ephpm_kv_setnx)
{
    char *key; size_t key_len;
    char *val; size_t val_len;
    zend_long ttl = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(key, key_len)
        Z_PARAM_STRING(val, val_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(ttl)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.set_nx) { RETURN_FALSE; }
    long long ttl_ms = ttl > 0 ? ttl * 1000LL : 0;
    /* Returns true if the value was inserted, false if a live entry was
     * already present at this key. The check-and-set is atomic under the
     * KV store's per-shard lock — this is the primitive the PHP-side lock
     * libraries (Cache::lock, Symfony LockFactory) build on. */
    RETURN_BOOL(g_kv_ops.set_nx(key, val, val_len, ttl_ms));
}

PHP_FUNCTION(ephpm_kv_del)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.del) { RETURN_LONG(0); }
    RETURN_LONG(g_kv_ops.del(key));
}

PHP_FUNCTION(ephpm_kv_exists)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.exists) { RETURN_FALSE; }
    RETURN_BOOL(g_kv_ops.exists(key));
}

PHP_FUNCTION(ephpm_kv_incr)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.incr_by) { RETURN_FALSE; }
    long long result = 0;
    if (!g_kv_ops.incr_by(key, 1, &result)) { RETURN_FALSE; }
    RETURN_LONG((zend_long)result);
}

PHP_FUNCTION(ephpm_kv_decr)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.incr_by) { RETURN_FALSE; }
    long long result = 0;
    if (!g_kv_ops.incr_by(key, -1, &result)) { RETURN_FALSE; }
    RETURN_LONG((zend_long)result);
}

PHP_FUNCTION(ephpm_kv_incr_by)
{
    char *key; size_t key_len;
    zend_long delta;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STRING(key, key_len)
        Z_PARAM_LONG(delta)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.incr_by) { RETURN_FALSE; }
    long long result = 0;
    if (!g_kv_ops.incr_by(key, (long long)delta, &result)) { RETURN_FALSE; }
    RETURN_LONG((zend_long)result);
}

PHP_FUNCTION(ephpm_kv_expire)
{
    char *key; size_t key_len;
    zend_long ttl;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STRING(key, key_len)
        Z_PARAM_LONG(ttl)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.expire) { RETURN_FALSE; }
    long long ttl_ms = ttl > 0 ? ttl * 1000LL : 0;
    RETURN_BOOL(g_kv_ops.expire(key, ttl_ms));
}

PHP_FUNCTION(ephpm_kv_ttl)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.pttl) { RETURN_LONG(-2); }
    long long pttl = g_kv_ops.pttl(key);
    if (pttl < 0) {
        /* -1 = no expiry, -2 = missing — pass through */
        RETURN_LONG((zend_long)pttl);
    }
    /* Convert milliseconds to seconds (round up so 1ms..999ms = 1s) */
    RETURN_LONG((zend_long)((pttl + 999) / 1000));
}

/* Redis-style PTTL: returns remaining TTL in milliseconds (or -1 / -2). */
PHP_FUNCTION(ephpm_kv_pttl)
{
    char *key; size_t key_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, key_len)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.pttl) { RETURN_LONG(-2); }
    RETURN_LONG((zend_long)g_kv_ops.pttl(key));
}

/* Redis-style FLUSHDB / FLUSHALL: removes every key from the effective
 * store (per-site store if one was bound for this request, otherwise the
 * global store). The Predis shim that backs the `redis-cache` WordPress
 * plugin calls this from its flushdb()/flushall() handlers. Returns true
 * on success, false if no KV store is registered. */
PHP_FUNCTION(ephpm_kv_flush_all)
{
    ZEND_PARSE_PARAMETERS_NONE();

    if (!g_kv_ops.flush_all) { RETURN_FALSE; }
    RETURN_BOOL(g_kv_ops.flush_all());
}

/* ephpm_kv_wait(string $key, int $last_version, int $timeout_ms): array|false
 *
 * Block until $key's watch version exceeds $last_version, or $timeout_ms
 * elapses. On change returns ['value' => string|null, 'version' => int]
 * ('value' is null when the key was deleted or has expired); on timeout
 * returns false (the SSE idiom: emit a keepalive and re-wait).
 *
 * The version is per-key and monotonic for the process lifetime; it only
 * advances for writes made AFTER the first wait on that key. Always seed
 * the protocol with $last_version = 0 — that first call registers the
 * watch and returns the current value+version immediately (race-free
 * snapshot), never trusting a version you didn't get from a prior call.
 * Negative $last_version/$timeout_ms are treated as 0; $timeout_ms = 0 is
 * a non-blocking poll.
 *
 * Blocking is intentional and safe: in worker mode this parks the
 * dedicated worker OS thread (the intended SSE pattern — replaces
 * poll+usleep loops with zero idle CPU and sub-ms wakeup); in fpm mode it
 * parks a spawn_blocking thread, so keep $timeout_ms well below
 * [server.timeouts] request there. Watches observe string keys only
 * (set/setnx/del/incr/decr/incr_by/append/expiry-reap/flush bump the
 * version; hset/hdel and TTL-only changes do not). */
PHP_FUNCTION(ephpm_kv_wait)
{
    char *key; size_t key_len;
    zend_long last_version;
    zend_long timeout_ms;
    ZEND_PARSE_PARAMETERS_START(3, 3)
        Z_PARAM_STRING(key, key_len)
        Z_PARAM_LONG(last_version)
        Z_PARAM_LONG(timeout_ms)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_kv_ops.wait) { RETURN_FALSE; }

    long long new_version = 0;
    int rc = g_kv_ops.wait(key, (long long)last_version, (long long)timeout_ms,
                           &new_version);
    if (rc == 0) { RETURN_FALSE; } /* timeout */

    array_init(return_value);
    add_assoc_long(return_value, "version", (zend_long)new_version);
    if (rc == 1) {
        const char *ptr; size_t len;
        g_kv_ops.get_result(&ptr, &len);
        add_assoc_stringl(return_value, "value", ptr, len);
    } else {
        /* rc == 2: version advanced but the key is absent (deleted). */
        add_assoc_null(return_value, "value");
    }
}

/* ── Argument info for reflection (arginfo) ──────────────────── */

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_get, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_set, 0, 0, 2)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, value)
    ZEND_ARG_INFO(0, ttl)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_setnx, 0, 0, 2)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, value)
    ZEND_ARG_INFO(0, ttl)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_del, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_exists, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_incr, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_decr, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_incr_by, 0, 0, 2)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, delta)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_expire, 0, 0, 2)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, ttl)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_ttl, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_pttl, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_flush_all, 0, 0, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_wait, 0, 0, 3)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, last_version)
    ZEND_ARG_INFO(0, timeout_ms)
ZEND_END_ARG_INFO()

/* ===================================================================
 * Embedded database native PHP functions
 *
 * ephpm_db_query() / ephpm_db_execute() / ephpm_db_run() execute SQL
 * through a per-thread litewire Session on the Rust side (db_bridge.rs) —
 * the SAME backend the MySQL wire frontend serves, so MySQL-dialect SQL,
 * SHOW/DESCRIBE emulation, SET-NAMES no-ops, and BEGIN/COMMIT/ROLLBACK all
 * behave exactly as they do over the wire, without a TCP round trip.
 *
 * Executing functions:
 *   ephpm_db_query(sql, params)   -> rows
 *   ephpm_db_execute(sql, params) -> OK metadata
 *   ephpm_db_run(sql, params)     -> both, plus has_rowset (issue #263)
 *
 * Introspection functions — none of them throw, none of them run SQL, and
 * none of them disturb what the executing functions staged:
 *   ephpm_db_columns()            -> last statement's column metadata (#262)
 *   ephpm_db_in_transaction()     -> this thread's transaction state (#260)
 *   ephpm_db_available()          -> can a statement reach a database? (#259)
 *   ephpm_db_errno() / ephpm_db_error() -> last error, classified (#259)
 *
 * All state lives in Rust thread-locals; g_db_ops is written once at
 * startup (ephpm_set_db_ops) before any PHP thread runs, then read-only —
 * same ZTS discipline as g_kv_ops.
 * =================================================================== */

typedef struct {
    /* Reset the staged parameter list for this thread. */
    void (*params_begin)(void);
    void (*param_null)(void);
    void (*param_int)(long long v);
    void (*param_float)(double v);
    /* Bytes param: valid UTF-8 binds as TEXT, anything else as BLOB
     * (mirrors the MySQL wire frontend's parameter decoding). */
    void (*param_bytes)(const char *p, size_t len);
    /* Execute sql with the staged params through the per-thread Session.
     * Returns 1 = result set staged, 2 = OK staged, -1 = error staged,
     * -2 = no backend registered ([db.sqlite] not active),
     * -3 = this thread's bridge thread-locals are already destroyed because
     *      the thread is exiting, so nothing was executed (issue #269 — a
     *      register_shutdown_function callback or destructor running inside
     *      php_request_shutdown() on a retiring worker thread). */
    int  (*run)(const char *sql, size_t sql_len);
    /* Result-set accessors — valid after run() returned 1, on the same
     * thread, until the next run()/finish(). */
    size_t (*row_count)(void);
    size_t (*col_count)(void);
    void (*col_name)(size_t col, const char **p, size_t *len);
    /* Cell accessor: *type = 0 null, 1 int (*ival), 2 float (*fval),
     * 3 text / 4 blob (*p / *len). */
    void (*cell)(size_t row, size_t col, int *type, long long *ival,
                 double *fval, const char **p, size_t *len);
    /* OK accessor — valid after run() returned 2 (zeros after 1). */
    void (*ok_info)(unsigned long long *affected_rows,
                    unsigned long long *last_insert_id);
    /* Error accessor — valid after run() returned -1. *sqlstate points at
     * 5 bytes (NOT NUL-terminated). Survives finish(). */
    void (*error_info)(unsigned int *code, const char **sqlstate,
                       const char **msg, size_t *msg_len);
    /* Release the staged ROWS. The last-call record (had_rowset, col_*,
     * ok_info) and the staged error deliberately survive this — see
     * db_bridge.rs, "What survives finish()". */
    void (*finish)(void);
    /* Did the last statement produce a result set (1) or an OK (0)? The
     * authoritative has-rowset signal (issue #263) — read from the executed
     * statement, never inferred from the SQL text. Valid after finish(). */
    int (*had_rowset)(void);
    /* Column declared type ("INTEGER", "TEXT", ...); *p is NULL when the
     * column has none (an expression). Valid after finish(). */
    void (*col_decltype)(size_t col, const char **p, size_t *len);
    /* Is this thread's session inside an explicit transaction? (issue #260) */
    int (*in_transaction)(void);
    /* Would a statement issued right now reach a database? (issue #259)
     * Must stay LAST-appended: the layout mirrors db_bridge.rs. */
    int (*available)(void);
} EphpmDbOps;

/* Reserved error codes for infrastructure failures — the stable signal that
 * tells an adapter "the bridge could not run this", as opposed to "your SQL
 * was wrong" (issue #259).
 *
 * They live in MySQL's CLIENT-error range (2000-2999, CR_*), which a server
 * never emits: litewire's error_map can only produce a server-range code, so
 * a reserved code can never collide with a real SQL error. Every ephpm_db_*
 * exception carries a nonzero code — code 0 is not used by this surface.
 *
 * Keep in sync with db_bridge.rs (ERR_UNAVAILABLE / ERR_NO_SITE_CONTEXT /
 * ERR_CONNECT). EPHPM_DB_ERR_BAD_PARAM and EPHPM_DB_ERR_GONE are thrown here
 * only: the first is a parameter-binding refusal that never reaches the Rust
 * side, the second is the run() == -3 case, where the Rust side deliberately
 * stages NO error (its error cell is gone too — issue #269), so the exception
 * code is the only signal a catch block gets. ephpm_db_errno() reads 0 after
 * it, exactly as it does after a success. */
#define EPHPM_DB_ERR_UNAVAILABLE     2000
#define EPHPM_DB_ERR_NO_SITE_CONTEXT 2001
#define EPHPM_DB_ERR_CONNECT         2002
#define EPHPM_DB_ERR_BAD_PARAM       2003
#define EPHPM_DB_ERR_GONE            2004

/* The one message text for "no embedded database is active". Adapters in the
 * wild match on it, so it is API: change the wording only with the same care
 * as a function signature. */
#define EPHPM_DB_UNAVAILABLE_MSG \
    "ephpm_db: no embedded database is active (requires [db.sqlite])"

static EphpmDbOps g_db_ops = {0};

/* Stage the optional $params array. Only null, bool, int, float, and
 * string bind (matching what the MySQL binary protocol can carry); any
 * other type throws. Returns 0 on success, -1 if an exception was thrown. */
static int ephpm_db_push_params(HashTable *params)
{
    zval *entry;
    g_db_ops.params_begin();
    if (!params) { return 0; }
    ZEND_HASH_FOREACH_VAL(params, entry) {
        ZVAL_DEREF(entry);
        switch (Z_TYPE_P(entry)) {
            case IS_NULL:   g_db_ops.param_null(); break;
            case IS_TRUE:   g_db_ops.param_int(1); break;
            case IS_FALSE:  g_db_ops.param_int(0); break;
            case IS_LONG:   g_db_ops.param_int((long long)Z_LVAL_P(entry)); break;
            case IS_DOUBLE: g_db_ops.param_float(Z_DVAL_P(entry)); break;
            case IS_STRING: g_db_ops.param_bytes(Z_STRVAL_P(entry), Z_STRLEN_P(entry)); break;
            default:
                zend_throw_exception_ex(zend_ce_exception, EPHPM_DB_ERR_BAD_PARAM,
                    "ephpm_db: unsupported parameter type %s (only null, bool, "
                    "int, float, and string parameters bind)",
                    zend_zval_type_name(entry));
                return -1;
        }
    } ZEND_HASH_FOREACH_END();
    return 0;
}

/* Sentinel: an exception has been thrown; the PHP_FUNCTION must
 * RETURN_THROWS(). */
#define EPHPM_DB_THREW (-1000)

/* Run sql through the bridge, converting error outcomes into PHP
 * exceptions. Returns the bridge's run() code (1 = rows, 2 = OK) or
 * EPHPM_DB_THREW after throwing.
 *
 * Error shape follows PDO's convention: message "SQLSTATE[xxxxx]: <backend
 * message>", exception code = the mapped MySQL error code (e.g. 1062).
 * Infrastructure failures instead carry one of the reserved EPHPM_DB_ERR_*
 * codes above, with their message text unchanged from before those codes
 * existed — adapters that match on the wording keep working (issue #259). */
static int ephpm_db_run_or_throw(const char *sql, size_t sql_len, HashTable *params)
{
    if (!g_db_ops.run) {
        zend_throw_exception(zend_ce_exception, EPHPM_DB_UNAVAILABLE_MSG,
                             EPHPM_DB_ERR_UNAVAILABLE);
        return EPHPM_DB_THREW;
    }
    if (ephpm_db_push_params(params) != 0) {
        return EPHPM_DB_THREW;
    }
    int rc = g_db_ops.run(sql, sql_len);
    if (rc == -2) {
        zend_throw_exception(zend_ce_exception, EPHPM_DB_UNAVAILABLE_MSG,
                             EPHPM_DB_ERR_UNAVAILABLE);
        return EPHPM_DB_THREW;
    }
    if (rc == -3) {
        /* Issue #269. The thread is exiting and the Rust bridge's per-thread
         * state has already been destroyed, so no statement ran. Deliberately
         * worded differently from the -2 case: a database IS configured, this
         * thread just cannot reach it any more. Adapters that key on the -2
         * wording must not treat this as "no database configured". */
        zend_throw_exception(zend_ce_exception,
            "ephpm_db: the database bridge is no longer available on this thread "
            "(it is shutting down) — a shutdown function or destructor ran after "
            "per-thread database state was released; no statement was executed",
            EPHPM_DB_ERR_GONE);
        return EPHPM_DB_THREW;
    }
    if (rc < 0) {
        unsigned int code = 0;
        const char *sqlstate = NULL;
        const char *msg = NULL;
        size_t msg_len = 0;
        g_db_ops.error_info(&code, &sqlstate, &msg, &msg_len);
        zend_throw_exception_ex(zend_ce_exception, (zend_long)code,
            "SQLSTATE[%.5s]: %.*s",
            sqlstate ? sqlstate : "HY000",
            (int)msg_len, msg ? msg : "");
        /* Releases the rows only — the error triple survives on purpose, so
         * ephpm_db_errno() / ephpm_db_error() can still describe this failure
         * after the exception has been caught (issue #259). */
        g_db_ops.finish();
        return EPHPM_DB_THREW;
    }
    return rc;
}

/* Append the staged rows to `out` as a list of associative arrays keyed by
 * column name. Must be called BEFORE finish() (the rows do not survive it).
 * Shared by ephpm_db_query() and ephpm_db_run() so the two can never drift. */
static void ephpm_db_append_rows(zval *out)
{
    size_t nrows = g_db_ops.row_count();
    size_t ncols = g_db_ops.col_count();
    for (size_t r = 0; r < nrows; r++) {
        zval rowz;
        array_init(&rowz);
        for (size_t c = 0; c < ncols; c++) {
            const char *name = NULL; size_t name_len = 0;
            g_db_ops.col_name(c, &name, &name_len);

            int type = 0; long long ival = 0; double fval = 0;
            const char *bytes = NULL; size_t bytes_len = 0;
            g_db_ops.cell(r, c, &type, &ival, &fval, &bytes, &bytes_len);

            zval cellz;
            switch (type) {
                case 1:  ZVAL_LONG(&cellz, (zend_long)ival); break;
                case 2:  ZVAL_DOUBLE(&cellz, fval); break;
                case 3:  /* text */
                case 4:  /* blob — PHP strings are binary-safe */
                         ZVAL_STRINGL(&cellz, bytes, bytes_len); break;
                default: ZVAL_NULL(&cellz); break;
            }
            add_assoc_zval_ex(&rowz, name ? name : "", name_len, &cellz);
        }
        add_next_index_zval(out, &rowz);
    }
}

/* Initialize `out` to the last statement's column metadata: a list of
 * ['name' => string, 'type' => ?string]. Empty list when the statement
 * produced no result set.
 *
 * Unlike the rows, this is valid both before and after finish() — which is
 * the whole point of issue #262: a zero-row SELECT has no rows to carry its
 * column names, but the names are still known. */
static void ephpm_db_init_columns(zval *out)
{
    array_init(out);
    if (!g_db_ops.col_count) { return; }
    size_t ncols = g_db_ops.col_count();
    for (size_t c = 0; c < ncols; c++) {
        const char *name = NULL; size_t name_len = 0;
        g_db_ops.col_name(c, &name, &name_len);

        const char *decl = NULL; size_t decl_len = 0;
        if (g_db_ops.col_decltype) { g_db_ops.col_decltype(c, &decl, &decl_len); }

        zval colz;
        array_init(&colz);
        add_assoc_stringl(&colz, "name", name ? name : "", name_len);
        if (decl) {
            add_assoc_stringl(&colz, "type", decl, decl_len);
        } else {
            add_assoc_null(&colz, "type");
        }
        add_next_index_zval(out, &colz);
    }
}

/* ephpm_db_query(string $sql, array $params = []): array
 *
 * Execute SQL and return the rows as a list of associative arrays keyed
 * by column name (a duplicate column name — SELECT a, a — keeps the last
 * value, like mysqli_fetch_assoc()). Integer/float columns come back as
 * PHP int/float, NULL as null, text/blob as string. A statement with no
 * result set (e.g. SET NAMES routed through the query function) returns
 * an empty array. Errors throw Exception (see ephpm_db_run_or_throw). */
PHP_FUNCTION(ephpm_db_query)
{
    char *sql; size_t sql_len;
    HashTable *params = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_STRING(sql, sql_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_ARRAY_HT(params)
    ZEND_PARSE_PARAMETERS_END();

    int rc = ephpm_db_run_or_throw(sql, sql_len, params);
    if (rc == EPHPM_DB_THREW) { RETURN_THROWS(); }

    array_init(return_value);
    if (rc != 1) {
        /* OK result (no rowset) — empty array, not an error. */
        g_db_ops.finish();
        return;
    }

    ephpm_db_append_rows(return_value);
    g_db_ops.finish();
}

/* ephpm_db_execute(string $sql, array $params = [])
 *     : array{affected_rows: int, last_insert_id: int}
 *
 * Execute SQL and return the OK metadata. Transactions flow through as
 * SQL — BEGIN / COMMIT / ROLLBACK here behave exactly as on the wire
 * path (the per-thread Session tracks the transaction state). A
 * transaction still open when the request ends is rolled back by the
 * server with a warning (db_bridge.rs on_request_end) — it never leaks
 * into the next request served by this thread. A SELECT routed through
 * execute returns zeros rather than throwing. Errors throw Exception
 * (see ephpm_db_run_or_throw). */
PHP_FUNCTION(ephpm_db_execute)
{
    char *sql; size_t sql_len;
    HashTable *params = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_STRING(sql, sql_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_ARRAY_HT(params)
    ZEND_PARSE_PARAMETERS_END();

    int rc = ephpm_db_run_or_throw(sql, sql_len, params);
    if (rc == EPHPM_DB_THREW) { RETURN_THROWS(); }

    unsigned long long affected = 0, last_id = 0;
    g_db_ops.ok_info(&affected, &last_id);
    g_db_ops.finish();

    array_init(return_value);
    add_assoc_long(return_value, "affected_rows", (zend_long)affected);
    add_assoc_long(return_value, "last_insert_id", (zend_long)last_id);
}

/* ephpm_db_run(string $sql, array $params = [])
 *     : array{has_rowset: bool, rows: array, columns: array,
 *             affected_rows: int, last_insert_id: int}
 *
 * The unified entry point (issue #263). Runs the statement and reports what
 * it actually did, so an adapter implementing a single query() API —
 * mysqli::query, wpdb::query, Laravel statement/affectingStatement, DBAL —
 * never has to classify the SQL by its first significant keyword to choose
 * between ephpm_db_query() and ephpm_db_execute(). WITH ... INSERT hybrids,
 * INSERT ... RETURNING, CALL, and anything a future dialect adds classify
 * correctly for free, because the answer comes from the executed statement.
 *
 * 'has_rowset' is the discriminator. 'rows' is always an array (empty, never
 * null, when has_rowset is false) so a caller can foreach unconditionally.
 * 'columns' carries the column metadata even for a zero-row result set
 * (issue #262). 'affected_rows'/'last_insert_id' are zero for a result set,
 * the same contract ephpm_db_execute() has always had on a SELECT.
 *
 * Errors throw exactly as ephpm_db_query()/ephpm_db_execute() do. */
PHP_FUNCTION(ephpm_db_run)
{
    char *sql; size_t sql_len;
    HashTable *params = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_STRING(sql, sql_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_ARRAY_HT(params)
    ZEND_PARSE_PARAMETERS_END();

    int rc = ephpm_db_run_or_throw(sql, sql_len, params);
    if (rc == EPHPM_DB_THREW) { RETURN_THROWS(); }

    /* Rows first: they are the only part that does not survive finish(). */
    zval rowsz;
    array_init(&rowsz);
    if (rc == 1) { ephpm_db_append_rows(&rowsz); }

    zval colsz;
    ephpm_db_init_columns(&colsz);

    unsigned long long affected = 0, last_id = 0;
    g_db_ops.ok_info(&affected, &last_id);
    /* Prefer the bridge's own answer; fall back to the run() code for an ops
     * table that predates had_rowset (it cannot happen in a build where C and
     * Rust ship together, but the table is read through a pointer). */
    int has_rowset = g_db_ops.had_rowset ? g_db_ops.had_rowset() : (rc == 1);
    g_db_ops.finish();

    array_init(return_value);
    add_assoc_bool(return_value, "has_rowset", has_rowset);
    add_assoc_zval(return_value, "rows", &rowsz);
    add_assoc_zval(return_value, "columns", &colsz);
    add_assoc_long(return_value, "affected_rows", (zend_long)affected);
    add_assoc_long(return_value, "last_insert_id", (zend_long)last_id);
}

/* ephpm_db_columns(): array
 *
 * Column metadata of the LAST ephpm_db_* statement on this thread, as a list
 * of ['name' => string, 'type' => ?string]. Empty list when that statement
 * produced no result set, or when nothing has run yet.
 *
 * Exists because a zero-row result cannot carry its own column names
 * (issue #262): ephpm_db_query() returns [] and the names go with it. This
 * reads them from the metadata the bridge keeps after the rows are released,
 * so wpdb::get_col_info(), mysqli_result::fetch_fields() and DBAL's
 * columnCount() work after a SELECT that matched nothing.
 *
 * 'type' is the column's DECLARED schema type. It is null both for a column
 * with no declared type and for an expression (SELECT a + 1) — SQLite makes
 * no distinction between those two, so neither can this.
 *
 * Never throws: with no embedded database it returns an empty list. Reading
 * metadata does not disturb it — this and the other introspection functions
 * leave the last-call record and errno alone. */
PHP_FUNCTION(ephpm_db_columns)
{
    ZEND_PARSE_PARAMETERS_NONE();
    ephpm_db_init_columns(return_value);
}

/* ephpm_db_in_transaction(): bool
 *
 * Whether THIS THREAD's session is inside an explicit transaction
 * (issue #260). Reads the session's own flag — the same state the MySQL wire
 * frontend reports as SERVER_STATUS_IN_TRANS — rather than guessing from the
 * statements that have gone past.
 *
 * Lets a transaction() helper stop firing ROLLBACK blind after a failure and
 * swallowing the resulting error: after a failed BEGIN, or after the
 * request-end rollback safety net has already run, there is nothing to roll
 * back and this says so.
 *
 * False when the thread has no session yet or no embedded database is active
 * — in both cases no transaction can be open, so it is not a guess. Never
 * throws. */
PHP_FUNCTION(ephpm_db_in_transaction)
{
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(g_db_ops.in_transaction ? g_db_ops.in_transaction() : 0);
}

/* ephpm_db_available(): bool
 *
 * Whether an ephpm_db_* statement issued right now would reach a database
 * (issue #259) — a backend is registered AND, in per-site mode, this request
 * has a tenant identity.
 *
 * The pre-flight form of the reserved-code contract: it rules out
 * EPHPM_DB_ERR_UNAVAILABLE and EPHPM_DB_ERR_NO_SITE_CONTEXT without running a
 * probe query and catching an exception. It does NOT open the database, so a
 * true here can still be followed by an EPHPM_DB_ERR_CONNECT failure if the
 * storage underneath is broken.
 *
 * Distinct from function_exists('ephpm_db_query'), which only tells you that
 * you are running inside ePHPm. Never throws. */
PHP_FUNCTION(ephpm_db_available)
{
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(g_db_ops.available ? g_db_ops.available() : 0);
}

/* ephpm_db_errno(): int
 *
 * Error code of the last ephpm_db_* statement on this thread, or 0 if it
 * succeeded (or none has run). Survives the exception being caught.
 *
 * A SQL failure reports the mapped MySQL SERVER error code (1062, 1064,
 * 1105, ...). An infrastructure failure reports one of the reserved
 * EPHPM_DB_ERR_* codes from the CLIENT range (2000-2999), which a server
 * never emits — that separation is the stable "bridge problem vs. your SQL"
 * signal, replacing message-text matching.
 *
 * Cleared by the next statement on this thread and at request end, so 0
 * means "the last statement succeeded", not "no error has ever occurred".
 *
 * Reports on the last statement that REACHED the bridge. A parameter-binding
 * refusal (EPHPM_DB_ERR_BAD_PARAM) is thrown by ephpm_db_push_params before
 * anything is executed, so it leaves errno untouched — the thrown exception's
 * code is the signal for that case. Never throws. */
PHP_FUNCTION(ephpm_db_errno)
{
    ZEND_PARSE_PARAMETERS_NONE();
    if (!g_db_ops.error_info) { RETURN_LONG(EPHPM_DB_ERR_UNAVAILABLE); }

    unsigned int code = 0;
    const char *sqlstate = NULL, *msg = NULL;
    size_t msg_len = 0;
    g_db_ops.error_info(&code, &sqlstate, &msg, &msg_len);
    RETURN_LONG((zend_long)code);
}

/* ephpm_db_error(): ?array{code: int, sqlstate: string, message: string}
 *
 * The last error in parts, or null when the last statement succeeded. The
 * structured companion to ephpm_db_errno(), for mysqli_error() /
 * mysqli_sqlstate() / PDO::errorInfo()-shaped adapter APIs.
 *
 * 'message' is the backend message on its own — the "SQLSTATE[xxxxx]: "
 * prefix belongs to the exception's composed message, not to this. Never
 * throws. */
PHP_FUNCTION(ephpm_db_error)
{
    ZEND_PARSE_PARAMETERS_NONE();
    if (!g_db_ops.error_info) {
        array_init(return_value);
        add_assoc_long(return_value, "code", EPHPM_DB_ERR_UNAVAILABLE);
        add_assoc_string(return_value, "sqlstate", "HY000");
        add_assoc_string(return_value, "message", EPHPM_DB_UNAVAILABLE_MSG);
        return;
    }

    unsigned int code = 0;
    const char *sqlstate = NULL, *msg = NULL;
    size_t msg_len = 0;
    g_db_ops.error_info(&code, &sqlstate, &msg, &msg_len);
    /* The Rust shim signals "nothing staged" with a NULL sqlstate pointer;
     * every staged error carries a nonzero code and five sqlstate bytes. */
    if (sqlstate == NULL) { RETURN_NULL(); }

    array_init(return_value);
    add_assoc_long(return_value, "code", (zend_long)code);
    add_assoc_stringl(return_value, "sqlstate", sqlstate ? sqlstate : "HY000", 5);
    add_assoc_stringl(return_value, "message", msg ? msg : "", msg_len);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_db_query, 0, 0, 1)
    ZEND_ARG_INFO(0, sql)
    ZEND_ARG_INFO(0, params)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_db_execute, 0, 0, 1)
    ZEND_ARG_INFO(0, sql)
    ZEND_ARG_INFO(0, params)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_db_run, 0, 0, 1)
    ZEND_ARG_INFO(0, sql)
    ZEND_ARG_INFO(0, params)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_db_noargs, 0, 0, 0)
ZEND_END_ARG_INFO()

/* ===================================================================
 * WebSocket native PHP functions
 *
 * ePHPm owns every WebSocket socket in Rust; PHP never holds one. These
 * functions push into a Rust-side registry (ws_bridge.rs -> ephpm-ws),
 * which hands the frame to the target connection's session task.
 *
 * They are callable from ANY PHP execution, not just a websocket
 * entrypoint event — an ordinary HTTP request handler pushing to a live
 * socket is the point of the design.
 *
 * TWO FORMS. Each operation comes in an implicit and an explicit form:
 *
 *   ephpm_ws_send($payload)                  -> the connection that fired
 *                                               THIS event
 *   ephpm_ws_connection_send($id, $payload)  -> any connection in this site
 *
 * The implicit form reads a per-thread "current connection" the router
 * installs for the duration of a websocket event dispatch (the same
 * pattern as db_bridge's per-thread site session, and cleared the same
 * way — every PHP execution sets it, to the event's connection or to
 * nothing, so it can never leak into the next request on a reused
 * thread). Called from an ordinary HTTP request there is no current
 * connection, and the implicit form THROWS rather than silently doing
 * nothing.
 *
 * SITE SCOPE. Every one of these is scoped to the CALLING request's
 * virtual host, read from a Rust thread-local the router installs before
 * PHP runs (exactly like ephpm_db_*). The scope is never taken from an
 * argument: a connection id names a connection only within the site that
 * created it, so site A cannot reach site B's sockets or channels even
 * holding a valid id. A request whose Host matched no vhost has no scope
 * and every call below throws.
 *
 * All state lives in Rust; g_ws_ops is written once at startup
 * (ephpm_set_ws_ops) before any PHP thread runs, then read-only — same
 * ZTS discipline as g_kv_ops and g_db_ops.
 * =================================================================== */

/* Bridge status codes. Non-negative is a result (0/1 for the boolean
 * operations, a receiver count for broadcast); negative is a condition
 * that must reach the script as an exception rather than as `false`. */
#define EPHPM_WS_NO_CONN     (-1)
#define EPHPM_WS_NO_SITE     (-2)
#define EPHPM_WS_NO_REGISTRY (-3)

typedef struct {
    /* Queue one frame. `conn_id == NULL` means "the connection that
     * fired the current event"; otherwise the id names a connection in
     * the calling request's site. Returns 1 on success, 0 when the
     * connection is unknown to this site, gone, or its bounded outbound
     * queue is full (which also closes that connection with 1013), or a
     * negative EPHPM_WS_* status. */
    long (*send)(const char *conn_id, size_t conn_id_len,
                 const char *payload, size_t payload_len, int binary);
    /* Subscribe / unsubscribe a connection to a site-scoped channel.
     * Same `conn_id == NULL` convention and same return contract. */
    long (*subscribe)(const char *conn_id, size_t conn_id_len,
                      const char *channel, size_t channel_len);
    long (*unsubscribe)(const char *conn_id, size_t conn_id_len,
                        const char *channel, size_t channel_len);
    /* Queue one frame to every member of a site-scoped channel. Returns
     * the number of connections it was queued to, or a negative
     * EPHPM_WS_* status. Needs no current connection. */
    long (*broadcast)(const char *channel, size_t channel_len,
                      const char *payload, size_t payload_len, int binary);
    /* Ask a connection's session task to close with `code`. Same
     * `conn_id == NULL` convention and same return contract.
     * Must stay LAST-appended: the layout mirrors ws_bridge.rs. */
    long (*close)(const char *conn_id, size_t conn_id_len, int code);
} EphpmWsOps;

static EphpmWsOps g_ws_ops = {0};

/* Turn a negative bridge status into a PHP exception. Returns 1 if it
 * threw (the caller must RETURN_THROWS()), 0 otherwise.
 *
 * These are all misuse or misconfiguration, never an ordinary outcome —
 * an unreachable connection is `false`, but "there is no websocket
 * subsystem" / "this request has no tenant" / "there is no current
 * connection here" are bugs the script must not be able to ignore. Same
 * reasoning as ephpm_db_*'s exception on a missing backend. */
static int ephpm_ws_threw(long rc)
{
    switch (rc) {
        case EPHPM_WS_NO_REGISTRY:
            zend_throw_exception(zend_ce_exception,
                "ephpm_ws: native websockets are not enabled on this server "
                "(set [server.websocket] enabled = true)", 0);
            return 1;
        case EPHPM_WS_NO_SITE:
            zend_throw_exception(zend_ce_exception,
                "ephpm_ws: no websocket context for this request — its Host "
                "matched no virtual host, so it has no websocket capability", 0);
            return 1;
        case EPHPM_WS_NO_CONN:
            zend_throw_exception(zend_ce_exception,
                "ephpm_ws: no current websocket connection. The implicit form "
                "is only valid inside a websocket event; from an ordinary HTTP "
                "request use the ephpm_ws_connection_*() form with an explicit "
                "connection id", 0);
            return 1;
        default:
            return 0;
    }
}

/* Guard for a never-installed ops table: identical to the registry being
 * absent, so it produces the same exception. */
#define EPHPM_WS_REQUIRE(fn)                                   \
    do {                                                       \
        if (!(fn)) {                                           \
            (void)ephpm_ws_threw(EPHPM_WS_NO_REGISTRY);        \
            RETURN_THROWS();                                   \
        }                                                      \
    } while (0)

/* ephpm_ws_send(string $payload, bool $binary = false): bool
 *
 * Push one frame to the connection that fired the current event.
 * Returns false if that connection has gone or its outbound queue is
 * full — the latter sheds it with WebSocket status 1013 rather than
 * buffering. Throws outside a websocket event. */
PHP_FUNCTION(ephpm_ws_send)
{
    char *payload; size_t payload_len;
    bool binary = 0;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_STRING(payload, payload_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_BOOL(binary)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.send);
    long rc = g_ws_ops.send(NULL, 0, payload, payload_len, binary ? 1 : 0);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_connection_send(string $connection_id, string $payload,
 *                          bool $binary = false): bool
 *
 * The explicit form: push to any connection in the calling request's
 * site. This is what an ordinary HTTP handler uses after looking the id
 * up (see the websockets guide's HTTP-pushes-to-socket pattern). */
PHP_FUNCTION(ephpm_ws_connection_send)
{
    char *conn_id; size_t conn_id_len;
    char *payload; size_t payload_len;
    bool binary = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(conn_id, conn_id_len)
        Z_PARAM_STRING(payload, payload_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_BOOL(binary)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.send);
    long rc = g_ws_ops.send(conn_id, conn_id_len, payload, payload_len,
                            binary ? 1 : 0);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_subscribe(string $channel): bool
 *
 * Join the current event's connection to a channel. Channels are
 * site-scoped: `chat` on one vhost and `chat` on another are different
 * channels. Throws outside a websocket event. */
PHP_FUNCTION(ephpm_ws_subscribe)
{
    char *channel; size_t channel_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(channel, channel_len)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.subscribe);
    long rc = g_ws_ops.subscribe(NULL, 0, channel, channel_len);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_connection_subscribe(string $connection_id,
 *                               string $channel): bool */
PHP_FUNCTION(ephpm_ws_connection_subscribe)
{
    char *conn_id; size_t conn_id_len;
    char *channel; size_t channel_len;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STRING(conn_id, conn_id_len)
        Z_PARAM_STRING(channel, channel_len)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.subscribe);
    long rc = g_ws_ops.subscribe(conn_id, conn_id_len, channel, channel_len);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_unsubscribe(string $channel): bool */
PHP_FUNCTION(ephpm_ws_unsubscribe)
{
    char *channel; size_t channel_len;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(channel, channel_len)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.unsubscribe);
    long rc = g_ws_ops.unsubscribe(NULL, 0, channel, channel_len);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_connection_unsubscribe(string $connection_id,
 *                                 string $channel): bool */
PHP_FUNCTION(ephpm_ws_connection_unsubscribe)
{
    char *conn_id; size_t conn_id_len;
    char *channel; size_t channel_len;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STRING(conn_id, conn_id_len)
        Z_PARAM_STRING(channel, channel_len)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.unsubscribe);
    long rc = g_ws_ops.unsubscribe(conn_id, conn_id_len, channel, channel_len);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_broadcast(string $channel, string $payload,
 *                    bool $binary = false): int
 *
 * Push one frame to every member of a channel in the calling request's
 * site. Returns the number of connections the frame was queued to;
 * subscribers whose queue was full are not counted (and are shed).
 *
 * Needs no current connection, so it works identically inside a
 * websocket event and inside an ordinary HTTP request. */
PHP_FUNCTION(ephpm_ws_broadcast)
{
    char *channel; size_t channel_len;
    char *payload; size_t payload_len;
    bool binary = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(channel, channel_len)
        Z_PARAM_STRING(payload, payload_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_BOOL(binary)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.broadcast);
    long rc = g_ws_ops.broadcast(channel, channel_len, payload, payload_len,
                                 binary ? 1 : 0);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_LONG((zend_long)rc);
}

/* ephpm_ws_close(int $code = 1000): bool
 *
 * Ask the current event's connection to close. Asynchronous: the socket
 * is closed by the task that owns it, not inside this call, so any frame
 * already queued ahead of the close is still delivered. Throws outside a
 * websocket event. */
PHP_FUNCTION(ephpm_ws_close)
{
    zend_long code = 1000;
    ZEND_PARSE_PARAMETERS_START(0, 1)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(code)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.close);
    long rc = g_ws_ops.close(NULL, 0, (int)code);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

/* ephpm_ws_connection_close(string $connection_id,
 *                           int $code = 1000): bool */
PHP_FUNCTION(ephpm_ws_connection_close)
{
    char *conn_id; size_t conn_id_len;
    zend_long code = 1000;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_STRING(conn_id, conn_id_len)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(code)
    ZEND_PARSE_PARAMETERS_END();

    EPHPM_WS_REQUIRE(g_ws_ops.close);
    long rc = g_ws_ops.close(conn_id, conn_id_len, (int)code);
    if (ephpm_ws_threw(rc)) { RETURN_THROWS(); }
    RETURN_BOOL(rc);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_send, 0, 0, 1)
    ZEND_ARG_INFO(0, payload)
    ZEND_ARG_INFO(0, binary)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_connection_send, 0, 0, 2)
    ZEND_ARG_INFO(0, connection_id)
    ZEND_ARG_INFO(0, payload)
    ZEND_ARG_INFO(0, binary)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_subscribe, 0, 0, 1)
    ZEND_ARG_INFO(0, channel)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_connection_subscribe, 0, 0, 2)
    ZEND_ARG_INFO(0, connection_id)
    ZEND_ARG_INFO(0, channel)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_unsubscribe, 0, 0, 1)
    ZEND_ARG_INFO(0, channel)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_connection_unsubscribe, 0, 0, 2)
    ZEND_ARG_INFO(0, connection_id)
    ZEND_ARG_INFO(0, channel)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_broadcast, 0, 0, 2)
    ZEND_ARG_INFO(0, channel)
    ZEND_ARG_INFO(0, payload)
    ZEND_ARG_INFO(0, binary)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_close, 0, 0, 0)
    ZEND_ARG_INFO(0, code)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_ws_connection_close, 0, 0, 1)
    ZEND_ARG_INFO(0, connection_id)
    ZEND_ARG_INFO(0, code)
ZEND_END_ARG_INFO()

/* ===================================================================
 * CLI process title — cli_set_process_title() / cli_get_process_title()
 *
 * php-src registers these two functions from the cli SAPI (sapi/cli/
 * php_cli_process_title.c on top of ps_title.c, itself lifted from
 * PostgreSQL's ps_status.c). The embed SAPI never registers them, so
 * `ephpm php` — which deliberately reports PHP_SAPI === "cli" — fataled
 * on code that keys on the SAPI name instead of function_exists()
 * (PsySH calls cli_set_process_title(), so `artisan tinker <file>`
 * died with "Call to undefined function"). Issue #316.
 *
 * This is a condensed port of ps_title.c for the platforms ePHPm ships:
 *
 *   Linux (glibc)  — PS_USE_CLOBBER_ARGV: overwrite the original argv
 *                    (+ contiguous environ) area so /proc/self/cmdline
 *                    and `ps` show the title. php-cli captures argv in
 *                    main(); our main() is Rust, so a glibc `.init_array`
 *                    constructor (which glibc calls with main's argc/
 *                    argv/envp) captures it instead, and the environment
 *                    is deep-copied out of the clobber area there —
 *                    single-threaded, pre-main, exactly when php-cli's
 *                    save_ps_args() would run. Unlike save_ps_args() we
 *                    do NOT rewrite the argv[i] pointer slots or hand
 *                    out an argv copy: Rust (clap, std::env::args) still
 *                    reads the original argv, which stays intact until
 *                    the first cli_set_process_title() call. After that
 *                    call std::env::args() would read the title — the
 *                    same property php-cli itself has for anything
 *                    reading its clobbered argv — and the CLI parses its
 *                    arguments long before any PHP script runs.
 *   Windows        — PS_USE_WIN32: SetConsoleTitleW / GetConsoleTitleW
 *                    with UTF-8 <-> UTF-16 conversion (php-src converts
 *                    via php_win32_cp_any_to_w; the runtime codepage is
 *                    UTF-8 in these builds). Fails honestly (false /
 *                    "Windows error code: N") when no console exists.
 *   elsewhere      — PS_TITLE_NOT_AVAILABLE, reported exactly as php's
 *                    ps_title.c reports an unsupported OS ("Not
 *                    available on this OS"): warning + false/NULL, never
 *                    fake success. (macOS could adopt the clobber path +
 *                    _NSGetArgv fix later; it is left honest-unsupported
 *                    rather than shipped unverified.)
 *
 * Return-value contract matches php_cli_process_title.c byte for byte:
 * set → true, or E_WARNING "cli_set_process_title had an error: <why>"
 * + false; get → the stored title, or the same-shaped warning + null.
 * PHP 8.5 changed set() to reject an over-long title ("Too long")
 * instead of truncating; mirrored under PHP_VERSION_ID.
 * =================================================================== */

/* Status codes, mirroring sapi/cli/ps_title.h. */
#define EPHPM_PS_TITLE_SUCCESS         0
#define EPHPM_PS_TITLE_NOT_AVAILABLE   1
#define EPHPM_PS_TITLE_NOT_INITIALIZED 2
#define EPHPM_PS_TITLE_WINDOWS_ERROR   4
#define EPHPM_PS_TITLE_TOO_LONG        5

#if defined(__linux__) && defined(__GLIBC__)
#define EPHPM_PS_USE_CLOBBER_ARGV 1

extern char **environ;

static char *g_ps_buffer = NULL;      /* the original argv area */
static size_t g_ps_buffer_size = 0;   /* clobberable bytes at g_ps_buffer */
static size_t g_ps_buffer_cur_len = 0;
static int g_ps_args_saved = 0;

/*
 * Capture the process argv area before Rust's main() runs. glibc invokes
 * `.init_array` constructors with (argc, argv, envp), which is the only way
 * to reach the REAL argv from a program whose main() is Rust. Gated to the
 * `ephpm php` invocation (argv[1] == "php"): only the CLI registers the
 * title functions, and the serve-mode process should not have its
 * environment relocated behind Rust's back. If the gate or the contiguity
 * check misses, the functions degrade to php's honest "Not initialized
 * correctly" failure instead of guessing at memory layout.
 *
 * The environ deep-copy-and-swap is verbatim save_ps_args() logic: the
 * kernel places environ strings directly after argv strings, so clobbering
 * a long title into the area would corrupt the environment unless it has
 * been moved first. Copying here is safe — pre-main is single-threaded, and
 * both glibc (getenv/setenv) and Rust std::env read the live `environ`
 * global rather than caching the startup block. The copies are process-
 * lifetime by design (php frees its own only for valgrind's benefit, from
 * a cleanup hook we don't have).
 */
__attribute__((constructor)) static void ephpm_ps_capture_argv(
    int argc, char **argv, char **envp)
{
    (void)envp;
    if (argc < 2 || !argv || !argv[0] || !argv[1] || strcmp(argv[1], "php") != 0) {
        return;
    }

    /* Contiguity check over argv, exactly as save_ps_args(). */
    char *end_of_area = NULL;
    for (int i = 0; i < argc; i++) {
        if (!argv[i] || (i != 0 && end_of_area + 1 != argv[i])) {
            return; /* unexpected layout — leave the title unsupported */
        }
        end_of_area = argv[i] + strlen(argv[i]);
    }

    /* Extend the clobber area over contiguous environ strings, then move
     * the environment out of it. */
    if (!environ) {
        return;
    }
    int n = 0;
    while (environ[n] != NULL) {
        n++;
    }
    char **new_environ = (char **)malloc(((size_t)n + 1) * sizeof(char *));
    if (!new_environ) {
        return;
    }
    for (int i = 0; i < n; i++) {
        if (end_of_area + 1 == environ[i]) {
            end_of_area = environ[i] + strlen(environ[i]);
        }
        new_environ[i] = strdup(environ[i]);
        if (!new_environ[i]) {
            for (int j = 0; j < i; j++) {
                free(new_environ[j]);
            }
            free(new_environ);
            return;
        }
    }
    new_environ[n] = NULL;
    environ = new_environ;

    g_ps_buffer = argv[0];
    g_ps_buffer_size = (size_t)(end_of_area - argv[0]);
    g_ps_buffer_cur_len = 0;
    g_ps_args_saved = 1;
}

#elif defined(_WIN32)
#define EPHPM_PS_USE_WIN32 1

/* UTF-8 rendering of the console title; MAX_PATH UTF-16 units can need up
 * to 3 bytes each. Mirrors ps_title.c's MAX_PATH-sized ps_buffer. */
static char g_ps_buffer[MAX_PATH * 3];
static size_t g_ps_buffer_cur_len = 0;
static char g_ps_windows_error[64];
#endif

/* is_ps_title_available(), condensed. */
static int ephpm_ps_title_available(void)
{
#if defined(EPHPM_PS_USE_CLOBBER_ARGV)
    return g_ps_args_saved ? EPHPM_PS_TITLE_SUCCESS : EPHPM_PS_TITLE_NOT_INITIALIZED;
#elif defined(EPHPM_PS_USE_WIN32)
    /* php-cli's save_ps_args() runs unconditionally in main(), so Windows
     * is always "initialized" there; the CLI-mode flag is our equivalent. */
    return g_cli_mode ? EPHPM_PS_TITLE_SUCCESS : EPHPM_PS_TITLE_NOT_INITIALIZED;
#else
    return EPHPM_PS_TITLE_NOT_AVAILABLE;
#endif
}

/* ps_title_errno(), same strings as sapi/cli/ps_title.c. */
static const char *ephpm_ps_title_errno(int rc)
{
    switch (rc) {
    case EPHPM_PS_TITLE_SUCCESS:
        return "Success";
    case EPHPM_PS_TITLE_NOT_AVAILABLE:
        return "Not available on this OS";
    case EPHPM_PS_TITLE_NOT_INITIALIZED:
        return "Not initialized correctly";
    case EPHPM_PS_TITLE_TOO_LONG:
        return "Too long";
#ifdef EPHPM_PS_USE_WIN32
    case EPHPM_PS_TITLE_WINDOWS_ERROR:
        snprintf(g_ps_windows_error, sizeof(g_ps_windows_error),
                 "Windows error code: %lu", GetLastError());
        return g_ps_windows_error;
#endif
    default:
        break;
    }
    return "Unknown error code";
}

/* set_ps_title(). PHP 8.5 rejects an over-long title; earlier truncate. */
static int ephpm_set_ps_title(const char *title, size_t title_len)
{
    int rc = ephpm_ps_title_available();
    if (rc != EPHPM_PS_TITLE_SUCCESS) {
        return rc;
    }

#if defined(EPHPM_PS_USE_CLOBBER_ARGV)
#if PHP_VERSION_ID >= 80500
    if (title_len >= g_ps_buffer_size) {
        return EPHPM_PS_TITLE_TOO_LONG;
    }
    /* Includes the final NUL: zend strings are NUL-terminated. */
    memcpy(g_ps_buffer, title, title_len + 1);
    g_ps_buffer_cur_len = title_len;
#else
    strncpy(g_ps_buffer, title, g_ps_buffer_size);
    g_ps_buffer[g_ps_buffer_size - 1] = '\0';
    g_ps_buffer_cur_len = strlen(g_ps_buffer);
    (void)title_len;
#endif
    /* Pad the rest of the area with NULs (PS_PADDING on Linux) so stale
     * argv/environ bytes never leak into /proc/self/cmdline. */
    if (g_ps_buffer_cur_len < g_ps_buffer_size) {
        memset(g_ps_buffer + g_ps_buffer_cur_len, '\0',
               g_ps_buffer_size - g_ps_buffer_cur_len);
    }
    return EPHPM_PS_TITLE_SUCCESS;
#elif defined(EPHPM_PS_USE_WIN32)
#if PHP_VERSION_ID >= 80500
    if (title_len >= sizeof(g_ps_buffer)) {
        return EPHPM_PS_TITLE_TOO_LONG;
    }
#else
    if (title_len >= sizeof(g_ps_buffer)) {
        title_len = sizeof(g_ps_buffer) - 1; /* pre-8.5: truncate like strncpy */
    }
#endif
    {
        wchar_t wide[MAX_PATH];
        char truncated[sizeof(g_ps_buffer)];
        memcpy(truncated, title, title_len);
        truncated[title_len] = '\0';
        int wlen = MultiByteToWideChar(CP_UTF8, 0, truncated, -1, wide, MAX_PATH);
        if (wlen == 0 || !SetConsoleTitleW(wide)) {
            return EPHPM_PS_TITLE_WINDOWS_ERROR;
        }
        memcpy(g_ps_buffer, truncated, title_len + 1);
        g_ps_buffer_cur_len = title_len;
    }
    return EPHPM_PS_TITLE_SUCCESS;
#else
    (void)title;
    (void)title_len;
    return EPHPM_PS_TITLE_NOT_AVAILABLE; /* unreachable: available() failed */
#endif
}

/* get_ps_title(). On Windows the console is re-queried, as php-src does. */
static int ephpm_get_ps_title(size_t *displen, const char **string)
{
    int rc = ephpm_ps_title_available();
    if (rc != EPHPM_PS_TITLE_SUCCESS) {
        return rc;
    }

#if defined(EPHPM_PS_USE_WIN32)
    {
        wchar_t wide[MAX_PATH];
        if (!GetConsoleTitleW(wide, MAX_PATH)) {
            return EPHPM_PS_TITLE_WINDOWS_ERROR;
        }
        int bytes = WideCharToMultiByte(
            CP_UTF8, 0, wide, -1, g_ps_buffer, (int)sizeof(g_ps_buffer), NULL, NULL);
        if (bytes == 0) {
            return EPHPM_PS_TITLE_WINDOWS_ERROR;
        }
        g_ps_buffer_cur_len = (size_t)bytes - 1; /* bytes includes the NUL */
    }
#endif
#if defined(EPHPM_PS_USE_CLOBBER_ARGV) || defined(EPHPM_PS_USE_WIN32)
    *displen = g_ps_buffer_cur_len;
    *string = g_ps_buffer;
    return EPHPM_PS_TITLE_SUCCESS;
#else
    (void)displen;
    (void)string;
    return EPHPM_PS_TITLE_NOT_AVAILABLE; /* unreachable: available() failed */
#endif
}

/* PHP_FUNCTION bodies: verbatim php_cli_process_title.c semantics. */
PHP_FUNCTION(cli_set_process_title)
{
    char *title = NULL;
    size_t title_len;
    int rc;

    if (zend_parse_parameters(ZEND_NUM_ARGS(), "s", &title, &title_len) == FAILURE) {
        RETURN_THROWS();
    }

    rc = ephpm_set_ps_title(title, title_len);
    if (rc == EPHPM_PS_TITLE_SUCCESS) {
        RETURN_TRUE;
    }

    php_error_docref(NULL, E_WARNING, "cli_set_process_title had an error: %s",
                     ephpm_ps_title_errno(rc));
    RETURN_FALSE;
}

PHP_FUNCTION(cli_get_process_title)
{
    size_t length = 0;
    const char *title = NULL;
    int rc;

    if (zend_parse_parameters_none() == FAILURE) {
        RETURN_THROWS();
    }

    rc = ephpm_get_ps_title(&length, &title);
    if (rc != EPHPM_PS_TITLE_SUCCESS) {
        php_error_docref(NULL, E_WARNING, "cli_get_process_title had an error: %s",
                         ephpm_ps_title_errno(rc));
        RETURN_NULL();
    }

    RETURN_STRINGL(title, length);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_cli_set_process_title, 0, 0, 1)
    ZEND_ARG_INFO(0, title)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_cli_get_process_title, 0, 0, 0)
ZEND_END_ARG_INFO()

/* ── PHP middleware lane ─────────────────────────────────────────
 *
 * ephpm_middleware_config(): ?string
 *
 * Returns the running mount's `config` table from `[[middleware]]`, serialised
 * to JSON, or NULL when the mount declares no `config` — and NULL from anywhere
 * that is not a middleware file, which is what makes
 * `json_decode(ephpm_middleware_config() ?? '{}', true)` safe to call
 * unconditionally.
 *
 * JSON rather than a PHP array on purpose: decoding here would mean linking
 * ext/json's C API out of a static embed build (PHP_JSON_API is dllimport on
 * Windows), and `json_decode()` is one idiomatic PHP call away. This is the
 * ONLY native function the lane adds — verdicts are expressed in stock PHP
 * (`exit`, `header()`, `$_SERVER`), not through a bespoke API.
 */
PHP_FUNCTION(ephpm_middleware_config)
{
    ZEND_PARSE_PARAMETERS_NONE();

    if (req_middleware_active < 0
        || (size_t)req_middleware_active >= req_middleware_count
        || !req_middleware_configs[req_middleware_active]) {
        RETURN_NULL();
    }
    RETURN_STRING(req_middleware_configs[req_middleware_active]);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_middleware_config, 0, 0, 0)
ZEND_END_ARG_INFO()

/* ── Function entry table (null-terminated) ──────────────────── */

/* The entries every mode gets. Kept as a macro so the serve-mode table and
 * the CLI table (which adds the cli-SAPI-only functions) share one list —
 * a new native function added here lands in both automatically. */
#define EPHPM_COMMON_FUNCTION_ENTRIES \
    PHP_FE(ephpm_kv_get,       arginfo_ephpm_kv_get) \
    PHP_FE(ephpm_kv_set,       arginfo_ephpm_kv_set) \
    PHP_FE(ephpm_kv_setnx,     arginfo_ephpm_kv_setnx) \
    PHP_FE(ephpm_kv_del,       arginfo_ephpm_kv_del) \
    PHP_FE(ephpm_kv_exists,    arginfo_ephpm_kv_exists) \
    PHP_FE(ephpm_kv_incr,      arginfo_ephpm_kv_incr) \
    PHP_FE(ephpm_kv_decr,      arginfo_ephpm_kv_decr) \
    PHP_FE(ephpm_kv_incr_by,   arginfo_ephpm_kv_incr_by) \
    PHP_FE(ephpm_kv_expire,    arginfo_ephpm_kv_expire) \
    PHP_FE(ephpm_kv_ttl,       arginfo_ephpm_kv_ttl) \
    PHP_FE(ephpm_kv_pttl,      arginfo_ephpm_kv_pttl) \
    PHP_FE(ephpm_kv_flush_all, arginfo_ephpm_kv_flush_all) \
    PHP_FE(ephpm_kv_wait,      arginfo_ephpm_kv_wait) \
    /* Embedded database bridge (per-thread litewire Session). \
     * ephpm_db_run is the unified entry point; the five introspection \
     * functions (columns / in_transaction / available / errno / error) \
     * never throw and never disturb the last-call record. */ \
    PHP_FE(ephpm_db_query,          arginfo_ephpm_db_query) \
    PHP_FE(ephpm_db_execute,        arginfo_ephpm_db_execute) \
    PHP_FE(ephpm_db_run,            arginfo_ephpm_db_run) \
    PHP_FE(ephpm_db_columns,        arginfo_ephpm_db_noargs) \
    PHP_FE(ephpm_db_in_transaction, arginfo_ephpm_db_noargs) \
    PHP_FE(ephpm_db_available,      arginfo_ephpm_db_noargs) \
    PHP_FE(ephpm_db_errno,          arginfo_ephpm_db_noargs) \
    PHP_FE(ephpm_db_error,          arginfo_ephpm_db_noargs) \
    /* WebSocket bridge (site-scoped connection registry). Implicit forms \
     * act on the connection that fired the current event; the \
     * ephpm_ws_connection_* forms take an explicit id. */ \
    PHP_FE(ephpm_ws_send,                    arginfo_ephpm_ws_send) \
    PHP_FE(ephpm_ws_connection_send,         arginfo_ephpm_ws_connection_send) \
    PHP_FE(ephpm_ws_subscribe,               arginfo_ephpm_ws_subscribe) \
    PHP_FE(ephpm_ws_connection_subscribe,    arginfo_ephpm_ws_connection_subscribe) \
    PHP_FE(ephpm_ws_unsubscribe,             arginfo_ephpm_ws_unsubscribe) \
    PHP_FE(ephpm_ws_connection_unsubscribe,  arginfo_ephpm_ws_connection_unsubscribe) \
    PHP_FE(ephpm_ws_broadcast,               arginfo_ephpm_ws_broadcast) \
    PHP_FE(ephpm_ws_close,                   arginfo_ephpm_ws_close) \
    PHP_FE(ephpm_ws_connection_close,        arginfo_ephpm_ws_connection_close) \
    /* PHP middleware lane: the running mount's `config` table as JSON. */ \
    PHP_FE(ephpm_middleware_config,          arginfo_ephpm_middleware_config)

static const zend_function_entry ephpm_kv_functions[] = {
    EPHPM_COMMON_FUNCTION_ENTRIES
    PHP_FE_END
};

/* CLI-mode table: everything above plus the functions the cli SAPI itself
 * registers in php-src (issue #316). Selected by ephpm_module_startup()
 * when g_cli_mode is set, so a web request never sees them. */
static const zend_function_entry ephpm_cli_functions[] = {
    EPHPM_COMMON_FUNCTION_ENTRIES
    PHP_FE(cli_set_process_title, arginfo_cli_set_process_title)
    PHP_FE(cli_get_process_title, arginfo_cli_get_process_title)
    PHP_FE_END
};


/* ===================================================================
 * Native session save handler — `session.save_handler = ephpm`.
 *
 * Stores PHP's serialised session blob in the same KV store used by the
 * ephpm_kv_* native functions. Because that store is automatically
 * site-namespaced in multi-tenant mode and replicated by the cluster
 * layer, sessions inherit per-tenant isolation and affinity-free load
 * balancing without any userland code or extra config.
 *
 * Wired via php_session_register_module() from inside our MINIT shim
 * (ephpm_module_startup) — that is the only safe window in which the
 * session extension's module list is initialised but PHP has not yet
 * fired RINIT for any thread, so the registration is visible to every
 * tokio worker that later copies GLOBAL_FUNCTION_TABLE / module_registry.
 *
 * Keys are namespaced as "session:<sid>". TTL comes from
 * session.gc_maxlifetime; we refresh it on every write and on every
 * timestamp update (so an active session does not expire mid-page).
 *
 * Concurrent requests on the same session id are serialized with a
 * pessimistic per-session lock at "session_lock:<sid>" — see the
 * "Session locking" section below.
 * =================================================================== */

#define EPHPM_SESSION_KEY_PREFIX "session:"
#define EPHPM_SESSION_KEY_PREFIX_LEN (sizeof(EPHPM_SESSION_KEY_PREFIX) - 1)

/*
 * Build a prefixed KV key for a session id on the caller's stack when
 * possible, falling back to emalloc for unusually long sids. Returns a
 * pointer that the caller must release with
 * `ephpm_session_key_free(buf, used_heap)` when finished. `stack_buf`
 * must be at least 64 bytes.
 */
static char *ephpm_session_make_prefixed_key(const char *prefix, size_t prefix_len,
                                             const char *sid, size_t sid_len,
                                             char *stack_buf, size_t stack_buf_len,
                                             int *used_heap)
{
    size_t need = prefix_len + sid_len + 1;
    char *buf;
    if (need <= stack_buf_len) {
        buf = stack_buf;
        *used_heap = 0;
    } else {
        buf = (char *)emalloc(need);
        *used_heap = 1;
    }
    memcpy(buf, prefix, prefix_len);
    memcpy(buf + prefix_len, sid, sid_len);
    buf[prefix_len + sid_len] = '\0';
    return buf;
}

/* Convenience wrapper for the data key ("session:<sid>"). */
static char *ephpm_session_make_key(const char *sid, size_t sid_len,
                                    char *stack_buf, size_t stack_buf_len,
                                    int *used_heap)
{
    return ephpm_session_make_prefixed_key(EPHPM_SESSION_KEY_PREFIX,
                                           EPHPM_SESSION_KEY_PREFIX_LEN,
                                           sid, sid_len,
                                           stack_buf, stack_buf_len, used_heap);
}

static void ephpm_session_key_free(char *buf, int used_heap)
{
    if (used_heap) {
        efree(buf);
    }
}

/* Read TTL (in seconds) from session.gc_maxlifetime, clamped to >= 0. */
static long long ephpm_session_ttl_ms(void)
{
    /* PS(gc_maxlifetime) is a zend_long. 0 or negative => no expiry. */
    long long lifetime = (long long)PS(gc_maxlifetime);
    if (lifetime <= 0) {
        return 0;
    }
    return lifetime * 1000LL;
}

/* ── Session locking ────────────────────────────────────────────────
 *
 * Pessimistic per-session lock, php-fpm files-handler style: without it,
 * two concurrent requests carrying the same session cookie both READ the
 * blob, both mutate their in-memory copy, and the second WRITE silently
 * clobbers the first (lost update).
 *
 * PS_READ acquires "session_lock:<sid>" via SETNX with a TTL before
 * fetching the blob; PS_CLOSE (which PHP guarantees after WRITE, including
 * during request shutdown after a bailout) releases it with DEL. On
 * contention we spin with exponential backoff (start 10ms, cap 100ms) up
 * to a total wait of 30s; if the lock is still held we log an E_WARNING
 * and proceed WITHOUT the lock — a degraded read-modify-write race is
 * strictly better than deadlocking the worker thread.
 *
 * The 30s TTL guards against crashed/stuck holders: a thread that dies
 * while holding the lock stops blocking the session forever.
 *
 * KNOWN LIMITATION (accepted for v1): if a holder outlives the 30s TTL,
 * the lock expires and another request may acquire it. Our release path
 * is an unconditional DEL — the KV ops table has no compare-and-delete —
 * so the original holder would then release the *new* holder's lock,
 * letting a third request in early. The window requires a request that
 * both holds a session open for >30s and overlaps two competitors, and
 * the failure mode is the same lost-update race that exists without
 * locking at all.
 *
 * Windows is ZTS like Linux/macOS (#326), so the same concurrent-request
 * locking applies there. On a hypothetical NTS build PHP execution is
 * serialized in-process and the lock is simply uncontended — the SETNX/DEL
 * pair still balances.
 */

#define EPHPM_SESSION_LOCK_PREFIX "session_lock:"
#define EPHPM_SESSION_LOCK_PREFIX_LEN (sizeof(EPHPM_SESSION_LOCK_PREFIX) - 1)

/* Lock TTL — also the bound on how long a crashed holder can block others. */
#define EPHPM_SESSION_LOCK_TTL_MS 30000LL
/* Total time a contender waits before giving up and proceeding lockless. */
#define EPHPM_SESSION_LOCK_MAX_WAIT_MS 30000u
/* Spin backoff: start at 10ms, double each miss, cap at 100ms. */
#define EPHPM_SESSION_LOCK_BACKOFF_START_MS 10u
#define EPHPM_SESSION_LOCK_BACKOFF_MAX_MS 100u

/* Lock ownership for the request running on this thread: the sid we hold
 * the lock for (plain malloc — must survive Zend's per-request allocator),
 * or NULL when no lock is held. NULL also covers the "gave up and
 * proceeded lockless" case, so the release path never deletes a lock this
 * thread did not acquire (except the TTL-expiry window described above). */
static EPHPM_TLS char *session_lock_sid = NULL;
static EPHPM_TLS size_t session_lock_sid_len = 0;

/* Millisecond sleep for the lock spin loop (portable). */
#if defined(PHP_WIN32) || defined(_WIN32)
#include <windows.h>
static void ephpm_sleep_ms(unsigned int ms)
{
    Sleep(ms);
}
#else
#include <time.h>
static void ephpm_sleep_ms(unsigned int ms)
{
    struct timespec ts;
    ts.tv_sec = ms / 1000u;
    ts.tv_nsec = (long)(ms % 1000u) * 1000000L;
    nanosleep(&ts, NULL);
}
#endif

/* Release the lock this thread holds, if any. Safe to call when no lock
 * is held (no-op). */
static void ephpm_session_lock_release(void)
{
    if (!session_lock_sid) {
        return;
    }
    if (g_kv_ops.del) {
        char stack[128];
        int used_heap = 0;
        char *lock_key = ephpm_session_make_prefixed_key(
            EPHPM_SESSION_LOCK_PREFIX, EPHPM_SESSION_LOCK_PREFIX_LEN,
            session_lock_sid, session_lock_sid_len,
            stack, sizeof(stack), &used_heap);
        (void)g_kv_ops.del(lock_key);
        ephpm_session_key_free(lock_key, used_heap);
    }
    free(session_lock_sid);
    session_lock_sid = NULL;
    session_lock_sid_len = 0;
}

/* Acquire the per-session lock for `sid`, spinning with backoff on
 * contention. On success, records ownership in the thread-local state so
 * PS_CLOSE / PS_DESTROY can release it. On sustained contention (30s),
 * warns and returns without the lock. */
static void ephpm_session_lock_acquire(const char *sid, size_t sid_len)
{
    if (!g_kv_ops.set_nx || !g_kv_ops.del) {
        /* No store (or no lock primitives) wired — nothing to lock with.
         * The read path already degrades to an empty session in this
         * configuration, so silently running lockless is consistent. */
        return;
    }

    if (session_lock_sid) {
        if (session_lock_sid_len == sid_len &&
            memcmp(session_lock_sid, sid, sid_len) == 0) {
            /* Already holding this session's lock (e.g. a second
             * session_start() after session_abort() in the same request). */
            return;
        }
        /* Stale lock from a different sid on this thread — a previous
         * request that never reached PS_CLOSE (bailout edge). Release it
         * so it cannot leak past its TTL. */
        ephpm_session_lock_release();
    }

    char stack[128];
    int used_heap = 0;
    char *lock_key = ephpm_session_make_prefixed_key(
        EPHPM_SESSION_LOCK_PREFIX, EPHPM_SESSION_LOCK_PREFIX_LEN,
        sid, sid_len, stack, sizeof(stack), &used_heap);

    unsigned int waited_ms = 0;
    unsigned int backoff_ms = EPHPM_SESSION_LOCK_BACKOFF_START_MS;
    int acquired = 0;

    for (;;) {
        if (g_kv_ops.set_nx(lock_key, "1", 1, EPHPM_SESSION_LOCK_TTL_MS)) {
            acquired = 1;
            break;
        }
        if (waited_ms >= EPHPM_SESSION_LOCK_MAX_WAIT_MS) {
            break;
        }
        unsigned int sleep_ms = backoff_ms;
        if (sleep_ms > EPHPM_SESSION_LOCK_MAX_WAIT_MS - waited_ms) {
            sleep_ms = EPHPM_SESSION_LOCK_MAX_WAIT_MS - waited_ms;
        }
        ephpm_sleep_ms(sleep_ms);
        waited_ms += sleep_ms;
        backoff_ms *= 2u;
        if (backoff_ms > EPHPM_SESSION_LOCK_BACKOFF_MAX_MS) {
            backoff_ms = EPHPM_SESSION_LOCK_BACKOFF_MAX_MS;
        }
    }

    if (acquired) {
        char *owned = (char *)malloc(sid_len + 1);
        if (owned) {
            memcpy(owned, sid, sid_len);
            owned[sid_len] = '\0';
            session_lock_sid = owned;
            session_lock_sid_len = sid_len;
        } else {
            /* OOM copying the sid: we cannot track ownership, so we must
             * not keep the lock — a lock we can't release would block the
             * session until the TTL fires. Undo and run lockless. */
            (void)g_kv_ops.del(lock_key);
            php_error_docref(NULL, E_WARNING,
                "ephpm session handler: out of memory tracking session lock; "
                "proceeding without lock");
        }
    } else {
        php_error_docref(NULL, E_WARNING,
            "ephpm session handler: could not acquire session lock after "
            "%u ms; proceeding without lock (concurrent request may still "
            "hold it)", waited_ms);
    }

    ephpm_session_key_free(lock_key, used_heap);
}

/* ── PS_OPEN / PS_CLOSE ─────────────────────────────────────────── */

/* Non-NULL sentinel for PS(mod_data). ext/session gates the write/close
 * handler calls on `PS(mod_data) || PS(mod_user_implemented)` (see
 * php_session_save_current_state / php_rshutdown_session_globals in
 * ext/session/session.c) — a native handler that leaves *mod_data NULL in
 * open() never gets its write or close callbacks invoked, every
 * session_write_close() warns "Failed to write session data", and nothing
 * is persisted. We keep no real per-handler state (the KV store is global
 * and the lock state is thread-local), so a shared marker address is all
 * that's needed. The value is never dereferenced. */
static int ephpm_session_mod_data_marker = 0;

PS_OPEN_FUNC(ephpm)
{
    /* save_path is irrelevant — we store in the in-process KV. session_name
     * is the cookie name and is already tracked by ext/session. We must not
     * fail because php_session_initialize() bails on any non-SUCCESS
     * return, and we MUST set *mod_data non-NULL or PHP will silently skip
     * our write/close handlers (see ephpm_session_mod_data_marker). */
    (void)save_path;
    (void)session_name;
    *mod_data = (void *)&ephpm_session_mod_data_marker;
    return SUCCESS;
}

PS_CLOSE_FUNC(ephpm)
{
    /* PHP calls close after write (session_write_close, request shutdown,
     * even after a bailout via RSHUTDOWN), making it the reliable place to
     * release the per-session lock taken in PS_READ. No-op when this
     * thread never acquired one (lockless fallback / no store). */
    ephpm_session_lock_release();
    /* Clear the sentinel like mod_files does — ext/session treats a
     * non-NULL mod_data as "handler still open". */
    *mod_data = NULL;
    return SUCCESS;
}

/* ── PS_READ ────────────────────────────────────────────────────── */

PS_READ_FUNC(ephpm)
{
    (void)mod_data;
    (void)maxlifetime;

    const char *sid_str = ZSTR_VAL(key);
    size_t sid_len = ZSTR_LEN(key);

    /* Serialize concurrent requests on the same session id: take the
     * per-session lock BEFORE reading the blob so the read-modify-write
     * spanning PS_READ..PS_WRITE is atomic across requests. Released in
     * PS_CLOSE / PS_DESTROY. */
    ephpm_session_lock_acquire(sid_str, sid_len);

    if (!g_kv_ops.get || !g_kv_ops.get_result) {
        /* No store wired — behave like an empty session rather than failing. */
        *val = ZSTR_EMPTY_ALLOC();
        return SUCCESS;
    }

    char stack[128];
    int used_heap = 0;
    char *kv_key = ephpm_session_make_key(sid_str, sid_len, stack, sizeof(stack), &used_heap);

    if (!g_kv_ops.get(kv_key)) {
        ephpm_session_key_free(kv_key, used_heap);
        /* Missing keys are NOT an error — return an empty string so PHP
         * treats the session as new. */
        *val = ZSTR_EMPTY_ALLOC();
        return SUCCESS;
    }

    const char *ptr = NULL;
    size_t len = 0;
    g_kv_ops.get_result(&ptr, &len);
    *val = zend_string_init(ptr ? ptr : "", len, 0);
    ephpm_session_key_free(kv_key, used_heap);
    return SUCCESS;
}

/* ── PS_WRITE ───────────────────────────────────────────────────── */

PS_WRITE_FUNC(ephpm)
{
    (void)mod_data;
    (void)maxlifetime;

    if (!g_kv_ops.set) {
        return FAILURE;
    }

    const char *sid_str = ZSTR_VAL(key);
    size_t sid_len = ZSTR_LEN(key);
    char stack[128];
    int used_heap = 0;
    char *kv_key = ephpm_session_make_key(sid_str, sid_len, stack, sizeof(stack), &used_heap);

    long long ttl_ms = ephpm_session_ttl_ms();
    int ok = g_kv_ops.set(kv_key, ZSTR_VAL(val), ZSTR_LEN(val), ttl_ms);
    ephpm_session_key_free(kv_key, used_heap);

    return ok ? SUCCESS : FAILURE;
}

/* ── PS_DESTROY ─────────────────────────────────────────────────── */

PS_DESTROY_FUNC(ephpm)
{
    (void)mod_data;

    const char *sid_str = ZSTR_VAL(key);
    size_t sid_len = ZSTR_LEN(key);

    /* session_destroy() / session_regenerate_id(true) — the destroyed sid
     * will never be written again by this request, so release its lock now
     * (only if this thread actually holds it; a lock for a different sid
     * must stay put). */
    if (session_lock_sid && session_lock_sid_len == sid_len &&
        memcmp(session_lock_sid, sid_str, sid_len) == 0) {
        ephpm_session_lock_release();
    }

    if (!g_kv_ops.del) {
        return SUCCESS;
    }

    char stack[128];
    int used_heap = 0;
    char *kv_key = ephpm_session_make_key(sid_str, sid_len, stack, sizeof(stack), &used_heap);
    (void)g_kv_ops.del(kv_key);
    ephpm_session_key_free(kv_key, used_heap);
    return SUCCESS;
}

/* ── PS_GC ──────────────────────────────────────────────────────── */

PS_GC_FUNC(ephpm)
{
    /* The KV store enforces TTLs natively (lazy expiry on access + active
     * reaper). PHP's GC sweep would be redundant work — report "0 sessions
     * cleaned" via *nrdels and let the store do the right thing. */
    (void)mod_data;
    (void)maxlifetime;
    if (nrdels) {
        *nrdels = 0;
    }
    return 0;
}

/* ── PS_CREATE_SID ──────────────────────────────────────────────── */

PS_CREATE_SID_FUNC(ephpm)
{
    /* Delegate to PHP's own SID generator so session.sid_length /
     * session.sid_bits_per_character / session.hash_function stay honoured.
     * php_session_create_id is the official entrypoint other save handlers
     * (files, memcached, redis) use for the same reason. */
    (void)mod_data;
    return php_session_create_id(NULL);
}

/* ── PS_VALIDATE_SID ────────────────────────────────────────────── */

PS_VALIDATE_SID_FUNC(ephpm)
{
    /* Required so session.use_strict_mode = 1 actually rejects forged SIDs
     * — PHP only accepts a client-supplied SID if validate() reports
     * SUCCESS. Return SUCCESS iff the key already exists in the store. */
    (void)mod_data;

    if (!g_kv_ops.exists) {
        return FAILURE;
    }

    const char *sid_str = ZSTR_VAL(key);
    size_t sid_len = ZSTR_LEN(key);
    char stack[128];
    int used_heap = 0;
    char *kv_key = ephpm_session_make_key(sid_str, sid_len, stack, sizeof(stack), &used_heap);
    int found = g_kv_ops.exists(kv_key);
    ephpm_session_key_free(kv_key, used_heap);
    return found ? SUCCESS : FAILURE;
}

/* ── PS_UPDATE_TIMESTAMP ────────────────────────────────────────── */

PS_UPDATE_TIMESTAMP_FUNC(ephpm)
{
    /* session.lazy_write = 1 (the default in modern PHP) skips PS_WRITE
     * when the serialised session blob is unchanged but still wants the
     * TTL refreshed. Use EXPIRE rather than SET so we don't rewrite the
     * potentially-large value blob on every request. */
    (void)mod_data;
    (void)maxlifetime;

    if (!g_kv_ops.expire) {
        /* Fall back to a full write if EXPIRE is unavailable. */
        return ps_write_ephpm(mod_data, key, val, maxlifetime);
    }

    const char *sid_str = ZSTR_VAL(key);
    size_t sid_len = ZSTR_LEN(key);
    char stack[128];
    int used_heap = 0;
    char *kv_key = ephpm_session_make_key(sid_str, sid_len, stack, sizeof(stack), &used_heap);

    long long ttl_ms = ephpm_session_ttl_ms();
    int ok = 1;
    if (ttl_ms > 0) {
        ok = g_kv_ops.expire(kv_key, ttl_ms);
        if (!ok && g_kv_ops.set) {
            /* Key may have expired between read and update — restore it
             * by falling through to a full write so the session is not
             * silently dropped. */
            ok = g_kv_ops.set(kv_key, ZSTR_VAL(val), ZSTR_LEN(val), ttl_ms);
        }
    }
    ephpm_session_key_free(kv_key, used_heap);
    return ok ? SUCCESS : FAILURE;
}

/* ── ps_module registration ─────────────────────────────────────── */

/* PS_MOD_UPDATE_TIMESTAMP expands to a comma-separated list of values
 * (the handler's name + 9 function pointers) — the surrounding braces
 * are the caller's job. Without them the comma-list is interpreted as
 * a sequence of fresh declarations and collides with the function
 * definitions above ("redeclared as different kind of symbol" cascade
 * across every ps_*_ephpm symbol). PHP's own ext/session/mod_files.c
 * uses the same braced form. */
static const ps_module ps_mod_ephpm = { PS_MOD_UPDATE_TIMESTAMP(ephpm) };

/* ===== INI file path ===== */
/* Holds the custom ini file path set via ephpm_set_ini_file() */
static const char *custom_ini_file = NULL;

/*
 * Set a custom php.ini file path.
 * Must be called BEFORE php_embed_init() so that php_module_startup()
 * uses this path instead of searching for php.ini in default locations.
 *
 * The ini_file pointer must remain valid until php_embed_init() completes.
 * Typically, this is a CString from Rust that lives on the stack during init.
 */
void ephpm_set_ini_file(const char *ini_file)
{
    custom_ini_file = ini_file;
    if (ini_file) {
        php_embed_module.php_ini_path_override = (char *)ini_file;
    }
}

/*
 * CLI `-n` — do not load any php.ini file. Mirrors php-cli's php_ini_ignore.
 * Must be called BEFORE php_embed_init() (the ini search happens during module
 * startup), so it is driven from the Rust CLI pre-scan alongside cli-mode.
 */
void ephpm_cli_set_no_ini(void)
{
    php_embed_module.php_ini_ignore = 1;
    php_embed_module.php_ini_ignore_cwd = 1;
}

/* ===== CLI PHP_BINARY (issue #339) =====
 *
 * php_module_startup() calls php_binary_init(), which fills PG(php_binary) —
 * the value the PHP_BINARY constant is registered from. On Windows that
 * function asks the OS (GetModuleFileName) and so has always been right here.
 * On every other platform it reads sapi_module.executable_location and, when
 * that has no '/' in it, searches PATH for a matching executable.
 *
 * That is precisely why PHP_BINARY was "" on Linux/macOS: php_embed_init()
 * assigns `php_embed_module.executable_location = argv[0]`
 * (sapi/embed/php_embed.c), and ePHPm hands it the bare string "ephpm" — so
 * php_binary_init() went hunting on PATH for something called "ephpm",
 * usually found nothing, and left PG(php_binary) NULL. The field was never
 * unset; it was set to a name that cannot be resolved.
 *
 * So the value cannot be installed before php_embed_init() — embed overwrites
 * it — and cannot be installed after, since php_module_startup() has already
 * struct-copied the module (`sapi_module = *sf`) and called php_binary_init().
 * The one window is inside ephpm_module_startup(), our startup shim, just
 * before it hands off to php_module_startup(); see the assignment there.
 *
 * The stored value is an already-resolved absolute path (Rust's
 * std::env::current_exe()) rather than an argv[0]-style name: `ephpm php` is
 * reached through a subcommand, so resolving a bare "ephpm" against PATH would
 * find *some* ephpm rather than *this* one. php_binary_init() still realpath()s
 * it and requires it to be executable, so a bad path degrades to the old ""
 * rather than to a lie.
 *
 * Left NULL in server mode (only the CLI path calls the setter), so
 * `ephpm serve` keeps its current PHP_BINARY behavior exactly.
 *
 * Owned for the process lifetime: sapi_module holds the pointer, exactly as
 * php-cli's own value (argv[0]) outlives startup.
 */
static char *cli_executable_location = NULL;

/*
 * Record the running executable's path for PHP_BINARY. Must be called BEFORE
 * php_embed_init() — ephpm_module_startup() consumes it during module startup.
 * The string is copied immediately; the caller's pointer need not outlive the
 * call.
 */
void ephpm_cli_set_executable_location(const char *path)
{
    if (!path || !*path) {
        return;
    }
    char *copy = ephpm_strdup_malloc(path);
    if (!copy) {
        return;
    }
    free(cli_executable_location);
    cli_executable_location = copy;
}

/* ===== CLI `-d` directives (startup-time, issue #331) =====
 *
 * php-cli applies `-d name[=value]` by appending "name=value\n" lines to
 * sapi_module.ini_entries BEFORE php_module_startup(), so the values are in
 * the configuration hash when every extension's MINIT runs. That timing is
 * load-bearing: OPcache decides once, in accel_startup() (a MINIT-time hook),
 * whether it will ever be active — on 8.3/8.4 accel_find_sapi() requires
 * `opcache.enable_cli=1` for the "cli" SAPI, and 8.5 keeps the same
 * enable_cli-at-startup check without the allowlist. The JIT is likewise
 * wired up at startup (its buffer lives in OPcache SHM). Applying `-d` after
 * php_embed_init() — the pre-#331 behavior — changed the ini entry (visible
 * to ini_get()) but could never re-run that decision, so
 * `-d opcache.enable_cli=1` reported 1 while opcache_get_status() stayed
 * false and `-d opcache.jit=…` silently did nothing.
 *
 * The buffer below accumulates directives in exactly the format php-cli's
 * php_ini_builder_define() produces (value double-quoted when it starts with
 * a non-alphanumeric that isn't already a quote; bare `-d name` = "name=1").
 * ephpm_module_startup() splices it AFTER the embed SAPI's HARDCODED_INI so
 * `-d` wins over those defaults, exactly as php-cli's `-d` wins over its
 * ini_defaults. Because the whole string is parsed during php_init_config()
 * this also restores php-cli's `-d extension=…` / `-d zend_extension=…`
 * behavior: the ini parser routes those to the extension lists that
 * php_ini_register_extensions() loads during module startup.
 *
 * Not implemented with PHP's own php_ini_builder API on purpose: these
 * helpers run BEFORE php_embed_init(), and keeping them libc-only avoids
 * depending on PHPAPI symbol export differences across the per-platform SDK
 * builds. The buffer intentionally lives for the process lifetime (php-cli
 * frees its builder only at exit; ours is handed to sapi_module.ini_entries).
 */
static char *g_cli_ini_defines = NULL;
static size_t g_cli_ini_defines_len = 0;
static size_t g_cli_ini_defines_cap = 0;

/* Append `len` bytes to the define buffer, growing it as needed. Returns 0
 * on allocation failure (the directive is then dropped — matching the spirit
 * of php-cli, where a failed realloc aborts; we degrade instead because this
 * runs before PHP's own error machinery exists). */
static int cli_ini_defines_append(const char *src, size_t len)
{
    if (g_cli_ini_defines_len + len + 1 > g_cli_ini_defines_cap) {
        size_t ncap = g_cli_ini_defines_cap ? g_cli_ini_defines_cap : 256;
        while (g_cli_ini_defines_len + len + 1 > ncap) {
            ncap *= 2;
        }
        char *nbuf = (char *)realloc(g_cli_ini_defines, ncap);
        if (!nbuf) {
            return 0;
        }
        g_cli_ini_defines = nbuf;
        g_cli_ini_defines_cap = ncap;
    }
    memcpy(g_cli_ini_defines + g_cli_ini_defines_len, src, len);
    g_cli_ini_defines_len += len;
    g_cli_ini_defines[g_cli_ini_defines_len] = '\0';
    return 1;
}

/*
 * Record one CLI `-d name[=value]` directive for startup-time application.
 * Must be called BEFORE php_embed_init() — driven from the Rust CLI pre-scan
 * alongside ephpm_enable_cli_mode()/-c/-n. The string is copied immediately;
 * the caller's pointer need not outlive the call.
 *
 * Quoting mirrors php-cli's php_ini_builder_define() exactly: a value whose
 * first character is non-alphanumeric and not already a quote is wrapped in
 * double quotes (so `-d error_reporting=E_ALL & ~E_NOTICE` parses as one
 * value), and a bare `-d name` becomes `name=1`.
 */
void ephpm_cli_add_ini_define(const char *def)
{
    if (!def || !*def) {
        return;
    }
    size_t len = strlen(def);
    const char *val = strchr(def, '=');

    if (val != NULL) {
        val++;
        if (!isalnum((unsigned char)*val) && *val != '"' && *val != '\'' && *val != '\0') {
            /* name= + "value" + \n */
            (void)(cli_ini_defines_append(def, (size_t)(val - def))
                && cli_ini_defines_append("\"", 1)
                && cli_ini_defines_append(val, len - (size_t)(val - def))
                && cli_ini_defines_append("\"\n", 2));
        } else {
            (void)(cli_ini_defines_append(def, len) && cli_ini_defines_append("\n", 1));
        }
    } else {
        (void)(cli_ini_defines_append(def, len) && cli_ini_defines_append("=1\n", 3));
    }
}

/*
 * Custom startup callback installed in place of php_embed_module.startup.
 *
 * Why this is necessary, and why post-init registration cannot work:
 *
 *  1. php_embed_init() unconditionally overwrites
 *     php_embed_module.additional_functions with its own array (just dl())
 *     at sapi/embed/php_embed.c:219, after sapi_startup() and before
 *     module startup. So pre-setting additional_functions in ephpm_pre_init
 *     is wiped out before php_module_startup sees it.
 *
 *  2. In ZTS, zend_startup() ends by copying the main thread's CG(function_table)
 *     into the static GLOBAL_FUNCTION_TABLE and then freeing the main thread's
 *     table (Zend/zend.c:1114-1124). New TSRM threads (our tokio workers)
 *     bootstrap their own CG(function_table) by copying from
 *     GLOBAL_FUNCTION_TABLE in compiler_globals_ctor (Zend/zend.c:720). So any
 *     functions we register after php_embed_init() returns land in nothing —
 *     the main thread's table is gone and new threads never see them.
 *
 * The only window that works is "after embed.c:219 overwrite, before
 * php_module_startup reads sapi_module.additional_functions." We get there by
 * replacing php_embed_module.startup with this shim, restoring the KV table on
 * the SAPI struct, then handing off to PHP's own php_module_startup. That puts
 * the functions in CG(function_table) during MINIT, which is then copied into
 * GLOBAL_FUNCTION_TABLE at the end of zend_startup() — exactly where new
 * threads will pick them up.
 */
static int ephpm_module_startup(sapi_module_struct *sm)
{
    /* In CLI mode the table additionally carries the cli-SAPI-only functions
     * (cli_set_process_title / cli_get_process_title, issue #316) — the `cli`
     * SAPI identity promises them, but a web request must never see them
     * (php-fpm has its own fastcgi_* equivalents; ours reports "ephpm"). */
    sm->additional_functions = g_cli_mode ? ephpm_cli_functions : ephpm_kv_functions;

    /* PHP_BINARY (issue #339). This is the only window: php_embed_init() has
     * just overwritten executable_location with argv[0] ("ephpm" — a bare name
     * php_binary_init() would fruitlessly hunt for on PATH), and
     * php_module_startup() below both struct-copies *sm into sapi_module and
     * calls php_binary_init(). NULL outside `ephpm php`, so the server's
     * PHP_BINARY is unchanged. See the ephpm_cli_set_executable_location
     * block for the full derivation. */
    if (cli_executable_location) {
        sm->executable_location = cli_executable_location;
    }

    /* Splice CLI `-d` directives into the SAPI's startup ini (issue #331).
     * php_embed_init() has just set sm->ini_entries to the embed SAPI's
     * HARDCODED_INI; php_module_startup() re-copies *sm into sapi_module and
     * php_init_config() parses ini_entries LAST (after any php.ini), so this
     * is the exact window php-cli's `-d` handling occupies. Our entries go
     * AFTER the hardcoded ones so `-d` overrides them, and after-the-file
     * parsing means `-d` overrides php.ini — both php-cli behaviors. The
     * merged buffer must outlive module startup; like php-cli's ini string it
     * is left to the process teardown. */
    if (g_cli_ini_defines) {
        size_t base_len = sm->ini_entries ? strlen(sm->ini_entries) : 0;
        char *merged = (char *)malloc(base_len + g_cli_ini_defines_len + 1);
        if (merged) {
            if (base_len) {
                memcpy(merged, sm->ini_entries, base_len);
            }
            memcpy(merged + base_len, g_cli_ini_defines, g_cli_ini_defines_len);
            merged[base_len + g_cli_ini_defines_len] = '\0';
            sm->ini_entries = merged;
        }
    }
    /* Register the worker module as php_module_startup's `additional_module` so
     * its functions (Ephpm\Worker\take_request/send_response) and its MINIT
     * (the Envelope class) land in CG(function_table)/CG(class_table) DURING
     * zend_startup — before those are frozen into GLOBAL_FUNCTION_TABLE /
     * GLOBAL_CLASS_TABLE that new ZTS worker threads inherit. Registering it
     * later (via zend_startup_module after this returns) leaves it invisible to
     * worker threads (function_exists() == false there). */
    int ret = php_module_startup(sm, &ephpm_worker_module_entry);

    /* Register the native "ephpm" session save handler. Must happen after
     * php_module_startup() — the session extension's MINIT is what wires up
     * the global module list this call inserts into. Doing it earlier
     * (before php_module_startup) crashes because the session extension's
     * own globals aren't constructed yet; doing it later (after
     * php_embed_init returns) is too late under ZTS, since the main
     * thread's CG/EG state has already been frozen into GLOBAL_FUNCTION_TABLE
     * for new worker threads to copy.
     *
     * php_session_register_module() returns 0 on success, but practically
     * cannot fail (it's an EG_HASH append). Even if it does, we don't
     * unwind module startup — users who haven't configured the handler
     * pay nothing for the absence. */
    if (ret == SUCCESS) {
        (void)php_session_register_module(&ps_mod_ephpm);
    }
    return ret;
}

/*
 * Pre-initialization: replace the embed SAPI's module startup callback
 * with our shim above. Must be called BEFORE php_embed_init().
 *
 * Hooking startup (rather than additional_functions directly) is the key:
 * php_embed_init() rewrites additional_functions but leaves startup alone,
 * so the shim still runs and gets a chance to put our table back before
 * php_module_startup is invoked.
 */
void ephpm_pre_init(void)
{
    php_embed_module.startup = ephpm_module_startup;

    /* The SAPI name must be settled BEFORE php_embed_init():
     * sapi_startup() struct-copies php_embed_module into sapi_module, and
     * OPcache's accel_startup() (inside php_module_startup) checks
     * sapi_module.name against its supported-SAPIs allowlist on PHP < 8.5,
     * caching the verdict for the process lifetime. Renaming only in
     * ephpm_install_sapi() (post-init) left OPcache seeing "embed" at
     * startup — permanently "Startup Failed" even though the SDK
     * whitelists "ephpm".
     *
     * In `ephpm php` (g_cli_mode set by ephpm_enable_cli_mode() just before
     * init) the name is "cli", so PHP_SAPI === "cli" — the drop-in identity.
     * "cli" is the SAPI OPcache is built to accept, so this is at least as
     * safe as "ephpm" here. */
    php_embed_module.name = (char *)EPHPM_SAPI_NAME;
    php_embed_module.pretty_name = (char *)EPHPM_SAPI_PRETTY_NAME;
}

/*
 * Set the KV ops function pointer table. Can be called at any time
 * before PHP scripts execute — typically after php_embed_init().
 */
void ephpm_set_kv_ops(const EphpmKvOps *ops)
{
    if (ops) {
        g_kv_ops = *ops;
    }
}

/*
 * Set the DB ops function pointer table backing ephpm_db_query(),
 * ephpm_db_execute(), ephpm_db_run(), and the introspection functions.
 * Same timing contract as ephpm_set_kv_ops: called
 * once at startup, before any PHP scripts execute; g_db_ops is read-only
 * afterwards (ZTS-safe without locking).
 */
void ephpm_set_db_ops(const EphpmDbOps *ops)
{
    if (ops) {
        g_db_ops = *ops;
    }
}

/*
 * Set the WebSocket ops function pointer table backing ephpm_ws_send() and
 * friends. Same timing contract as ephpm_set_kv_ops / ephpm_set_db_ops:
 * called once at startup, before any PHP scripts execute; g_ws_ops is
 * read-only afterwards (ZTS-safe without locking).
 *
 * Left NULL when [server.websocket] is disabled. The PHP functions still
 * exist — so a script can feature-detect with function_exists() rather
 * than fataling on an undefined function — but calling one throws
 * "native websockets are not enabled". Deliberately an exception rather
 * than a `false`: a silent no-op here would look exactly like a
 * delivered frame.
 */
void ephpm_set_ws_ops(const EphpmWsOps *ops)
{
    if (ops) {
        g_ws_ops = *ops;
    }
}

/* ===================================================================
 * CLI mode — `ephpm php ...` subcommand
 *
 * Provides a PHP CLI interface using the embed SAPI by handling
 * argc/argv with php_getopt and calling the same PHP APIs that
 * the real CLI SAPI uses. Output goes directly to stdout/stderr.
 * =================================================================== */

#include "main/php_getopt.h"
#include "ext/standard/info.h"
#include "main/php_output.h"
#include "Zend/zend_extensions.h"
#include "Zend/zend_highlight.h"
#include "ext/standard/basic_functions.h"

/*
 * ub_write callback that writes directly to stdout.
 * Used temporarily during CLI-mode execution.
 */
static size_t ephpm_sapi_ub_write_stdout(const char *str, size_t str_length)
{
    return fwrite(str, 1, str_length, stdout);
}

/*
 * Get the PHP version string (compile-time constant).
 * Does NOT require php_embed_init() — safe to call at any time.
 */
const char *ephpm_get_php_version(void)
{
    return PHP_VERSION;
}

/*
 * Helper: switch to CLI-mode output (stdout).
 * Saves the current ub_write and swaps in stdout mode. Also sets
 * headers_sent + no_headers so PHP doesn't try to emit HTTP headers.
 */
static void cli_begin(size_t (**orig_ub_write)(const char *, size_t))
{
    *orig_ub_write = sapi_module.ub_write;
    sapi_module.ub_write = ephpm_sapi_ub_write_stdout;
    SG(headers_sent) = 1;
    SG(request_info).no_headers = 1;
}

/*
 * Helper: finish CLI-mode execution. Flushes stdout and restores
 * the original ub_write.
 */
static void cli_end(size_t (*orig_ub_write)(const char *, size_t))
{
    fflush(stdout);
    sapi_module.ub_write = orig_ub_write;
}

/*
 * Register $argv/$argc and script-identity $_SERVER vars for the CLI path.
 *
 * The embedded request starts during runtime init — before CLI argument
 * parsing — so php_hash_environment ran without SG(request_info).argv and
 * userland saw no $argv at all (php-cli parses args first, so it never
 * hits this). Symfony Console / artisan silently degrade to the default
 * command when $argv is missing. Populate request_info, let
 * php_build_argv install $argv/$argc into the global symbol table and
 * $_SERVER, then top up the script-identity keys frameworks read.
 *
 * SG(request_info).path_translated is deliberately left NULL: SAPI
 * deactivation efree()s it, so it must never point at C argv memory.
 */
static void cli_register_argv(
    int argc, char **argv, int script_ind, const char *script_name, int is_file)
{
    argv[script_ind] = (char *)script_name;
    SG(request_info).argc = argc - script_ind;
    SG(request_info).argv = &argv[script_ind];

    zend_is_auto_global_str("_SERVER", sizeof("_SERVER") - 1);
    zval *server = &PG(http_globals)[TRACK_VARS_SERVER];

    /* php-cli's sapi_cli_register_variables() imports the whole process
     * environment into $_SERVER first (its default variables_order is EGPCS,
     * and the "S" of a CLI process is its environment), then layers the
     * script-identity keys on top. Composer, PHPUnit and friends read
     * $_SERVER['HOME'] / $_SERVER['PATH'] / $_SERVER['APPDATA'] directly, so
     * without this they see nothing (issue #338).
     *
     * Deliberately done HERE — on the CLI-only path — and not in
     * ephpm_sapi_register_server_variables(): in serve mode $_SERVER is
     * per-request and multi-tenant, and the process environment holds
     * cross-tenant material (see the site-key/credential model in
     * ephpm-server). It must never leak into a web request's $_SERVER.
     * Ordering matches php-cli: environment first, so the explicit keys
     * below win over any same-named environment variable. */
    if (Z_TYPE_P(server) == IS_ARRAY) {
        php_import_environment_variables(server);
    }

    php_build_argv(NULL, Z_TYPE_P(server) == IS_ARRAY ? server : NULL);

    if (Z_TYPE_P(server) == IS_ARRAY) {
        /* php-cli sets PHP_SELF/SCRIPT_NAME to the script identity in every
         * mode, but SCRIPT_FILENAME/PATH_TRANSLATED only when a real file is
         * being executed — for stdin programs and -r they are "" (verified
         * against php 8.5 cli). */
        static const char *const self_keys[] = { "PHP_SELF", "SCRIPT_NAME" };
        static const char *const file_keys[] = { "SCRIPT_FILENAME", "PATH_TRANSLATED" };
        for (size_t i = 0; i < sizeof(self_keys) / sizeof(self_keys[0]); i++) {
            zval tmp;
            ZVAL_STRING(&tmp, script_name);
            zend_hash_str_update(
                Z_ARRVAL_P(server), self_keys[i], strlen(self_keys[i]), &tmp);
        }
        for (size_t i = 0; i < sizeof(file_keys) / sizeof(file_keys[0]); i++) {
            zval tmp;
            ZVAL_STRING(&tmp, is_file ? script_name : "");
            zend_hash_str_update(
                Z_ARRVAL_P(server), file_keys[i], strlen(file_keys[i]), &tmp);
        }

        /* php-cli registers DOCUMENT_ROOT as an empty string ("just make it
         * available", sapi/cli/php_cli.c) — there is no document root in CLI
         * mode, but code that reads the key unconditionally must not warn
         * (issue #338). */
        zval docroot;
        ZVAL_EMPTY_STRING(&docroot);
        zend_hash_str_update(
            Z_ARRVAL_P(server), "DOCUMENT_ROOT", sizeof("DOCUMENT_ROOT") - 1, &docroot);
    }
}

/*
 * Helper: evaluate raw code (-r, and the --rf/--rc/--re/--rz/--ri reflection
 * flags) with bailout protection. Returns the PHP exit status. Scripts go
 * through cli_execute_script_protected instead.
 */
static int cli_eval_protected(const char *code, const char *label)
{
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        /* handle_exceptions=1: an uncaught exception becomes the same
         * E_ERROR php-cli reports ("Uncaught … in Command line code"),
         * which sets EG(exit_status) = 255 — plain zend_eval_string would
         * leave the exception pending and unreported. -r passes php-cli's
         * own label so error messages are byte-identical. */
        zend_eval_string_ex((char *)code, NULL, (char *)label, 1);

        /* PHP 8.x: exit() throws an unwind exit exception instead of
         * calling zend_bailout(); clear it if still pending. Then take
         * EG(exit_status) unconditionally — the engine catches fatals
         * internally (its error handler already set exit_status to 255),
         * so "0 unless we bailed" would drop exit()/fatal codes. */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
        }
        result = (int)EG(exit_status);
    } else {
        /* PHP bailed out (fatal error) */
        result = EG(exit_status);
        if (result == 0) result = 1;
    }
    EG(bailout) = __orig_bailout;
    return result;
}

/*
 * Define one of the STDIN / STDOUT / STDERR constants as an open stream
 * resource, opened through the `php://std*` wrapper exactly as stock php-cli's
 * cli_register_file_handles does (sapi/cli/php_cli.c).
 *
 * Going through the wrapper — rather than wrapping the process FILE* directly —
 * is what gives the constants their `php://` provenance: without it
 * stream_get_meta_data(STDIN) reports no `wrapper_type` and no `uri`, and code
 * that sniffs those to tell real stdio from an arbitrary stream misbehaves
 * (issue #340). The wrapper is not a different handle: for the "cli" SAPI name
 * — which `ephpm php` uses — ext/standard/php_fopen_wrapper.c hands the very
 * first php://stdin / php://stdout / php://stderr open the process's own
 * stdin/stdout/stderr FILE*, so this is the same handle the old FILE*-based
 * code used, now carrying the metadata php-cli's does.
 *
 * Both no-close flags are set:
 *   * NO_RSCR_DTOR_CLOSE is php-cli's own (its comment: extensions writing to
 *     stderr during MSHUTDOWN still need it open), and
 *   * NO_FCLOSE additionally guarantees the shared FILE* is never fclose()d if
 *     the stream is closed some other way — ePHPm keeps writing diagnostics to
 *     stderr after the CLI request ends, so losing the handle is not survivable.
 */
static void cli_register_file_handle(const char *url, const char *mode,
                                     const char *name, size_t name_len)
{
    php_stream *stream = php_stream_open_wrapper_ex(url, mode, 0, NULL, NULL);
    if (!stream) {
        return;
    }
    stream->flags |= PHP_STREAM_FLAG_NO_RSCR_DTOR_CLOSE | PHP_STREAM_FLAG_NO_FCLOSE;

    zend_constant c;
    php_stream_to_zval(stream, &c.value);
    ZEND_CONSTANT_SET_FLAGS(&c, CONST_CS, 0);
    c.name = zend_string_init_interned(name, name_len, 0);
    zend_register_constant(&c);
}

/*
 * Register the STDIN / STDOUT / STDERR constants, exactly as php-cli does.
 * Many CLI tools reference these directly (wp-cli's isPiped(), phpunit,
 * fwrite(STDERR, …), fgets(STDIN)) and fatal with "Undefined constant STDOUT"
 * without them — the embed SAPI never defines them, so `ephpm php` must.
 */
static void cli_register_file_handles(void)
{
    cli_register_file_handle("php://stdin", "rb", "STDIN", sizeof("STDIN") - 1);
    cli_register_file_handle("php://stdout", "wb", "STDOUT", sizeof("STDOUT") - 1);
    cli_register_file_handle("php://stderr", "wb", "STDERR", sizeof("STDERR") - 1);
}

/*
 * php-cli's name for a program that came from stdin. It is used verbatim as
 * the compiled file name, the lint label, $argv[0] and $_SERVER['PHP_SELF'],
 * exactly as stock php-cli does (php_cli.c: `php_self = "Standard input
 * code"`), so error messages and script self-identification match.
 */
#define CLI_STDIN_NAME "Standard input code"

/*
 * Open the primary script source, mirroring php-cli's cli_seek_file_begin.
 *
 * A named file is fopen'd in binary mode and wrapped with zend_stream_init_fp
 * — the same FILE*-backed handle php-cli compiles from, so reading is
 * byte-exact (binary-safe, no CRLF translation) and the exact path is used
 * with no include_path search. A NULL filename means the program comes from
 * the process's stdin, which is how `php < script.php` and `… | php` work;
 * that path also works in the embed build, where the `php://stdin` stream
 * wrapper does not open for compilation.
 *
 * On failure prints php-cli's exact message and returns 0; the caller exits 1.
 */
static int cli_open_script(zend_file_handle *fh, const char *filename)
{
    if (filename) {
        FILE *fp = VCWD_FOPEN(filename, "rb");
        if (!fp) {
            fprintf(stderr, "Could not open input file: %s\n", filename);
            return 0;
        }
        zend_stream_init_fp(fh, fp, filename);
    } else {
        zend_stream_init_fp(fh, stdin, CLI_STDIN_NAME);
    }
    fh->primary_script = 1;
    return 1;
}

/*
 * Execute the primary script with bailout protection. `filename` NULL means
 * "read the program from stdin". The program is a real script (expects
 * `<?php` tags), unlike -r's raw code.
 */
static int cli_execute_script_protected(const char *filename)
{
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        zend_file_handle file_handle;
        if (!cli_open_script(&file_handle, filename)) {
            EG(bailout) = __orig_bailout;
            return 1;
        }
        php_execute_script(&file_handle);
        zend_destroy_file_handle(&file_handle);

        /* See cli_eval_protected: exit()/fatal codes live in
         * EG(exit_status) after php_execute_script returns. */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
        }
        result = (int)EG(exit_status);
    } else {
        result = EG(exit_status);
        if (result == 0) result = 1;
    }
    EG(bailout) = __orig_bailout;
    return result;
}

/*
 * -l (lint), -w (strip) and -s (highlight), all bailout-protected and all
 * accepting the program on stdin when `filename` is NULL — php-cli routes
 * every one of these modes through the same file handle, so `php -l < f.php`
 * and `… | php -w` work there and must work here. `mode` is the CLI option
 * letter. Returns the exit code (255 for a lint failure, 1 if the file can't
 * be opened, matching php-cli).
 */
static int cli_scan_protected(int mode, const char *filename, const char *php_self)
{
    /* volatile: assigned between SETJMP and a possible longjmp, then read
     * after it — without this the value is indeterminate (and gcc warns
     * -Wclobbered). */
    volatile int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        zend_file_handle file_handle;
        if (!cli_open_script(&file_handle, filename)) {
            EG(bailout) = __orig_bailout;
            return 1;
        }
        if (mode == 'l') {
            if (php_lint_script(&file_handle) == SUCCESS) {
                php_printf("No syntax errors detected in %s\n", php_self);
            } else {
                php_printf("Errors parsing %s\n", php_self);
                result = 255;
            }
        } else if (open_file_for_scanning(&file_handle) == SUCCESS) {
            if (mode == 'w') {
                zend_strip();
            } else {
                zend_syntax_highlighter_ini syntax_highlighter_ini;
                php_get_highlight_struct(&syntax_highlighter_ini);
                zend_highlight(&syntax_highlighter_ini);
            }
        }
        /* php-cli destroys the handle once, after the mode has run
         * (php_cli.c's `out:` label); the dtor NULLs the FILE* so this is
         * safe whether or not the scanner already consumed it. */
        zend_destroy_file_handle(&file_handle);
    } else {
        result = EG(exit_status);
        if (result == 0) result = 1;
    }
    EG(bailout) = __orig_bailout;
    return result;
}

/*
 * Portable line reader for the -R/-F/-B/-E stdin line processor and REPL-free
 * stdin execution. Reads one line (including any trailing newline) from `fp`
 * into a malloc'd buffer. Returns the buffer (caller frees) and sets *out_len,
 * or NULL at EOF with no data. Uses fgetc so it needs no POSIX getline (absent
 * on MSVC).
 */
static char *cli_read_line(FILE *fp, size_t *out_len)
{
    size_t cap = 256;
    size_t len = 0;
    char *buf = (char *)malloc(cap);
    if (!buf) {
        return NULL;
    }
    int ch;
    while ((ch = fgetc(fp)) != EOF) {
        if (len + 2 >= cap) {
            size_t ncap = cap * 2;
            char *nbuf = (char *)realloc(buf, ncap);
            if (!nbuf) {
                free(buf);
                return NULL;
            }
            buf = nbuf;
            cap = ncap;
        }
        buf[len++] = (char)ch;
        if (ch == '\n') {
            break;
        }
    }
    if (len == 0 && ch == EOF) {
        free(buf);
        return NULL;
    }
    buf[len] = '\0';
    *out_len = len;
    return buf;
}

/*
 * Install $argn (current line, trailing CR/LF stripped) and $argi (line
 * number) into the global symbol table for the -R/-F line processor, the
 * same variables php-cli exposes. $argi is **1-based**: php-cli increments
 * before assigning, so the first line is 1 (verified against php 8.5:
 * `printf 'a\nb\n' | php -R 'echo $argi;'` prints 1 then 2).
 */
static void cli_set_line_vars(const char *line, size_t len, zend_long argi)
{
    while (len > 0 && (line[len - 1] == '\n' || line[len - 1] == '\r')) {
        len--;
    }
    zval zn;
    ZVAL_STRINGL(&zn, line, len);
    zend_hash_str_update(&EG(symbol_table), "argn", sizeof("argn") - 1, &zn);

    zval zi;
    ZVAL_LONG(&zi, argi);
    zend_hash_str_update(&EG(symbol_table), "argi", sizeof("argi") - 1, &zi);
}

/*
 * The awk-like -B/-R/-F/-E line processor. Runs `begin_code` once, then for
 * each stdin line sets $argn/$argi and runs either `per_line_code` (-R) or
 * `per_line_file` (-F), then `end_code` once. The whole run is wrapped in one
 * bailout so a fatal aborts it exactly as php-cli does. Returns the exit code.
 */
static int cli_process_stdin_lines(
    const char *begin_code,
    const char *per_line_code,
    const char *per_line_file,
    const char *end_code)
{
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        if (begin_code) {
            zend_eval_string_ex(
                (char *)begin_code, NULL, "Command line begin code", 1);
        }
        if (!(EG(exception) && zend_is_unwind_exit(EG(exception)))) {
            zend_long argi = 0;
            char *line;
            size_t len;
            while ((line = cli_read_line(stdin, &len)) != NULL) {
                cli_set_line_vars(line, len, ++argi);
                free(line);
                if (per_line_code) {
                    zend_eval_string_ex(
                        (char *)per_line_code, NULL, "Command line run code", 1);
                } else if (per_line_file) {
                    zend_file_handle fh;
                    zend_stream_init_filename(&fh, per_line_file);
                    php_execute_script(&fh);
                    zend_destroy_file_handle(&fh);
                }
                if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
                    break;
                }
            }
        }
        if (end_code && !(EG(exception) && zend_is_unwind_exit(EG(exception)))) {
            zend_eval_string_ex((char *)end_code, NULL, "Command line end code", 1);
        }

        /* See cli_eval_protected: take EG(exit_status) unconditionally
         * so exit() inside -B/-R/-F/-E code keeps its code. */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
        }
        result = (int)EG(exit_status);
    } else {
        result = EG(exit_status);
        if (result == 0) {
            result = 1;
        }
    }
    EG(bailout) = __orig_bailout;
    return result;
}

/*
 * End the CLI request exactly as php-cli's do_cli() does at its `out:` label
 * (sapi/cli/php_cli.c): run php_request_shutdown() while the CLI stdout
 * ub_write is still installed, and (re)read the exit status from
 * EG(exit_status) AFTERWARDS.
 *
 * php_request_shutdown() is what fires PHP's end-of-request userland
 * machinery, in its documented order: registered shutdown functions first
 * (registration order, including ones registered during shutdown), then
 * end-of-script object destructors (zend_call_destructors), then the output
 * flush. This CLI used to skip the call entirely and leave the request for
 * php_embed_shutdown() at process exit — by which point cli_end() had
 * restored the HTTP capture ub_write and the exit code had been captured, so
 * shutdown functions and destructors ran invisibly and exit() inside a
 * shutdown function could not set the status (issue #334).
 *
 * exit() inside a shutdown function throws/longjmps again (a nested
 * bailout). php_request_shutdown() guards each phase with its own zend_try
 * internally, but a bailout must never escape into the CLI teardown with the
 * request half torn down, so the call carries a guard of its own — the role
 * php-cli's zend_first_try in main() plays.
 *
 * Exit-status contract (verified against php-cli 8.5.4): do_cli returns
 * EG(exit_status) read after request shutdown, so exit(N) in a shutdown
 * function overrides even the script's own exit() — including exit(0)
 * clearing a nonzero status. When shutdown did NOT change EG(exit_status),
 * the pre-shutdown `result` is kept; that preserves the two statuses that
 * live outside EG(exit_status) in this CLI: the lint-failure 255 from
 * cli_scan_protected and the bailed-with-status-0 → 1 fallback (#317/#321).
 * (php-cli 8.5 keeps those inside EG(exit_status), so the pre/post
 * comparison is observably identical to its unconditional read.)
 *
 * Finally, the embed lifecycle expects one active request when
 * php_embed_shutdown() runs (it unconditionally calls php_request_shutdown),
 * so a fresh, empty request is started before returning — the same
 * shutdown→startup pairing ephpm_execute_request uses per HTTP request.
 * $argv/$argc in SG(request_info) are cleared first so nothing in the
 * throwaway request (or its eventual teardown, which outlives the caller's
 * argv memory) ever reads the CLI argv pointers again. It runs no user code
 * and produces no output; the capture ub_write restored by cli_end() would
 * swallow anything it did produce.
 */
static int cli_shutdown_request(int result)
{
    int pre_status = (int)EG(exit_status);

    zend_try {
        php_request_shutdown(NULL);
    } zend_catch {
        /* A bailout escaped php_request_shutdown's internal phase guards;
         * the request is as torn down as it will get — carry on so the
         * exit status is still captured and cli_end() still runs. */
    } zend_end_try();

    int post_status = (int)EG(exit_status);
    if (post_status != pre_status) {
        result = post_status;
    }

    /* Restore the embed invariant: one active request left open for
     * php_embed_shutdown() to close at process exit. If startup fails
     * there is nothing to recover — the process is about to exit. */
    SG(request_info).argc = 0;
    SG(request_info).argv = NULL;
    if (php_request_startup() == SUCCESS) {
        SG(headers_sent) = 1;
        SG(request_info).no_headers = 1;
    }

    return result;
}

/* PHP CLI option table — matches the real PHP CLI SAPI options.
 * Used by php_getopt() to parse argc/argv.
 *
 * Keeping this in step with php-cli's OPTIONS[] became load-bearing with
 * issue #336: an option missing from this table is now a hard error (usage +
 * exit 1) rather than a silent no-op, so an entry php-cli has and this one
 * lacks would turn a working command line into a failure. The one deliberate
 * omission is php-cli's `{16, 1, "repeat"}`, which php-src itself labels
 * "internal testing option -- may be changed or removed without notice":
 * accepting and ignoring it would silently run a script once where the caller
 * asked for N. */
static const opt_struct cli_options[] = {
    {'a', 0, "interactive"},
    {'B', 1, "process-begin"},
    {'C', 0, "no-chdir"},
    {'c', 1, "php-ini"},
    {'d', 1, "define"},
    {'E', 1, "process-end"},
    {'e', 0, "profile-info"},
    {'F', 1, "process-file"},
    {'f', 1, "file"},
    {'h', 0, "help"},
    {'i', 0, "info"},
    {'l', 0, "syntax-check"},
    {'m', 0, "modules"},
    {'n', 0, "no-php-ini"},
    {'q', 0, "no-header"},
    {'R', 1, "process-code"},
    {'H', 0, "hide-args"},
    {'r', 1, "run"},
    {'S', 1, "server"},
    {'s', 0, "syntax-highlight"},
    {'s', 0, "syntax-highlighting"}, /* php-cli carries both spellings */
    {'t', 1, "docroot"},
    {'w', 0, "strip"},
    {'?', 0, "usage"},
    {'v', 0, "version"},
    {10,  1, "rf"},
    {10,  1, "rfunction"},
    {11,  1, "rc"},
    {11,  1, "rclass"},
    {12,  1, "re"},
    {12,  1, "rextension"},
    {13,  1, "rz"},
    {13,  1, "rzendextension"},
    {14,  1, "ri"},
    {14,  1, "rextinfo"},
    {15,  2, "ini"},
    {'-', 0, NULL}
};

/* Helper: print module names (for -m flag) */
static int cli_print_module(zval *zv)
{
    zend_module_entry *module = Z_PTR_P(zv);
    php_printf("%s\n", module->name);
    return ZEND_HASH_APPLY_KEEP;
}

/* Helper: print Zend extension names (for -m flag) */
static void cli_print_extension(zend_extension *ext)
{
    php_printf("%s\n", ext->name);
}

/*
 * Usage text for `-h`/`--usage` and for an unrecognized option, mirroring
 * php-cli's php_cli_usage(): stdout, not stderr, in both cases.
 *
 * It deliberately does NOT reproduce php-cli's text verbatim. The program name
 * is `ephpm php`, and the options list omits the ones this build honestly does
 * not implement (-S/-t built-in server, --repeat, --ini=diff), because a usage
 * screen advertising flags that then refuse to run is worse than a shorter one.
 * The CLI conformance corpus therefore keeps 137-unknown-option xfail'd on the
 * stdout text while the stderr diagnostic and the exit status match exactly.
 */
static void cli_usage(void)
{
    fprintf(stdout,
        "Usage: ephpm php [options] [-f] <file> [--] [args...]\n"
        "       ephpm php [options] -r <code> [--] [args...]\n"
        "       ephpm php [options] -- [args...]\n"
        "\n"
        "  -a               Run as interactive shell\n"
        "  -c <path>|<file> Look for php.ini file in this directory\n"
        "  -n               No configuration (ini) files will be used\n"
        "  -d foo[=bar]     Define INI entry foo with value 'bar'\n"
        "  -e               Generate extended information for debugger/profiler\n"
        "  -f <file>        Parse and execute <file>\n"
        "  -h               This help\n"
        "  -i               PHP information\n"
        "  -l               Syntax check only (lint)\n"
        "  -m               Show compiled in modules\n"
        "  -r <code>        Run PHP <code> without using script tags <?..?>\n"
        "  -B <begin_code>  Run PHP <begin_code> before processing input lines\n"
        "  -R <code>        Run PHP <code> for every input line\n"
        "  -F <file>        Parse and execute <file> for every input line\n"
        "  -E <end_code>    Run PHP <end_code> after processing all input lines\n"
        "  -H               Hide any passed arguments from external tools\n"
        "  -s               Output HTML syntax highlighted source\n"
        "  -v               Version number\n"
        "  -w               Output source with stripped comments and whitespace\n"
        "\n"
        "  args...          Arguments passed to script. Use -- args when first argument\n"
        "                   starts with - or script is read from stdin\n"
        "\n"
        "  --ini            Show configuration file names\n"
        "  --rf <name>      Show information about function <name>\n"
        "  --rc <name>      Show information about class <name>\n"
        "  --re <name>      Show information about extension <name>\n"
        "  --rz <name>      Show information about Zend extension <name>\n"
        "  --ri <name>      Show configuration for extension <name>\n"
    );
}

/*
 * PHP CLI main entry point. Parses argc/argv using php_getopt with
 * the same option table as the real PHP CLI, then dispatches to the
 * appropriate PHP APIs.
 *
 * Call AFTER php_embed_init(). The embed SAPI must already be running.
 * php_embed_init() starts a request automatically — we shut it down
 * first so we can start fresh CLI-mode requests.
 *
 * Returns the process exit code (0 = success).
 */
int ephpm_cli_main(int argc, char **argv)
{
    int c;
    char *php_optarg = NULL;
    int php_optind = 1;
    int result = 0;
    size_t (*orig_ub_write)(const char *, size_t) = NULL;

    char *exec_direct = NULL;   /* -r code */
    char *script_file = NULL;   /* -f file or positional */
    int mode = 0;               /* 0=standard, 'r'=run, 'l'=lint, etc. */

    char *begin_code = NULL;    /* -B */
    char *end_code = NULL;      /* -E */
    char *line_code = NULL;     /* -R */
    char *line_file = NULL;     /* -F */
    char *server_addr = NULL;   /* -S <addr> */
    int want_interactive = 0;   /* -a */
    int read_stdin = 0;         /* script/program comes from stdin */

    /* First pass: handle flags that print info and exit immediately.
     *
     * show_err = 1 (php-cli passes 1 here too, and 0 in its later passes):
     * php_getopt itself writes the "Error in argument N, char M: …" diagnostic
     * to stderr and returns PHP_GETOPT_INVALID_ARG, which the switch below
     * turns into usage + exit 1 (issue #336). The second pass keeps show_err
     * = 0 so a bad option is never reported twice — it can't be reached
     * anyway, since this pass returns first. */
    while ((c = php_getopt(argc, argv, cli_options, &php_optarg, &php_optind, 1, 2)) != -1) {
        switch (c) {
        case 'v': /* version */
            sapi_module.ub_write = ephpm_sapi_ub_write_stdout;
            /* Match stock php-cli's shape: `PHP x.y.z (cli) (built: …) (NTS)`.
             * The SAPI token is "cli" in `ephpm php` (g_cli_mode) so scripts
             * and tooling that scrape `php -v` see the CLI they expect. */
            php_printf("PHP %s (%s) (built: %s %s) (%s)\n"
                       "Copyright (c) The PHP Group\n"
                       "Zend Engine v%s, Copyright (c) Zend Technologies\n",
                       PHP_VERSION, EPHPM_SAPI_NAME, __DATE__, __TIME__,
#ifdef ZTS
                       "ZTS",
#else
                       "NTS",
#endif
                       ZEND_VERSION);
            fflush(stdout);
            return 0;

        case 'i': /* phpinfo */
            cli_begin(&orig_ub_write);
            php_print_info(0x7FFFFFFF & ~0x200); /* PHP_INFO_ALL & ~PHP_INFO_CREDITS */
            php_output_end_all();
            cli_end(orig_ub_write);
            return 0;

        case 'm': /* modules */
            cli_begin(&orig_ub_write);
            php_printf("[PHP Modules]\n");
            zend_hash_apply(&module_registry, (apply_func_t)cli_print_module);
            php_printf("\n[Zend Modules]\n");
            zend_llist_apply(&zend_extensions, (llist_apply_func_t)cli_print_extension);
            php_printf("\n");
            php_output_end_all();
            cli_end(orig_ub_write);
            return 0;

        case 'h':
        case '?':
            /* Print a help message */
            cli_usage();
            return 0;

        case PHP_GETOPT_INVALID_ARG:
            /* Unrecognized option, a `-:` flag, or a missing required option
             * argument. php_getopt already put the diagnostic on stderr (see
             * the show_err note above); php-cli then prints usage to stdout
             * and exits 1, and so do we (issue #336).
             *
             * Before this, an unknown flag fell through both getopt passes,
             * selected no mode, and left the CLI reading (empty) stdin as a
             * program — a typo'd flag silently ran nothing and reported
             * success, the worst possible outcome for a script. */
            cli_usage();
            return 1;

        case 15: /* --ini */
            cli_begin(&orig_ub_write);
            zend_eval_string(
                "echo 'Loaded Configuration File:         ' "
                ". (php_ini_loaded_file() ?: '(none)') . \"\\n\";\n"
                "$s = php_ini_scanned_files();\n"
                "if ($s) echo 'Additional .ini files parsed:      ' . $s . \"\\n\";\n",
                NULL, "ephpm --ini");
            php_output_end_all();
            cli_end(orig_ub_write);
            return 0;

        case 10: /* --rf / --rfunction  <name> */
        case 11: /* --rc / --rclass     <name> */
        case 12: /* --re / --rextension <name> */
        case 13: /* --rz / --rzendextension <name> */
        case 14: /* --ri / --rextinfo   <name> */
        {
            /* Reflection info flags, matching php-cli. Implemented via the
             * always-compiled Reflection extension, and bailout-protected.
             *
             * The name is bound as $__ephpm_r rather than interpolated into
             * the snippet: a name containing a quote would otherwise change
             * the code being evaluated.
             *
             * A bad name is caught and reported as php-cli reports it —
             * `Exception: <message>` on stdout, NOT an uncaught-exception
             * fatal — and, like php-cli, the process then exits 1 (issue
             * #335: php_cli.c sets EG(exit_status) = 1 in exactly this
             * branch; the previous comment here claimed status 0 was
             * php-cli's, which was wrong).
             *
             * The status is carried out of PHP by exit(1) in the catch block
             * rather than by a C-side sentinel: cli_eval_protected already
             * unwraps PHP 8's unwind-exit and returns EG(exit_status), which
             * is the same path `-r 'exit(1);'` takes.
             *
             * --ri keeps its own message for an absent extension ("Extension
             * 'x' not present."), also php-cli's. php-cli exits 1 there too,
             * but that is a different php_cli.c branch (PHP_CLI_MODE_-
             * REFLECTION_EXT_INFO) whose `--ri main` special case ePHPm does
             * not implement, so it is left alone here rather than half-matched.
             */
            zval reflect_name;
            ZVAL_STRING(&reflect_name, php_optarg ? php_optarg : "");
            zend_hash_str_update(
                &EG(symbol_table), "__ephpm_r", sizeof("__ephpm_r") - 1, &reflect_name);

            const char *expr;
            switch (c) {
            case 10: expr = "echo new ReflectionFunction($__ephpm_r), \"\\n\";"; break;
            case 11: expr = "echo new ReflectionClass($__ephpm_r), \"\\n\";"; break;
            case 12: expr = "echo new ReflectionExtension($__ephpm_r), \"\\n\";"; break;
            case 13: expr = "echo new ReflectionZendExtension($__ephpm_r), \"\\n\";"; break;
            default: /* --ri */
                expr = "if (!extension_loaded($__ephpm_r)) {"
                       "  echo \"Extension '\", $__ephpm_r, \"' not present.\\n\";"
                       "} else { (new ReflectionExtension($__ephpm_r))->info(); }";
                break;
            }
            char code[1024];
            snprintf(code, sizeof(code),
                "try { %s } catch (Throwable $e) {"
                "  echo 'Exception: ', $e->getMessage(), \"\\n\"; exit(1); }",
                expr);
            cli_begin(&orig_ub_write);
            result = cli_eval_protected(code, "ephpm php reflection");
            php_output_end_all();
            cli_end(orig_ub_write);
            return result;
        }

        default:
            break;
        }
    }

    /* Second pass: collect execution options */
    php_optind = 1;
    php_optarg = NULL;
    while ((c = php_getopt(argc, argv, cli_options, &php_optarg, &php_optind, 0, 2)) != -1) {
        switch (c) {
        case 'r':
            exec_direct = php_optarg;
            mode = 'r';
            break;
        case 'f':
            script_file = php_optarg;
            break;
        case 'l':
            mode = 'l';
            break;
        case 'w':
            mode = 'w';
            break;
        case 's':
            mode = 's';
            break;
        case 'd':
            /* Already applied. `-d` must take effect at module startup (the
             * OPcache/JIT decision is made in MINIT — issue #331), so the
             * Rust pre-scan collected these and ephpm_cli_add_ini_define()
             * spliced them into sapi_module.ini_entries before
             * php_module_startup(), exactly as php-cli does. php_getopt still
             * consumes the argument here so positional detection stays
             * correct. */
            break;
        case 'B':
            begin_code = php_optarg;
            break;
        case 'R':
            line_code = php_optarg;
            break;
        case 'F':
            line_file = php_optarg;
            break;
        case 'E':
            end_code = php_optarg;
            break;
        case 'a':
            want_interactive = 1;
            break;
        case 'S':
            server_addr = php_optarg;
            break;
        /* -c, -n and -d are init-time (which ini to load, and startup ini
         * overrides) and are handled by the Rust pre-scan before
         * php_embed_init; php_getopt still consumes their arguments here so
         * positional detection stays correct. */
        default:
            break;
        }
    }

    /* -a interactive shell: the PHP interactive shell lives in the standalone
     * cli SAPI (sapi/cli + the readline shell), which is not linked into the
     * embed build ePHPm uses. Rather than a silent no-op or a half-working
     * bespoke REPL that diverges from php-cli, refuse honestly. */
    if (want_interactive) {
        fprintf(stderr,
            "ephpm php: interactive shell (-a) is not supported in this build.\n"
            "The PHP interactive shell is part of the standalone php-cli SAPI, which\n"
            "is not linked into ePHPm's embedded runtime. Use `ephpm php -r <code>`,\n"
            "`ephpm php <file>`, or pipe a script to stdin instead.\n");
        return 1;
    }

    /* -S built-in server: deliberately NOT aliased to `ephpm serve`. A user who
     * asks for php's genuine built-in dev server should get exactly that or an
     * honest error — never a different server wearing its flag. The cli-server
     * SAPI (php_cli_server.c) is not part of the embed build, so it cannot be
     * provided here. */
    if (server_addr) {
        fprintf(stderr,
            "ephpm php: the PHP built-in server (-S) SAPI is not linked in this build.\n"
            "Use a full php-cli for `php -S`, or run `ephpm serve` / `ephpm dev` for\n"
            "ePHPm's own HTTP server.\n");
        return 1;
    }

    /* Define STDIN/STDOUT/STDERR before any script runs, like php-cli. */
    cli_register_file_handles();

    /* Pick up a positional script name, mirroring php-cli's condition
     * (php_cli.c): only when no script is set yet, the mode is not -r or
     * line-mode (both of which take no script file), and the previous argv
     * slot is not "--" — everything after "--" belongs to the script, so
     * `… | ephpm php -- a b` keeps reading the program from stdin. */
    /* Any of -B/-R/-F/-E selects php-cli's stdin line-processing mode, so
     * stdin is consumed as input lines rather than compiled as a program.
     * (Verified: `printf 'a\n' | php -B 'echo "S\n";'` prints only S — the
     * lines are read and discarded, not executed.) */
    int want_lines =
        (line_code != NULL || line_file != NULL || begin_code != NULL || end_code != NULL);
    if (argc > php_optind && !script_file && !exec_direct && !want_lines
        && strcmp(argv[php_optind - 1], "--") != 0) {
        script_file = argv[php_optind];
        php_optind++; /* consume it: what follows are the script's args */
    }

    /* No script named and no -r/line-mode: the program comes from stdin.
     * php-cli does this unconditionally (it does not test isatty), so an
     * interactive `ephpm php` waits on stdin exactly as `php` does. Note
     * php-cli has no `-` stdin sentinel: `php -` reports "Could not open
     * input file: -", and so does this. */
    if (!script_file && !exec_direct && !want_lines) {
        read_stdin = 1;
    }

    /* Script identity, following php-cli: the script path when one was
     * named, otherwise "Standard input code" — for stdin programs AND for
     * -r (verified against php 8.5: `php -r 'var_dump($argv);'` prints
     * $argv[0] === "Standard input code"). */
    const char *php_self = script_file ? script_file : CLI_STDIN_NAME;

    /* Make script arguments visible to userland ($argv/$argc/$_SERVER).
     * argv[php_optind - 1] is repurposed as $argv[0] (php-cli does the
     * same slot trick). */
    if (script_file || exec_direct || read_stdin || want_lines) {
        cli_register_argv(argc, argv, php_optind - 1, php_self, script_file != NULL);
    }

    /* CLI scripts routinely start with a shebang (artisan, composer, …);
     * the embed compiler does not skip it by default. */
    CG(skip_shebang) = 1;

    /* Execute based on mode */
    cli_begin(&orig_ub_write);
    if (want_lines) {
        /* -B/-R/-F/-E awk-like stdin line processor */
        result = cli_process_stdin_lines(begin_code, line_code, line_file, end_code);
    } else if (mode == 'r' && exec_direct) {
        /* -r "code" */
        result = cli_eval_protected(exec_direct, "Command line code");
    } else if (mode == 'l' || mode == 'w' || mode == 's') {
        /* -l (lint), -w (strip), -s (highlight): a named file, or the
         * program on stdin when none was named. */
        result = cli_scan_protected(mode, script_file, php_self);
    } else {
        /* Standard mode: execute a named script, or the program read from
         * stdin (`ephpm php < file.php`, `… | ephpm php`). Both compile from
         * a FILE*-backed handle, so `<?php` tags, $argv[0] and file
         * semantics match php-cli — unlike -r, which is raw code. */
        result = cli_execute_script_protected(script_file);
    }

    /* php-cli parity (#334): request shutdown — user shutdown functions,
     * then destructors, then the output flush (which also ends any output
     * buffers the mode left open) — runs BEFORE the stdout ub_write is torn
     * down, and the exit status is re-read afterwards so exit() inside a
     * shutdown function sets it. See cli_shutdown_request. */
    result = cli_shutdown_request(result);
    cli_end(orig_ub_write);

    return result;
}
