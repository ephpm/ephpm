//! Upstream health state for the SQL proxies.
//!
//! A proxy is two independent things: a listener PHP connects to, and a
//! pool of connections to some upstream database. Those two can be in
//! different states — the listener can be up and serving while the
//! upstream is unreachable — and the server needs to say so out loud
//! rather than reporting a uniform "healthy".
//!
//! [`ProxyHealth`] is the shared handle that carries that distinction.
//! One is created per configured proxy at startup and cloned into (a) the
//! background upstream-connect loop, (b) the pool's connect closure, and
//! (c) the HTTP readiness check. All state transitions also move
//! Prometheus metrics, so a proxy that never reaches its upstream is
//! visible without scraping logs:
//!
//! | Metric | Type | Meaning |
//! |---|---|---|
//! | `ephpm_db_proxy_upstream_ever_connected` | gauge | 1 once the proxy has completed one upstream handshake since boot. Stays 1. |
//! | `ephpm_db_proxy_upstream_up` | gauge | 1 if the last upstream connect attempt succeeded, 0 if it failed. Flaps with the upstream. |
//! | `ephpm_db_proxy_connect_failures_total` | counter | Upstream connect/handshake failures. |
//!
//! Labels: `db` (`mysql` / `postgres`) and `upstream` (the resolved
//! `host:port` — never the URL, which carries credentials).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use metrics::{counter, gauge};
use tracing::{error, warn};

/// How often a still-failing upstream may log after the initial burst.
const LOG_INTERVAL_SECS: u64 = 60;

/// How many consecutive failures log unconditionally before rate limiting
/// kicks in. The first few carry the diagnosis (refused vs. auth vs. TLS);
/// the thousandth carries nothing new.
const UNTHROTTLED_FAILURES: u64 = 3;

/// How long a proxy keeps trying to reach its upstream before giving up.
///
/// The two variants exist because the two entry points have different
/// callers. [`RetryBudget::Bounded`] backs the eager constructors
/// (`MySqlProxy::new` / `PgProxy::new`), which return the error to a caller
/// that can report it — an unbounded loop there would be an unkillable
/// hang. [`RetryBudget::Unbounded`] backs the deferred startup path, where
/// the listener is already bound and serving and there is nobody left to
/// report to: giving up permanently would leave a live listener wired to a
/// dead upstream for the lifetime of the process, recoverable only by a
/// restart. Upstreams restart (RDS failover, a sidecar rollout, a local
/// listener that binds a moment later) — the proxy should survive that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryBudget {
    /// Give up after this many attempts and return the last error.
    Bounded(u32),
    /// Retry forever, at [`RetryBudget::max_backoff_ms`] steady state.
    Unbounded,
}

impl RetryBudget {
    /// Is `attempt` (1-based) the last one this budget allows?
    #[must_use]
    pub fn is_final_attempt(self, attempt: u32) -> bool {
        match self {
            Self::Bounded(max) => attempt >= max,
            Self::Unbounded => false,
        }
    }

    /// Backoff ceiling in milliseconds.
    ///
    /// The bounded budget keeps the historical 8 s cap so its total stays
    /// ~40 s across 10 attempts. The unbounded budget settles at 30 s: one
    /// TCP connect every 30 s costs nothing, and it keeps a long outage
    /// from turning into a reconnect storm the moment the upstream returns.
    #[must_use]
    pub fn max_backoff_ms(self) -> u64 {
        match self {
            Self::Bounded(_) => 8_000,
            Self::Unbounded => 30_000,
        }
    }
}

/// Shared upstream-health state for one configured proxy.
///
/// Cheap to read (two relaxed atomic loads) — the readiness probe touches
/// it on every scrape, and nothing else is on a hot path.
#[derive(Debug)]
pub struct ProxyHealth {
    /// Short proxy kind, used as the `db` metric label: `mysql` or `postgres`.
    kind: &'static str,
    /// The address PHP connects to.
    listen: String,
    /// The upstream `host:port`. Never the full URL — that carries the password.
    upstream: String,
    /// Set once the proxy completes its first upstream handshake, and never
    /// cleared. This is the readiness gate (see [`ProxyHealth::ever_connected`]).
    ever_connected: AtomicBool,
    /// Outcome of the most recent upstream connect attempt.
    upstream_up: AtomicBool,
    /// Consecutive failures since the last success — drives log throttling.
    consecutive_failures: AtomicU64,
    /// Unix seconds of the last emitted failure log.
    last_log_unix: AtomicU64,
}

impl ProxyHealth {
    /// Create health state for a proxy, in the "never connected" state.
    ///
    /// `upstream` must already be redacted to `host:port`.
    #[must_use]
    pub fn new(
        kind: &'static str,
        listen: impl Into<String>,
        upstream: impl Into<String>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            kind,
            listen: listen.into(),
            upstream: upstream.into(),
            ever_connected: AtomicBool::new(false),
            upstream_up: AtomicBool::new(false),
            consecutive_failures: AtomicU64::new(0),
            last_log_unix: AtomicU64::new(0),
        });
        // Publish the initial (down) state so a proxy that never connects
        // shows up as an explicit 0 rather than an absent series.
        this.set_gauge("ephpm_db_proxy_upstream_ever_connected", 0.0);
        this.set_gauge("ephpm_db_proxy_upstream_up", 0.0);
        this
    }

    /// The `db` label / proxy kind (`mysql`, `postgres`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The address PHP connects to.
    #[must_use]
    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// The upstream `host:port`.
    #[must_use]
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// Has this proxy ever completed an upstream handshake since boot?
    ///
    /// This — not [`ProxyHealth::is_up`] — is what readiness gates on. A
    /// proxy that has never reached its upstream cannot serve a single
    /// query and the process should stay out of load-balancer rotation; a
    /// proxy that connected once and later lost its upstream is a database
    /// outage, and dropping every replica out of rotation for that turns a
    /// degraded service into a dead one.
    #[must_use]
    pub fn ever_connected(&self) -> bool {
        self.ever_connected.load(Ordering::Relaxed)
    }

    /// Did the most recent upstream connect attempt succeed?
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.upstream_up.load(Ordering::Relaxed)
    }

    /// Record a successful upstream connect/handshake.
    pub fn record_up(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if !self.ever_connected.swap(true, Ordering::Relaxed) {
            self.set_gauge("ephpm_db_proxy_upstream_ever_connected", 1.0);
        }
        if !self.upstream_up.swap(true, Ordering::Relaxed) {
            self.set_gauge("ephpm_db_proxy_upstream_up", 1.0);
        }
    }

    /// Record a failed upstream connect/handshake.
    ///
    /// Logs the first [`UNTHROTTLED_FAILURES`] unconditionally, then at most
    /// once per [`LOG_INTERVAL_SECS`] — an upstream that is down for an hour
    /// must not turn into an hour of log volume, but it also must not go
    /// completely silent.
    pub fn record_down(&self, error: &dyn std::fmt::Display) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        self.upstream_up.store(false, Ordering::Relaxed);
        self.set_gauge("ephpm_db_proxy_upstream_up", 0.0);
        counter!(
            "ephpm_db_proxy_connect_failures_total",
            "db" => self.kind,
            "upstream" => self.upstream.clone(),
        )
        .increment(1);

        if failures <= UNTHROTTLED_FAILURES {
            warn!(
                db = self.kind,
                upstream = %self.upstream,
                listen = %self.listen,
                failures,
                "database proxy upstream connect failed: {error}"
            );
        } else if self.should_log_now() {
            error!(
                db = self.kind,
                upstream = %self.upstream,
                listen = %self.listen,
                failures,
                ever_connected = self.ever_connected(),
                "database proxy upstream still unreachable (throttled to one log \
                 per {LOG_INTERVAL_SECS}s): {error}"
            );
        }
    }

    /// A short human-readable identity for probe payloads and logs.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} proxy {} -> {}", self.kind, self.listen, self.upstream)
    }

    /// Claim the throttled log slot, if the interval has elapsed.
    fn should_log_now(&self) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let last = self.last_log_unix.load(Ordering::Relaxed);
        if now.saturating_sub(last) < LOG_INTERVAL_SECS {
            return false;
        }
        // CAS so two threads failing at once emit one line, not two.
        self.last_log_unix.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    }

    fn set_gauge(&self, name: &'static str, value: f64) {
        gauge!(name, "db" => self.kind, "upstream" => self.upstream.clone()).set(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_never_connected_and_down() {
        let h = ProxyHealth::new("mysql", "127.0.0.1:3306", "127.0.0.1:3307");
        assert!(!h.ever_connected());
        assert!(!h.is_up());
    }

    #[test]
    fn first_success_latches_ever_connected() {
        let h = ProxyHealth::new("mysql", "127.0.0.1:3306", "127.0.0.1:3307");
        h.record_up();
        assert!(h.ever_connected());
        assert!(h.is_up());

        // A later outage clears `is_up` but must NOT clear `ever_connected`:
        // readiness is a boot-time gate, not a live database probe.
        h.record_down(&"connection refused");
        assert!(h.ever_connected(), "ever_connected must latch");
        assert!(!h.is_up());
    }

    #[test]
    fn throttle_allows_first_burst_then_closes() {
        let h = ProxyHealth::new("mysql", "127.0.0.1:3306", "127.0.0.1:3307");
        // The first call after construction may log (last_log_unix == 0).
        assert!(h.should_log_now());
        // The immediate next one must not — the interval has not elapsed.
        assert!(!h.should_log_now());
    }

    #[test]
    fn describe_never_contains_credentials() {
        let h = ProxyHealth::new("postgres", "127.0.0.1:5432", "db.internal:5432");
        let d = h.describe();
        assert!(d.contains("db.internal:5432"));
        assert!(!d.contains('@'), "upstream label must be host:port, never a URL");
    }
}
