//! Process-environment scrub (multi-tenant breakout audit finding **F1**).
//!
//! A PHP request reads the *process* environment through `getenv()` (including
//! the no-argument form), the `$_ENV` superglobal and `phpinfo()` — all three
//! read libc's `environ` directly. In a multi-tenant deployment (`[server]
//! sites_dir`) one process serves every tenant, so that environment is a shared
//! surface: it holds the operator's `[kv] secret` / `[cluster] secret` (passed
//! as `EPHPM_KV__SECRET` / `EPHPM_CLUSTER__SECRET`), `AWS_*` credentials and any
//! other secret the operator exported. Any tenant's PHP could read them and,
//! with `[kv] secret`, derive every other tenant's database and KV password —
//! a full cross-tenant breakout.
//!
//! The fix clears every variable not on the operator's `[php] env_allowlist`
//! (plus the small internal keep-set ePHPm itself still needs,
//! [`ephpm_config::env_scrub_reserved`], and the middleware `env:NAME`
//! references the config consumes) from the live environment **once**, on the
//! single-threaded startup path, *before* `php_embed_init()` and before any PHP
//! pool thread exists. After it runs, `getenv()`, `$_ENV` and `phpinfo()` are
//! clean at the source.
//!
//! `remove_var` only drops the pointer out of libc's `environ` array; the
//! exec-time `NAME=VALUE` string it pointed at stays in the kernel's env region
//! and leaks through `/proc/self/environ` (readable by a tenant's PHP in
//! single-site mode, where there is no `open_basedir` confining `/proc`). So in
//! `serve` mode the scrub also zeroes each removed variable's **value** bytes in
//! place before dropping the pointer (overwriting them through libc's `environ`),
//! leaving `NAME=` intact so `remove_var` still unsets the entry. After the
//! scrub `/proc/self/environ` shows `NAME=` with an empty value for every
//! scrubbed variable — the secret value is gone from the exec block, not just
//! from the live array.
//!
//! This mirrors what [`crate::exec_sandbox`] already does for the children
//! `ephpm exec` spawns (`env_clear` + a fixed allowlist), applied here to the
//! server's own process instead.
//!
//! # Secrets ePHPm consumes are cached first
//!
//! Everything ePHPm reads from the environment is captured before the scrub, so
//! the scrub never breaks the server: figment has already read the `EPHPM_*`
//! config into [`ephpm_config::Config`] at load, and the caller caches `AWS_*`
//! into a process-global (`ephpm_server::cache_aws_credentials`) immediately
//! before calling [`scrub_process_environment`]. The middleware `env:NAME`
//! references are kept in place (they are resolved lazily when middleware is
//! built, after the scrub) — a documented residual: a secret an operator
//! *chose* to pass via a `[[middleware]]` `env:` reference stays readable by
//! that node's PHP unless a `file:` indirection is used instead.

// `std::env::remove_var` is `unsafe` in the 2024 edition because mutating the
// process environment races any concurrent *reader* of it. The call sites run
// on the single-threaded startup path, before `php_embed_init()` and before any
// PHP pool thread exists; the threads that may exist by then (the tracing-
// appender writer, an OTLP exporter) never read the environment, so that hazard
// cannot occur here. The `/proc`-zeroing pass additionally reads libc's raw
// `environ` directly, which is sound under the same single-reader invariant.
#![allow(unsafe_code)]

/// What [`scrub_process_environment`] did, for a single startup log line.
pub(crate) struct ScrubReport {
    /// How many environment variables were removed.
    removed: usize,
    /// The names that were kept, sorted — logged at debug so an operator can
    /// confirm exactly what a tenant's PHP can still see. Names, never values.
    kept: Vec<String>,
}

impl ScrubReport {
    /// Emit the startup summary: one INFO line with the counts, and the kept
    /// names at DEBUG (names only — a value could be a secret, a name is not).
    pub(crate) fn log(&self) {
        tracing::info!(
            removed = self.removed,
            kept = self.kept.len(),
            "scrubbed the process environment before PHP started (finding F1): a tenant's PHP \
             sees only the allowlisted variables via getenv()/$_ENV/phpinfo()"
        );
        tracing::debug!(kept = ?self.kept, "environment variables kept visible to PHP after the scrub");
    }
}

/// Decide, for a set of environment variable `names`, which to remove and
/// which to keep — the pure core of [`scrub_process_environment`], split out so
/// it is testable without mutating the real process environment.
///
/// A name is **kept** when it matches the operator's `allowlist`
/// ([`ephpm_config::env_allowlist_matches`]), is on ePHPm's internal keep-set
/// ([`ephpm_config::env_scrub_reserved`]), or is one of the middleware
/// `env:NAME` references in `env_refs`. Everything else is removed. Returns
/// `(to_remove, kept)` with `kept` sorted.
fn scrub_plan(
    names: impl IntoIterator<Item = String>,
    allowlist: &[String],
    env_refs: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut to_remove = Vec::new();
    let mut kept = Vec::new();
    for name in names {
        let keep = ephpm_config::env_allowlist_matches(allowlist, &name)
            || ephpm_config::env_scrub_reserved(&name)
            || env_refs.iter().any(|r| r == &name);
        if keep {
            kept.push(name);
        } else {
            to_remove.push(name);
        }
    }
    kept.sort_unstable();
    (to_remove, kept)
}

/// Scrub the live process environment in place for `ephpm serve` (finding F1).
///
/// Removes every variable not kept by [`scrub_plan`], and — on glibc Linux —
/// zeroes each removed variable's value bytes in the exec block first so
/// `/proc/self/environ` cannot leak it (see the module docs). Call on the
/// single-threaded startup path, before `php_embed_init()` and before any
/// worker thread is spawned.
pub(crate) fn scrub_process_environment(allowlist: &[String], env_refs: &[String]) -> ScrubReport {
    scrub_environment(allowlist, env_refs, ZeroBacking::Yes)
}

/// Scrub the environment for the `ephpm php --site` tenant CLI lane (finding
/// F1, half b): a tenant cron/`wp`/`artisan` script must not be able to
/// `getenv()` the operator's `[kv] secret` / `[cluster] secret` / `AWS_*`.
///
/// Pointer-only removal, no `/proc` value zeroing: by the time this runs the
/// `ephpm php` process-title constructor (`ephpm_wrapper.c`,
/// `ephpm_ps_capture_argv`, gated to `argv[1] == "php"`) has already relocated
/// `environ` to heap copies, so the exec block is no longer what `environ`
/// points at and zeroing those copies would clean nothing. (The `/proc` leak
/// also matters far less here: this process serves exactly one tenant.) Caller
/// keeps this single-site process's needed names — the per-site `DB_*` it just
/// exported — via `env_refs`.
pub(crate) fn scrub_cli_environment(allowlist: &[String], env_refs: &[String]) -> ScrubReport {
    scrub_environment(allowlist, env_refs, ZeroBacking::No)
}

/// Whether [`scrub_environment`] zeroes a removed variable's value bytes in the
/// exec block before dropping its `environ` pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ZeroBacking {
    /// `serve` mode: `environ` still points at the original exec block, so zero
    /// the value bytes in place to close the `/proc/self/environ` residual.
    Yes,
    /// `ephpm php` CLI: `environ` was relocated to heap copies, so skip it.
    No,
}

/// The shared scrub core for both lanes.
fn scrub_environment(
    allowlist: &[String],
    env_refs: &[String],
    zero_backing: ZeroBacking,
) -> ScrubReport {
    // Collect the current names first (as lossy UTF-8 for matching, keeping the
    // original `OsString` for removal), so we never mutate `environ` while
    // iterating it. A name that is not valid UTF-8 cannot match a `String`
    // allowlist entry, so it is removed — the safe direction.
    let current: Vec<(std::ffi::OsString, String)> =
        std::env::vars_os().map(|(k, _)| (k.clone(), k.to_string_lossy().into_owned())).collect();

    let (to_remove, kept) =
        scrub_plan(current.iter().map(|(_, name)| name.clone()), allowlist, env_refs);

    // Map the UTF-8 names chosen for removal back to their original `OsString`
    // keys. Removing by the lossy string could miss a non-UTF-8 name, so remove
    // by the exact key instead.
    let remove_set: std::collections::HashSet<&str> =
        to_remove.iter().map(String::as_str).collect();

    // Zero each removed variable's value bytes in the exec block *before* the
    // removal drops its `environ` pointer (serve mode only). Keeps `NAME=`
    // intact so the `remove_var` below still unsets the entry.
    if zero_backing == ZeroBacking::Yes {
        zero_removed_value_bytes(&remove_set);
    }

    let removed = current
        .iter()
        .filter(|(_, name)| remove_set.contains(name.as_str()))
        .map(|(key, _)| {
            // SAFETY: this runs on the single-threaded startup path — before
            // `php_embed_init()` and before any PHP pool thread is spawned — so
            // no thread that reads the environment can be reading `environ`
            // concurrently. `std::env::remove_var` is `unsafe` in the 2024
            // edition purely because of that cross-thread hazard, which cannot
            // occur here.
            unsafe { std::env::remove_var(key) };
        })
        .count();

    ScrubReport { removed, kept }
}

/// Overwrite, in place, the value bytes of every `environ` entry whose name is
/// in `remove_set` (glibc Linux only; a no-op elsewhere).
///
/// `remove_var` unlinks the pointer but leaves the exec-time `NAME=VALUE` string
/// in the kernel env region, where `/proc/self/environ` still exposes it. This
/// zeroes the bytes after the first `=`, leaving `NAME=` so the subsequent
/// `remove_var` still matches. Only the value — never the name — is touched, and
/// kept variables are not visited.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn zero_removed_value_bytes(remove_set: &std::collections::HashSet<&str>) {
    // glibc exposes the process environment as the `environ` global. The `libc`
    // crate only declares it under feature flags this crate does not enable, so
    // declare it directly here; it resolves against glibc at link time.
    unsafe extern "C" {
        #[link_name = "environ"]
        static mut ENVIRON: *mut *mut core::ffi::c_char;
    }
    // SAFETY: single-threaded startup path (see the module `#![allow]` note), so
    // no concurrent reader of `environ`. In `serve` mode the process-title
    // constructor has not run (it is gated to `ephpm php`), so each `environ[i]`
    // still points at the original, writable exec-block string. We read the
    // array pointers and overwrite only the value bytes after `=` with volatile
    // writes (so the zeroing is not elided); we never free or move the array.
    unsafe {
        // Read the `environ` pointer value through a raw pointer to the static,
        // never a reference to it (edition-2024 `static_mut_refs`).
        let mut entry = (&raw const ENVIRON).read();
        if entry.is_null() {
            return;
        }
        while !(*entry).is_null() {
            let s = *entry;
            let cstr = std::ffi::CStr::from_ptr(s);
            let bytes = cstr.to_bytes();
            if let Some(eq) = bytes.iter().position(|&b| b == b'=')
                && let Ok(name) = std::str::from_utf8(&bytes[..eq])
                && remove_set.contains(name)
            {
                let value = s.add(eq + 1);
                for i in 0..(bytes.len() - (eq + 1)) {
                    core::ptr::write_volatile(value.add(i), 0);
                }
            }
            entry = entry.add(1);
        }
    }
}

/// Non-glibc targets (musl, macOS, Windows): `remove_var` is the whole scrub —
/// libc either does not export `environ`/`explicit_bzero` the same way, or the
/// `/proc/self/environ` surface does not exist.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn zero_removed_value_bytes(_remove_set: &std::collections::HashSet<&str>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| (*x).to_string()).collect()
    }

    /// The default allowlist keeps exactly the four safe families and removes
    /// the F1-named secrets.
    #[test]
    fn default_allowlist_drops_secrets_keeps_locale_and_path() {
        let allowlist = s(ephpm_config::DEFAULT_ENV_ALLOWLIST);
        let names = s(&[
            "PATH",
            "TZ",
            "LANG",
            "LC_ALL",
            "LC_TIME",
            // The F1 secrets — must all be removed.
            "EPHPM_KV__SECRET",
            "EPHPM_CLUSTER__SECRET",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "SOME_APP_SECRET",
        ]);
        let (removed, kept) = scrub_plan(names, &allowlist, &[]);
        assert_eq!(kept, s(&["LANG", "LC_ALL", "LC_TIME", "PATH", "TZ"]));
        for secret in
            ["EPHPM_KV__SECRET", "EPHPM_CLUSTER__SECRET", "AWS_ACCESS_KEY_ID", "GITHUB_TOKEN"]
        {
            assert!(removed.iter().any(|r| r == secret), "{secret} must be scrubbed");
        }
    }

    /// ePHPm's own operational variables survive the scrub regardless of the
    /// allowlist, or the server would break after the scrub.
    #[test]
    fn reserved_operational_vars_survive() {
        let allowlist = s(ephpm_config::DEFAULT_ENV_ALLOWLIST);
        let names = s(&[
            "EPHPM_MIDDLEWARE_DIR",
            "EPHPM_DISPATCH_SPIN_US",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "RUST_LOG",
        ]);
        let (removed, _kept) = scrub_plan(names, &allowlist, &[]);
        assert!(
            removed.is_empty(),
            "reserved operational vars must be kept, got removed: {removed:?}"
        );
    }

    /// A middleware `env:NAME` reference is kept so middleware can resolve it
    /// after the scrub.
    #[test]
    fn middleware_env_references_are_kept() {
        let allowlist = s(ephpm_config::DEFAULT_ENV_ALLOWLIST);
        let names = s(&["GH_CLIENT_SECRET", "UNRELATED"]);
        let (removed, kept) = scrub_plan(names, &allowlist, &s(&["GH_CLIENT_SECRET"]));
        assert!(kept.contains(&"GH_CLIENT_SECRET".to_string()));
        assert_eq!(removed, s(&["UNRELATED"]));
    }

    /// A widened allowlist (exact name + prefix glob) keeps extra app vars.
    #[test]
    fn widened_allowlist_keeps_named_and_globbed_vars() {
        let mut allowlist = s(ephpm_config::DEFAULT_ENV_ALLOWLIST);
        allowlist.push("APP_BASE".to_string());
        allowlist.push("MYAPP_*".to_string());
        let names = s(&["APP_BASE", "MYAPP_DEBUG", "MYAPP_REGION", "OTHER"]);
        let (removed, kept) = scrub_plan(names, &allowlist, &[]);
        assert!(kept.contains(&"APP_BASE".to_string()));
        assert!(kept.contains(&"MYAPP_DEBUG".to_string()));
        assert!(kept.contains(&"MYAPP_REGION".to_string()));
        assert_eq!(removed, s(&["OTHER"]));
    }
}
