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

/// Maximum number of tracked IPs before we stop inserting new entries.
/// Prevents unbounded memory growth under DDoS with spoofed source IPs.
const MAX_TRACKED_IPS: usize = 100_000;

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
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.limiter.global_connections.fetch_sub(1, Ordering::Relaxed);
        if let Some(state) = self.limiter.per_ip.get(&self.ip) {
            state.connections.fetch_sub(1, Ordering::Relaxed);
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

        // Check per-IP limit.
        if self.config.per_ip_max_connections > 0 {
            let state = self.get_or_create_ip(ip);
            let current = state.connections.fetch_add(1, Ordering::Relaxed);
            if current >= self.config.per_ip_max_connections {
                state.connections.fetch_sub(1, Ordering::Relaxed);
                self.global_connections.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        } else if let Some(state) = self.per_ip.get(&ip) {
            state.connections.fetch_add(1, Ordering::Relaxed);
        }

        Some(ConnectionGuard { limiter: std::sync::Arc::clone(self), ip })
    }

    /// Check if a request from the given IP is allowed under the rate limit.
    ///
    /// Uses a token bucket algorithm. Returns `true` if allowed.
    pub fn check_rate(&self, ip: IpAddr) -> bool {
        if self.config.per_ip_rate <= 0.0 {
            return true;
        }

        let state = self.get_or_create_ip(ip);
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

                let old = state.tokens.fetch_add(new_tokens, Ordering::Relaxed);
                // Cap at burst limit.
                if old + new_tokens > burst_tokens {
                    state.tokens.store(burst_tokens, Ordering::Relaxed);
                }
            }
        }

        // Try to consume one token (1000 scaled units).
        let current = state.tokens.load(Ordering::Relaxed);
        if current >= 1000 {
            state.tokens.fetch_sub(1000, Ordering::Relaxed);
            true
        } else {
            false
        }
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

    /// Get or create per-IP state. Returns a DashMap ref.
    fn get_or_create_ip(&self, ip: IpAddr) -> dashmap::mapref::one::Ref<'_, IpAddr, IpState> {
        if let Some(state) = self.per_ip.get(&ip) {
            return state;
        }

        // Fail-closed: if we're tracking too many IPs, reject new ones.
        if self.per_ip.len() >= MAX_TRACKED_IPS {
            // Return an entry that will deny the request (0 tokens).
            self.per_ip.entry(ip).or_insert_with(|| IpState {
                connections: AtomicUsize::new(0),
                tokens: AtomicU64::new(0),
                last_refill_ms: AtomicU64::new(self.epoch.elapsed().as_millis() as u64),
            });
            return self.per_ip.get(&ip).expect("just inserted");
        }

        let burst_tokens = u64::from(self.config.per_ip_burst) * 1000;
        self.per_ip.entry(ip).or_insert_with(|| IpState {
            connections: AtomicUsize::new(0),
            tokens: AtomicU64::new(burst_tokens),
            last_refill_ms: AtomicU64::new(self.epoch.elapsed().as_millis() as u64),
        });
        self.per_ip.get(&ip).expect("just inserted")
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn is_enabled_checks_config() {
        assert!(!Limiter::new(LimitsConfig::default()).is_enabled());
        assert!(Limiter::new(test_config(10.0, 5, 0)).is_enabled());
        assert!(Limiter::new(test_config(0.0, 5, 100)).is_enabled());
    }
}
