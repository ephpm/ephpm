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
 *   php_embed_init()          — module startup (no request started)
 *   ephpm_install_sapi()      — override default callbacks with ours
 *   ephpm_execute_request()×N — per-request: startup → execute → capture
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
static void capture_response_headers(void)
{
    headers_buf_len = 0;

    zend_llist_position pos;
    sapi_header_struct *h = (sapi_header_struct *)
        zend_llist_get_first_ex(&SG(sapi_headers).headers, &pos);

    while (h) {
        size_t needed = h->header_len + 1; /* header + newline */
        while (headers_buf_len + needed > headers_buf_cap) {
            size_t new_cap = headers_buf_cap ? headers_buf_cap * 2 : 1024;
            char *new_buf = realloc(headers_buf, new_cap);
            if (!new_buf) return;
            headers_buf = new_buf;
            headers_buf_cap = new_cap;
        }
        memcpy(headers_buf + headers_buf_len, h->header, h->header_len);
        headers_buf[headers_buf_len + h->header_len] = '\n';
        headers_buf_len += needed;

        h = (sapi_header_struct *)
            zend_llist_get_next_ex(&SG(sapi_headers).headers, &pos);
    }
}

/* ===================================================================
 * Public API — called from Rust via FFI
 * =================================================================== */

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
    zend_string *key = zend_string_init(
        "zend.max_allowed_stack_size",
        sizeof("zend.max_allowed_stack_size") - 1, 1);
    zend_string *val = zend_string_init("0", 1, 1);
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
 * Execute a PHP request with proper lifecycle management.
 *
 * 1. Shuts down any previous request
 * 2. Populates SG(request_info) from the data set by Rust
 * 3. Calls php_request_startup() (triggers register_server_variables, etc.)
 * 4. Executes the script with bailout protection
 * 5. Captures response data (headers, status, output already in buffer)
 *
 * Returns:
 *   0  on success
 *  -1  if php_request_startup failed
 *  -2  if PHP bailed out (fatal error, exit(), die())
 */
int ephpm_execute_request(const char *filename)
{
    /* Reset output and response buffers */
    output_len = 0;
    headers_buf_len = 0;

    /* Disable stack size checking */
    EG(max_allowed_stack_size) = 0;

    /* Shut down the previous request if one is active */
    if (request_active) {
        php_request_shutdown(NULL);
        request_active = 0;
    }

    /* Populate SAPI request info before php_request_startup.
     * PHP reads these during request startup to set up superglobals. */
    SG(request_info).request_method = (char *)req_method;
    SG(request_info).request_uri = (char *)req_uri;
    SG(request_info).query_string = (char *)req_query_string;
    SG(request_info).content_type = req_content_type;
    SG(request_info).cookie_data = (char *)req_cookie_data;
    SG(request_info).content_length = (long)req_post_data_len;
    SG(request_info).path_translated = (char *)req_path_translated;
    SG(request_info).proto_num = 1001; /* HTTP/1.1 */

    /* Start the new request. This calls register_server_variables,
     * read_cookies, and other SAPI callbacks. */
    if (php_request_startup() == FAILURE) {
        return -1;
    }
    request_active = 1;

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
    } else {
        /* PHP bailed out (fatal error, exit(), die()) */
        result = -2;
    }
    EG(bailout) = __orig_bailout;

    /* Capture response data while the request is still active */
    capture_response_headers();
    response_status_code = SG(sapi_headers).http_response_code;

    /* Note: we do NOT call php_request_shutdown here.
     * It will be called at the start of the next request
     * or by php_embed_shutdown() at process exit. */

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
