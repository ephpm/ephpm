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
#include "Zend/zend.h"
#include "Zend/zend_ini.h"
#include "Zend/zend_stream.h"
#include "Zend/zend_call_stack.h"
#include "Zend/zend_exceptions.h"
#include "Zend/zend_globals.h"
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
 * state must be thread-local to avoid races. On non-ZTS builds (Windows),
 * thread-local storage is harmless (single thread executes PHP).
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

/* Non-NULL sentinel for SG(server_context). sapi_activate() only parses the
 * POST body when server_context is set; the value itself is never dereferenced
 * by our SAPI, so a single shared marker address suffices. */
static int ephpm_server_context_marker = 0;

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

    /* Disable stack size checking (tokio threads have small stacks) */
    EG(max_allowed_stack_size) = 0;

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
 * zend_set_timeout() directly calls sigaction(SIGPROF) + setitimer(ITIMER_PROF),
 * bypassing the zend_signal_* system. Must also be a no-op.
 */
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

/*
 * zend_call_stack_init() probes the current thread's stack boundaries
 * on every request startup. It can fail on tokio's spawn_blocking threads
 * which have non-standard stack layouts. Since we disable stack checking
 * (EG(max_allowed_stack_size) = 0), this init is unnecessary.
 */
void __wrap_zend_call_stack_init(void)
{
    /* no-op — stack checking is disabled */
}

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

    /* Update SAPI name visible to phpinfo() and $_SERVER['SERVER_SOFTWARE'] */
    sapi_module.name = "ephpm";
    sapi_module.pretty_name = "ePHPm Embedded Server";
}

/*
 * Apply INI settings after php_embed_init().
 *
 * Disables stack size checking which fails on tokio's spawn_blocking
 * threads (small default stack). The embed SAPI doesn't process -d
 * command-line flags, so we set INI entries programmatically.
 */
int ephpm_apply_ini_settings(void)
{
    zend_string *key;
    zend_string *val;

    /* Disable stack size checking — fails on tokio's spawn_blocking
     * threads which have a small default stack. */
    key = zend_string_init(
        "zend.max_allowed_stack_size",
        sizeof("zend.max_allowed_stack_size") - 1, 1);
    val = zend_string_init("0", 1, 1);
    zend_alter_ini_entry(key, val, PHP_INI_SYSTEM, PHP_INI_STAGE_RUNTIME);
    zend_string_release(val);
    zend_string_release(key);
    EG(max_allowed_stack_size) = 0;

    return 0;
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
 *   0  on success
 *  -1  if php_request_startup failed (only on cold start)
 *  -2  if PHP bailed out (fatal error, exit(), die())
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

    if (php_request_startup() != SUCCESS) {
        return -1;
    }
    request_active = 1;

    /* tokio spawn_blocking threads have small stacks; request startup may
     * reset this guard, so clear it afterwards. */
    EG(max_allowed_stack_size) = 0;

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
     * next request's php_request_shutdown(). */
    for (size_t i = 0; i < request_ini_count; i++) {
        zend_string *zkey = zend_string_init(request_ini_keys[i],
                                             strlen(request_ini_keys[i]), 0);
        zend_string *zval = zend_string_init(request_ini_vals[i],
                                             strlen(request_ini_vals[i]), 0);
        zend_alter_ini_entry(zkey, zval, PHP_INI_SYSTEM, PHP_INI_STAGE_ACTIVATE);
        zend_string_release(zval);
        zend_string_release(zkey);
    }

    /* Reset PHP's last-error tracking so we can tell whether THIS script
     * raised a fatal (vs. a value carried over from a prior request). */
    PG(last_error_type) = 0;

    /* Execute the script with bailout protection.
     * PHP's zend_try/zend_catch uses setjmp/longjmp. */
    int result = 0;
    int fatal_bailout = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        zend_file_handle file_handle;
        zend_stream_init_filename(&file_handle, filename);
        php_execute_script(&file_handle);

        /* PHP 8.x: exit()/die() throws an unwind exit exception instead
         * of calling zend_bailout(). Treat it like the old bailout path,
         * but DO NOT mark it as a fatal bailout — exit() is intentional
         * and should preserve whatever status the script set. */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
            result = -2;
        }
    } else {
        /* PHP bailed out via zend_bailout() — out-of-memory, max
         * execution time, etc. Older fatal classes hit this path. */
        result = -2;
        fatal_bailout = 1;
    }
    EG(bailout) = __orig_bailout;

    /* Capture response data while the request is still active */
    capture_response_headers();
    response_status_code = SG(sapi_headers).http_response_code;

    /* Decide whether to override status with 500. There are two paths:
     *
     *   1. zend_bailout() longjmps out of execute (legacy fatal path):
     *      caught by SETJMP above — fatal_bailout = 1.
     *
     *   2. PHP 8.x uncaught Throwable: zend_exception_error() calls
     *      zend_error_va(... | E_DONT_BAIL ...) which prints the fatal
     *      message and lets php_execute_script return normally. SETJMP
     *      sees nothing, so we MUST also check PG(last_error_type) to
     *      catch this case. Without it, "Fatal error: Uncaught Error:
     *      Call to undefined function ..." comes back as 200 OK.
     *
     * Either way, we only override when the script hasn't already set
     * an explicit error status (PHP exit() / http_response_code()). */
    int fatal_error_mask = E_ERROR | E_CORE_ERROR | E_COMPILE_ERROR
                           | E_USER_ERROR | E_RECOVERABLE_ERROR | E_PARSE;
    int hit_fatal = fatal_bailout || (PG(last_error_type) & fatal_error_mask);
    if (hit_fatal && response_status_code == 200) {
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

/* ── Function entry table (null-terminated) ──────────────────── */

static const zend_function_entry ephpm_kv_functions[] = {
    PHP_FE(ephpm_kv_get,       arginfo_ephpm_kv_get)
    PHP_FE(ephpm_kv_set,       arginfo_ephpm_kv_set)
    PHP_FE(ephpm_kv_setnx,     arginfo_ephpm_kv_setnx)
    PHP_FE(ephpm_kv_del,       arginfo_ephpm_kv_del)
    PHP_FE(ephpm_kv_exists,    arginfo_ephpm_kv_exists)
    PHP_FE(ephpm_kv_incr,      arginfo_ephpm_kv_incr)
    PHP_FE(ephpm_kv_decr,      arginfo_ephpm_kv_decr)
    PHP_FE(ephpm_kv_incr_by,   arginfo_ephpm_kv_incr_by)
    PHP_FE(ephpm_kv_expire,    arginfo_ephpm_kv_expire)
    PHP_FE(ephpm_kv_ttl,       arginfo_ephpm_kv_ttl)
    PHP_FE(ephpm_kv_pttl,      arginfo_ephpm_kv_pttl)
    PHP_FE(ephpm_kv_flush_all, arginfo_ephpm_kv_flush_all)
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
 * On NTS builds (Windows) PHP execution is serialized in-process, so the
 * lock is simply uncontended — the SETNX/DEL pair still balances.
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
    sm->additional_functions = ephpm_kv_functions;
    int ret = php_module_startup(sm, NULL);

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
    EG(max_allowed_stack_size) = 0;
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
 * Helper: execute code or a file with bailout protection.
 * If `code` is non-NULL, evaluates it via zend_eval_string.
 * If `filename` is non-NULL, executes it via php_execute_script.
 * Returns the PHP exit status.
 */
static int cli_execute_protected(const char *code, const char *filename)
{
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        if (code) {
            zend_eval_string((char *)code, NULL, "ephpm php -r");
        } else if (filename) {
            zend_file_handle file_handle;
            zend_stream_init_filename(&file_handle, filename);
            php_execute_script(&file_handle);
        }

        /* PHP 8.x: exit() throws an unwind exit exception instead of
         * calling zend_bailout(). Check for it after normal return. */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
            result = EG(exit_status);
        }
    } else {
        /* PHP bailed out (fatal error) */
        result = EG(exit_status);
        if (result == 0) result = 1;
    }
    EG(bailout) = __orig_bailout;
    return result;
}

/* PHP CLI option table — matches the real PHP CLI SAPI options.
 * Used by php_getopt() to parse argc/argv. */
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
    {'s', 0, "syntax-highlight"},
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

    /* First pass: handle flags that print info and exit immediately */
    while ((c = php_getopt(argc, argv, cli_options, &php_optarg, &php_optind, 0, 2)) != -1) {
        switch (c) {
        case 'v': /* version */
            sapi_module.ub_write = ephpm_sapi_ub_write_stdout;
            php_printf("PHP %s (ephpm) (built: %s %s)\n"
                       "Copyright (c) The PHP Group\n"
                       "Zend Engine v%s, Copyright (c) Zend Technologies\n",
                       PHP_VERSION, __DATE__, __TIME__,
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
                "  -z <file>        Load Zend extension <file>\n"
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
            return 0;

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
        default:
            break;
        }
    }

    /* Positional argument: if no -f and no -r, first non-option arg is the script */
    if (!script_file && !exec_direct && php_optind < argc && argv[php_optind][0] != '-') {
        script_file = argv[php_optind];
    }

    /* Execute based on mode */
    if (mode == 'r' && exec_direct) {
        /* -r "code" */
        cli_begin(&orig_ub_write);
        result = cli_execute_protected(exec_direct, NULL);
        cli_end(orig_ub_write);
    } else if (mode == 'l' && script_file) {
        /* -l file (syntax check) */
        cli_begin(&orig_ub_write);
        {
            zend_file_handle file_handle;
            zend_stream_init_filename(&file_handle, script_file);
            if (php_lint_script(&file_handle) == SUCCESS) {
                php_printf("No syntax errors detected in %s\n", script_file);
            } else {
                result = 255;
            }
        }
        php_output_end_all();
        cli_end(orig_ub_write);
    } else if (script_file) {
        /* Execute a file (standard mode, -w, -s) */
        cli_begin(&orig_ub_write);
        if (mode == 'w') {
            char code[4096];
            snprintf(code, sizeof(code),
                "echo php_strip_whitespace('%s');", script_file);
            cli_execute_protected(code, NULL);
            php_output_end_all();
        } else if (mode == 's') {
            char code[4096];
            snprintf(code, sizeof(code),
                "highlight_file('%s');", script_file);
            cli_execute_protected(code, NULL);
            php_output_end_all();
        } else {
            result = cli_execute_protected(NULL, script_file);
        }
        cli_end(orig_ub_write);
    } else {
        /* No script or code provided */
        fprintf(stderr, "ephpm php: no input file\n"
                        "Run 'ephpm php -h' for usage information.\n");
        return 1;
    }

    return result;
}
