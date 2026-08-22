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
//! # Miss semantics: overlay vs sealed
//!
//! Overlay ([`BundleSemantics::Overlay`], the default) only ever *accelerates*
//! — a miss costs a bundle lookup and then does exactly what PHP would have
//! done. That leaves the dominant cost of a PSR-4 autoloader untouched: its
//! probes are mostly **misses**, one per candidate directory that does not hold
//! the class, and each falls through to a real `stat`.
//!
//! [`BundleSemantics::Sealed`] closes that hole for **explicitly declared
//! subtrees only**. Inside a sealed root the scan has enumerated every
//! indexed-extension file, so absence from the index is **authoritative** and
//! can be answered "does not exist" from RAM with zero syscalls.
//!
//! Sealed is a **correctness** change, not a pure optimization: inside a sealed
//! root it deletes the overlay's "a miss falls through, so a file created at
//! runtime still works" property. Three properties keep that safe:
//!
//! 1. **Declared roots, not the whole docroot.** The win and the risk live in
//!    *disjoint* directories: the misses worth eliminating are PSR-4 decoy
//!    probes under `vendor/`, while every framework write that would break us
//!    is under `var/cache/`, `bootstrap/cache/`, `storage/framework/views/`.
//!    Sealing only `vendor/` removes the failure mode **by construction**
//!    rather than by detection. With no roots declared, `Sealed` behaves
//!    exactly like [`BundleSemantics::Overlay`] — the dangerous half is
//!    unreachable without a per-path opt-in.
//! 2. **Authority is a one-way latch.** A sealed root starts *armed*. Anything
//!    that proves the index could be wrong about it — a write open inside it, or
//!    a confirmed negative that turned out to exist on disk — **permanently
//!    disarms that root** ([`SealedRoot`]). A disarmed root degrades to overlay
//!    semantics: correct, slower, forever. There is no re-arm, so there is no
//!    stale-generation race and no lost-event window, and the index itself is
//!    never mutated — every FFI lifetime contract below stays valid unchanged.
//! 3. **It fails loudly.** Source opens and include resolution always confirm a
//!    negative against disk before returning it ([`Probe::Source`]); a mismatch
//!    logs `WARN` naming the path, disarms, and falls through.
//!    `verify_negatives` extends that confirmation to the hot `is_file`/`stat`
//!    probes too — slow by construction, for diagnosing a suspected breakage.
//!
//! # Publication: one atomic `set`, never mutation
//!
//! The scan is not on the startup critical path. The C hooks are installed
//! synchronously (inert while the index is empty — `enabled()` is false, so
//! every hook delegates, which is byte-for-byte `code_bundle = "off"`
//! behaviour), and a single background thread scans and publishes the finished
//! index with **one** [`OnceLock::set`]. There is no incremental fill and no
//! half-built state, which matters most for sealed roots: a partially scanned
//! index answering negatives authoritatively would report "does not exist" for
//! files it simply had not reached yet. If the scan fails, nothing is ever
//! published and the process stays on the fall-through path permanently.
//!
//! Because authority is only ever *removed*, the index stays immutable for its
//! whole life and the FFI callbacks can keep handing C **borrowed pointers into
//! it** (`FileEntry::canon`, and the resident source bytes for
//! [`BundleCompression::None`]) that outlive the call. Introducing a swappable
//! index later would invalidate exactly those two escapes — they are confined to
//! [`cb_resolve`] and [`cb_get_source`] on purpose.
//!
//! # Mechanism
//!
//! The C side (`code_bundle_hooks.c`) overrides four PHP indirection points at
//! SAPI init and delegates to the saved originals on a miss:
//!
//! * `zend_resolve_path` — include/require path resolution.
//! * `zend_stream_open_function` — the compiler's source open when OPcache is
//!   off.
//! * `php_plain_files_wrapper`'s `url_stat` op — userland
//!   `file_exists`/`is_file`/`stat`/`filemtime` and OPcache probing.
//! * `php_plain_files_wrapper`'s `stream_opener` op — the source read OPcache
//!   itself performs (it calls the *saved original*
//!   `zend_stream_open_function`, so the override above does not cover it).
//!
//! Both source-serving hooks hand PHP a real `php_stream` whose `stat` op
//! reports the index's recorded mtime, because OPcache reads a script's
//! timestamp through `stream->ops->stat` and refuses to cache anything whose
//! timestamp it cannot obtain.
//!
//! Those C hooks query **this** module through a small [`BundleCallbacks`]
//! vtable installed once at startup. The bundle is immutable after load and
//! stored in a process-lifetime [`OnceLock`], so every `spawn_blocking` PHP
//! thread reads it concurrently with no locking — trivially ZTS-safe.
//!
//! # Scope (POC)
//!
//! `.php` code only. Directory listing (`scandir`/`glob` from the manifest) is
//! a follow-on: this POC covers the drop-in autoloader path (resolve +
//! stream-open + `url_stat` + `fopen`), which is what a Composer/Symfony
//! autoloader actually exercises.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uchar};
use std::path::Path;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// The one filename extension the scanner indexes. Absence from the index is
/// only ever authoritative for this extension, because it is the only one the
/// scan is guaranteed to have enumerated exhaustively — hence a single
/// constant shared by [`Bundle::from_scan`] and [`is_indexed_extension`],
/// pinned by the `sealed_scope_matches_scan_filter` test.
const INDEXED_EXTENSION: &str = "php";

/// Lookup outcome handed to C. Mirrored by the `EPHPM_BUNDLE_*` defines in
/// `code_bundle_hooks.c`.
///
/// `HIT` — answer from RAM. `UNKNOWN` — delegate to PHP's real handler.
/// `ABSENT` — answer "does not exist" from RAM with no syscall.
const BUNDLE_HIT: c_int = 1;
/// See [`BUNDLE_HIT`].
const BUNDLE_UNKNOWN: c_int = 0;
/// See [`BUNDLE_HIT`].
const BUNDLE_ABSENT: c_int = -1;

/// What a bundle **miss** means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleSemantics {
    /// A miss falls through to the real filesystem. Always correct: the bundle
    /// can only make things faster, never change an answer.
    Overlay,
    /// A miss on an indexed-extension path under a **declared sealed root** is
    /// answered "does not exist" from RAM, with no syscall. Every other miss —
    /// including everywhere else under the document root — still falls through.
    ///
    /// With no sealed roots declared this is identical to [`Self::Overlay`].
    Sealed,
}

impl BundleSemantics {
    /// Lowercase label for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Overlay => "overlay",
            Self::Sealed => "sealed",
        }
    }
}

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
    /// Normalized key of the scanned document root. Used for logging and for
    /// validating declared sealed roots; it is **not** itself an authority
    /// boundary.
    docroot_key: String,
    /// Subtrees the index is allowed to speak authoritatively about, each with
    /// its own one-way arm/disarm latch. Empty ⇒ no authoritative negatives
    /// anywhere, whatever `semantics` says.
    sealed_roots: Vec<SealedRoot>,
    /// What a miss means.
    semantics: BundleSemantics,
    /// Diagnostic: confirm **every** authoritative negative against disk, not
    /// just the source-open ones, and log a `WARN` on any mismatch. Gives back
    /// the syscalls sealed mode removes — for debugging only.
    verify_negatives: bool,
    /// Number of `.php` files indexed.
    file_count: usize,
    /// Total uncompressed source bytes.
    raw_bytes: usize,
    /// Bytes actually resident in RAM (raw for Model A/None, compressed for
    /// Model B).
    resident_bytes: usize,
}

/// One declared subtree the index may speak authoritatively about, plus its
/// **one-way** authority latch.
///
/// Armed at publication; disarmed the first time anything proves the index could
/// be wrong about this subtree (a write inside it, or a confirmed negative that
/// existed on disk). There is deliberately **no way to re-arm**: a disarmed root
/// falls back to overlay semantics — correct, slower, forever. That is what lets
/// the index stay immutable and the FFI pointer lifetimes stay trivially valid.
struct SealedRoot {
    /// Normalized absolute path key of the subtree.
    key: String,
    /// `true` while negatives under [`Self::key`] may be answered from RAM.
    armed: std::sync::atomic::AtomicBool,
}

impl SealedRoot {
    /// Whether `key` lies at or strictly under this root. Component-aware, so
    /// `/app/vendor-backup/x.php` is not inside `/app/vendor`.
    fn contains(&self, key: &str) -> bool {
        let Some(rest) = key.strip_prefix(self.key.as_str()) else {
            return false;
        };
        if self.key.ends_with(['/', '\\']) {
            !rest.is_empty()
        } else {
            rest.starts_with(['/', '\\'])
        }
    }
}

/// Why the index is being consulted — decides how much we are willing to pay to
/// double-check an authoritative negative before returning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// `is_file` / `stat` / `filemtime`: the hot path, hundreds per request and
    /// the entire reason sealed mode exists. A negative is returned without a
    /// syscall unless `verify_negatives` is on.
    Metadata,
    /// A source open or an `include`/`require` path resolution: rare, and a
    /// wrong "does not exist" here is a hard, confusing failure. An
    /// authoritative negative is **always** confirmed against disk first.
    Source,
}

/// What a normalized path key resolves to in the index.
enum Lookup<'a> {
    /// An indexed file.
    File(&'a FileEntry),
    /// An indexed directory (an ancestor of some indexed file).
    Dir,
    /// Not indexed, and the bundle cannot speak for it — fall through to disk.
    Unknown,
    /// Not indexed, and the bundle *is* the authority for it (sealed mode).
    Absent,
}

impl std::fmt::Debug for Bundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bundle")
            .field("file_count", &self.file_count)
            .field("raw_bytes", &self.raw_bytes)
            .field("resident_bytes", &self.resident_bytes)
            .field("compression", &self.algo.label())
            .field("semantics", &self.semantics.label())
            .finish_non_exhaustive()
    }
}

/// Everything that decides what a scan produces and how the resulting index is
/// allowed to answer. Build it with [`BundleSpec::new`], which validates the
/// declared sealed roots.
#[derive(Debug, Clone)]
pub struct BundleSpec {
    /// Directory scanned recursively for `.php` files.
    pub docroot: std::path::PathBuf,
    /// How source bytes are held in RAM.
    pub compression: BundleCompression,
    /// Resident-byte cap; exceeding it refuses the whole bundle.
    pub max_bytes: usize,
    /// What a miss means.
    pub semantics: BundleSemantics,
    /// Normalized keys of the subtrees the index may answer negatives for.
    /// Empty ⇒ no authoritative negatives anywhere.
    sealed_roots: Vec<String>,
    /// Diagnostic: confirm every authoritative negative against disk.
    pub verify_negatives: bool,
}

impl BundleSpec {
    /// Validate and normalize a bundle build request.
    ///
    /// `sealed_paths` entries may be relative (resolved against `docroot`) or
    /// absolute. Every one of them **must** land inside `docroot`: a sealed root
    /// outside the document root is a configuration error, not something to warn
    /// about and continue with, because it would let the index speak for a tree
    /// it never scanned.
    ///
    /// `sealed_paths` is ignored unless `semantics` is
    /// [`BundleSemantics::Sealed`]; with `Sealed` and an empty list the bundle
    /// behaves exactly like [`BundleSemantics::Overlay`].
    ///
    /// # Errors
    ///
    /// [`BundleError::SealedPathOutsideDocroot`] if any declared sealed path is
    /// not inside `docroot`.
    pub fn new(
        docroot: std::path::PathBuf,
        compression: BundleCompression,
        max_bytes: usize,
        semantics: BundleSemantics,
        sealed_paths: &[String],
        verify_negatives: bool,
    ) -> Result<Self, BundleError> {
        let docroot_key = normalize_key(&docroot.to_string_lossy());
        let mut sealed_roots = Vec::new();
        if semantics == BundleSemantics::Sealed {
            for raw in sealed_paths {
                let joined = docroot.join(raw);
                let key = normalize_key(&joined.to_string_lossy());
                let inside = key
                    .strip_prefix(docroot_key.as_str())
                    .is_some_and(|rest| rest.starts_with(['/', '\\']));
                if !inside {
                    return Err(BundleError::SealedPathOutsideDocroot {
                        path: raw.clone(),
                        resolved: key,
                        docroot: docroot_key,
                    });
                }
                if !sealed_roots.contains(&key) {
                    sealed_roots.push(key);
                }
            }
        }
        Ok(Self { docroot, compression, max_bytes, semantics, sealed_roots, verify_negatives })
    }

    /// The validated, normalized sealed-root keys.
    #[must_use]
    pub fn sealed_roots(&self) -> &[String] {
        &self.sealed_roots
    }
}

/// Errors building a bundle.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// A declared `code_bundle_sealed_paths` entry resolved outside the document
    /// root. Fail-closed: the index must never claim authority over a tree it
    /// did not scan.
    #[error(
        "[php] code_bundle_sealed_paths entry {path:?} resolves to {resolved:?}, which is \
         outside the document root {docroot:?}; sealed roots must be inside the document root"
    )]
    SealedPathOutsideDocroot {
        /// The entry as configured.
        path: String,
        /// Its normalized absolute form.
        resolved: String,
        /// The normalized document root.
        docroot: String,
    },
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

    /// What a miss means for this bundle.
    #[must_use]
    pub fn semantics(&self) -> BundleSemantics {
        self.semantics
    }

    /// Whether every authoritative negative is confirmed against disk.
    #[must_use]
    pub fn verify_negatives(&self) -> bool {
        self.verify_negatives
    }

    /// Normalized key of the scanned document root (diagnostics; not an
    /// authority boundary — see [`Self::sealed_root_keys`]).
    #[must_use]
    pub fn docroot_key(&self) -> &str {
        &self.docroot_key
    }

    /// The declared sealed roots (normalized keys), in declaration order.
    #[must_use]
    pub fn sealed_root_keys(&self) -> Vec<&str> {
        self.sealed_roots.iter().map(|r| r.key.as_str()).collect()
    }

    /// Build a bundle by scanning `spec.docroot` recursively for `.php` files.
    ///
    /// `spec.max_bytes` caps the resident footprint: if adding a file would push
    /// the resident total past the cap, the scan aborts with
    /// [`BundleError::TooLarge`] and the caller falls through to disk
    /// (refuse-to-bundle-beyond-cap; no partial bundle is ever installed).
    ///
    /// # Errors
    ///
    /// [`BundleError::Scan`] on an I/O failure, [`BundleError::TooLarge`] if the
    /// cap is exceeded.
    pub fn from_scan(spec: &BundleSpec) -> Result<Self, BundleError> {
        let docroot = spec.docroot.as_path();
        let algo = spec.compression;
        let max_bytes = spec.max_bytes;
        let semantics = spec.semantics;
        let verify_negatives = spec.verify_negatives;
        let mut files = HashMap::new();
        let mut dirs = HashSet::new();
        let mut raw_bytes = 0usize;
        let mut resident_bytes = 0usize;
        let mut next_inode: u64 = 1;

        // Always index the docroot itself as a directory.
        let docroot_key = normalize_key(&docroot.to_string_lossy());
        dirs.insert(docroot_key.clone());

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
                    .is_some_and(|e| e.eq_ignore_ascii_case(INDEXED_EXTENSION));
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
                files.insert(key, FileEntry { canon, data, raw_len, mtime, inode });
            }
        }

        let file_count = files.len();
        // Sealed roots are armed exactly here — after a COMPLETE scan. A
        // partially built index that answered negatives authoritatively would
        // report "does not exist" for files it merely had not reached yet.
        let sealed_roots = if semantics == BundleSemantics::Sealed {
            spec.sealed_roots
                .iter()
                .map(|key| SealedRoot {
                    key: key.clone(),
                    armed: std::sync::atomic::AtomicBool::new(true),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            files,
            dirs,
            algo,
            docroot_key,
            sealed_roots,
            semantics,
            verify_negatives,
            file_count,
            raw_bytes,
            resident_bytes,
        })
    }

    /// Resolve an already-normalized key to a [`Lookup`].
    ///
    /// This is **the** correctness boundary for [`BundleSemantics::Sealed`].
    /// [`Lookup::Absent`] — "this file does not exist, answered without a
    /// syscall" — is returned only when **all** of the following hold:
    ///
    /// 1. the bundle was built with [`BundleSemantics::Sealed`];
    /// 2. the key is not an indexed file **and** not an indexed directory;
    /// 3. the key carries the [indexed extension](INDEXED_EXTENSION) — the only
    ///    extension the scan enumerates exhaustively, so the only one whose
    ///    absence the index can vouch for;
    /// 4. the key lies under a **declared sealed root** that is still *armed*.
    ///
    /// Everything else — anywhere under the document root that was not declared
    /// sealed, a path outside the docroot entirely, a non-`.php` file, a
    /// relative/`include_path` probe, a runtime directory, an upload, a session
    /// file, or a sealed root that has since been disarmed — yields
    /// [`Lookup::Unknown`] and falls through to the real filesystem exactly as
    /// before.
    ///
    /// Even inside the scope, a negative is **confirmed against disk** before
    /// being returned when `probe` is [`Probe::Source`] (rare, and a wrong
    /// answer is fatal) or when `verify_negatives` is on. A confirmed mismatch
    /// logs a `WARN`, **permanently disarms** that root, and degrades to
    /// [`Lookup::Unknown`].
    fn lookup(&self, key: &str, probe: Probe) -> Lookup<'_> {
        if let Some(entry) = self.files.get(key) {
            return Lookup::File(entry);
        }
        if self.dirs.contains(key) {
            return Lookup::Dir;
        }
        if !is_indexed_extension(key) {
            return Lookup::Unknown;
        }
        let Some(root) = self.armed_root_for(key) else {
            return Lookup::Unknown;
        };
        if (probe == Probe::Source || self.verify_negatives) && Path::new(key).exists() {
            disarm(root, key, DisarmCause::NegativeWasWrong(probe));
            return Lookup::Unknown;
        }
        Lookup::Absent
    }

    /// The still-armed sealed root containing `key`, if any. Only meaningful in
    /// [`BundleSemantics::Sealed`]; `sealed_roots` is empty otherwise.
    fn armed_root_for(&self, key: &str) -> Option<&SealedRoot> {
        if self.semantics != BundleSemantics::Sealed {
            return None;
        }
        self.sealed_roots
            .iter()
            .find(|r| r.contains(key) && r.armed.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Called from the plain-files `stream_opener` hook when an application
    /// opens a path for **writing**. A write of a `.php` file inside a sealed
    /// root proves the index can no longer speak for that subtree, so the root
    /// is permanently disarmed — the hazard is removed at the moment it is
    /// created, not detected later when it bites.
    fn note_write(&self, key: &str) {
        if !is_indexed_extension(key) {
            return;
        }
        // A write to a path in ANY declared root (armed or not) is worth
        // reporting once; `disarm` is idempotent and only logs on the
        // arm→disarm edge.
        let Some(root) = self.sealed_roots.iter().find(|r| r.contains(key)) else {
            return;
        };
        let cause = if self.files.contains_key(key) {
            DisarmCause::BundledFileOverwritten
        } else {
            DisarmCause::FileCreatedInside
        };
        disarm(root, key, cause);
    }
}

/// Why a sealed root lost its authority. Each variant is a distinct, actionable
/// operator message.
#[derive(Debug, Clone, Copy)]
enum DisarmCause {
    /// A confirmed negative turned out to exist on disk.
    NegativeWasWrong(Probe),
    /// An application created a `.php` file inside the root.
    FileCreatedInside,
    /// An application overwrote a file the index already holds.
    BundledFileOverwritten,
}

/// Flip a sealed root's one-way latch and log the reason, exactly once per root.
///
/// `Relaxed` ordering is sufficient: the latch guards a *policy* decision, not
/// the visibility of any data. A racing reader that still sees `true` for a few
/// nanoseconds answers one more negative from an index that was correct until
/// this instant — the same answer it would have given a moment earlier.
fn disarm(root: &SealedRoot, path: &str, cause: DisarmCause) {
    use std::sync::atomic::Ordering;
    if !root.armed.swap(false, Ordering::Relaxed) {
        return; // already disarmed — no duplicate log
    }
    let (detail, probe) = match cause {
        DisarmCause::NegativeWasWrong(probe) => (
            "a .php file exists on disk that is NOT in the startup index (it was created \
             after the scan)",
            Some(probe),
        ),
        DisarmCause::FileCreatedInside => ("an application CREATED a .php file inside it", None),
        DisarmCause::BundledFileOverwritten => (
            "an application OVERWROTE a .php file the index holds (PHP will keep executing \
             the bytes captured at startup until restart)",
            None,
        ),
    };
    tracing::warn!(
        sealed_root = root.key,
        path,
        probe = ?probe,
        "[php] code_bundle sealed root PERMANENTLY DISARMED: {detail}. Lookups under this \
         root now fall through to the filesystem — correct, but without the syscall saving \
         sealed mode exists for. There is no re-arm; restart to re-index, or drop this path \
         from [php] code_bundle_sealed_paths."
    );
}

/// Whether a normalized key names a file with the [indexed
/// extension](INDEXED_EXTENSION).
///
/// Purely lexical (no syscall): looks only at the final path component, so a
/// *directory* called `foo.php` also qualifies — harmless, because
/// [`Bundle::lookup`] consults the indexed `dirs` set first.
fn is_indexed_extension(key: &str) -> bool {
    let name = key.rsplit(['/', '\\']).next().unwrap_or(key);
    name.rsplit_once('.').is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(INDEXED_EXTENSION))
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

/// Source bytes plus the metadata a bundle-backed `php_stream` needs to answer
/// `stat`. `#[repr(C)]` — mirrored by `ephpm_bundle_source_t` in
/// `code_bundle_hooks.c`.
#[repr(C)]
pub struct BundleSource {
    /// Uncompressed source bytes.
    pub data: *const c_uchar,
    /// Length of `data`.
    pub len: usize,
    /// 1 when `data` was freshly allocated (a Model B decompress) and must be
    /// released with `free_source`; 0 when it points into resident RAM.
    pub needs_free: c_int,
    /// Modification time, Unix seconds — what the stream's `stat` op reports,
    /// and therefore what OPcache uses as the script timestamp.
    pub mtime: i64,
    /// Synthetic stable inode.
    pub inode: u64,
}

/// The vtable the C hooks call to query the bundle. `#[repr(C)]` — mirrored by
/// `ephpm_bundle_callbacks_t` in `code_bundle_hooks.c`.
///
/// `resolve`, `stat` and `get_source` are tri-state: they return
/// [`BUNDLE_HIT`] (out-parameter written), [`BUNDLE_UNKNOWN`] (caller must
/// delegate to PHP's real handler) or [`BUNDLE_ABSENT`] (caller must answer
/// "does not exist" without touching the filesystem).
#[repr(C)]
pub struct BundleCallbacks {
    /// Returns non-zero while a bundle is installed.
    pub enabled: extern "C" fn() -> c_int,
    /// On [`BUNDLE_HIT`], writes the canonical NUL-terminated absolute path to
    /// `*out_canon`. That pointer is process-lifetime stable and must NOT be
    /// freed by the caller.
    pub resolve: extern "C" fn(*const c_char, usize, *mut *const c_char) -> c_int,
    /// On [`BUNDLE_HIT`], fills `*out`.
    pub stat: extern "C" fn(*const c_char, usize, *mut BundleStat) -> c_int,
    /// On [`BUNDLE_HIT`], fills `*out`. Ownership of `out.data` passes to the
    /// caller when `out.needs_free` is 1.
    pub get_source: extern "C" fn(*const c_char, usize, *mut BundleSource) -> c_int,
    /// Release a buffer returned by `get_source` with `needs_free == 1`.
    pub free_source: extern "C" fn(*const c_uchar, usize),
    /// Breadcrumb hook: the plain-files wrapper is about to open this path for
    /// **writing**. Used only to log the sealed-mode contract violation; it
    /// never changes what PHP does.
    pub note_write: extern "C" fn(*const c_char, usize),
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

extern "C" fn cb_resolve(path: *const c_char, len: usize, out_canon: *mut *const c_char) -> c_int {
    let Some(bundle) = BUNDLE.get() else { return BUNDLE_UNKNOWN };
    let Some(key) = key_from_c(path, len) else { return BUNDLE_UNKNOWN };
    if out_canon.is_null() {
        return BUNDLE_UNKNOWN;
    }
    // Include/require resolution: a wrong negative is fatal, so it is confirmed.
    match bundle.lookup(&key, Probe::Source) {
        Lookup::File(entry) => {
            // SAFETY: `out_canon` is a valid writable pointer supplied by the C
            // hook; `entry.canon` lives as long as the process-lifetime bundle.
            unsafe { *out_canon = entry.canon.as_ptr() };
            BUNDLE_HIT
        }
        Lookup::Absent => BUNDLE_ABSENT,
        Lookup::Dir | Lookup::Unknown => BUNDLE_UNKNOWN,
    }
}

extern "C" fn cb_stat(path: *const c_char, len: usize, out: *mut BundleStat) -> c_int {
    let Some(bundle) = BUNDLE.get() else { return BUNDLE_UNKNOWN };
    let Some(key) = key_from_c(path, len) else { return BUNDLE_UNKNOWN };
    if out.is_null() {
        return BUNDLE_UNKNOWN;
    }
    // The hot path: hundreds of these per request. Negatives are answered from
    // RAM with no syscall unless `verify_negatives` is on.
    match bundle.lookup(&key, Probe::Metadata) {
        Lookup::File(entry) => {
            // SAFETY: `out` is a valid, writable BundleStat provided by the C hook.
            unsafe {
                (*out).is_dir = 0;
                (*out).size = i64::try_from(entry.raw_len).unwrap_or(i64::MAX);
                (*out).mtime = entry.mtime;
                (*out).inode = entry.inode;
            }
            BUNDLE_HIT
        }
        Lookup::Dir => {
            // SAFETY: as above.
            unsafe {
                (*out).is_dir = 1;
                (*out).size = 0;
                (*out).mtime = 0;
                (*out).inode = 0;
            }
            BUNDLE_HIT
        }
        Lookup::Absent => BUNDLE_ABSENT,
        Lookup::Unknown => BUNDLE_UNKNOWN,
    }
}

extern "C" fn cb_get_source(path: *const c_char, len: usize, out: *mut BundleSource) -> c_int {
    let Some(bundle) = BUNDLE.get() else { return BUNDLE_UNKNOWN };
    let Some(key) = key_from_c(path, len) else { return BUNDLE_UNKNOWN };
    if out.is_null() {
        return BUNDLE_UNKNOWN;
    }
    // A source read: confirmed against disk before any negative is returned.
    let entry = match bundle.lookup(&key, Probe::Source) {
        Lookup::File(entry) => entry,
        Lookup::Absent => return BUNDLE_ABSENT,
        // A directory has no source; let PHP produce its own error for it.
        Lookup::Dir | Lookup::Unknown => return BUNDLE_UNKNOWN,
    };
    let (data, data_len, needs_free) = match &entry.data {
        StoredData::Raw(v) => (v.as_ptr(), v.len(), 0),
        StoredData::Compressed(v) => {
            let Some(plain) = decompress(v, bundle.algo, entry.raw_len) else {
                return BUNDLE_UNKNOWN;
            };
            let boxed = plain.into_boxed_slice();
            let out_bytes = boxed.len();
            (Box::into_raw(boxed).cast::<u8>().cast_const(), out_bytes, 1)
        }
    };
    // SAFETY: `out` is a valid, writable BundleSource provided by the C hook.
    // When `needs_free` is 1 the C side owns the leaked box and returns it
    // through `free_source`; when 0 the pointer borrows process-lifetime RAM.
    unsafe {
        (*out).data = data;
        (*out).len = data_len;
        (*out).needs_free = needs_free;
        (*out).mtime = entry.mtime;
        (*out).inode = entry.inode;
    }
    BUNDLE_HIT
}

extern "C" fn cb_note_write(path: *const c_char, len: usize) {
    let Some(bundle) = BUNDLE.get() else { return };
    if bundle.semantics != BundleSemantics::Sealed {
        return;
    }
    let Some(key) = key_from_c(path, len) else { return };
    bundle.note_write(&key);
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

/// Wire the C overrides into PHP. **Inert until a bundle is published**: with
/// [`BUNDLE`] unset, `cb_enabled` returns 0 and every hook delegates to the
/// saved original, which is byte-for-byte `code_bundle = "off"` behaviour.
///
/// Call **once**, on the single-threaded startup path after `php_embed_init()`
/// and before any PHP request exists — the hooks overwrite PHP's global function
/// pointers, which must not race a reader. Publication of the index itself is a
/// separate, thread-safe step ([`publish`]). In stub mode (no `php_linked`) this
/// is a no-op.
///
/// The verbose name mirrors the C entry point it wraps.
pub fn install_code_bundle_hooks() {
    let cb = BundleCallbacks {
        enabled: cb_enabled,
        resolve: cb_resolve,
        stat: cb_stat,
        get_source: cb_get_source,
        free_source: cb_free_source,
        note_write: cb_note_write,
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
}

/// Publish a **fully built** index as the process bundle, in one atomic
/// [`OnceLock::set`].
///
/// Safe to call from a background thread while requests are already being
/// served: until it returns, every hook falls through to the filesystem; after
/// it returns, every hook consults a complete, immutable index. There is no
/// partially populated state — which is what makes sealed roots safe, since a
/// half-scanned index would report "does not exist" for files it had not reached
/// yet.
///
/// # Errors
///
/// Returns the rejected bundle (boxed) if one was already published.
pub fn publish(bundle: Bundle) -> Result<(), Box<Bundle>> {
    BUNDLE.set(bundle).map_err(Box::new)
}

/// Whether a bundle has been published (test/introspection helper). `false`
/// while a background scan is still running — the fall-through state.
#[must_use]
pub fn is_installed() -> bool {
    BUNDLE.get().is_some()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

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

    /// Fixture docroot laid out like a real app: `vendor/` (the tree worth
    /// sealing) and `var/cache/` (the tree a framework writes into).
    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/acme/lib")).unwrap();
        std::fs::create_dir_all(dir.path().join("var/cache")).unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        std::fs::write(dir.path().join("src/Foo.php"), b"<?php class Foo {}").unwrap();
        std::fs::write(dir.path().join("vendor/acme/lib/Bar.php"), b"<?php class Bar {}").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"nope").unwrap();
        dir
    }

    fn spec_for(
        dir: &tempfile::TempDir,
        semantics: BundleSemantics,
        sealed: &[&str],
        verify: bool,
    ) -> BundleSpec {
        let owned: Vec<String> = sealed.iter().map(|s| (*s).to_string()).collect();
        BundleSpec::new(
            dir.path().to_path_buf(),
            BundleCompression::None,
            usize::MAX,
            semantics,
            &owned,
            verify,
        )
        .expect("spec should validate")
    }

    /// Standard fixture: sealed roots default to `vendor` so the interesting
    /// cases (a sealed tree and an unsealed one) are both present.
    fn fixture(semantics: BundleSemantics) -> (tempfile::TempDir, Bundle) {
        fixture_sealing(semantics, &["vendor"], false)
    }

    fn fixture_sealing(
        semantics: BundleSemantics,
        sealed: &[&str],
        verify: bool,
    ) -> (tempfile::TempDir, Bundle) {
        let dir = fixture_dir();
        let spec = spec_for(&dir, semantics, sealed, verify);
        let b = Bundle::from_scan(&spec).unwrap();
        (dir, b)
    }

    /// Normalized key for `rel` under the fixture docroot.
    fn key_under(dir: &tempfile::TempDir, rel: &str) -> String {
        normalize_key(&dir.path().join(rel).to_string_lossy())
    }

    #[test]
    fn scan_indexes_php_and_skips_others() {
        let (dir, b) = fixture(BundleSemantics::Overlay);
        assert_eq!(b.file_count(), 3, "only .php files indexed");

        let key = key_under(&dir, "src/Foo.php");
        let entry = b.files.get(&key).expect("Foo.php present");
        assert_eq!(entry.raw_len, "<?php class Foo {}".len());
        // The parent dir is registered.
        assert!(b.dirs.contains(&key_under(&dir, "src")), "src/ registered as a directory");
    }

    #[test]
    fn max_bytes_cap_refuses_partial_bundle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.php"), vec![b'x'; 10_000]).unwrap();
        let spec = BundleSpec::new(
            dir.path().to_path_buf(),
            BundleCompression::None,
            1024,
            BundleSemantics::Overlay,
            &[],
            false,
        )
        .unwrap();
        let err = Bundle::from_scan(&spec).unwrap_err();
        assert!(matches!(err, BundleError::TooLarge { .. }));
    }

    // ── sealed-root scoping (the correctness boundary) ───────────────────

    /// Overlay never claims authority, even for a path inside what would
    /// otherwise be a sealed root.
    #[test]
    fn overlay_never_answers_absent() {
        let (dir, b) = fixture(BundleSemantics::Overlay);
        assert!(b.sealed_root_keys().is_empty(), "overlay arms no roots");
        for rel in ["src/Missing.php", "vendor/acme/lib/Missing.php"] {
            assert!(matches!(b.lookup(&key_under(&dir, rel), Probe::Metadata), Lookup::Unknown));
        }
    }

    /// `Sealed` with NO declared roots is exactly `Overlay`. This is the default
    /// shape of the config, so the dangerous half must be unreachable.
    #[test]
    fn sealed_without_declared_roots_is_overlay() {
        let (dir, b) = fixture_sealing(BundleSemantics::Sealed, &[], false);
        assert!(b.sealed_root_keys().is_empty());
        assert!(matches!(
            b.lookup(&key_under(&dir, "vendor/acme/lib/Missing.php"), Probe::Metadata),
            Lookup::Unknown
        ));
    }

    /// Inside a declared root, an unindexed `.php` is `Absent` — the PSR-4
    /// decoy-directory case that is 88% of a warm autoload request. Outside it,
    /// even under the same docroot, nothing changes.
    #[test]
    fn sealed_answers_absent_only_inside_declared_roots() {
        let (dir, b) = fixture(BundleSemantics::Sealed);
        assert_eq!(b.sealed_root_keys(), vec![key_under(&dir, "vendor").as_str()]);

        assert!(
            matches!(
                b.lookup(&key_under(&dir, "vendor/_decoy_a/Pkg0/Class0.php"), Probe::Metadata),
                Lookup::Absent
            ),
            "inside the sealed root: answered from RAM"
        );
        for rel in [
            "src/Missing.php",                 // under docroot, NOT sealed
            "var/cache/Container.php",         // the framework-write tree
            "bootstrap/cache/services.php",    // ditto
            "storage/framework/views/abc.php", // ditto
        ] {
            assert!(
                matches!(b.lookup(&key_under(&dir, rel), Probe::Metadata), Lookup::Unknown),
                "{rel} is outside every sealed root and must fall through"
            );
        }
        // Hits and directories are unaffected.
        assert!(matches!(
            b.lookup(&key_under(&dir, "vendor/acme/lib/Bar.php"), Probe::Metadata),
            Lookup::File(_)
        ));
        assert!(matches!(b.lookup(&key_under(&dir, "vendor"), Probe::Metadata), Lookup::Dir));
    }

    /// Everything outside the narrow predicate falls through: non-`.php`
    /// extensions, extension-less paths, absolute paths elsewhere, and a sibling
    /// directory that merely shares a name prefix with the sealed root.
    #[test]
    fn sealed_falls_through_outside_its_scope() {
        let (dir, b) = fixture(BundleSemantics::Sealed);

        for rel in [
            "vendor/acme/logo.jpg",     // inside the root, wrong extension
            "vendor/acme/cache",        // no extension at all
            "readme.txt",               // present on disk, not indexed
            "var/session/sess_abc123",  // runtime state
            "vendor-backup/x.php",      // prefix-sharing sibling of the root
            "vendorish/pkg/Class0.php", // ditto
        ] {
            assert!(
                matches!(b.lookup(&key_under(&dir, rel), Probe::Metadata), Lookup::Unknown),
                "{rel} must fall through to disk"
            );
        }

        for abs in [
            normalize_key("/etc/php/conf.d/extra.php"),
            normalize_key(r"C:\Windows\Temp\upload.php"),
            normalize_key("relative/include.php"),
        ] {
            assert!(
                matches!(b.lookup(&abs, Probe::Metadata), Lookup::Unknown),
                "{abs} is outside the docroot and must fall through"
            );
        }

        // The docroot itself is a directory, never `Absent`.
        let root = b.docroot_key().to_string();
        assert!(matches!(b.lookup(&root, Probe::Metadata), Lookup::Dir));
    }

    /// A sealed path that escapes the document root is a hard error, not a
    /// warning — the index must never claim authority over an unscanned tree.
    #[test]
    fn sealed_path_outside_docroot_is_a_hard_error() {
        let dir = fixture_dir();
        for escape in ["../elsewhere", "/etc", "vendor/../.."] {
            let err = BundleSpec::new(
                dir.path().to_path_buf(),
                BundleCompression::None,
                usize::MAX,
                BundleSemantics::Sealed,
                &[escape.to_string()],
                false,
            )
            .expect_err("{escape} must be rejected");
            assert!(matches!(err, BundleError::SealedPathOutsideDocroot { .. }));
        }
        // The docroot itself is not "inside" itself either — sealing everything
        // is exactly the docroot-wide behaviour this design rejects.
        assert!(matches!(
            BundleSpec::new(
                dir.path().to_path_buf(),
                BundleCompression::None,
                usize::MAX,
                BundleSemantics::Sealed,
                &[".".to_string()],
                false,
            ),
            Err(BundleError::SealedPathOutsideDocroot { .. })
        ));
    }

    // ── the one-way authority latch ──────────────────────────────────────

    /// A source lookup confirms a negative against disk. When the file turns out
    /// to exist, the root is **permanently disarmed** — so even the hot metadata
    /// probes stop answering authoritatively from that moment on.
    #[test]
    fn a_wrong_negative_permanently_disarms_the_root() {
        let (dir, b) = fixture(BundleSemantics::Sealed);
        let rel = "vendor/acme/lib/GeneratedAfterScan.php";
        let key = key_under(&dir, rel);

        assert!(matches!(b.lookup(&key, Probe::Metadata), Lookup::Absent));
        assert!(matches!(b.lookup(&key, Probe::Source), Lookup::Absent));
        assert!(b.sealed_roots[0].armed.load(Ordering::Relaxed));

        std::fs::write(dir.path().join(rel), b"<?php class G {}").unwrap();

        assert!(
            matches!(b.lookup(&key, Probe::Source), Lookup::Unknown),
            "a source open must confirm the negative and fall through to disk"
        );
        assert!(
            !b.sealed_roots[0].armed.load(Ordering::Relaxed),
            "and it must disarm the root, not just answer this one lookup"
        );
        assert!(
            matches!(b.lookup(&key, Probe::Metadata), Lookup::Unknown),
            "after disarming, even the hot probe falls through"
        );
        // Every other path in the root now falls through too — correct, slower.
        assert!(matches!(
            b.lookup(&key_under(&dir, "vendor/_decoy_a/X.php"), Probe::Metadata),
            Lookup::Unknown
        ));
    }

    /// A write into a sealed root disarms it at the moment the hazard is
    /// created, before any lookup can be wrong.
    #[test]
    fn a_write_inside_a_sealed_root_disarms_it_immediately() {
        let (dir, b) = fixture(BundleSemantics::Sealed);
        let cache = key_under(&dir, "vendor/acme/lib/Compiled.php");
        assert!(b.sealed_roots[0].armed.load(Ordering::Relaxed));

        b.note_write(&cache);

        assert!(!b.sealed_roots[0].armed.load(Ordering::Relaxed), "write must disarm");
        assert!(matches!(b.lookup(&cache, Probe::Metadata), Lookup::Unknown));
        // Idempotent: a second write must not panic or re-arm.
        b.note_write(&cache);
        assert!(!b.sealed_roots[0].armed.load(Ordering::Relaxed));
    }

    /// Writes that cannot invalidate a sealed root leave it armed: a non-`.php`
    /// write, and a `.php` write in an unsealed part of the docroot (which is
    /// exactly where frameworks write).
    #[test]
    fn harmless_writes_do_not_disarm() {
        let (dir, b) = fixture(BundleSemantics::Sealed);
        for rel in [
            "vendor/acme/lib/data.json",    // inside the root, not indexed
            "var/cache/Container.php",      // the framework-write tree
            "bootstrap/cache/services.php", // ditto
            "vendor-backup/x.php",          // prefix-sharing sibling
        ] {
            b.note_write(&key_under(&dir, rel));
            assert!(
                b.sealed_roots[0].armed.load(Ordering::Relaxed),
                "{rel} must not disarm the vendor root"
            );
        }
    }

    /// There is no re-arm path in the API surface: once disarmed, a root stays
    /// disarmed for the life of the (immutable) index.
    #[test]
    fn authority_is_one_way() {
        let (dir, b) = fixture(BundleSemantics::Sealed);
        b.note_write(&key_under(&dir, "vendor/acme/lib/Compiled.php"));
        assert!(!b.sealed_roots[0].armed.load(Ordering::Relaxed));
        // Re-running every operation that touches the latch must keep it off.
        b.note_write(&key_under(&dir, "vendor/acme/lib/Other.php"));
        let _ = b.lookup(&key_under(&dir, "vendor/x.php"), Probe::Source);
        let _ = b.lookup(&key_under(&dir, "vendor/y.php"), Probe::Metadata);
        assert!(!b.sealed_roots[0].armed.load(Ordering::Relaxed));
        assert!(b.armed_root_for(&key_under(&dir, "vendor/z.php")).is_none());
    }

    /// The diagnostic mode extends confirmation to the hot probes, so a stale
    /// index costs latency instead of correctness — and disarms on the spot.
    #[test]
    fn verify_negatives_confirms_metadata_probes_too() {
        let (dir, b) = fixture_sealing(BundleSemantics::Sealed, &["vendor"], true);
        assert!(b.verify_negatives());
        let rel = "vendor/acme/lib/LateFile.php";
        let key = key_under(&dir, rel);

        assert!(matches!(b.lookup(&key, Probe::Metadata), Lookup::Absent));
        std::fs::write(dir.path().join(rel), b"<?php class L {}").unwrap();
        assert!(
            matches!(b.lookup(&key, Probe::Metadata), Lookup::Unknown),
            "verify mode must fall through once the file really exists"
        );
        assert!(!b.sealed_roots[0].armed.load(Ordering::Relaxed));
    }

    /// The sealed scope predicate and the scan filter must agree on exactly
    /// which extensions the index enumerates exhaustively. If they diverge, the
    /// bundle would vouch for the absence of files it never looked for.
    #[test]
    fn sealed_scope_matches_scan_filter() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.php", "b.PHP", "c.phtml", "d.inc", "e.txt", "f"] {
            std::fs::write(dir.path().join(name), b"<?php ;").unwrap();
        }
        let spec = BundleSpec::new(
            dir.path().to_path_buf(),
            BundleCompression::None,
            usize::MAX,
            BundleSemantics::Overlay,
            &[],
            false,
        )
        .unwrap();
        let b = Bundle::from_scan(&spec).unwrap();
        assert_eq!(b.file_count(), 2, "only .php/.PHP indexed");

        // Every name the scan indexed must be claimed by the scope predicate,
        // and every name it skipped must not be.
        for name in ["a.php", "b.PHP", "zz.php", "zz.PhP"] {
            assert!(
                is_indexed_extension(&normalize_key(name)),
                "{name}: scan indexes this extension, scope must claim it"
            );
        }
        for name in ["c.phtml", "d.inc", "e.txt", "f", "g.php5", "h.php.bak"] {
            assert!(
                !is_indexed_extension(&normalize_key(name)),
                "{name}: scan skips this extension, scope must not claim it"
            );
        }
    }

    #[test]
    fn compression_roundtrips_every_algo() {
        let src =
            b"<?php\n// a representative source file\nfunction f(){ return 42; }\n".repeat(50);
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
        let spec = BundleSpec::new(
            dir.path().to_path_buf(),
            BundleCompression::Zstd,
            usize::MAX,
            BundleSemantics::Overlay,
            &[],
            false,
        )
        .unwrap();
        let b = Bundle::from_scan(&spec).unwrap();
        let key = normalize_key(&dir.path().join("cfg.php").to_string_lossy());
        let entry = b.files.get(&key).unwrap();
        let StoredData::Compressed(stored) = &entry.data else {
            panic!("expected compressed storage");
        };
        let plain = decompress(stored, BundleCompression::Zstd, entry.raw_len).unwrap();
        assert_eq!(plain, body);
    }
}
