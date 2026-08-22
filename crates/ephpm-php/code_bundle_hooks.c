/*
 * code_bundle_hooks.c — C-level PHP overrides backing the in-memory code
 * bundle (see crates/ephpm-php/src/code_bundle.rs and
 * site/content/roadmap/in-memory-code-bundle.md).
 *
 * We interpose on four PHP indirection points that are *designed* to be
 * overridden (OPcache itself overrides zend_compile_file / zend_resolve_path),
 * install them once at SAPI init, and delegate to the saved originals on a
 * bundle MISS so behaviour for anything not in the bundle is byte-for-byte
 * unchanged:
 *
 *   1. zend_resolve_path        — include/require path resolution. On a hit we
 *                                 return the bundle's canonical absolute path so
 *                                 __FILE__/__DIR__/realpath() math stays correct.
 *   2. zend_stream_open_function — the compiler's source open (primary script +
 *                                 every include/require) when OPcache is off.
 *   3. php_plain_files_wrapper.url_stat — userland file_exists / is_file /
 *                                 stat / filemtime and OPcache's own probing.
 *   4. php_plain_files_wrapper.stream_opener — the source read OPcache reaches
 *                                 through (it calls the *saved original*
 *                                 zend_stream_open_function, so #2 alone does
 *                                 not cover the OPcache-on path).
 *
 * ── Two lookup outcomes, three answers ────────────────────────────────────
 *
 * Every callback is tri-state (EPHPM_BUNDLE_HIT / _UNKNOWN / _ABSENT):
 *
 *   HIT     — answer from RAM.
 *   UNKNOWN — delegate to the saved original (overlay semantics; the default).
 *   ABSENT  — answer "does not exist" from RAM with **zero syscalls**. Only ever
 *             returned in `sealed` mode, and only for a path that is (a) under
 *             the bundled document root and (b) carries the extension the
 *             scanner indexes exhaustively. Rust owns that predicate — see
 *             `Bundle::lookup` — so the correctness boundary lives in one place.
 *
 * ── Bundle-backed streams ─────────────────────────────────────────────────
 *
 * Both source-serving hooks (#2 and #4) return a *real* `php_stream` built on
 * `ephpm_bundle_stream_ops`, whose `stat` op reports the index's recorded mtime
 * and size. That matters beyond tidiness: OPcache's
 * `zend_get_file_handle_timestamp()` casts `zend_file_handle.handle.stream.handle`
 * to `php_stream *` and dereferences `stream->ops->stat`. Handing it anything
 * that is not a php_stream is a wild-pointer call; handing it a stream whose
 * stat reports mtime 0 makes OPcache refuse to cache the script at all
 * (`timestamp == 0` → "possibly a socket" → never cached).
 *
 * The bundle data lives in Rust (immutable, process-lifetime). These hooks
 * query it through a small callback vtable installed by
 * ephpm_bundle_install_hooks(). CRITICAL FFI invariant (CLAUDE.md rule 3): the
 * Rust callbacks return plain pointers/scalars and hold NO live Rust destructor
 * across any subsequent PHP call that could zend_bailout(); the only heap buffer
 * we hold (a Model-B decompress) is malloc-owned by the stream's abstract and
 * released through free_source in the stream's close op.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "main/php.h"
#include "main/php_globals.h"
#include "main/php_streams.h"
#include "main/streams/php_stream_plain_wrapper.h"
#include "Zend/zend.h"
#include "Zend/zend_API.h"
#include "Zend/zend_compile.h"
#include "Zend/zend_globals.h"
#include "Zend/zend_hash.h"
#include "Zend/zend_stream.h"
#include "Zend/zend_string.h"

#ifndef S_IFREG
# define S_IFREG 0100000
#endif
#ifndef S_IFDIR
# define S_IFDIR 0040000
#endif

/* Tri-state lookup result. Mirrors the Rust constants in code_bundle.rs. */
#define EPHPM_BUNDLE_HIT      1
#define EPHPM_BUNDLE_UNKNOWN  0
#define EPHPM_BUNDLE_ABSENT (-1)

/* Mirror of Rust `BundleStat` (#[repr(C)]). */
typedef struct {
    int      is_dir;
    int      readonly; /* 1 => 0444, 0 => 0644 (keeps fileperms/is_writable consistent) */
    int64_t  size;
    int64_t  mtime;
    uint64_t inode;
} ephpm_bundle_stat_t;

/* Mirror of Rust `BundleSource` (#[repr(C)]). */
typedef struct {
    const unsigned char *data;
    size_t               len;
    int                  needs_free; /* release `data` via free_source */
    int64_t              mtime;
    uint64_t             inode;
} ephpm_bundle_source_t;

/* Mirror of Rust `BundleCallbacks` (#[repr(C)]).
 * resolve/stat/get_source all return HIT / UNKNOWN / ABSENT and only write
 * through their out-parameter on HIT. */
typedef struct {
    int (*enabled)(void);
    int (*resolve)(const char *path, size_t len, const char **out_canon);
    int (*stat)(const char *path, size_t len, ephpm_bundle_stat_t *out);
    int (*get_source)(const char *path, size_t len, ephpm_bundle_source_t *out);
    void (*free_source)(const unsigned char *ptr, size_t len);
    /* Breadcrumb only: the plain-files wrapper is about to open this path for
     * WRITING. Never changes what PHP does; Rust logs the sealed-mode contract
     * violation. */
    void (*note_write)(const char *path, size_t len);
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
 * few calls of each hook with the path and the lookup outcome so we can tell
 * whether a hook fires at all and whether the first probe keys match the bundle.
 * Zero cost when the env var is unset (checked once). */
static int ephpm_bundle_trace(void)
{
    static int cached = -1;
    if (cached < 0) {
        const char *v = getenv("EPHPM_BUNDLE_TRACE");
        cached = (v && v[0] == '1') ? 1 : 0;
    }
    return cached;
}

static void ephpm_bundle_trace_line(const char *hook, const char *url, int rc)
{
    static int n = 0;
    if (!ephpm_bundle_trace() || n >= 12) {
        return;
    }
    n++;
    fprintf(stderr, "[bundle-trace] hook=%s rc=%s url=%s\n", hook,
            rc == EPHPM_BUNDLE_HIT ? "hit"
                : (rc == EPHPM_BUNDLE_ABSENT ? "absent" : "unknown"),
            url ? url : "(null)");
}

/* ---- bundle-backed php_stream ------------------------------------------- *
 * A read-only stream over a bundle source buffer. The abstract is malloc'd
 * (never emalloc'd) so its lifetime is decoupled from the Zend request
 * allocator; it is released in the close op, which is also where a Model-B
 * decompress buffer goes back to Rust. */

typedef struct {
    const unsigned char *data;
    size_t               len;
    size_t               pos;
    int                  needs_free;
    int64_t              mtime;
    uint64_t             inode;
} ephpm_bundle_stream_data;

static ssize_t ephpm_bundle_sop_write(php_stream *stream, const char *buf, size_t count)
{
    (void)stream; (void)buf; (void)count;
    return -1; /* the bundle is immutable */
}

static ssize_t ephpm_bundle_sop_read(php_stream *stream, char *buf, size_t count)
{
    ephpm_bundle_stream_data *d = (ephpm_bundle_stream_data *)stream->abstract;
    size_t remain, n;
    if (!d) {
        return -1;
    }
    remain = d->len - d->pos;
    n = remain < count ? remain : count;
    if (n == 0) {
        stream->eof = 1;
        return 0;
    }
    memcpy(buf, d->data + d->pos, n);
    d->pos += n;
    return (ssize_t)n;
}

static int ephpm_bundle_sop_close(php_stream *stream, int close_handle)
{
    ephpm_bundle_stream_data *d = (ephpm_bundle_stream_data *)stream->abstract;
    (void)close_handle;
    if (d) {
        if (d->needs_free && d->data && g_cb.free_source) {
            g_cb.free_source(d->data, d->len);
        }
        free(d);
        stream->abstract = NULL;
    }
    return 0;
}

static int ephpm_bundle_sop_flush(php_stream *stream)
{
    (void)stream;
    return 0;
}

static int ephpm_bundle_sop_seek(php_stream *stream, zend_off_t offset, int whence,
                                 zend_off_t *newoffset)
{
    ephpm_bundle_stream_data *d = (ephpm_bundle_stream_data *)stream->abstract;
    zend_off_t base, target;
    if (!d) {
        return -1;
    }
    switch (whence) {
        case SEEK_SET: base = 0; break;
        case SEEK_CUR: base = (zend_off_t)d->pos; break;
        case SEEK_END: base = (zend_off_t)d->len; break;
        default: return -1;
    }
    target = base + offset;
    if (target < 0 || (size_t)target > d->len) {
        return -1;
    }
    d->pos = (size_t)target;
    stream->eof = 0;
    if (newoffset) {
        *newoffset = target;
    }
    return 0;
}

/* The reason this whole ops table exists: OPcache reads a script's timestamp
 * through stream->ops->stat, and refuses to cache anything whose timestamp it
 * cannot obtain (mtime 0 → treated as a socket → never cached). We report the
 * index's recorded mtime, which is the real on-disk source mtime captured at
 * scan time, so OPcache caches normally with file_update_protection left at its
 * default. */
static int ephpm_bundle_sop_stat(php_stream *stream, php_stream_statbuf *ssb)
{
    ephpm_bundle_stream_data *d = (ephpm_bundle_stream_data *)stream->abstract;
    if (!d || !ssb) {
        return -1;
    }
    memset(ssb, 0, sizeof(*ssb));
    /* Direct assignment: C implicitly converts to each field's platform type
     * (off_t/time_t/ino_t on Unix, __int64 variants on Win64). No typeof —
     * MSVC lacks it pre-C23. */
    ssb->sb.st_mode = S_IFREG | 0444;
    ssb->sb.st_size = d->len;
    ssb->sb.st_mtime = d->mtime;
    ssb->sb.st_atime = d->mtime;
    ssb->sb.st_ctime = d->mtime;
    ssb->sb.st_ino = d->inode;
    ssb->sb.st_nlink = 1;
    return 0;
}

static const php_stream_ops ephpm_bundle_stream_ops = {
    ephpm_bundle_sop_write,
    ephpm_bundle_sop_read,
    ephpm_bundle_sop_close,
    ephpm_bundle_sop_flush,
    "ephpm-code-bundle",
    ephpm_bundle_sop_seek,
    NULL, /* cast: a bundle entry has no file descriptor */
    ephpm_bundle_sop_stat,
    NULL, /* set_option */
};

/* Wrap an already-fetched bundle source in a php_stream. Takes ownership of
 * `src->data` when `src->needs_free` is set: on success the stream's close op
 * releases it, on failure this function releases it before returning NULL.
 *
 * OOM note: php_stream_alloc emalloc's and can therefore zend_bailout(). If it
 * does, `d` (and a Model-B decompress buffer) leak. That window is unavoidable
 * without a zend_try around an allocation that is itself the OOM, and the
 * request is being torn down anyway. No Rust *destructor* is live across it —
 * only a raw pointer — so the bailout itself is safe (CLAUDE.md rule 3). */
static php_stream *ephpm_bundle_stream_from_source(const ephpm_bundle_source_t *src)
{
    ephpm_bundle_stream_data *d;
    php_stream *stream;

    d = (ephpm_bundle_stream_data *)malloc(sizeof(*d));
    if (!d) {
        if (src->needs_free && src->data && g_cb.free_source) {
            g_cb.free_source(src->data, src->len);
        }
        return NULL;
    }
    d->data = src->data;
    d->len = src->len;
    d->pos = 0;
    d->needs_free = src->needs_free;
    d->mtime = src->mtime;
    d->inode = src->inode;

    stream = php_stream_alloc(&ephpm_bundle_stream_ops, d, NULL, "rb");
    if (!stream) {
        if (d->needs_free && d->data && g_cb.free_source) {
            g_cb.free_source(d->data, d->len);
        }
        free(d);
        return NULL;
    }
    return stream;
}

/* zend_stream_* trampolines for a zend_file_handle backed by a php_stream.
 * PHP's own php_stream_open_for_zend_ex() installs equivalents, but they are
 * `static` in main/main.c, so we provide our own. */

static ssize_t ephpm_bundle_zend_reader(void *handle, char *buf, size_t len)
{
    return php_stream_read((php_stream *)handle, buf, len);
}

static size_t ephpm_bundle_zend_fsizer(void *handle)
{
    php_stream_statbuf ssb;
    if (php_stream_stat((php_stream *)handle, &ssb) == 0 && ssb.sb.st_size >= 0) {
        return (size_t)ssb.sb.st_size;
    }
    return 0;
}

static void ephpm_bundle_zend_closer(void *handle)
{
    php_stream_close((php_stream *)handle);
}

/* ---- 1. zend_resolve_path ------------------------------------------------ */

static zend_string *ephpm_bundle_resolve_path_hook(zend_string *filename)
{
    if (ephpm_bundle_active() && filename) {
        const char *canon = NULL;
        int rc = g_cb.resolve(ZSTR_VAL(filename), ZSTR_LEN(filename), &canon);
        ephpm_bundle_trace_line("resolve", ZSTR_VAL(filename), rc);
        if (rc == EPHPM_BUNDLE_HIT && canon) {
            /* zend_string_init can zend_bailout() on OOM — but no Rust
             * destructor is live here (canon is a stable pointer into the
             * immutable bundle), so a longjmp is safe. */
            return zend_string_init(canon, strlen(canon), 0);
        }
        if (rc == EPHPM_BUNDLE_ABSENT) {
            /* Sealed: the bundle is the authority for this path. NULL is
             * "cannot resolve", answered without a single syscall. */
            return NULL;
        }
    }
    return g_orig_resolve_path ? g_orig_resolve_path(filename) : NULL;
}

/* ---- 2. zend_stream_open_function ---------------------------------------- */

static zend_result ephpm_bundle_stream_open_hook(zend_file_handle *handle)
{
    if (ephpm_bundle_active() && handle && handle->filename) {
        ephpm_bundle_source_t src;
        int rc = g_cb.get_source(ZSTR_VAL(handle->filename),
                                 ZSTR_LEN(handle->filename), &src);
        ephpm_bundle_trace_line("stream_open", ZSTR_VAL(handle->filename), rc);
        if (rc == EPHPM_BUNDLE_HIT) {
            php_stream *stream = ephpm_bundle_stream_from_source(&src);
            if (stream) {
                /* Serve via a zend_stream reader (not by pre-filling
                 * handle->buf): zend_stream_fixup only consults handle->buf
                 * BEFORE calling the open function, so the reader path is the
                 * correct, version-robust layer. handle.stream.handle is a
                 * genuine php_stream — OPcache's timestamp read casts it to one
                 * and calls ops->stat. */
                handle->type = ZEND_HANDLE_STREAM;
                handle->handle.stream.handle = stream;
                handle->handle.stream.isatty = 0;
                handle->handle.stream.reader = ephpm_bundle_zend_reader;
                handle->handle.stream.fsizer = ephpm_bundle_zend_fsizer;
                handle->handle.stream.closer = ephpm_bundle_zend_closer;
                if (!handle->opened_path) {
                    handle->opened_path = zend_string_copy(handle->filename);
                }
                return SUCCESS;
            }
            /* Stream construction failed (OOM); fall through to disk. */
        } else if (rc == EPHPM_BUNDLE_ABSENT) {
            return FAILURE;
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
        int rc = g_cb.stat(url, strlen(url), &st);
        ephpm_bundle_trace_line("url_stat", url, rc);
        if (rc == EPHPM_BUNDLE_HIT) {
            memset(ssb, 0, sizeof(*ssb));
            /* Direct assignment: C implicitly converts int64_t to each field's
             * platform type (off_t/time_t/ino_t on Unix, __int64 variants on
             * Win64). No typeof — MSVC lacks it pre-C23. */
            if (st.is_dir) {
                ssb->sb.st_mode = S_IFDIR | 0555;
            } else {
                /* Report the real writability. Hardcoding 0444 made
                 * fileperms() claim read-only for a file is_writable() (which
                 * goes through VCWD_ACCESS and never reaches this hook) said
                 * was writable — two answers about one file in one request. */
                ssb->sb.st_mode = S_IFREG | (st.readonly ? 0444 : 0644);
                ssb->sb.st_size = st.size;
            }
            ssb->sb.st_mtime = st.mtime;
            ssb->sb.st_atime = st.mtime;
            ssb->sb.st_ctime = st.mtime;
            ssb->sb.st_ino = st.inode;
            ssb->sb.st_nlink = 1;
            return 0; /* success */
        }
        if (rc == EPHPM_BUNDLE_ABSENT) {
            /* Sealed: answer "no such file" from RAM. This is the hot path —
             * a PSR-4 autoloader's misses are the bulk of its probes. */
            return -1;
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
 * read-only bundle stream. Only strictly read-only modes are fronted; writes
 * (including "r+", which mode[0]=='r' alone would wrongly admit) and every
 * UNKNOWN delegate to the original opener (overlay semantics). */
static int ephpm_bundle_mode_is_readonly(const char *mode)
{
    return mode && mode[0] == 'r' && strchr(mode, '+') == NULL;
}

static php_stream *ephpm_bundle_stream_opener(php_stream_wrapper *wrapper,
                                              const char *filename, const char *mode,
                                              int options, zend_string **opened_path,
                                              php_stream_context *context STREAMS_DC)
{
    if (ephpm_bundle_active() && filename && !ephpm_bundle_mode_is_readonly(mode)
        && g_cb.note_write) {
        /* Diagnostic only — the open itself always proceeds to the real
         * filesystem below. */
        g_cb.note_write(filename, strlen(filename));
    }
    if (ephpm_bundle_active() && filename && ephpm_bundle_mode_is_readonly(mode)) {
        ephpm_bundle_source_t src;
        int rc = g_cb.get_source(filename, strlen(filename), &src);
        ephpm_bundle_trace_line("stream_opener", filename, rc);
        if (rc == EPHPM_BUNDLE_HIT) {
            php_stream *stream = ephpm_bundle_stream_from_source(&src);
            if (stream) {
                if (opened_path) {
                    *opened_path = zend_string_init(filename, strlen(filename), 0);
                }
                return stream;
            }
            /* OOM building the stream; fall through to disk. */
        } else if (rc == EPHPM_BUNDLE_ABSENT) {
            errno = ENOENT;
            return NULL;
        }
    }
    return g_orig_stream_opener(wrapper, filename, mode, options, opened_path,
                               context STREAMS_REL_CC);
}

/* ---- 5. internal FUNCTION HANDLER overrides ------------------------------ *
 *
 * The four access functions (file_exists / is_readable / is_writable /
 * is_executable) and realpath() do NOT go through the stream wrapper. They
 * reach the filesystem through the VCWD_* macros, which are direct calls with
 * no pointer to swap — which is why hooks 1-4 above never front them, and why
 * a real Composer autoloader (which probes with file_exists) got zero benefit
 * from this feature.
 *
 * There is, however, a layer ABOVE the streams layer that IS swappable: the
 * internal function's own handler. PHP keeps every internal function in
 * CG(function_table) as a pointer to a heap-allocated zend_internal_function,
 * and the `handler` field of that struct is just a function pointer. OPcache
 * itself does exactly this in zend_accel_override_file_functions() to implement
 * opcache.enable_file_override — no source patch, no SDK change.
 *
 * ── ZTS ────────────────────────────────────────────────────────────────────
 * The table stores POINTERS to shared zend_internal_function structs; a new
 * thread's compiler_globals_ctor copies the table by pointer, so mutating
 * `handler` is a PROCESS-WIDE change, not a per-thread one. We therefore swap
 * exactly once, on the single-threaded startup path (before any tokio blocking
 * thread has registered with TSRM), so no thread can observe a torn pointer.
 * Empirical confirmation that this works on this exact ZTS build: turning on
 * opcache.enable_file_override — which uses this same mechanism — measurably
 * changes behaviour process-wide.
 *
 * ── Composing with OPcache rather than fighting it ─────────────────────────
 * We install AFTER php_embed_init(), i.e. after every MINIT including
 * OPcache's. So when enable_file_override is also on, the handler we save is
 * OPcache's accel_file_exists, and our miss path delegates to it: bundle first,
 * then OPcache's SHM-cache-first version, then the real syscall. Both
 * accelerations compose. When it is off we save the genuine zif_file_exists.
 * Either way the saved pointer is whatever was installed last before us.
 *
 * ── Correctness ────────────────────────────────────────────────────────────
 * A fast answer is taken ONLY when all of the following hold, otherwise we
 * delegate and behaviour is byte-for-byte unchanged:
 *   - a bundle is published;
 *   - exactly one argument was passed and it is already a string (no coercion,
 *     no named-argument reshuffling to reason about);
 *   - open_basedir is NOT in force. The real handlers enforce it and emit its
 *     warning; answering from RAM would leak the existence of paths outside it.
 *     The check is a pointer test, not a syscall.
 * Rust owns the scope predicate (docroot + indexed extension), so a stream URL
 * ("phar://…"), a relative path, or anything outside the document root can only
 * ever come back UNKNOWN.
 */

static zif_handler g_orig_file_exists = NULL;
static zif_handler g_orig_realpath = NULL;

/* Kill switch for the handler overrides: EPHPM_BUNDLE_FRONT_FILE_EXISTS=0 turns
 * them off, anything else (including unset) leaves them on.
 *
 * ON by default because without them the bundle does not accelerate a real
 * Composer autoloader AT ALL — measured on real Laravel, one binary, this one
 * variable: 1223 filesystem syscalls per request with them off versus 161
 * (sealed) / 397 (lazy) with them on. Turning them off by default would ship a
 * feature that measurably does nothing.
 *
 * An env var rather than a config field: it exists to bisect an interaction on
 * a binary you did not build, not as a tuning knob. See the roadmap page for
 * the separate, PRE-EXISTING Windows tracing-JIT crash that these overrides
 * were initially — and wrongly — blamed for. */
static int ephpm_bundle_fn_overrides_requested(void)
{
    const char *v = getenv("EPHPM_BUNDLE_FRONT_FILE_EXISTS");
    return !(v && v[0] == '0');
}

/* Non-zero when the fast path is allowed to answer at all. */
static int ephpm_bundle_fn_fastpath_ok(void)
{
    return ephpm_bundle_active()
        && (PG(open_basedir) == NULL || PG(open_basedir)[0] == '\0');
}

/* Read the single path argument.
 *
 * MUST go through ZEND_PARSE_PARAMETERS, not a hand-rolled
 * ZEND_NUM_ARGS()/ZEND_CALL_ARG() peek. Reading the frame directly is only
 * valid for a frame the VM built the ordinary way; once OPcache's tracing JIT
 * compiles a hot trace containing this call it sets the frame up differently,
 * and the raw peek reads garbage and dereferences it. Measured: with
 * `opcache_jit = "disable"` a server survived 150 consecutive requests, and
 * with the JIT on (the ePHPm serve default) the identical binary and config
 * died with 0xC0000005 after 3 — precisely once the trace went hot.
 *
 * This is also exactly what OPcache's own accel_common_file_func() does for the
 * same set of functions: parse first, then delegate with PASSTHRU. Re-parsing
 * in the original handler is harmless (it only re-reads the frame).
 */
#define EPHPM_BUNDLE_PARSE_PATH(dest)                                          \
    do {                                                                       \
        ZEND_PARSE_PARAMETERS_START(1, 1)                                      \
            Z_PARAM_PATH_STR(dest)                                             \
        ZEND_PARSE_PARAMETERS_END();                                           \
    } while (0)

static void ZEND_FASTCALL ephpm_bundle_zif_file_exists(INTERNAL_FUNCTION_PARAMETERS)
{
    zend_string *p;
    EPHPM_BUNDLE_PARSE_PATH(p);

    if (p && ephpm_bundle_fn_fastpath_ok()) {
        ephpm_bundle_stat_t st;
        int rc = g_cb.stat(ZSTR_VAL(p), ZSTR_LEN(p), &st);
        ephpm_bundle_trace_line("zif_file_exists", ZSTR_VAL(p), rc);
        if (rc == EPHPM_BUNDLE_HIT) {
            RETURN_TRUE;
        }
        if (rc == EPHPM_BUNDLE_ABSENT) {
            RETURN_FALSE;
        }
    }
    g_orig_file_exists(INTERNAL_FUNCTION_PARAM_PASSTHRU);
}

/* is_readable / is_writable / is_executable are deliberately NOT overridden.
 *
 * They are the three access checks whose answer depends on the calling
 * process's effective uid/gid, not on the file's existence. The only correct
 * way to answer them is access(2) — which IS the syscall we are trying to
 * remove, so there is nothing to win — and answering them from the index would
 * report "readable" for a mode-000 file that the server cannot actually open.
 * Real Composer probes with file_exists (ClassLoader.php findFileWithExtension),
 * so nothing measurable is given up by leaving these alone.
 */

static void ZEND_FASTCALL ephpm_bundle_zif_realpath(INTERNAL_FUNCTION_PARAMETERS)
{
    zend_string *p;
    EPHPM_BUNDLE_PARSE_PATH(p);

    if (p && ephpm_bundle_fn_fastpath_ok()) {
        const char *canon = NULL;
        int rc = g_cb.resolve(ZSTR_VAL(p), ZSTR_LEN(p), &canon);
        ephpm_bundle_trace_line("zif_realpath", ZSTR_VAL(p), rc);
        if (rc == EPHPM_BUNDLE_HIT && canon) {
            RETVAL_STRING(canon);
            return;
        }
        if (rc == EPHPM_BUNDLE_ABSENT) {
            RETURN_FALSE;
        }
    }
    g_orig_realpath(INTERNAL_FUNCTION_PARAM_PASSTHRU);
}

/* Swap one internal function's handler, saving the previous one. Returns 1 on
 * success. A function that is not registered (disabled via disable_functions,
 * or removed by another extension) is simply left alone. */
static int ephpm_bundle_swap_handler(const char *name, size_t name_len,
                                     zif_handler replacement, zif_handler *saved)
{
    zend_function *fn = zend_hash_str_find_ptr(CG(function_table), name, name_len);
    if (!fn || fn->type != ZEND_INTERNAL_FUNCTION || !fn->internal_function.handler) {
        return 0;
    }
    *saved = fn->internal_function.handler;
    fn->internal_function.handler = replacement;
    return 1;
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

    /* Copy the plain-files wrapper ops, swap url_stat + stream_opener, repoint
     * the wrapper. The other ops (dir_opener/unlink/...) keep delegating to the
     * originals, so userland scandir/writes still hit disk. */
    if (php_plain_files_wrapper.wops) {
        g_orig_url_stat = php_plain_files_wrapper.wops->url_stat;
        g_orig_stream_opener = php_plain_files_wrapper.wops->stream_opener;
        g_plain_ops = *php_plain_files_wrapper.wops;
        g_plain_ops.url_stat = ephpm_bundle_url_stat_hook;
        g_plain_ops.stream_opener = ephpm_bundle_stream_opener;
        php_plain_files_wrapper.wops = &g_plain_ops;
    }

    /* Internal-function handler overrides — the ONLY way to reach the VCWD_*
     * functions without patching PHP. Done last, so the handlers we save are
     * whatever OPcache (or any other extension) installed during MINIT, and our
     * miss path delegates to them. Single-threaded startup: no reader can
     * observe a torn function pointer.
     *
     * Behind a kill switch (default ON) purely so the interaction can be
     * bisected in the field. They were initially blamed for a Windows
     * 0xC0000005 under the tracing JIT; the control run disproved that — the
     * same crash reproduces with `code_bundle = "off"`, i.e. with every line of
     * this file inert. See the roadmap page. */
    if (ephpm_bundle_fn_overrides_requested()) {
        ephpm_bundle_swap_handler("file_exists", sizeof("file_exists") - 1,
                                  ephpm_bundle_zif_file_exists, &g_orig_file_exists);
        ephpm_bundle_swap_handler("realpath", sizeof("realpath") - 1,
                                  ephpm_bundle_zif_realpath, &g_orig_realpath);
    }

    g_cb_installed = 1;
}

/* Diagnostic for the Rust side: which handler overrides actually took. A
 * function removed by disable_functions is skipped rather than faked. */
int ephpm_bundle_fn_overrides_installed(void)
{
    return (g_orig_file_exists ? 1 : 0)
         | (g_orig_realpath ? 2 : 0);
}
