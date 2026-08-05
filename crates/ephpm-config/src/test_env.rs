//! Test-only serialisation of process-environment access.
//!
//! # Why this exists
//!
//! [`Config::load`](crate::Config::load) and
//! [`Config::default_config`](crate::Config::default_config) read the *whole*
//! `EPHPM_*` process environment through figment's `Env` provider. There is no
//! per-call environment to inject, so a test that installs one override
//! perturbs every test running concurrently with it — figment re-reads the
//! global environment on every load.
//!
//! The previous approach (the `temp-env` crate) took a lock that excluded only
//! *other `temp-env` callers*. The ~80 tests in this crate that merely call
//! `Config::load` held no such lock, so they could — and did — observe a
//! neighbour's temporary override. Under the default parallel test harness that
//! produced a ~20 % failure rate for `cargo test -p ephpm-config`, landing on a
//! different assertion each run, which is why it read as "flaky test X" rather
//! than "unsound test suite". Running with `--test-threads=1` hid it, and
//! `cargo nextest` (one process per test) hides it too.
//!
//! # How it works
//!
//! Readers and writers share one [`RwLock`]:
//!
//! * `Config::load` / `Config::default_config` take the **shared** side for the
//!   duration of the environment read (via [`read_guard`], compiled in only
//!   under `cfg(test)`).
//! * [`EnvVars`] takes the **exclusive** side for as long as an override is
//!   installed, and restores the previous values when dropped.
//!
//! Read-only tests therefore still run concurrently with each other; a test
//! that mutates the environment excludes everyone. Crucially, nothing has to be
//! remembered at the call site: a test added tomorrow that calls `Config::load`
//! is protected without its author knowing this module exists.
//!
//! # Caveat
//!
//! A thread holding an [`EnvVars`] guard must not hand off a `Config::load`
//! call to *another* thread and wait for it — that thread would block on the
//! shared side until the guard drops. No test does this, and there is no reason
//! for one to.

// `std::env::set_var` and `remove_var` are `unsafe` in edition 2024. This
// module exists precisely to establish the invariant that makes them sound;
// see the SAFETY note on `apply`.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::env;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Guards every read of, and write to, the `EPHPM_*` process environment
/// performed by this crate's test binary.
static ENV_LOCK: RwLock<()> = RwLock::new(());

thread_local! {
    /// How many [`EnvVars`] guards this thread currently holds.
    ///
    /// Non-zero means the thread already owns the exclusive side of
    /// [`ENV_LOCK`]. Both a nested `EnvVars` and any `Config::load` made
    /// *inside* a guarded scope must then skip locking: `RwLock` is not
    /// reentrant, so either would deadlock the thread against itself.
    static EXCLUSIVE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Acquire the shared (reader) side of the environment lock.
///
/// Returns `None` — meaning "no lock needed" — when the calling thread already
/// holds the exclusive side through an [`EnvVars`] guard.
///
/// Lock poisoning is deliberately ignored. A failing assertion inside a guarded
/// scope is an ordinary event in a test suite; propagating poison would turn
/// one genuine failure into ~90 spurious ones and bury the real signal.
pub(crate) fn read_guard() -> Option<RwLockReadGuard<'static, ()>> {
    if EXCLUSIVE_DEPTH.with(Cell::get) > 0 {
        return None;
    }
    Some(ENV_LOCK.read().unwrap_or_else(PoisonError::into_inner))
}

/// An RAII scope in which the given `EPHPM_*` variables hold overridden values.
///
/// Holding one excludes all concurrent environment readers in this test binary.
/// Dropping it restores each variable to whatever it was before — including
/// removing it again if it was previously unset — and does so on the unwinding
/// path too, so a failing assertion inside the scope cannot leak state into the
/// rest of the suite.
///
/// Guards nest on a single thread; the outermost one owns the lock.
#[must_use = "the overrides are reverted the moment the guard is dropped"]
pub(crate) struct EnvVars {
    /// `None` when nested inside another `EnvVars` on the same thread.
    _lock: Option<RwLockWriteGuard<'static, ()>>,
    /// Previous value of each variable, in the order the overrides were applied.
    saved: Vec<(String, Option<String>)>,
}

impl EnvVars {
    /// Override several variables at once. A `None` value removes the variable
    /// for the lifetime of the guard.
    pub(crate) fn many<'a, I>(vars: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, Option<&'a str>)>,
    {
        let lock = if EXCLUSIVE_DEPTH.with(Cell::get) > 0 {
            None
        } else {
            Some(ENV_LOCK.write().unwrap_or_else(PoisonError::into_inner))
        };
        // Raise the depth *before* touching the environment so that a
        // `Config::load` performed inside the scope takes the reentrant path.
        EXCLUSIVE_DEPTH.with(|depth| depth.set(depth.get() + 1));

        let mut saved = Vec::new();
        for (key, value) in vars {
            saved.push((key.to_owned(), env::var(key).ok()));
            apply(key, value);
        }
        Self { _lock: lock, saved }
    }

    /// Override a single variable — the common case.
    pub(crate) fn set(key: &str, value: &str) -> Self {
        Self::many([(key, Some(value))])
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        // Restore in reverse application order, so a variable overridden twice
        // in one call still ends up at the value it had before the guard.
        for (key, previous) in self.saved.iter().rev() {
            apply(key, previous.as_deref());
        }
        EXCLUSIVE_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Set `key` to `value`, or remove it when `value` is `None`.
fn apply(key: &str, value: Option<&str>) {
    // SAFETY: `set_var`/`remove_var` are unsound only if another thread may be
    // reading the environment concurrently. This function is reachable only
    // from `EnvVars`, which holds the exclusive side of `ENV_LOCK`, and every
    // environment read in this crate goes through `Config::load` /
    // `Config::default_config`, which take the shared side. No reader can be
    // in flight here, and the test binary starts no threads of its own that
    // touch the environment.
    unsafe {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names are unique per test: these run concurrently with each other and
    /// with the rest of the suite, and `EnvVars` serialises them but does not
    /// isolate them.
    const A: &str = "EPHPM_TEST_ENV_GUARD_A";
    const B: &str = "EPHPM_TEST_ENV_GUARD_B";
    const C: &str = "EPHPM_TEST_ENV_GUARD_C";
    const D: &str = "EPHPM_TEST_ENV_GUARD_D";
    const E: &str = "EPHPM_TEST_ENV_GUARD_E";

    #[test]
    fn set_applies_then_removes_a_previously_unset_var() {
        {
            let _env = EnvVars::set(A, "value");
            assert_eq!(env::var(A).as_deref(), Ok("value"));
        }
        assert!(env::var(A).is_err(), "guard must remove a var it introduced");
    }

    #[test]
    fn nested_guard_restores_the_outer_value() {
        let outer = EnvVars::set(B, "outer");
        assert_eq!(env::var(B).as_deref(), Ok("outer"));
        {
            let _inner = EnvVars::set(B, "inner");
            assert_eq!(env::var(B).as_deref(), Ok("inner"));
        }
        assert_eq!(env::var(B).as_deref(), Ok("outer"), "inner drop must not clobber outer");
        drop(outer);
        assert!(env::var(B).is_err());
    }

    #[test]
    fn many_sets_and_unsets_together() {
        let _seed = EnvVars::set(C, "seeded");
        {
            let _env = EnvVars::many([(C, None), (D, Some("d"))]);
            assert!(env::var(C).is_err(), "None must remove the variable");
            assert_eq!(env::var(D).as_deref(), Ok("d"));
        }
        assert_eq!(env::var(C).as_deref(), Ok("seeded"));
        assert!(env::var(D).is_err());
    }

    /// Note: this test deliberately panics, so a `panicked at ... boom` line in
    /// the harness output is expected and does not indicate a failure. The
    /// default panic hook is left in place on purpose — replacing it is
    /// process-global and would swallow the location of a *genuine* failure in
    /// a test running concurrently.
    #[test]
    fn restores_on_unwind() {
        let _seed = EnvVars::set(E, "seeded");
        let panicked = std::panic::catch_unwind(|| {
            let _env = EnvVars::set(E, "temporary");
            panic!("boom");
        });
        assert!(panicked.is_err());
        assert_eq!(
            env::var(E).as_deref(),
            Ok("seeded"),
            "a panic inside the scope must not leak the override"
        );
    }

    #[test]
    fn config_load_inside_a_guard_does_not_deadlock() {
        // The reentrancy path: `default_config` calls `read_guard`, which must
        // decline to lock because this thread already holds the write side.
        let _env = EnvVars::set("EPHPM_PHP__MEMORY_LIMIT", "321M");
        let config = crate::Config::default_config().unwrap();
        assert_eq!(config.php.memory_limit, "321M");
    }

    #[test]
    fn read_guard_is_reentrant_only_while_a_write_guard_is_held() {
        assert!(read_guard().is_some(), "unguarded reads must take the shared lock");
        let _env = EnvVars::set("EPHPM_TEST_ENV_GUARD_REENTRANCY", "1");
        assert!(read_guard().is_none(), "reads inside a write scope must not re-lock");
    }
}
