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

/* ===== Output buffer ===== */

static char *output_buf = NULL;
static size_t output_len = 0;
static size_t output_cap = 0;

/* ===== Response header buffer ===== */
/* Stored as "Name: Value\n" lines after script execution */

static char *headers_buf = NULL;
static size_t headers_buf_len = 0;
static size_t headers_buf_cap = 0;

/* ===== Saved response status ===== */

static int response_status_code = 200;

/* ===== Request info ===== */
/* Pointers into Rust-owned CStrings, valid only during execution */

static const char *req_method = NULL;
static const char *req_uri = NULL;
static const char *req_query_string = NULL;
static const char *req_content_type = NULL;
static const char *req_cookie_data = NULL;
static const char *req_post_data = NULL;
static size_t req_post_data_len = 0;
static size_t req_post_data_offset = 0;
static const char *req_path_translated = NULL;

/* ===== Server variables ===== */

#define MAX_SERVER_VARS 128

static struct {
    const char *key;
    const char *value;
} server_vars[MAX_SERVER_VARS];

static int server_var_count = 0;

/* Track whether a PHP request is currently active */
static int request_active = 0;

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
 * Future: when ZTS + worker pool lands, we could add a thread-safe
 * signal forwarding layer that delivers signals only to the PHP thread.
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
 * Uses PHP_INI_SYSTEM + PHP_INI_STAGE_RUNTIME so the value takes effect
 * immediately and cannot be overridden by userland ini_set().
 * Call before ephpm_execute_request().
 */
void ephpm_request_set_ini(const char *key, const char *value)
{
    zend_string *zkey = zend_string_init(key, strlen(key), 0);
    zend_string *zval = zend_string_init(value, strlen(value), 0);
    zend_alter_ini_entry(zkey, zval, PHP_INI_SYSTEM, PHP_INI_STAGE_RUNTIME);
    zend_string_release(zval);
    zend_string_release(zkey);
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
 * For the NTS MVP (single PHP worker, mutex-serialized), this is safe.
 * The ZTS worker pool milestone will use proper per-worker request
 * lifecycle management.
 *
 * Returns:
 *   0  on success
 *  -1  if php_request_startup failed (only on cold start)
 *  -2  if PHP bailed out (fatal error, exit(), die())
 */
int ephpm_execute_request(const char *filename)
{
    /* Reset output and response buffers */
    output_len = 0;
    headers_buf_len = 0;

    /* Disable stack size checking */
    EG(max_allowed_stack_size) = 0;

    /* Update SAPI request info for this HTTP request */
    SG(request_info).request_method = (char *)req_method;
    SG(request_info).request_uri = (char *)req_uri;
    SG(request_info).query_string = (char *)req_query_string;
    SG(request_info).content_type = req_content_type;
    SG(request_info).cookie_data = (char *)req_cookie_data;
    SG(request_info).content_length = (long)req_post_data_len;
    SG(request_info).path_translated = (char *)req_path_translated;
    SG(request_info).proto_num = 1001; /* HTTP/1.1 */

    /* Reset per-request SAPI state */
    SG(headers_sent) = 0;
    SG(request_info).no_headers = 0;
    SG(sapi_headers).http_response_code = 200;
    req_post_data_offset = 0;

    /* Reset POST reading state so PHP re-reads body data.
     * -1 means "not yet read"; PHP will call our read_post callback.
     * Also close the request_body stream so php://input reads fresh. */
    SG(read_post_bytes) = -1;
    if (SG(request_info).request_body) {
        php_stream_close(SG(request_info).request_body);
        SG(request_info).request_body = NULL;
    }

    /* Destroy old superglobal arrays so they don't carry stale data.
     * PG(http_globals) holds zvals for $_GET, $_POST, $_SERVER, etc. */
    for (int i = 0; i < NUM_TRACK_VARS; i++) {
        if (Z_TYPE(PG(http_globals)[i]) != IS_UNDEF) {
            zval_ptr_dtor(&PG(http_globals)[i]);
            ZVAL_UNDEF(&PG(http_globals)[i]);
        }
    }

    /* Clear the response header list from the previous request */
    zend_llist_clean(&SG(sapi_headers).headers);

    /* Rebuild superglobals by manually creating fresh arrays and
     * populating them. We can't call php_hash_environment() because
     * it depends on internal state set up by php_request_startup().
     *
     * $_SERVER — populated via our register_server_variables callback */
    zval server_vars_zv;
    array_init(&server_vars_zv);
    ephpm_sapi_register_server_variables(&server_vars_zv);
    zend_hash_update(&EG(symbol_table),
        zend_string_init("_SERVER", sizeof("_SERVER") - 1, 0),
        &server_vars_zv);
    ZVAL_COPY(&PG(http_globals)[TRACK_VARS_SERVER], &server_vars_zv);

    /* $_GET — parse from query string */
    zval get_vars;
    array_init(&get_vars);
    if (req_query_string && *req_query_string) {
        /* Use PHP's query string parser */
        char *qs_copy = estrdup(req_query_string);
        sapi_module.treat_data(PARSE_STRING, qs_copy, &get_vars);
        /* treat_data frees qs_copy */
    }
    zend_hash_update(&EG(symbol_table),
        zend_string_init("_GET", sizeof("_GET") - 1, 0),
        &get_vars);
    ZVAL_COPY(&PG(http_globals)[TRACK_VARS_GET], &get_vars);

    /* Pre-read POST data so php://input and multipart parsing work.
     * sapi_read_post_data() calls our read_post callback and creates
     * the request_body stream. Must happen before sapi_handle_post(). */
    if (req_post_data && req_post_data_len > 0) {
        sapi_read_post_data();
    }

    /* $_POST + $_FILES — use PHP's built-in POST handler.
     * For multipart/form-data, this invokes rfc1867 parsing which
     * populates both $_POST and $_FILES. For url-encoded, it populates
     * $_POST. For other content types, both stay empty. */
    zval post_vars;
    array_init(&post_vars);
    zval files_vars;
    array_init(&files_vars);
    ZVAL_COPY(&PG(http_globals)[TRACK_VARS_POST], &post_vars);
    ZVAL_COPY(&PG(http_globals)[TRACK_VARS_FILES], &files_vars);
    if (req_post_data && req_post_data_len > 0 && req_content_type) {
        sapi_handle_post(&PG(http_globals)[TRACK_VARS_POST]);
        /* sapi_handle_post may have populated FILES directly */
        zval_ptr_dtor(&files_vars);
        ZVAL_COPY(&files_vars, &PG(http_globals)[TRACK_VARS_FILES]);
        /* Re-read POST from http_globals in case handler replaced it */
        zval_ptr_dtor(&post_vars);
        ZVAL_COPY(&post_vars, &PG(http_globals)[TRACK_VARS_POST]);
    }
    zend_hash_update(&EG(symbol_table),
        zend_string_init("_POST", sizeof("_POST") - 1, 0),
        &post_vars);
    ZVAL_COPY_VALUE(&PG(http_globals)[TRACK_VARS_POST], &post_vars);

    /* $_COOKIE — parse from cookie header.
     * Cookies are "key=value" pairs separated by "; ".
     * We parse manually since treat_data(PARSE_COOKIE) requires
     * internal SAPI state that isn't set up in our reuse model. */
    zval cookie_vars;
    array_init(&cookie_vars);
    if (req_cookie_data && *req_cookie_data) {
        char *cookie_copy = estrdup(req_cookie_data);
        char *saveptr = NULL;
        char *pair = strtok_r(cookie_copy, ";", &saveptr);
        while (pair) {
            /* Skip leading whitespace */
            while (*pair == ' ') pair++;
            char *eq = strchr(pair, '=');
            if (eq) {
                *eq = '\0';
                char *val = eq + 1;
                /* Trim trailing whitespace from value */
                size_t vlen = strlen(val);
                while (vlen > 0 && val[vlen - 1] == ' ') vlen--;
                php_register_variable_safe(pair, val, vlen, &cookie_vars);
            }
            pair = strtok_r(NULL, ";", &saveptr);
        }
        efree(cookie_copy);
    }
    zend_hash_update(&EG(symbol_table),
        zend_string_init("_COOKIE", sizeof("_COOKIE") - 1, 0),
        &cookie_vars);
    ZVAL_COPY(&PG(http_globals)[TRACK_VARS_COOKIE], &cookie_vars);

    /* $_FILES — populated by sapi_handle_post() for multipart requests */
    zend_hash_update(&EG(symbol_table),
        zend_string_init("_FILES", sizeof("_FILES") - 1, 0),
        &files_vars);
    ZVAL_COPY_VALUE(&PG(http_globals)[TRACK_VARS_FILES], &files_vars);

    /* $_REQUEST — merge of $_GET + $_POST + $_COOKIE per request_order */
    zval request_vars;
    array_init(&request_vars);
    zend_hash_copy(Z_ARRVAL(request_vars), Z_ARRVAL(get_vars), zval_add_ref);
    zend_hash_copy(Z_ARRVAL(request_vars), Z_ARRVAL(post_vars), zval_add_ref);
    zend_hash_copy(Z_ARRVAL(request_vars), Z_ARRVAL(cookie_vars), zval_add_ref);
    zend_hash_update(&EG(symbol_table),
        zend_string_init("_REQUEST", sizeof("_REQUEST") - 1, 0),
        &request_vars);

    /* Re-disable stack checking (request startup may reset it) */
    EG(max_allowed_stack_size) = 0;

    /* Execute the script with bailout protection.
     * PHP's zend_try/zend_catch uses setjmp/longjmp. */
    int result = 0;
    JMP_BUF *__orig_bailout = EG(bailout);
    JMP_BUF __bailout;

    EG(bailout) = &__bailout;
    if (SETJMP(__bailout) == 0) {
        zend_file_handle file_handle;
        zend_stream_init_filename(&file_handle, filename);
        php_execute_script(&file_handle);

        /* PHP 8.x: exit()/die() throws an unwind exit exception instead
         * of calling zend_bailout(). Treat it like the old bailout path. */
        if (EG(exception) && zend_is_unwind_exit(EG(exception))) {
            zend_clear_exception();
            result = -2;
        }
    } else {
        /* PHP bailed out (fatal error) */
        result = -2;
    }
    EG(bailout) = __orig_bailout;

    /* Capture response data while the request is still active */
    capture_response_headers();
    response_status_code = SG(sapi_headers).http_response_code;

    /* Note: we do NOT call php_request_shutdown here.
     * We reuse the single embed request for all HTTP requests.
     * php_embed_shutdown() handles final cleanup at process exit. */

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
    long (*del)(const char *key);
    int  (*exists)(const char *key);
    int  (*incr_by)(const char *key, long long delta, long long *result);
    int  (*expire)(const char *key, long long ttl_ms);
    long long (*pttl)(const char *key);
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

/* ── Argument info for reflection (arginfo) ──────────────────── */

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_get, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_ephpm_kv_set, 0, 0, 2)
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

/* ── Function entry table (null-terminated) ──────────────────── */

static const zend_function_entry ephpm_kv_functions[] = {
    PHP_FE(ephpm_kv_get,      arginfo_ephpm_kv_get)
    PHP_FE(ephpm_kv_set,      arginfo_ephpm_kv_set)
    PHP_FE(ephpm_kv_del,      arginfo_ephpm_kv_del)
    PHP_FE(ephpm_kv_exists,   arginfo_ephpm_kv_exists)
    PHP_FE(ephpm_kv_incr,     arginfo_ephpm_kv_incr)
    PHP_FE(ephpm_kv_decr,     arginfo_ephpm_kv_decr)
    PHP_FE(ephpm_kv_incr_by,  arginfo_ephpm_kv_incr_by)
    PHP_FE(ephpm_kv_expire,   arginfo_ephpm_kv_expire)
    PHP_FE(ephpm_kv_ttl,      arginfo_ephpm_kv_ttl)
    PHP_FE_END
};

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
 * Pre-initialization: set additional_functions on the embed module.
 * Must be called BEFORE php_embed_init() so that php_module_startup()
 * registers these functions during module initialization.
 */
void ephpm_pre_init(void)
{
    php_embed_module.additional_functions = ephpm_kv_functions;
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
