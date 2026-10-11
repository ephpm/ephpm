//! Shared-engine PHP: the runtime half of the Windows `--variant shared` build.
//!
//! The default Windows build links PHP statically (`php8embed.lib` from the
//! php-sdk static tarball). A static PHP cannot load extension DLLs: every
//! `php_*.dll`, Xdebug included, imports the engine from `php8ts.dll`, and a
//! static build has none. The shared variant links the official windows.php.net
//! ZTS engine instead, so `php8ts.dll` ships next to `ephpm.exe` and extension
//! DLLs load normally.
//!
//! The official build compiles fewer extensions into `php8ts.dll` than the
//! static SDK does: mbstring, openssl, curl, intl, `pdo_mysql`, … (and OPcache on
//! 8.3/8.4) are separate `ext\php_*.dll` files. The SDK's `ephpm-sdk.json`
//! lists them under `preload`, `crates/ephpm-php/build.rs` bakes that list into
//! the binary, and [`SharedRuntime::ini_lines`] turns it into php.ini lines that
//! load them — by absolute path, from the `ext` directory next to the
//! executable — so a shared build starts with the same extension set as a
//! static one. It also points `extension_dir` there, so `[php] extensions =
//! ["redis"]` finds `ext\php_redis.dll`.
//!
//! Where the lines go:
//!
//! - `ephpm serve` / `ephpm dev`: first in the generated php.ini, ahead of
//!   `[php] extensions`, `ini_file` and `ini_overrides`, so an operator's own
//!   extensions load after the ones they may depend on and any setting can be
//!   overridden (`extension_dir` included — the preload lines are absolute, so
//!   they do not depend on it).
//! - `ephpm php`: as startup `-d` directives queued before the user's own `-d`
//!   flags (`PhpRuntime::cli_main`), so they apply under `-n` as well — the
//!   same extension set a static build has with `-n`.
//!
//! Everything here is plain Rust and compiles in every build; only
//! [`SharedRuntime::detect`] depends on the `php_shared` cfg.

use std::path::{Path, PathBuf};

/// One extension the shared engine must load at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadEntry {
    /// File name inside the `ext` directory, e.g. `php_mbstring.dll`.
    pub file: String,
    /// Load with `zend_extension=` (OPcache on 8.3/8.4) instead of `extension=`.
    pub zend_extension: bool,
}

/// Why the shared-engine runtime files cannot be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SharedError {
    /// The build-time preload list is malformed (a build.rs bug).
    #[error("malformed shared-engine preload list entry {0:?}")]
    BadPreload(String),
    /// A preloaded extension DLL is not next to the executable.
    #[error(
        "{missing} is missing. This ephpm was built against the shared PHP engine \
         (php8ts.dll) and needs its runtime files beside the executable: php8ts.dll, the \
         dependency DLLs, and the ext\\ directory from the php-sdk \
         -windows-x86_64-shared tarball's bin\\ folder (`cargo xtask release --target windows \
         --variant shared` copies them)"
    )]
    MissingRuntimeFile {
        /// The absent file.
        missing: String,
    },
    /// The executable's own directory (where `ext\` must be) is unknown.
    #[error(
        "cannot locate the directory of the running executable, so the shared PHP engine's \
         ext\\ directory cannot be found; refusing to load extension DLLs from a relative path"
    )]
    NoExeDir,
    /// The path cannot be written into a php.ini value safely.
    #[error(
        "cannot reference {0} from php.ini: the path must not contain quotes, line breaks, or \
         \"${{\""
    )]
    UnsafePath(String),
}

/// Parse the `EPHPM_PHP_PRELOAD` encoding build.rs emits:
/// `e:<file>` / `z:<file>` entries joined by `;`.
///
/// # Errors
///
/// [`SharedError::BadPreload`] on an entry that is not `e:`/`z:` followed by a
/// plain file name.
pub fn parse_preload(encoded: &str) -> Result<Vec<PreloadEntry>, SharedError> {
    encoded
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|item| {
            let (kind, file) =
                item.split_once(':').ok_or_else(|| SharedError::BadPreload(item.into()))?;
            let zend_extension = match kind {
                "e" => false,
                "z" => true,
                _ => return Err(SharedError::BadPreload(item.into())),
            };
            if file.is_empty()
                || file.starts_with('.')
                || !file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            {
                return Err(SharedError::BadPreload(item.into()));
            }
            Ok(PreloadEntry { file: file.to_string(), zend_extension })
        })
        .collect()
}

/// The shared engine's extension directory and startup preload list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRuntime {
    /// `<directory of the executable>\ext`.
    pub ext_dir: PathBuf,
    /// Extensions to load at startup, in order.
    pub preload: Vec<PreloadEntry>,
}

impl SharedRuntime {
    /// The runtime for this binary: `Some` only in a build linked against the
    /// Windows shared-engine SDK (`php_shared`), `None` everywhere else (every
    /// static build, and stub mode).
    #[must_use]
    pub fn detect() -> Option<Self> {
        #[cfg(php_shared)]
        {
            // The preload list is a compile-time constant produced by
            // build.rs from a validated manifest; failing to parse it is a
            // build bug, so fall back to no preload rather than refusing to
            // start, and let ini_lines() report the directory problem.
            let preload = parse_preload(env!("EPHPM_PHP_PRELOAD")).unwrap_or_default();
            // When the executable's directory cannot be determined, leave
            // ext_dir empty: ini_lines() refuses a non-absolute directory
            // rather than loading DLLs from a cwd-relative `ext`.
            let ext_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("ext")))
                .unwrap_or_default();
            Some(Self { ext_dir, preload })
        }
        #[cfg(not(php_shared))]
        {
            None
        }
    }

    /// Whether this binary links the shared engine (compile-time).
    #[must_use]
    pub const fn is_shared_build() -> bool {
        cfg!(php_shared)
    }

    /// The php.ini lines that make a shared engine look like a static one:
    /// `extension_dir` pointing at [`Self::ext_dir`], then one absolute
    /// `extension=` / `zend_extension=` line per preload entry. Values are
    /// double-quoted with forward slashes (php.ini treats `\\` inside quotes
    /// as an escape; Windows accepts `/`).
    ///
    /// # Errors
    ///
    /// [`SharedError::MissingRuntimeFile`] when the `ext` directory or a
    /// preloaded DLL is absent — failing here names the real problem, where
    /// PHP would only print one "Unable to load dynamic library" warning per
    /// DLL and start without them. [`SharedError::UnsafePath`] when the path
    /// cannot be quoted into php.ini.
    pub fn ini_lines(&self) -> Result<Vec<String>, SharedError> {
        if !self.ext_dir.is_absolute() {
            return Err(SharedError::NoExeDir);
        }
        if !self.ext_dir.is_dir() {
            return Err(SharedError::MissingRuntimeFile {
                missing: self.ext_dir.display().to_string(),
            });
        }
        let mut lines = vec![format!("extension_dir={}", ini_quote(&self.ext_dir)?)];
        for entry in &self.preload {
            let path = self.ext_dir.join(&entry.file);
            if !path.is_file() {
                return Err(SharedError::MissingRuntimeFile {
                    missing: path.display().to_string(),
                });
            }
            let directive = if entry.zend_extension { "zend_extension" } else { "extension" };
            lines.push(format!("{directive}={}", ini_quote(&path)?));
        }
        Ok(lines)
    }
}

/// `"C:/dir/file"` — double-quoted, forward slashes.
fn ini_quote(path: &Path) -> Result<String, SharedError> {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.contains(['"', '\n', '\r', '\0']) || s.contains("${") {
        return Err(SharedError::UnsafePath(path.display().to_string()));
    }
    Ok(format!("\"{s}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_build_encoding() {
        let parsed = parse_preload("e:php_mbstring.dll;z:php_opcache.dll;e:php_exif.dll").unwrap();
        assert_eq!(
            parsed,
            vec![
                PreloadEntry { file: "php_mbstring.dll".into(), zend_extension: false },
                PreloadEntry { file: "php_opcache.dll".into(), zend_extension: true },
                PreloadEntry { file: "php_exif.dll".into(), zend_extension: false },
            ]
        );
        assert_eq!(parse_preload("").unwrap(), Vec::new());
    }

    #[test]
    fn rejects_malformed_entries() {
        for bad in [
            "php_mbstring.dll",
            "x:php_a.dll",
            "e:",
            "e:../php_a.dll",
            "e:a\\b.dll",
            "e:a\nb.dll",
            "e:.hidden.dll",
            "e:a b.dll",
        ] {
            assert!(parse_preload(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn ini_lines_are_absolute_quoted_and_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join("ext");
        std::fs::create_dir(&ext).unwrap();
        std::fs::write(ext.join("php_mbstring.dll"), b"").unwrap();
        std::fs::write(ext.join("php_opcache.dll"), b"").unwrap();
        let rt = SharedRuntime {
            ext_dir: ext.clone(),
            preload: parse_preload("e:php_mbstring.dll;z:php_opcache.dll").unwrap(),
        };
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        assert_eq!(
            rt.ini_lines().unwrap(),
            vec![
                format!("extension_dir=\"{}\"", fwd(&ext)),
                format!("extension=\"{}\"", fwd(&ext.join("php_mbstring.dll"))),
                format!("zend_extension=\"{}\"", fwd(&ext.join("php_opcache.dll"))),
            ]
        );
    }

    #[test]
    fn missing_runtime_files_fail_closed() {
        // Unknown executable directory: never fall back to a relative `ext`.
        let rt = SharedRuntime {
            ext_dir: PathBuf::new(),
            preload: parse_preload("e:php_mbstring.dll").unwrap(),
        };
        assert_eq!(rt.ini_lines(), Err(SharedError::NoExeDir));
        let rt = SharedRuntime { ext_dir: PathBuf::from("ext"), preload: Vec::new() };
        assert_eq!(rt.ini_lines(), Err(SharedError::NoExeDir));

        let dir = tempfile::tempdir().unwrap();
        let rt = SharedRuntime {
            ext_dir: dir.path().join("ext"),
            preload: parse_preload("e:php_mbstring.dll").unwrap(),
        };
        // No ext directory at all.
        assert!(matches!(rt.ini_lines(), Err(SharedError::MissingRuntimeFile { .. })));
        // Directory present, DLL absent: names the DLL.
        std::fs::create_dir(dir.path().join("ext")).unwrap();
        match rt.ini_lines() {
            Err(SharedError::MissingRuntimeFile { missing }) => {
                assert!(missing.ends_with("php_mbstring.dll"), "{missing}");
            }
            other => panic!("expected MissingRuntimeFile, got {other:?}"),
        }
    }

    #[test]
    fn unsafe_paths_are_refused() {
        assert!(ini_quote(Path::new("C:/a\"b/ext")).is_err());
        assert!(ini_quote(Path::new("C:/${HOME}/ext")).is_err());
        assert_eq!(ini_quote(Path::new(r"C:\Program Files\ephpm\ext")).unwrap(), {
            "\"C:/Program Files/ephpm/ext\"".to_string()
        });
    }

    #[test]
    fn detect_matches_the_build() {
        assert_eq!(SharedRuntime::detect().is_some(), SharedRuntime::is_shared_build());
    }
}
