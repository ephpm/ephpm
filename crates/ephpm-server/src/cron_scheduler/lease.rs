//! The per-tick lease and the schedule cursor — the second exactly-once layer,
//! and the one piece of durable scheduler state.
//!
//! Before executing a tick, the scheduler acquires a lease keyed on
//! `(job, tick_ts)` in the clustered KV via the atomic conditional-set
//! (`SET key token NX EX ttl`). This closes the window where the ownership gate
//! is momentarily ambiguous (an ownership move / election flap): during a
//! handoff where two nodes both pass the gate for the same tick, exactly one
//! wins the lease. **Fail closed** — an unreachable key owner is
//! [`LeaseOutcome::Unavailable`], i.e. *skip, do not run*, never *assume free*.
//!
//! The lease is deliberately **not released on success**: it TTLs out (at
//! `tick_ts`-unique granularity), so a crash-restart in the same tick cannot
//! re-run it and a peer that has not yet seen the advanced cursor cannot steal
//! the tick. `lastScheduleTime` is kept per job in the same KV so an ownership
//! move resumes window evaluation where the old owner left off.
//!
//! Both are traits so the orchestrator's exactly-once behaviour can be tested
//! deterministically against an in-memory store; the production implementations
//! wrap the server's `Arc<Store>` (owner-arbitrated and fail-closed in
//! clustered mode, local in single-node mode — the identical call the RESP and
//! PHP paths make).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use ephpm_kv::store::{SetNxOutcome, Store};

/// The outcome of a lease acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// This node won the tick and may run.
    Acquired,
    /// Another node already holds this tick; skip.
    Held,
    /// The key's owner was unreachable (or the store refused): the outcome is
    /// unknown, so fail closed and skip.
    Unavailable,
}

/// Acquire/release a per-tick lease.
pub trait LeaseStore: Send + Sync {
    /// Atomically acquire `key` with `token`, expiring after `ttl`.
    fn acquire(&self, key: &str, token: &[u8], ttl: Duration) -> LeaseOutcome;
    /// Best-effort release of a lease this node holds (compare-and-delete by
    /// token). Used only by `run-now --force`-style ad-hoc releases, never on
    /// the normal success path.
    fn release(&self, key: &str, token: &[u8]);
}

/// Read/write the `lastScheduleTime` cursor for a job.
pub trait CursorStore: Send + Sync {
    /// The job's last scheduled instant, if recorded.
    fn get(&self, job: &str) -> Option<DateTime<Utc>>;
    /// Record the job's last scheduled instant (best effort).
    fn set(&self, job: &str, ts: DateTime<Utc>);
}

/// The lease key for a job at a canonical fire instant.
#[must_use]
pub fn lease_key(job: &str, tick_ts: DateTime<Utc>) -> String {
    format!("cron:lease:{job}:{}", tick_ts.timestamp())
}

/// The cursor key for a job.
#[must_use]
pub fn cursor_key(job: &str) -> String {
    format!("cron:state:{job}")
}

/// A fresh, unforgeable lease token: the node id plus 128 bits of randomness,
/// so a holder releases only its own lease and two nodes never collide.
#[must_use]
pub fn mint_token(self_id: &str) -> Vec<u8> {
    let mut rnd = [0u8; 16];
    // A failure to seed is treated as all-zeros: the token is still unique
    // enough (node id prefix) for correctness; randomness only guards against
    // a same-node same-tick collision, which the cursor already prevents.
    let _ = getrandom::fill(&mut rnd);
    let mut token = Vec::with_capacity(self_id.len() + 1 + 32);
    token.extend_from_slice(self_id.as_bytes());
    token.push(b':');
    for b in rnd {
        token.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    token
}

/// Production [`LeaseStore`]/[`CursorStore`] backed by the server's KV store.
pub struct KvStore {
    store: Arc<Store>,
}

impl KvStore {
    /// Wrap the server's shared KV store.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl LeaseStore for KvStore {
    fn acquire(&self, key: &str, token: &[u8], ttl: Duration) -> LeaseOutcome {
        match self.store.set_nx_outcome(key.to_string(), token.to_vec(), Some(ttl)) {
            SetNxOutcome::Inserted => LeaseOutcome::Acquired,
            SetNxOutcome::Exists => LeaseOutcome::Held,
            // Over memory or owner unreachable: the key state is uncertain, so
            // fail closed (never run).
            SetNxOutcome::OutOfMemory | SetNxOutcome::OwnerUnreachable => LeaseOutcome::Unavailable,
        }
    }

    fn release(&self, key: &str, token: &[u8]) {
        // Ignore the result: a miss / mismatch / unreachable owner all leave
        // the lease to TTL, which is safe.
        let _ = self.store.try_del_if_eq(key, token);
    }
}

impl CursorStore for KvStore {
    fn get(&self, job: &str) -> Option<DateTime<Utc>> {
        let raw = self.store.get(&cursor_key(job))?;
        let text = std::str::from_utf8(&raw).ok()?;
        let secs: i64 = text.trim().parse().ok()?;
        Utc.timestamp_opt(secs, 0).single()
    }

    fn set(&self, job: &str, ts: DateTime<Utc>) {
        let value = ts.timestamp().to_string().into_bytes();
        // No TTL: the cursor must survive so a new owner resumes where the old
        // one left off. A lost cursor is re-derived as "window starts now".
        let _ = self.store.set(cursor_key(job), value, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_and_tick_scoped() {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        assert_eq!(lease_key("catalog", ts), "cron:lease:catalog:1700000000");
        assert_eq!(cursor_key("catalog"), "cron:state:catalog");
    }

    #[test]
    fn token_carries_node_prefix_and_is_unique() {
        let a = mint_token("ephpm-0");
        let b = mint_token("ephpm-0");
        assert!(a.starts_with(b"ephpm-0:"));
        assert_ne!(a, b, "two tokens from the same node differ (randomness)");
    }
}
