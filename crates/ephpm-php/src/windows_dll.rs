//! Embed `php8embed.dll` inside the binary and extract it at startup.
//!
//! On Windows, PHP is distributed as `php8embed.dll` (a shared library).
//! Rather than requiring users to deploy the DLL alongside the binary, we
//! embed the DLL bytes with `include_bytes!` at compile time, extract them
//! to a temporary directory on first startup, and register that directory
//! with `SetDllDirectoryW` so Windows can find the DLL when it is
//! delay-loaded.
//!
//! The binary is linked with `/DELAYLOAD:php8embed.dll`, so Windows does
//! not try to resolve the DLL at process start — giving us a window to
//! extract and register it before any PHP function is called.
//!
//! Cleanup is automatic: [`PhpDllGuard`] resets the search path and deletes
//! the temporary directory when dropped. Keep the guard alive for the entire
//! process lifetime (i.e. until after [`crate::PhpRuntime::shutdown()`]).

use std::os::windows::ffi::OsStrExt as _;
use std::path::PathBuf;

/// `php8embed.dll` bytes embedded at compile time via `include_bytes!`.
/// `PHP_EMBED_DLL_PATH` is set by `ephpm-php/build.rs` to the DLL copied
/// into `OUT_DIR`.
static PHP_EMBED_DLL: &[u8] = include_bytes!(env!("PHP_EMBED_DLL_PATH"));

// Raw Win32 import — avoids pulling in `windows-sys` as a dependency.
unsafe extern "system" {
    fn SetDllDirectoryW(lp_path_name: *const u16) -> i32;
}

/// Guard that owns the temporary DLL directory.
///
/// On drop, resets the `SetDllDirectory` search path and removes the
/// temporary directory containing the extracted DLL.
pub struct PhpDllGuard {
    dir: PathBuf,
}

impl Drop for PhpDllGuard {
    fn drop(&mut self) {
        // Restore default DLL search path before deleting the directory.
        // Passing null restores the application-directory slot in the search
        // order (documented MSDN behaviour for SetDllDirectory).
        //
        // Safety: null is the documented sentinel value for this call.
        unsafe { SetDllDirectoryW(std::ptr::null()) };
        let _ = std::fs::remove_dir_all(&self.dir);
        tracing::debug!(dir = %self.dir.display(), "php8embed.dll temp directory removed");
    }
}

/// Extract `php8embed.dll` to a temporary directory and register it with
/// the Windows DLL loader via `SetDllDirectoryW`.
///
/// Must be called before the first PHP function is invoked (i.e. before
/// [`crate::PhpRuntime::init()`]). Since `php8embed.dll` is delay-loaded,
/// Windows will not try to find it until the first call into the DLL —
/// provided this function has been called by then, the loader will find it
/// in the registered temp directory.
///
/// # Errors
///
/// Returns an `std::io::Error` if the temp directory cannot be created,
/// the DLL bytes cannot be written, or `SetDllDirectoryW` fails.
pub fn extract_php_dll() -> std::io::Result<PhpDllGuard> {
    // Use process ID for uniqueness: %TEMP%\ephpm-<pid>
    let mut dir = std::env::temp_dir();
    dir.push(format!("ephpm-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    std::fs::write(dir.join("php8embed.dll"), PHP_EMBED_DLL)?;

    // Build a null-terminated UTF-16 path for the Win32 API.
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect();

    // Safety: `wide` is a valid null-terminated UTF-16 string pointing to
    // an existing directory. SetDllDirectoryW only reads the string; it does
    // not retain the pointer after returning.
    let ok = unsafe { SetDllDirectoryW(wide.as_ptr()) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }

    tracing::debug!(
        dir = %dir.display(),
        bytes = PHP_EMBED_DLL.len(),
        "extracted php8embed.dll to temp directory"
    );
    Ok(PhpDllGuard { dir })
}
