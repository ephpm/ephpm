//! In-memory PHP **code bundle** — a transparent, read-only filesystem overlay
//! that answers PHP's source-discovery, source-read, and metadata (`stat`)
//! probes for `.php` files from Rust memory instead of the host filesystem.
//!
//! # Why (the Windows filesystem tax)
//!
//! On Windows every `stat`/`file_exists`/`is_file`/`realpath`/`opendir` and
//! every cold source read funnels through `CreateFileW` /
//! `GetFileAttributesExW` → the NTFS path parser → the Defender minifilter
//! stack — ~50 µs per metadata syscall versus ~1–3 µs for Linux `statx`. A warm
//! OPcache does **not** remove those syscalls (timestamp validation, autoloader
//! probing, `realpath`). This module lets source discovery and reads resolve
//! from an immutable, indexed, in-RAM bundle so the code path never touches the
//! filesystem. See `site/content/roadmap/in-memory-code-bundle.md`.
//!
//! # Transparency (zero application changes)
//!
//! Apps keep calling `require '/app/src/Foo.php'`, `file_exists(...)`,
//! `is_file(...)`, `filemtime(...)` with **ordinary filesystem paths**. There is
//! no `mem://` scheme. A bundle **hit** is answered from RAM; a **miss** falls
//! through to PHP's real filesystem handler unchanged (uploads, session files,
//! runtime config all still hit disk).
//!
//! # Mechanism
//!
//! The C side (`code_bundle_hooks.c`) overrides three PHP indirection points at
//! SAPI init and delegates to the saved originals on a miss:
//!
//! * `zend_resolve_path` — include/require path resolution.
//! * `zend_stream_open_function` — the compiler's source open (fills the
//!   `zend_file_handle` buffer from RAM).
//! * `php_plain_files_wrapper`'s `url_stat` op — userland
//!   `file_exists`/`is_file`/`stat`/`filemtime` and OPcache probing.
//!
//! Those C hooks query **this** module through a small [`BundleCallbacks`]
//! vtable installed once at startup. The bundle is immutable after load and
//! stored in a process-lifetime [`OnceLock`], so every `spawn_blocking` PHP
//! thread reads it concurrently with no locking — trivially ZTS-safe.
//!
//! # Scope (POC)
//!
//! `.php` code only. Directory listing (`scandir`/`glob` from the manifest) and
//! the userland `fopen` of a bundled file are follow-on: this POC covers the
//! drop-in autoloader path (resolve + stream-open + `url_stat`), which is what a
//! Composer/Symfony autoloader actually exercises.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uchar};
use std::path::Path;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// How bundle source bytes are held in RAM.
///
/// The task's two compression models map onto this as follows:
///
/// * **Model A — decompress once at load.** RAM holds *raw* bytes, so at
///   runtime it is identical to [`BundleCompression::None`]; it differs only in
///   image footprint and load time. Represented as [`StoredData::Raw`].
/// * **Model B — keep compressed in RAM, decompress per open.** RAM holds the
///   *compressed* bytes and every source open pays a decompress. Represented as
///   [`StoredData::Compressed`]; selected by any non-`None`
///   [`BundleCompression`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleCompression {
    /// Raw source resident in RAM (no per-open cost). Default.
    None,
    /// gzip (flate2), decompressed per open.
    Gzip,
    /// zstd, decompressed per open.
    Zstd,
    /// brotli, decompressed per open.
    Brotli,
}

impl BundleCompression {
    /// Parse a config string (`"none"`/`"gzip"`/`"zstd"`/`"brotli"`),
    /// case-insensitively. Returns `None` for an unrecognised value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "" => Some(Self::None),
            "gzip" | "gz" => Some(Self::Gzip),
            "zstd" => Some(Self::Zstd),
            "brotli" | "br" => Some(Self::Brotli),
            _ => None,
        }
    }

    /// Lowercase label for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Brotli => "brotli",
        }
    }
}

/// Per-file source storage.
enum StoredData {
    /// Raw source, resident. Source pointer handed straight to C (no free).
    Raw(Vec<u8>),
    /// Compressed source (Model B). Decompressed into a fresh buffer per open.
    Compressed(Vec<u8>),
}

/// One bundled `.php` file.
struct FileEntry {
    /// Canonical absolute path exactly as it lives on disk (original case and
    /// separators), NUL-terminated for zero-cost hand-off to C as
    /// `__FILE__`/`opened_path`.
    canon: CString,
    /// Stored source bytes (raw or compressed).
    data: StoredData,
    /// Uncompressed length in bytes (what `stat` reports and what the compiler
    /// receives).
    raw_len: usize,
    /// Stable modification time (Unix seconds) for `filemtime` cache-busting.
    mtime: i64,
    /// Stable synthetic inode.
    inode: u64,
}

/// An immutable, in-memory index of an application's `.php` code.
pub struct Bundle {
    /// Normalized-path → file entry.
    files: HashMap<String, FileEntry>,
    /// Normalized directory keys (every ancestor dir of a bundled file, up to
    /// and including the docroot) so `is_dir`/`stat` answer for directories.
    dirs: HashSet<String>,
    /// Active compression model.
    algo: BundleCompression,
    /// Number of `.php` files indexed.
    file_count: usize,
    /// Total uncompressed source bytes.
    raw_bytes: usize,
    /// Bytes actually resident in RAM (raw for Model A/None, compressed for
    /// Model B).
    resident_bytes: usize,
}

impl std::fmt::Debug for Bundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bundle")
            .field("file_count", &self.file_count)
            .field("raw_bytes", &self.raw_bytes)
            .field("resident_bytes", &self.resident_bytes)
            .field("compression", &self.algo.label())
            .finish_non_exhaustive()
    }
}

/// Errors building a bundle.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// The docroot could not be scanned.
    #[error("failed to scan docroot {path}: {source}")]
    Scan {
        /// The path that failed.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The bundle's resident size would exceed the configured cap.
    #[error(
        "code bundle exceeds max size: {resident} bytes resident > cap {cap} bytes \
         ({files} files scanned); refusing to bundle — falling through to disk"
    )]
    TooLarge {
        /// Resident bytes accumulated when the cap was hit.
        resident: usize,
        /// Configured cap.
        cap: usize,
        /// Files scanned so far.
        files: usize,
    },
}

impl Bundle {
    /// Number of `.php` files indexed.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.file_count
    }

    /// Total uncompressed source bytes across all files.
    #[must_use]
    pub fn raw_bytes(&self) -> usize {
        self.raw_bytes
    }

    /// Bytes actually resident in RAM.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// The active compression model.
    #[must_use]
    pub fn compression(&self) -> BundleCompression {
        self.algo
    }

    /// Build a bundle by scanning `docroot` recursively for `.php` files.
    ///
    /// `max_bytes` caps the resident footprint: if adding a file would push the
    /// resident total past the cap, the scan aborts with [`BundleError::TooLarge`]
    /// and the caller falls through to disk (refuse-to-bundle-beyond-cap; no
    /// partial bundle is ever installed).
    ///
    /// # Errors
    ///
    /// [`BundleError::Scan`] on an I/O failure, [`BundleError::TooLarge`] if the
    /// cap is exceeded.
    pub fn from_scan(
        docroot: &Path,
        algo: BundleCompression,
        max_bytes: usize,
    ) -> Result<Self, BundleError> {
        let mut files = HashMap::new();
        let mut dirs = HashSet::new();
        let mut raw_bytes = 0usize;
        let mut resident_bytes = 0usize;
        let mut next_inode: u64 = 1;

        // Always index the docroot itself as a directory.
        dirs.insert(normalize_key(&docroot.to_string_lossy()));

        let mut stack = vec![docroot.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = std::fs::read_dir(&dir).map_err(|source| BundleError::Scan {
                path: dir.to_string_lossy().into_owned(),
                source,
            })?;
            for entry in rd {
                let entry = entry.map_err(|source| BundleError::Scan {
                    path: dir.to_string_lossy().into_owned(),
                    source,
                })?;
                let path = entry.path();
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    dirs.insert(normalize_key(&path.to_string_lossy()));
                    stack.push(path);
                    continue;
                }
                if !meta.is_file() {
                    continue;
                }
                let is_php = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("php"));
                if !is_php {
                    continue;
                }

                let raw = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let raw_len = raw.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));

                let data = match algo {
                    BundleCompression::None => StoredData::Raw(raw),
                    other => StoredData::Compressed(compress(&raw, other)),
                };
                let stored_len = match &data {
                    StoredData::Raw(v) | StoredData::Compressed(v) => v.len(),
                };

                resident_bytes += stored_len;
                if resident_bytes > max_bytes {
                    return Err(BundleError::TooLarge {
                        resident: resident_bytes,
                        cap: max_bytes,
                        files: files.len(),
                    });
                }
                raw_bytes += raw_len;

                let canon_str = path.to_string_lossy().into_owned();
                let key = normalize_key(&canon_str);
                // Record ancestor directories so is_dir answers from the bundle.
                register_ancestors(&mut dirs, &key);
                let canon = CString::new(canon_str.replace('\0', "")).unwrap_or_default();
                let inode = next_inode;
                next_inode += 1;
                files.insert(
                    key,
                    FileEntry { canon, data, raw_len, mtime, inode },
                );
            }
        }

        let file_count = files.len();
        Ok(Self {
            files,
            dirs,
            algo,
            file_count,
            raw_bytes,
            resident_bytes,
        })
    }

    /// Look up a file entry by an already-normalized key.
    fn get(&self, key: &str) -> Option<&FileEntry> {
        self.files.get(key)
    }
}

/// Record every ancestor directory of a normalized file key into `dirs`.
fn register_ancestors(dirs: &mut HashSet<String>, key: &str) {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let mut cur = key;
    while let Some(idx) = cur.rfind(sep) {
        if idx == 0 {
            dirs.insert(sep.to_string());
            break;
        }
        cur = &cur[..idx];
        // Stop at a bare drive root like "c:".
        if cur.ends_with(':') {
            dirs.insert(cur.to_string());
            break;
        }
        dirs.insert(cur.to_string());
    }
}

/// Lexically normalize a filesystem path into a stable lookup key: unify
/// separators, resolve `.`/`..` **without touching disk** (the whole point is to
/// avoid syscalls), and case-fold on Windows (NTFS is case-insensitive).
///
/// Hermetic: identical input from `zend_resolve_path`, the stream-open hook and
/// `url_stat` must produce identical keys or the overlay is inconsistent.
#[must_use]
pub fn normalize_key(p: &str) -> String {
    let win = cfg!(windows);
    let sep = if win { '\\' } else { '/' };

    let mut prefix = String::new();
    let mut rest = p;
    if win {
        // Strip a Windows extended-length / verbatim prefix. `std::fs::canonicalize`
        // (used by the router to resolve the request script safely) returns
        // `\\?\C:\...`, so PHP's __DIR__-derived probe paths carry it while a
        // plain-config scan does not. Collapse both to the same key. `\\?\UNC\`
        // is left as-is (rare; POC scope).
        if let Some(stripped) = rest.strip_prefix(r"\\?\").or_else(|| rest.strip_prefix("//?/")) {
            rest = stripped;
        }
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            prefix = rest[..2].to_string();
            rest = &rest[2..];
        }
    }
    let is_abs = rest.starts_with('/') || rest.starts_with('\\');

    let mut comps: Vec<&str> = Vec::new();
    for part in rest.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if comps.last().is_some_and(|l| *l != "..") {
                    comps.pop();
                } else if !is_abs {
                    comps.push("..");
                }
            }
            other => comps.push(other),
        }
    }

    let mut out = String::with_capacity(p.len());
    out.push_str(&prefix);
    if is_abs {
        out.push(sep);
    }
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        out.push_str(c);
    }
    if win {
        out = out.to_lowercase();
    }
    out
}

fn compress(data: &[u8], algo: BundleCompression) -> Vec<u8> {
    use std::io::Write;
    match algo {
        BundleCompression::None => data.to_vec(),
        BundleCompression::Gzip => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
            if enc.write_all(data).is_err() {
                return data.to_vec();
            }
            enc.finish().unwrap_or_else(|_| data.to_vec())
        }
        BundleCompression::Zstd => zstd::encode_all(data, 6).unwrap_or_else(|_| data.to_vec()),
        BundleCompression::Brotli => {
            let mut out = Vec::new();
            {
                let mut enc = brotli::CompressorWriter::new(&mut out, 4096, 6, 22);
                if enc.write_all(data).is_err() {
                    return data.to_vec();
                }
            }
            out
        }
    }
}

fn decompress(data: &[u8], algo: BundleCompression, hint: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    match algo {
        BundleCompression::None => Some(data.to_vec()),
        BundleCompression::Gzip => {
            let mut out = Vec::with_capacity(hint);
            flate2::read::GzDecoder::new(data).read_to_end(&mut out).ok().map(|_| out)
        }
        BundleCompression::Zstd => zstd::decode_all(data).ok(),
        BundleCompression::Brotli => {
            let mut out = Vec::with_capacity(hint);
            brotli::Decompressor::new(data, 4096).read_to_end(&mut out).ok().map(|_| out)
        }
    }
}

// ===================================================================
// Global bundle + FFI vtable (queried by code_bundle_hooks.c)
// ===================================================================

/// The process-wide bundle. Set once at startup, immutable thereafter, read
/// concurrently by every PHP thread with no lock.
static BUNDLE: OnceLock<Bundle> = OnceLock::new();

/// Metadata answer for the C `url_stat` hook. `#[repr(C)]` — mirrored by
/// `ephpm_bundle_stat_t` in `code_bundle_hooks.c`.
#[repr(C)]
pub struct BundleStat {
    /// Non-zero if the path is a bundled directory.
    pub is_dir: c_int,
    /// File size in bytes (uncompressed). Zero for directories.
    pub size: i64,
    /// Modification time, Unix seconds.
    pub mtime: i64,
    /// Synthetic stable inode.
    pub inode: u64,
}

/// The vtable the C hooks call to query the bundle. `#[repr(C)]` — mirrored by
/// `ephpm_bundle_callbacks_t` in `code_bundle_hooks.c`.
#[repr(C)]
pub struct BundleCallbacks {
    /// Returns non-zero while a bundle is installed.
    pub enabled: extern "C" fn() -> c_int,
    /// Resolve `path` (bytes+len) to its canonical NUL-terminated absolute path
    /// on a hit, else null. The returned pointer is process-lifetime stable and
    /// must NOT be freed by the caller.
    pub resolve: extern "C" fn(*const c_char, usize) -> *const c_char,
    /// Fill `*out` for a hit and return 1; return 0 on a miss.
    pub stat: extern "C" fn(*const c_char, usize, *mut BundleStat) -> c_int,
    /// Return a pointer to the uncompressed source (`*out_len` bytes) on a hit,
    /// else null. `*needs_free` is set to 1 when the buffer was freshly
    /// allocated (Model B decompress) and must be released with `free_source`,
    /// 0 when it points into resident RAM.
    pub get_source:
        extern "C" fn(*const c_char, usize, *mut usize, *mut c_int) -> *const c_uchar,
    /// Release a buffer returned by `get_source` with `needs_free == 1`.
    pub free_source: extern "C" fn(*const c_uchar, usize),
}

#[cfg(php_linked)]
unsafe extern "C" {
    /// Install the bundle overrides into PHP's C indirection points. Copies the
    /// callbacks vtable into a C static and swaps `zend_resolve_path`,
    /// `zend_stream_open_function`, and the plain-files `url_stat` op, saving the
    /// originals for miss-delegation. Idempotent; call once after
    /// `php_embed_init()`.
    fn ephpm_bundle_install_hooks(cb: *const BundleCallbacks);
}

/// Borrow a byte slice from a C `(ptr, len)` pair as a normalized key.
fn key_from_c(path: *const c_char, len: usize) -> Option<String> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the C hooks pass a valid pointer to `len` readable bytes (the
    // NUL-terminated filename PHP handed us). We only read within `len`.
    let bytes = unsafe { std::slice::from_raw_parts(path.cast::<u8>(), len) };
    let s = String::from_utf8_lossy(bytes);
    Some(normalize_key(&s))
}

extern "C" fn cb_enabled() -> c_int {
    c_int::from(BUNDLE.get().is_some())
}

extern "C" fn cb_resolve(path: *const c_char, len: usize) -> *const c_char {
    let Some(bundle) = BUNDLE.get() else { return std::ptr::null() };
    let Some(key) = key_from_c(path, len) else { return std::ptr::null() };
    match bundle.get(&key) {
        Some(entry) => entry.canon.as_ptr(),
        None => std::ptr::null(),
    }
}

extern "C" fn cb_stat(path: *const c_char, len: usize, out: *mut BundleStat) -> c_int {
    let Some(bundle) = BUNDLE.get() else { return 0 };
    let Some(key) = key_from_c(path, len) else { return 0 };
    if out.is_null() {
        return 0;
    }
    if let Some(entry) = bundle.get(&key) {
        // SAFETY: `out` is a valid, writable BundleStat provided by the C hook.
        unsafe {
            (*out).is_dir = 0;
            (*out).size = i64::try_from(entry.raw_len).unwrap_or(i64::MAX);
            (*out).mtime = entry.mtime;
            (*out).inode = entry.inode;
        }
        return 1;
    }
    if bundle.dirs.contains(&key) {
        // SAFETY: as above.
        unsafe {
            (*out).is_dir = 1;
            (*out).size = 0;
            (*out).mtime = 0;
            (*out).inode = 0;
        }
        return 1;
    }
    0
}

extern "C" fn cb_get_source(
    path: *const c_char,
    len: usize,
    out_len: *mut usize,
    needs_free: *mut c_int,
) -> *const c_uchar {
    let Some(bundle) = BUNDLE.get() else { return std::ptr::null() };
    let Some(key) = key_from_c(path, len) else { return std::ptr::null() };
    let Some(entry) = bundle.get(&key) else { return std::ptr::null() };
    if out_len.is_null() || needs_free.is_null() {
        return std::ptr::null();
    }
    match &entry.data {
        StoredData::Raw(v) => {
            // SAFETY: out_len/needs_free are valid writable pointers.
            unsafe {
                *out_len = v.len();
                *needs_free = 0;
            }
            v.as_ptr()
        }
        StoredData::Compressed(v) => {
            let Some(plain) = decompress(v, bundle.algo, entry.raw_len) else {
                return std::ptr::null();
            };
            let boxed = plain.into_boxed_slice();
            let out_bytes = boxed.len();
            let ptr = Box::into_raw(boxed).cast::<u8>();
            // SAFETY: as above.
            unsafe {
                *out_len = out_bytes;
                *needs_free = 1;
            }
            ptr.cast_const()
        }
    }
}

extern "C" fn cb_free_source(ptr: *const c_uchar, len: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr`/`len` came from a `get_source` call that set `needs_free=1`,
    // i.e. a `Box<[u8]>` of exactly `len` bytes we leaked with `Box::into_raw`.
    // Reconstructing the same boxed slice and dropping it frees it correctly.
    unsafe {
        let slice = std::slice::from_raw_parts_mut(ptr.cast::<u8>().cast_mut(), len);
        drop(Box::from_raw(std::ptr::from_mut::<[u8]>(slice)));
    }
}

/// Install `bundle` as the process bundle and wire the C overrides into PHP.
///
/// Call **once**, after `php_embed_init()` and before serving requests. A
/// second call is ignored (the bundle is already set). In stub mode (no
/// `php_linked`) this stores the bundle but installs no PHP hooks — used by unit
/// tests.
///
/// # Errors
///
/// Returns the passed-back bundle (boxed) in `Err` if a bundle was already
/// installed.
pub fn install(bundle: Bundle) -> Result<(), Box<Bundle>> {
    BUNDLE.set(bundle).map_err(Box::new)?;

    let cb = BundleCallbacks {
        enabled: cb_enabled,
        resolve: cb_resolve,
        stat: cb_stat,
        get_source: cb_get_source,
        free_source: cb_free_source,
    };

    #[cfg(php_linked)]
    // SAFETY: `ephpm_bundle_install_hooks` copies the vtable into a C static and
    // only swaps global function pointers; it dereferences `&cb` only during the
    // call, so a stack value is fine. Called once on the single-threaded startup
    // path before any tokio worker exists.
    unsafe {
        ephpm_bundle_install_hooks(&raw const cb);
    }
    #[cfg(not(php_linked))]
    {
        let _ = cb;
    }
    Ok(())
}

/// Whether a bundle is currently installed (test/introspection helper).
#[must_use]
pub fn is_installed() -> bool {
    BUNDLE.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dot_dot_and_case() {
        let a = normalize_key("/app/src/../vendor/./Foo.php");
        let b = normalize_key("/app/vendor/Foo.php");
        assert_eq!(a, b);
        if cfg!(windows) {
            assert_eq!(normalize_key(r"C:\App\Src\Foo.PHP"), normalize_key(r"c:/app/src/foo.php"));
            // Verbatim `\\?\` prefix (from std::fs::canonicalize) must key the
            // same as the plain path — the real POC activation bug.
            assert_eq!(
                normalize_key(r"\\?\C:\app\vendor\Foo.php"),
                normalize_key(r"C:\app\vendor\Foo.php")
            );
        }
    }

    #[test]
    fn scan_indexes_php_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        std::fs::write(dir.path().join("src/Foo.php"), b"<?php class Foo {}").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"nope").unwrap();

        let b = Bundle::from_scan(dir.path(), BundleCompression::None, usize::MAX).unwrap();
        assert_eq!(b.file_count(), 2, "only .php files indexed");

        let key = normalize_key(&dir.path().join("src/Foo.php").to_string_lossy());
        let entry = b.get(&key).expect("Foo.php present");
        assert_eq!(entry.raw_len, "<?php class Foo {}".len());
        // The parent dir is registered.
        let dkey = normalize_key(&dir.path().join("src").to_string_lossy());
        assert!(b.dirs.contains(&dkey), "src/ registered as a directory");
    }

    #[test]
    fn max_bytes_cap_refuses_partial_bundle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.php"), vec![b'x'; 10_000]).unwrap();
        let err = Bundle::from_scan(dir.path(), BundleCompression::None, 1024).unwrap_err();
        assert!(matches!(err, BundleError::TooLarge { .. }));
    }

    #[test]
    fn compression_roundtrips_every_algo() {
        let src = b"<?php\n// a representative source file\nfunction f(){ return 42; }\n".repeat(50);
        for algo in [BundleCompression::Gzip, BundleCompression::Zstd, BundleCompression::Brotli] {
            let c = compress(&src, algo);
            let back = decompress(&c, algo, src.len()).expect("decompress ok");
            assert_eq!(back, src, "{} roundtrip", algo.label());
        }
    }

    #[test]
    fn compressed_bundle_serves_original_source() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"<?php return ['k' => 'value', 'n' => 12345];";
        std::fs::write(dir.path().join("cfg.php"), body).unwrap();
        let b = Bundle::from_scan(dir.path(), BundleCompression::Zstd, usize::MAX).unwrap();
        let key = normalize_key(&dir.path().join("cfg.php").to_string_lossy());
        let entry = b.get(&key).unwrap();
        let StoredData::Compressed(stored) = &entry.data else {
            panic!("expected compressed storage");
        };
        let plain = decompress(stored, BundleCompression::Zstd, entry.raw_len).unwrap();
        assert_eq!(plain, body);
    }
}
