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
// process environment races concurrent readers on other threads. The one call
// site runs on the single-threaded startup path, before `php_embed_init()` and
// before any tokio/PHP worker thread exists, so that hazard cannot occur here;
// its SAFETY comment states the invariant.
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

/// Scrub the live process environment in place (finding F1).
///
/// Removes every variable not kept by [`scrub_plan`]. Call on the
/// single-threaded startup path, before `php_embed_init()` and before any
/// worker thread is spawned.
pub(crate) fn scrub_process_environment(allowlist: &[String], env_refs: &[String]) -> ScrubReport {
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
    let removed = current
        .iter()
        .filter(|(_, name)| remove_set.contains(name.as_str()))
        .map(|(key, _)| {
            // SAFETY: this runs on the single-threaded startup path — before
            // `php_embed_init()` and before any tokio/PHP worker thread is
            // spawned — so no other thread can be reading `environ` concurrently.
            // `std::env::remove_var` is `unsafe` in the 2024 edition purely
            // because of that cross-thread hazard, which cannot occur here.
            unsafe { std::env::remove_var(key) };
        })
        .count();

    ScrubReport { removed, kept }
}

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
