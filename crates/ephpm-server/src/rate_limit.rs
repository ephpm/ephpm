//! Rate limiting and connection limiting.
//!
//! Provides a [`Limiter`] that enforces:
//! - Global maximum concurrent connections
//! - Per-IP maximum concurrent connections
//! - Per-IP request rate limiting (token bucket)
//!
//! Connection limits are enforced at the TCP accept loop via [`ConnectionGuard`].
//! Rate limits are enforced at the HTTP layer in the router.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use ephpm_config::LimitsConfig;

/// Maximum number of tracked IPs.
///
/// Once the map holds this many entries, requests and connections from
/// IPs that are not already tracked are refused outright — no new entry
/// is created. Refusing is what actually bounds the map: the previous
/// code inserted a zero-token entry for over-cap IPs, so the map kept
/// growing past the "cap" *and* every new IP was pinned at a permanent
/// 429 until the sweep evicted it.
///
/// This is fail-closed by design. Reaching 100,000 distinct source IPs
/// within one 30s sweep window is a spoofed-source flood, not organic
/// traffic, and an untracked IP would otherwise bypass the limiter
/// entirely.
const MAX_TRACKED_IPS: usize = 100_000;

/// One token-bucket consumption step, for `AtomicU64::fetch_update`.
///
/// Takes one token (1000 scaled units), or returns `None` when the bucket
/// is empty so the update is refused instead of wrapping the counter.
fn take_one_token(tokens: u64) -> Option<u64> {
    tokens.checked_sub(1000)
}

/// Per-IP state: connection count and token bucket for rate limiting.
struct IpState {
    /// Active connections from this IP.
    connections: AtomicUsize,
    /// Token bucket: available tokens (scaled by 1000 for sub-token precision).
    tokens: AtomicU64,
    /// Last time tokens were refilled (millis since `Limiter` creation).
    last_refill_ms: AtomicU64,
}

/// Connection and request rate limiter.
///
/// Thread-safe — all state is behind atomics and [`DashMap`].
pub struct Limiter {
    global_connections: AtomicUsize,
    per_ip: DashMap<IpAddr, IpState>,
    config: LimitsConfig,
    /// Epoch for monotonic time calculations.
    epoch: Instant,
}

/// RAII guard that decrements connection counters when dropped.
///
/// Created by [`Limiter::try_acquire_connection`]. Holds the connection
/// slot until the guard is dropped (typically when the connection closes).
pub struct ConnectionGuard {
    limiter: std::sync::Arc<Limiter>,
    ip: IpAddr,
    /// Whether this guard incremented the per-IP connection counter.
    ///
    /// The per-IP counter is only incremented when a per-IP entry exists
    /// at acquire time. Decrementing unconditionally on drop underflowed
    /// the `AtomicUsize` whenever the entry was created *later* by
    /// `check_rate` — with `per_ip_max_connections = 0` (the default)
    /// that is the ordinary sequence. The counter wrapped to `usize::MAX`,
    /// so `cleanup_stale` (which keeps anything with `conns > 0`) could
    /// never evict the entry again.
    counted: bool,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.limiter.global_connections.fetch_sub(1, Ordering::Relaxed);
        if self.counted {
            if let Some(state) = self.limiter.per_ip.get(&self.ip) {
                state.connections.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

impl Limiter {
    /// Create a new limiter with the given configuration.
    #[must_use]
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            global_connections: AtomicUsize::new(0),
            per_ip: DashMap::new(),
            config,
            epoch: Instant::now(),
        }
    }

    /// Check if any limits are configured (non-zero).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.max_connections > 0
            || self.config.per_ip_max_connections > 0
            || self.config.per_ip_rate > 0.0
    }

    /// Try to acquire a connection slot for the given IP.
    ///
    /// Returns a [`ConnectionGuard`] on success, or `None` if the global
    /// or per-IP connection limit has been reached.
    pub fn try_acquire_connection(
        self: &std::sync::Arc<Self>,
        ip: IpAddr,
    ) -> Option<ConnectionGuard> {
        // Check global limit.
        if self.config.max_connections > 0 {
            let current = self.global_connections.fetch_add(1, Ordering::Relaxed);
            if current >= self.config.max_connections {
                self.global_connections.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        } else {
            self.global_connections.fetch_add(1, Ordering::Relaxed);
        }

        // Check per-IP limit. `counted` records whether we incremented the
        // per-IP counter, so `ConnectionGuard::drop` decrements exactly
        // what it took and never underflows an entry it never touched.
        let counted = if self.config.per_ip_max_connections > 0 {
            let Some(state) = self.get_or_create_ip(ip) else {
                // At the tracked-IP cap we cannot account for this IP's
                // connections, so refuse rather than let it slip past the
                // per-IP limit untracked.
                self.global_connections.fetch_sub(1, Ordering::Relaxed);
                return None;
            };
            let current = state.connections.fetch_add(1, Ordering::Relaxed);
            if current >= self.config.per_ip_max_connections {
                state.connections.fetch_sub(1, Ordering::Relaxed);
                self.global_connections.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
            true
        } else if let Some(state) = self.per_ip.get(&ip) {
            state.connections.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            // No per-IP limit configured and no entry to charge. An entry
            // may be created later by `check_rate`; it must not be
            // decremented on drop.
            false
        };

        Some(ConnectionGuard { limiter: std::sync::Arc::clone(self), ip, counted })
    }

    /// Check if a request from the given IP is allowed under the rate limit.
    ///
    /// Uses a token bucket algorithm. Returns `true` if allowed.
    ///
    /// Returns `false` for an untracked IP once the tracked-IP cap
    /// (`MAX_TRACKED_IPS`) is reached — see that constant for why this is
    /// deliberately fail-closed.
    pub fn check_rate(&self, ip: IpAddr) -> bool {
        if self.config.per_ip_rate <= 0.0 {
            return true;
        }

        let Some(state) = self.get_or_create_ip(ip) else {
            return false;
        };
        let now_ms = self.epoch.elapsed().as_millis() as u64;
        let burst_tokens = u64::from(self.config.per_ip_burst) * 1000;

        // Refill tokens based on elapsed time.
        let last_ms = state.last_refill_ms.load(Ordering::Relaxed);
        let elapsed_ms = now_ms.saturating_sub(last_ms);

        if elapsed_ms > 0 {
            // Tokens to add (rate is per second, so tokens_per_ms = rate / 1000).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let new_tokens = (self.config.per_ip_rate * elapsed_ms as f64) as u64;

            if new_tokens > 0 {
                // CAS loop to atomically refill.
                let _ = state.last_refill_ms.compare_exchange(
                    last_ms,
                    now_ms,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );

                // Add and cap in one atomic update. The old `fetch_add`
                // followed by a conditional `store` could overflow the
                // `old + new_tokens` sum (a panic in debug builds) and
                // raced with concurrent refills of the same bucket.
                let _ = state.tokens.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |tokens| {
                    Some(tokens.saturating_add(new_tokens).min(burst_tokens))
                });
            }
        }

        // Consume one token (1000 scaled units), atomically.
        //
        // This must be a compare-and-swap, not `load` then `fetch_sub`:
        // with the latter, two threads that both observe the last token
        // both subtract, wrapping the `AtomicU64` to ~`u64::MAX` — which
        // silently disables rate limiting for that IP until the entry is
        // swept, the exact opposite of the intended behaviour under load.
        // `take_one_token` returns `None` when the bucket is empty, which
        // `fetch_update` reports as `Err` without writing.
        state.tokens.fetch_update(Ordering::Relaxed, Ordering::Relaxed, take_one_token).is_ok()
    }

    /// Remove stale per-IP entries to prevent memory growth.
    ///
    /// An entry is stale when it has zero connections and a full token bucket.
    pub fn cleanup_stale(&self) {
        let burst_tokens = u64::from(self.config.per_ip_burst) * 1000;
        self.per_ip.retain(|_, state| {
            let conns = state.connections.load(Ordering::Relaxed);
            let tokens = state.tokens.load(Ordering::Relaxed);
            // Keep entries that have active connections or partially consumed buckets.
            conns > 0 || tokens < burst_tokens
        });
    }

    /// Number of currently active connections (global).
    #[must_use]
    pub fn active_connections(&self) -> usize {
        self.global_connections.load(Ordering::Relaxed)
    }

    /// Get or create per-IP state.
    ///
    /// Returns `None` when the IP is not already tracked and the map is at
    /// [`MAX_TRACKED_IPS`] — callers treat that as "deny". No entry is
    /// inserted in that case, which is what keeps the map bounded.
    fn get_or_create_ip(
        &self,
        ip: IpAddr,
    ) -> Option<dashmap::mapref::one::Ref<'_, IpAddr, IpState>> {
        // Bounded retry: `cleanup_stale` can evict the freshly inserted
        // entry (full bucket, no connections) between the insert and the
        // read-back. Re-inserting once or twice is cheaper and more
        // correct than holding a write guard across the whole hot path.
        for _ in 0..3 {
            if let Some(state) = self.per_ip.get(&ip) {
                return Some(state);
            }

            if self.per_ip.len() >= MAX_TRACKED_IPS {
                return None;
            }

            let burst_tokens = u64::from(self.config.per_ip_burst) * 1000;
            self.per_ip.entry(ip).or_insert_with(|| IpState {
                connections: AtomicUsize::new(0),
                tokens: AtomicU64::new(burst_tokens),
                last_refill_ms: AtomicU64::new(self.epoch.elapsed().as_millis() as u64),
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn test_config(rate: f64, burst: u32, max_conn: usize) -> LimitsConfig {
        LimitsConfig {
            max_connections: max_conn,
            per_ip_max_connections: 0,
            per_ip_rate: rate,
            per_ip_burst: burst,
        }
    }

    #[test]
    fn rate_limit_allows_burst() {
        let limiter = Limiter::new(test_config(10.0, 5, 0));
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // Should allow burst_size requests immediately.
        for _ in 0..5 {
            assert!(limiter.check_rate(ip), "request within burst should be allowed");
        }
        // 6th should be denied (bucket empty).
        assert!(!limiter.check_rate(ip), "request beyond burst should be denied");
    }

    #[test]
    fn rate_limit_disabled_when_zero() {
        let limiter = Limiter::new(test_config(0.0, 50, 0));
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        for _ in 0..100 {
            assert!(limiter.check_rate(ip), "unlimited rate should always allow");
        }
    }

    #[test]
    fn connection_limit_enforced() {
        let config = LimitsConfig { max_connections: 2, ..LimitsConfig::default() };
        let limiter = std::sync::Arc::new(Limiter::new(config));
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        let g1 = limiter.try_acquire_connection(ip);
        assert!(g1.is_some(), "first connection should be allowed");

        let g2 = limiter.try_acquire_connection(ip);
        assert!(g2.is_some(), "second connection should be allowed");

        let g3 = limiter.try_acquire_connection(ip);
        assert!(g3.is_none(), "third connection should be rejected");

        // Drop one — next should succeed.
        drop(g1);
        let g4 = limiter.try_acquire_connection(ip);
        assert!(g4.is_some(), "connection after drop should be allowed");
    }

    #[test]
    fn cleanup_removes_stale_entries() {
        let limiter = Limiter::new(test_config(10.0, 5, 0));
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // Consume some tokens so the entry is not stale.
        limiter.check_rate(ip);
        assert_eq!(limiter.per_ip.len(), 1);

        // Entry has partial tokens — should not be cleaned up.
        limiter.cleanup_stale();
        assert_eq!(limiter.per_ip.len(), 1);
    }

    #[test]
    fn cleanup_removes_fully_replenished_idle_entry() {
        let limiter = Limiter::new(test_config(10.0, 5, 0));
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        // Consume one token to create the entry with a partially consumed bucket.
        assert!(limiter.check_rate(ip));
        assert_eq!(limiter.per_ip.len(), 1);

        // Partially consumed — cleanup should retain it.
        limiter.cleanup_stale();
        assert_eq!(limiter.per_ip.len(), 1, "partially consumed entry should be retained");

        // Simulate the bucket being fully replenished by directly setting
        // tokens back to the burst limit. This tests cleanup's eviction logic
        // in isolation — refill timing is tested separately via check_rate.
        let burst_tokens = u64::from(limiter.config.per_ip_burst) * 1000;
        if let Some(entry) = limiter.per_ip.get(&ip) {
            entry.tokens.store(burst_tokens, Ordering::Relaxed);
        }

        // Now cleanup should remove the entry (0 connections + full bucket).
        limiter.cleanup_stale();
        assert_eq!(
            limiter.per_ip.len(),
            0,
            "fully replenished idle entry should be removed by cleanup"
        );
    }

    /// An exhausted bucket must stay exhausted. `load` + `fetch_sub` let a
    /// denied request subtract anyway on the next racing caller, wrapping
    /// the counter to ~`u64::MAX` and disabling the limit for that IP.
    #[test]
    fn exhausted_bucket_does_not_wrap() {
        let limiter = Limiter::new(test_config(0.001, 5, 0));
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        for _ in 0..5 {
            assert!(limiter.check_rate(ip));
        }
        for _ in 0..100 {
            assert!(!limiter.check_rate(ip), "empty bucket must keep denying");
        }

        let tokens = limiter.per_ip.get(&ip).unwrap().tokens.load(Ordering::Relaxed);
        assert!(tokens < 1000, "token counter wrapped instead of emptying: {tokens}");
    }

    /// Concurrent consumption must hand out exactly `burst` permits, never
    /// more. The rate is small enough that refill contributes nothing over
    /// the lifetime of the test, so the expected count is exact.
    #[test]
    fn concurrent_consumption_never_exceeds_burst() {
        let limiter = Limiter::new(test_config(0.001, 100, 0));
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let allowed = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..200 {
                        if limiter.check_rate(ip) {
                            allowed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert_eq!(allowed.load(Ordering::Relaxed), 100, "exactly `burst` requests may pass");
        let tokens = limiter.per_ip.get(&ip).unwrap().tokens.load(Ordering::Relaxed);
        assert!(tokens < 1000, "token counter wrapped under contention: {tokens}");
    }

    /// With `per_ip_max_connections = 0` (the default) a guard acquired
    /// before the IP has an entry must not decrement the entry that
    /// `check_rate` creates later — that underflows to `usize::MAX` and
    /// makes the entry permanently un-evictable.
    #[test]
    fn guard_does_not_decrement_a_counter_it_never_incremented() {
        let limiter = std::sync::Arc::new(Limiter::new(test_config(10.0, 5, 0)));
        let ip: IpAddr = "10.1.2.3".parse().unwrap();

        let guard = limiter.try_acquire_connection(ip).expect("no connection limit configured");
        // The per-IP entry comes into existence only now, via the rate path.
        assert!(limiter.check_rate(ip));
        drop(guard);

        let conns = limiter.per_ip.get(&ip).unwrap().connections.load(Ordering::Relaxed);
        assert_eq!(conns, 0, "guard underflowed a counter it never incremented");

        // An underflowed counter would keep this entry alive forever, since
        // `cleanup_stale` retains anything with connections > 0.
        let burst_tokens = u64::from(limiter.config.per_ip_burst) * 1000;
        limiter.per_ip.get(&ip).unwrap().tokens.store(burst_tokens, Ordering::Relaxed);
        limiter.cleanup_stale();
        assert_eq!(limiter.per_ip.len(), 0, "idle entry must be evictable");
    }

    /// At `MAX_TRACKED_IPS` the map must stop growing: new IPs are denied
    /// without an entry being inserted, and tracked IPs keep working.
    #[test]
    fn tracked_ip_cap_bounds_the_map() {
        let limiter = Limiter::new(test_config(10.0, 5, 0));

        for i in 0..MAX_TRACKED_IPS {
            let ip = IpAddr::V4(Ipv4Addr::from(u32::try_from(i).expect("index fits in u32")));
            assert!(limiter.check_rate(ip));
        }
        assert_eq!(limiter.per_ip.len(), MAX_TRACKED_IPS);

        // Well outside the range generated above, so genuinely untracked.
        let fresh: IpAddr = "203.0.113.7".parse().unwrap();
        assert!(!limiter.check_rate(fresh), "untracked IP must be denied at the cap");
        assert_eq!(limiter.per_ip.len(), MAX_TRACKED_IPS, "the cap must bound the map");

        // Already-tracked IPs are unaffected.
        assert!(limiter.check_rate(IpAddr::V4(Ipv4Addr::from(0))));
    }

    #[test]
    fn is_enabled_checks_config() {
        assert!(!Limiter::new(LimitsConfig::default()).is_enabled());
        assert!(Limiter::new(test_config(10.0, 5, 0)).is_enabled());
        assert!(Limiter::new(test_config(0.0, 5, 100)).is_enabled());
    }
}
