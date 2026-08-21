/*
 * code_bundle_hooks.c — C-level PHP overrides backing the in-memory code
 * bundle (see crates/ephpm-php/src/code_bundle.rs and
 * site/content/roadmap/in-memory-code-bundle.md).
 *
 * We interpose on three PHP indirection points that are *designed* to be
 * overridden (OPcache itself overrides zend_compile_file / zend_resolve_path),
 * install them once at SAPI init, and delegate to the saved originals on a
 * bundle MISS so behaviour for anything not in the bundle is byte-for-byte
 * unchanged:
 *
 *   1. zend_resolve_path        — include/require path resolution. On a hit we
 *                                 return the bundle's canonical absolute path so
 *                                 __FILE__/__DIR__/realpath() math stays correct.
 *   2. zend_stream_open_function — the compiler's source open (primary script +
 *                                 every include/require). On a hit we serve the
 *                                 source from RAM via a zend_stream reader; the
 *                                 filesystem is never touched.
 *   3. php_plain_files_wrapper.url_stat — userland file_exists / is_file /
 *                                 stat / filemtime and OPcache's own probing.
 *                                 On a hit we fill php_stream_statbuf from the
 *                                 bundle manifest.
 *
 * The bundle data lives in Rust (immutable, process-lifetime). These hooks
 * query it through a small callback vtable installed by
 * ephpm_bundle_install_hooks(). CRITICAL FFI invariant (CLAUDE.md rule 3): the
 * Rust callbacks return plain pointers/scalars and hold NO live Rust destructor
 * across any subsequent PHP call that could zend_bailout(); the only heap buffer
 * we hold (a Model-B decompress) is freed through free_source in the stream
 * closer, never across a bailout-capable emalloc.
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "main/php.h"
#include "main/php_streams.h"
#include "main/php_memory_streams.h"
#include "main/streams/php_stream_plain_wrapper.h"
#include "Zend/zend.h"
#include "Zend/zend_stream.h"
#include "Zend/zend_string.h"

#ifndef S_IFREG
# define S_IFREG 0100000
#endif
#ifndef S_IFDIR
# define S_IFDIR 0040000
#endif

/* Mirror of Rust `BundleStat` (#[repr(C)]). */
typedef struct {
    int      is_dir;
    int64_t  size;
    int64_t  mtime;
    uint64_t inode;
} ephpm_bundle_stat_t;

/* Mirror of Rust `BundleCallbacks` (#[repr(C)]). */
typedef struct {
    int (*enabled)(void);
    const char *(*resolve)(const char *path, size_t len);
    int (*stat)(const char *path, size_t len, ephpm_bundle_stat_t *out);
    const unsigned char *(*get_source)(const char *path, size_t len,
                                       size_t *out_len, int *needs_free);
    void (*free_source)(const unsigned char *ptr, size_t len);
} ephpm_bundle_callbacks_t;

/* Installed vtable + saved originals. Written once on the single-threaded
 * startup path (before any tokio worker exists), read-only thereafter. */
static ephpm_bundle_callbacks_t g_cb;
static int g_cb_installed = 0;

static zend_string *(*g_orig_resolve_path)(zend_string *filename) = NULL;
static zend_result (*g_orig_stream_open)(zend_file_handle *handle) = NULL;
static int (*g_orig_url_stat)(php_stream_wrapper *wrapper, const char *url,
                              int flags, php_stream_statbuf *ssb,
                              php_stream_context *context) = NULL;
static php_stream *(*g_orig_stream_opener)(php_stream_wrapper *wrapper,
                                           const char *filename, const char *mode,
                                           int options, zend_string **opened_path,
                                           php_stream_context *context STREAMS_DC) = NULL;

/* Non-const copy of the plain-files wrapper ops with our url_stat swapped in.
 * php_plain_files_wrapper.wops is declared non-const (the `const` is commented
 * out in the header), so repointing it at this struct is ABI-legal. */
static php_stream_wrapper_ops g_plain_ops;

static int ephpm_bundle_active(void)
{
    return g_cb_installed && g_cb.enabled && g_cb.enabled();
}

/* One-shot per-hook diagnostic, gated on EPHPM_BUNDLE_TRACE=1. Prints the first
 * few calls of each hook with the path and hit/miss so we can tell whether a
 * hook fires at all and whether the first probe keys match the bundle. Zero cost
 * when the env var is unset (checked once). */
static int ephpm_bundle_trace(void)
{
    static int cached = -1;
    if (cached < 0) {
        const char *v = getenv("EPHPM_BUNDLE_TRACE");
        cached = (v && v[0] == '1') ? 1 : 0;
    }
    return cached;
}

static void ephpm_bundle_trace_line(const char *hook, const char *url, int hit)
{
    static int n = 0;
    if (!ephpm_bundle_trace() || n >= 12) {
        return;
    }
    n++;
    fprintf(stderr, "[bundle-trace] hook=%s hit=%d url=%s\n", hook, hit,
            url ? url : "(null)");
}

/* ---- 1. zend_resolve_path ------------------------------------------------ */

static zend_string *ephpm_bundle_resolve_path_hook(zend_string *filename)
{
    if (ephpm_bundle_active() && filename) {
        const char *canon = g_cb.resolve(ZSTR_VAL(filename), ZSTR_LEN(filename));
        ephpm_bundle_trace_line("resolve", ZSTR_VAL(filename), canon ? 1 : 0);
        if (canon) {
            /* zend_string_init can zend_bailout() on OOM — but no Rust
             * destructor is live here (canon is a stable pointer into the
             * immutable bundle), so a longjmp is safe. */
            return zend_string_init(canon, strlen(canon), 0);
        }
    }
    return g_orig_resolve_path ? g_orig_resolve_path(filename) : NULL;
}

/* ---- 2. zend_stream_open_function ---------------------------------------- */

/* Per-open cursor over a bundle source buffer. malloc'd (never emalloc), so it
 * survives independently of the Zend request allocator and its lifetime is the
 * zend_file_handle's — freed in the closer. */
typedef struct {
    const unsigned char *data;
    size_t               len;
    size_t               pos;
    int                  needs_free; /* release data via free_source on close */
} ephpm_bundle_stream_ctx;

static ssize_t ephpm_bundle_stream_reader(void *h, char *buf, size_t len)
{
    ephpm_bundle_stream_ctx *c = (ephpm_bundle_stream_ctx *)h;
    size_t remain = c->len - c->pos;
    size_t n = remain < len ? remain : len;
    if (n) {
        memcpy(buf, c->data + c->pos, n);
        c->pos += n;
    }
    return (ssize_t)n;
}

static size_t ephpm_bundle_stream_fsizer(void *h)
{
    return ((ephpm_bundle_stream_ctx *)h)->len;
}

static void ephpm_bundle_stream_closer(void *h)
{
    ephpm_bundle_stream_ctx *c = (ephpm_bundle_stream_ctx *)h;
    if (c->needs_free && c->data && g_cb.free_source) {
        g_cb.free_source(c->data, c->len);
    }
    free(c);
}

static zend_result ephpm_bundle_stream_open_hook(zend_file_handle *handle)
{
    if (ephpm_bundle_active() && handle && handle->filename) {
        size_t src_len = 0;
        int needs_free = 0;
        const unsigned char *src = g_cb.get_source(
            ZSTR_VAL(handle->filename), ZSTR_LEN(handle->filename),
            &src_len, &needs_free);
        ephpm_bundle_trace_line("stream_open", ZSTR_VAL(handle->filename), src ? 1 : 0);
        if (src) {
            ephpm_bundle_stream_ctx *ctx =
                (ephpm_bundle_stream_ctx *)malloc(sizeof(*ctx));
            if (!ctx) {
                if (needs_free && g_cb.free_source) {
                    g_cb.free_source(src, src_len);
                }
                return g_orig_stream_open(handle);
            }
            ctx->data = src;
            ctx->len = src_len;
            ctx->pos = 0;
            ctx->needs_free = needs_free;

            /* Serve via a zend_stream reader (not by pre-filling handle->buf):
             * zend_stream_fixup only consults handle->buf BEFORE calling the
             * open function, so the reader path is the correct, version-robust
             * layer. This is FrankenPHP's proven technique. */
            handle->type = ZEND_HANDLE_STREAM;
            handle->handle.stream.handle = ctx;
            handle->handle.stream.isatty = 0;
            handle->handle.stream.reader = ephpm_bundle_stream_reader;
            handle->handle.stream.fsizer = ephpm_bundle_stream_fsizer;
            handle->handle.stream.closer = ephpm_bundle_stream_closer;
            if (!handle->opened_path) {
                handle->opened_path = zend_string_copy(handle->filename);
            }
            return SUCCESS;
        }
    }
    return g_orig_stream_open(handle);
}

/* ---- 3. php_plain_files_wrapper.url_stat --------------------------------- */

static int ephpm_bundle_url_stat_hook(php_stream_wrapper *wrapper,
                                      const char *url, int flags,
                                      php_stream_statbuf *ssb,
                                      php_stream_context *context)
{
    if (ephpm_bundle_active() && url && ssb) {
        ephpm_bundle_stat_t st;
        int hit = g_cb.stat(url, strlen(url), &st);
        ephpm_bundle_trace_line("url_stat", url, hit);
        if (hit) {
            memset(ssb, 0, sizeof(*ssb));
            /* Direct assignment: C implicitly converts int64_t to each field's
             * platform type (off_t/time_t/ino_t on Unix, __int64 variants on
             * Win64). No typeof — MSVC lacks it pre-C23. */
            if (st.is_dir) {
                ssb->sb.st_mode = S_IFDIR | 0555;
            } else {
                ssb->sb.st_mode = S_IFREG | 0444;
                ssb->sb.st_size = st.size;
            }
            ssb->sb.st_mtime = st.mtime;
            ssb->sb.st_atime = st.mtime;
            ssb->sb.st_ctime = st.mtime;
            ssb->sb.st_ino = st.inode;
            ssb->sb.st_nlink = 1;
            return 0; /* success */
        }
    }
    return g_orig_url_stat
        ? g_orig_url_stat(wrapper, url, flags, ssb, context)
        : -1;
}

/* ---- 4. php_plain_files_wrapper.stream_opener ---------------------------- *
 * OPcache captures the ORIGINAL zend_stream_open_function at startup and calls
 * its saved copy (not the live global we override in #2), so with opcache on the
 * cold-compile source read reaches disk through php_stream_open_wrapper -> this
 * plain-wrapper stream_opener. Overriding it here catches opcache's read (and
 * userland fopen/file_get_contents of a bundled file) and serves it from a
 * read-only in-memory php_stream. Only read modes are fronted; writes and every
 * miss delegate to the original opener (overlay semantics). */
static php_stream *ephpm_bundle_stream_opener(php_stream_wrapper *wrapper,
                                              const char *filename, const char *mode,
                                              int options, zend_string **opened_path,
                                              php_stream_context *context STREAMS_DC)
{
    if (ephpm_bundle_active() && filename && mode && mode[0] == 'r') {
        size_t src_len = 0;
        int needs_free = 0;
        const unsigned char *src =
            g_cb.get_source(filename, strlen(filename), &src_len, &needs_free);
        ephpm_bundle_trace_line("stream_opener", filename, src ? 1 : 0);
        if (src) {
            /* zend_string_init copies the bytes, so the decompress buffer can be
             * released immediately — nothing Rust-owned outlives this call. */
            zend_string *buf = zend_string_init((const char *)src, src_len, 0);
            if (needs_free && g_cb.free_source) {
                g_cb.free_source(src, src_len);
            }
            /* READONLY: the memory stream takes ownership of buf. */
            php_stream *stream = php_stream_memory_open(TEMP_STREAM_READONLY, buf);
            if (!stream) {
                zend_string_release(buf);
                return g_orig_stream_opener(wrapper, filename, mode, options,
                                            opened_path, context STREAMS_REL_CC);
            }
            if (opened_path) {
                *opened_path = zend_string_init(filename, strlen(filename), 0);
            }
            return stream;
        }
    }
    return g_orig_stream_opener(wrapper, filename, mode, options, opened_path,
                               context STREAMS_REL_CC);
}

/* ---- install ------------------------------------------------------------- */

void ephpm_bundle_install_hooks(const ephpm_bundle_callbacks_t *cb)
{
    if (g_cb_installed || !cb) {
        return;
    }
    g_cb = *cb;

    /* zend_resolve_path + zend_stream_open_function are ZEND_API extern global
     * function pointers PHP sets to its defaults at startup. Save and swap. */
    g_orig_resolve_path = zend_resolve_path;
    zend_resolve_path = ephpm_bundle_resolve_path_hook;

    g_orig_stream_open = zend_stream_open_function;
    zend_stream_open_function = ephpm_bundle_stream_open_hook;

    /* Copy the plain-files wrapper ops, swap url_stat, repoint the wrapper. The
     * other ops (stream_opener/dir_opener/unlink/...) keep delegating to the
     * originals, so userland fopen/scandir/writes still hit disk (overlay
     * semantics: bundle-first for stat, fall through for everything else). */
    if (php_plain_files_wrapper.wops) {
        g_orig_url_stat = php_plain_files_wrapper.wops->url_stat;
        g_orig_stream_opener = php_plain_files_wrapper.wops->stream_opener;
        g_plain_ops = *php_plain_files_wrapper.wops;
        g_plain_ops.url_stat = ephpm_bundle_url_stat_hook;
        g_plain_ops.stream_opener = ephpm_bundle_stream_opener;
        php_plain_files_wrapper.wops = &g_plain_ops;
    }

    g_cb_installed = 1;
}
