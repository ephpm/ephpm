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
#include "main/php_streams.h"
#include "main/php_output.h"
#include "Zend/zend.h"
#include "Zend/zend_ini.h"
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
 * OPcache clustered invalidation (design: opcache-clustering.md, phase 1)
 *
 * ephpm_opcache_invalidate_under(docroot) walks opcache_get_status(true)['scripts']
 * and calls opcache_invalidate($path, true) for every cached script whose
 * full_path starts with the vhost's docroot prefix. Returns the number of
 * scripts invalidated, or -1 if OPcache is not loaded / not enabled.
 *
 * Runs entirely inside userland by evaluating a small PHP snippet with
 * zend_eval_string_ex() under a SETJMP bailout guard. This avoids the
 * fragility of walking OPcache's internal HashTable via extension APIs
 * (accelerator_shm layout has shifted between PHP minors) and matches the
 * pattern already used by cli_execute_protected() for CLI mode.
 *
 * Must be called on a TSRM-registered thread WITH an active PHP request
 * (i.e. at the start of ephpm_execute_request(), after php_request_startup).
 * Callers gate the invocation with a Rust-side per-vhost version comparison
 * so the actual invalidation runs only when a deploy has advanced the
 * cluster-wide version key, not on every request.
 * =================================================================== */

/* Small PHP snippet that returns the number of invalidated scripts, or -1
 * when opcache_get_status is unavailable. The prefix is inlined as a
 * single-quoted literal — the C side escapes single quotes and backslashes
 * before splicing it in. force=true so the invalidation drops the bytecode
 * even if the file's mtime hasn't advanced (deploys often keep timestamps). */
static const char EPHPM_OPCACHE_SNIPPET_HEAD[] =
    "return (function(){"
    "if (!function_exists('opcache_get_status') || "
    "!function_exists('opcache_invalidate')) { return -1; }"
    "$s = @opcache_get_status(true);"
    "if (!is_array($s) || empty($s['scripts'])) { return 0; }"
    "$prefix = '";
static const char EPHPM_OPCACHE_SNIPPET_TAIL[] =
    "'; $n = 0;"
    "foreach ($s['scripts'] as $p => $_info) {"
    "if (strncmp($p, $prefix, strlen($prefix)) === 0) {"
    "opcache_invalidate($p, true); $n++;"
    "}}"
    "return $n;"
    "})();";

/* Escape a docroot for embedding inside a single-quoted PHP literal.
 * Only ' and \ need escaping in that context. Writes into `out` (which must
 * have capacity `out_cap`) and null-terminates. Returns 1 on success, 0 if
 * the escaped string would not fit. */
static int ephpm_opcache_escape_prefix(const char *prefix, char *out, size_t out_cap)
{
    size_t o = 0;
    for (size_t i = 0; prefix[i] != '\0'; i++) {
        char c = prefix[i];
        if (c == '\'' || c == '\\') {
            if (o + 2 >= out_cap) return 0;
            out[o++] = '\\';
            out[o++] = c;
        } else {
            if (o + 1 >= out_cap) return 0;
            out[o++] = c;
        }
    }
    if (o + 1 > out_cap) return 0;
    out[o] = '\0';
    return 1;
}

/*
 * Invalidate every cached OPcache script whose path starts with `docroot`.
 *
 * Returns the number of scripts invalidated (>= 0), or -1 if OPcache is not
 * available (extension missing / disabled / snippet compile failed / bailout).
 * Must be called from a TSRM-registered thread with an active PHP request.
 */
long ephpm_opcache_invalidate_under(const char *docroot)
{
    if (!docroot || docroot[0] == '\0') {
        return -1;
    }

    /* Escape the docroot into the assembled PHP snippet. 2048 covers the
     * longest realistic filesystem path with plenty of headroom. */
    char escaped[2048];
    if (!ephpm_opcache_escape_prefix(docroot, escaped, sizeof(escaped))) {
        return -1;
    }

    /* Assemble HEAD + escaped prefix + TAIL into a single null-terminated
     * buffer for zend_eval_string_ex. */
    size_t head_len = sizeof(EPHPM_OPCACHE_SNIPPET_HEAD) - 1;
    size_t tail_len = sizeof(EPHPM_OPCACHE_SNIPPET_TAIL) - 1;
    size_t esc_len = strlen(escaped);
    size_t total = head_len + esc_len + tail_len + 1;
    char *snippet = (char *)malloc(total);
    if (!snippet) {
        return -1;
    }
    memcpy(snippet, EPHPM_OPCACHE_SNIPPET_HEAD, head_len);
    memcpy(snippet + head_len, escaped, esc_len);
    memcpy(snippet + head_len + esc_len, EPHPM_OPCACHE_SNIPPET_TAIL, tail_len);
    snippet[total - 1] = '\0';

    long count = -1;
    zval retval;
    ZVAL_UNDEF(&retval);

    /* SETJMP guard: a bailout inside opcache_get_status / opcache_invalidate
     * (shouldn't happen in normal builds, but OOM / OPcache-in-bad-state can
     * still trip it) must not unwind through Rust. */
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;
    EG(bailout) = &__bailout;

    if (SETJMP(__bailout) == 0) {
        int rc = zend_eval_string_ex(snippet, &retval, "ephpm_opcache_invalidate", 0);
        if (rc == SUCCESS) {
            if (Z_TYPE(retval) == IS_LONG) {
                count = (long)Z_LVAL(retval);
            } else if (Z_TYPE(retval) == IS_DOUBLE) {
                count = (long)Z_DVAL(retval);
            }
        }
        /* Clear ANY pending exception the snippet raised. This eval runs in
         * the still-active previous/initial request, immediately before the
         * next request's script executes — a leaked pending exception would
         * surface inside that unrelated script. The snippet is defensive
         * (function_exists guards, @-suppressed status call), so a real
         * exception here is exceptional; report it as "unavailable". */
        if (EG(exception)) {
            if (!zend_is_unwind_exit(EG(exception))) {
                count = -1;
            }
            zend_clear_exception();
        }
    } else {
        /* zend_bailout() longjmped out of the snippet. Nothing to do — the
         * live request context is still valid, we just report -1. */
        count = -1;
    }

    EG(bailout) = __orig_bailout;
    zval_ptr_dtor(&retval);
    free(snippet);
    return count;
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
    array_init(arr);
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
            add_assoc_str(arr, keys[i], joined);
        } else {
            add_assoc_string(arr, keys[i], (char *)v);
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

    /* Build the Envelope object. */
    object_init_ex(return_value, ephpm_worker_envelope_ce);

    zval tmp;
    ephpm_worker_fill_str_array(&tmp, req.server_var_count,
                                req.server_var_keys, req.server_var_vals);
    ephpm_worker_set_prop_array(return_value, "serverVars", &tmp);

    ephpm_worker_fill_str_array(&tmp, req.header_count,
                                req.header_keys, req.header_vals);
    ephpm_worker_set_prop_array(return_value, "headers", &tmp);

    ephpm_worker_parse_cookies(&tmp, req.cookie_data);
    ephpm_worker_set_prop_array(return_value, "cookies", &tmp);

    ephpm_worker_parse_query(&tmp, req.query_string);
    ephpm_worker_set_prop_array(return_value, "query", &tmp);

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

PHP_METHOD(Ephpm_Worker_Envelope, serverVars)  { ephpm_worker_return_prop(INTERNAL_FUNCTION_PARAM_PASSTHRU, "serverVars"); }
PHP_METHOD(Ephpm_Worker_Envelope, headers)     { ephpm_worker_return_prop(INTERNAL_FUNCTION_PARAM_PASSTHRU, "headers"); }
PHP_METHOD(Ephpm_Worker_Envelope, cookies)     { ephpm_worker_return_prop(INTERNAL_FUNCTION_PARAM_PASSTHRU, "cookies"); }
PHP_METHOD(Ephpm_Worker_Envelope, query)       { ephpm_worker_return_prop(INTERNAL_FUNCTION_PARAM_PASSTHRU, "query"); }

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
 *   -2  a fatal / zend_bailout unwound out of the framework (recycle + the
 *       Rust supervisor fulfils any parked oneshot with a 500)
 */
int ephpm_worker_run(const char *script)
{
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        /* Worker entrypoints are routinely composer bin proxies / CLI-style
         * scripts with a "#!/usr/bin/env php" shebang. The CLI SAPI skips
         * that line; without this flag the embed compiler treats it as output
         * BEFORE the first statement — a fatal compile error for any script
         * opening with a namespace/declare statement (composer proxies do). */
        CG(skip_shebang) = 1;

        zend_file_handle file_handle;
        zend_stream_init_filename(&file_handle, script);
        php_execute_script(&file_handle);

        /* exit()/die() throws an unwind-exit exception rather than bailing;
         * treat it as a clean loop end (the framework asked to stop). */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
        }

        /* The script ended with a request still in flight (exit()/die()
         * mid-request, or a break out of the loop without send_response).
         * Deliver what the request actually produced — SAPI status, headers
         * emitted via header()/setcookie(), and the captured echo output —
         * instead of letting the parked oneshot die with the thread (which
         * would turn every wp_die()/admin-ajax exit into a bogus 500). This
         * is safe here: unwind-exit is clean stack unwinding, not a bailout,
         * so SAPI globals and the capture buffers are intact. */
        if (req_in_flight && g_worker_ops.send_response) {
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
            g_worker_ops.send_response(status > 0 ? status : 200,
                                       hbuf.s ? ZSTR_VAL(hbuf.s) : "",
                                       hbuf.s ? ZSTR_LEN(hbuf.s) : 0,
                                       output_buf ? output_buf : "",
                                       output_len);
            smart_str_free(&hbuf);
            output_len = 0;
            req_in_flight = 0;
            result = 1;
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
