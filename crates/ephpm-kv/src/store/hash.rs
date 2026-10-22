//! Hash values: per-field storage, per-field expiry, and the hash commands.
//!
//! # Representation (what a snapshot must carry)
//!
//! A hash lives in `Store::hashes` as a [`HashEntry`] keyed by the hash's key.
//! It is a separate map from the string keyspace, so a string and a hash may
//! share a name (a long-standing divergence from Redis, which would answer
//! `WRONGTYPE`). The **persistent** state of one hash is:
//!
//! * the key;
//! * `expires_at` — the whole-key expiry set by `EXPIRE`/`PEXPIRE`/`EXPIREAT`
//!   on the hash key (`None` = persistent);
//! * for every field: its name (UTF-8), its value (raw bytes — hash values are
//!   never compressed), and its own `expires_at` set by `HEXPIRE` and friends
//!   (`None` = no field TTL).
//!
//! Everything else on [`HashEntry`] — `mem`, `volatile`, `next_field_expiry`,
//! `last_accessed` — and the `Store::hash_ttl_keys` index hint are **derived**
//! and are rebuilt by writing the persistent state back through
//! [`Store::apply_hash_field_local`]. A snapshot that stores expiries as
//! absolute wall-clock times (the store holds `Instant`s) loses nothing.
//!
//! # Field expiry semantics (Redis 7.4 / Valkey 9)
//!
//! An expired field is invisible to every read (`HGET`, `HMGET`, `HGETALL`,
//! `HKEYS`, `HVALS`, `HLEN`, `HEXISTS`, `HSTRLEN`, `HSCAN`, `HTTL`, ...) from
//! the instant its deadline passes, before anything reclaims it. Reclaiming is
//! lazy (any write to the hash purges its expired fields) and active (the
//! hash's earliest deadline is kept in `Store::hash_ttl_keys`, which the
//! active-expiry cycle samples just as it samples string TTLs). A hash whose
//! last field expires stops existing. `HSET` on a field clears its TTL;
//! `HINCRBY`/`HINCRBYFLOAT` keep it.
//!
//! # Replication
//!
//! Every field mutation is one replicated write through
//! [`Replicator::replicate_hash_field`](super::Replicator::replicate_hash_field):
//! the replicator orders it (a stamp per field) and fans out the field's
//! resulting [`HashFieldState`], so concurrent writes to *different* fields on
//! different nodes never overwrite each other — last-writer-wins is per field,
//! not per hash. Watch versions (`ephpm_kv_wait`) are bumped by every write
//! that changes a field's value or removes it; TTL-only changes do not bump,
//! matching the string keyspace.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{ExpireCondition, HashFieldState, Store, entry, ttl_index_cost};

/// Bytes charged for a hash beyond its fields: the key's own copy plus the
/// map slot and the entry struct.
pub(super) const HASH_BASE_OVERHEAD: usize = 64;

/// Bytes charged per field beyond its name and value: the `HashMap` slot
/// (`String` + [`HashField`]) sized up for the table's load factor.
pub(super) const HASH_FIELD_OVERHEAD: usize = 96;

/// Bytes charged for a hash keyed `key` with no fields.
pub(super) const fn hash_base_cost(key: &str) -> usize {
    key.len() + HASH_BASE_OVERHEAD
}

/// Bytes charged for one field.
pub(super) const fn hash_field_cost(field: &str, value_len: usize) -> usize {
    field.len() + value_len + HASH_FIELD_OVERHEAD
}

/// One field of a hash.
#[derive(Debug)]
pub(super) struct HashField {
    /// The field's value. [`Bytes`] so reads hand out `Arc`-clones.
    pub(super) value: Bytes,
    /// The field's own expiry (`HEXPIRE`), or `None`.
    pub(super) expires_at: Option<Instant>,
}

impl HashField {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|at| now < at)
    }
}

/// A hash value. See the module docs for which parts are persistent state.
#[derive(Debug)]
pub(super) struct HashEntry {
    /// Field name → field. May hold expired fields not yet purged.
    pub(super) fields: HashMap<String, HashField>,
    /// Whole-key expiry (`EXPIRE` on the hash key), or `None`.
    pub(super) expires_at: Option<Instant>,
    /// Last access, as nanoseconds since the store anchor (the LRU clock, see
    /// [`super::Entry::last_accessed`]).
    pub(super) last_accessed: AtomicU64,
    /// Bytes this hash is charged in `Store::mem_used`: the base cost plus
    /// every field's cost. Kept exactly in step with what was added and
    /// subtracted, so a removal credits back precisely what was charged.
    pub(super) mem: usize,
    /// Number of fields carrying an expiry. `0` lets reads skip the per-field
    /// liveness scan entirely.
    volatile: usize,
    /// No later than the earliest field expiry (an early hint is corrected by
    /// the next purge). `None` when no field has an expiry.
    next_field_expiry: Option<Instant>,
}

impl HashEntry {
    /// An empty hash for `key`, charged [`hash_base_cost`].
    pub(super) fn new(key: &str, now_nanos: u64) -> Self {
        Self {
            fields: HashMap::new(),
            expires_at: None,
            last_accessed: AtomicU64::new(now_nanos),
            mem: hash_base_cost(key),
            volatile: 0,
            next_field_expiry: None,
        }
    }

    /// Rebuild a hash from its persistent state (a snapshot record): every
    /// field with its own expiry, and the key expiry. Fields already expired
    /// at `now` are skipped. `None` when the key itself has expired or no
    /// field is left — such a hash does not exist.
    pub(super) fn from_parts(
        key: &str,
        fields: impl IntoIterator<Item = (String, Bytes, Option<Instant>)>,
        expires_at: Option<Instant>,
        now: Instant,
        now_nanos: u64,
    ) -> Option<Self> {
        if expires_at.is_some_and(|at| at <= now) {
            return None;
        }
        let mut entry = Self::new(key, now_nanos);
        entry.expires_at = expires_at;
        for (name, value, field_at) in fields {
            if field_at.is_some_and(|at| at <= now) {
                continue;
            }
            entry.put_field(&name, value, field_at, now);
        }
        (!entry.fields.is_empty()).then_some(entry)
    }

    /// Every live field as `(name, value, expiry)`, for a snapshot.
    pub(super) fn live_parts(&self, now: Instant) -> Vec<(String, Bytes, Option<Instant>)> {
        self.live_fields(now).map(|(k, f)| (k.clone(), f.value.clone(), f.expires_at)).collect()
    }

    /// Touch the LRU clock, with the same coarse granularity as
    /// [`super::Entry::touch`].
    pub(super) fn touch(&self, now_nanos: u64) {
        let prev = self.last_accessed.load(Ordering::Relaxed);
        if now_nanos.saturating_sub(prev) >= entry::LRU_TOUCH_GRANULARITY_NANOS {
            self.last_accessed.store(now_nanos, Ordering::Relaxed);
        }
    }

    /// Whether the whole-key expiry has passed.
    pub(super) fn key_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }

    /// Whether some field may have expired (the hint has passed).
    fn may_have_expired_fields(&self, now: Instant) -> bool {
        self.next_field_expiry.is_some_and(|at| now >= at)
    }

    /// Whether this hash exists for a reader at `now`: not key-expired and at
    /// least one live field.
    pub(super) fn is_live(&self, now: Instant) -> bool {
        if self.key_expired(now) {
            return false;
        }
        if self.may_have_expired_fields(now) {
            self.fields.values().any(|f| f.is_live(now))
        } else {
            !self.fields.is_empty()
        }
    }

    /// The live field `name`, if any.
    fn live(&self, name: &str, now: Instant) -> Option<&HashField> {
        self.fields.get(name).filter(|f| f.is_live(now))
    }

    /// Number of live fields.
    fn live_len(&self, now: Instant) -> usize {
        if self.may_have_expired_fields(now) {
            self.fields.values().filter(|f| f.is_live(now)).count()
        } else {
            self.fields.len()
        }
    }

    /// Iterate over the live fields.
    fn live_fields(&self, now: Instant) -> impl Iterator<Item = (&String, &HashField)> {
        self.fields.iter().filter(move |(_, f)| f.is_live(now))
    }

    /// The earliest moment anything in this hash expires: the key itself or
    /// (no later than) its first field. What the TTL index records.
    pub(super) fn earliest_deadline(&self) -> Option<Instant> {
        match (self.expires_at, self.next_field_expiry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Drop every field whose expiry has passed and recompute the hint.
    /// Returns the bytes freed.
    pub(super) fn purge_expired_fields(&mut self, now: Instant) -> usize {
        if !self.may_have_expired_fields(now) {
            return 0;
        }
        let mut freed = 0;
        self.fields.retain(|name, f| {
            if f.is_live(now) {
                true
            } else {
                freed += hash_field_cost(name, f.value.len());
                false
            }
        });
        self.mem = self.mem.saturating_sub(freed);
        self.recompute_field_expiry();
        freed
    }

    /// Recompute `volatile` and `next_field_expiry` from the fields.
    fn recompute_field_expiry(&mut self) {
        let mut volatile = 0;
        let mut next: Option<Instant> = None;
        for f in self.fields.values() {
            if let Some(at) = f.expires_at {
                volatile += 1;
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        self.volatile = volatile;
        self.next_field_expiry = next;
    }

    /// Reset a key-expired hash to empty, keeping only its base charge.
    /// Returns the bytes freed.
    fn reset(&mut self, base: usize) -> usize {
        let freed = self.mem.saturating_sub(base);
        self.fields.clear();
        self.fields.shrink_to_fit();
        self.expires_at = None;
        self.volatile = 0;
        self.next_field_expiry = None;
        self.mem = base;
        freed
    }

    /// Note that a field now carries `at` as its expiry.
    fn note_field_expiry(&mut self, at: Instant) {
        self.next_field_expiry = Some(self.next_field_expiry.map_or(at, |n| n.min(at)));
    }

    /// Insert or replace `name`, with the given field expiry. Returns whether
    /// the field was newly created (a replaced *expired* field counts as new)
    /// and `(added, freed)` bytes.
    fn put_field(
        &mut self,
        name: &str,
        value: Bytes,
        expires_at: Option<Instant>,
        now: Instant,
    ) -> (bool, usize, usize) {
        let added = hash_field_cost(name, value.len());
        let previous = self.fields.insert(name.to_string(), HashField { value, expires_at });
        let (created, freed) = match previous {
            Some(old) => {
                if old.expires_at.is_some() {
                    self.volatile = self.volatile.saturating_sub(1);
                }
                (!old.is_live(now), hash_field_cost(name, old.value.len()))
            }
            None => (true, 0),
        };
        if let Some(at) = expires_at {
            self.volatile += 1;
            self.note_field_expiry(at);
        }
        self.mem = self.mem.saturating_add(added).saturating_sub(freed);
        (created, added, freed)
    }

    /// Remove `name`. Returns the removed field (live or not) and the bytes
    /// freed.
    fn take_field(&mut self, name: &str) -> Option<(HashField, usize)> {
        let old = self.fields.remove(name)?;
        if old.expires_at.is_some() {
            self.volatile = self.volatile.saturating_sub(1);
        }
        let freed = hash_field_cost(name, old.value.len());
        self.mem = self.mem.saturating_sub(freed);
        Some((old, freed))
    }

    /// Set (or clear) the expiry of an existing field.
    fn set_field_expiry(&mut self, name: &str, at: Option<Instant>) {
        let Some(field) = self.fields.get_mut(name) else {
            return;
        };
        match (field.expires_at.is_some(), at.is_some()) {
            (false, true) => self.volatile += 1,
            (true, false) => self.volatile = self.volatile.saturating_sub(1),
            _ => {}
        }
        field.expires_at = at;
        if let Some(at) = at {
            self.note_field_expiry(at);
        }
        if self.volatile == 0 {
            self.next_field_expiry = None;
        }
    }
}

/// Why a hash write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashWriteError {
    /// The memory budget refused the write (Redis `-OOM`).
    OutOfMemory,
    /// Clustered only: the value is larger than a hash field can be and still
    /// replicate. Hash fields ride the gossip tier, whose payload is bounded
    /// by `[cluster.kv] small_key_threshold`; a larger field would be
    /// node-local, which is refused rather than done silently.
    TooLargeForCluster {
        /// The value's length in bytes.
        len: usize,
        /// The largest value a clustered hash field may hold.
        max: usize,
    },
}

impl std::fmt::Display for HashWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str(super::OOM_ERROR),
            Self::TooLargeForCluster { len, max } => write!(
                f,
                "ERR hash field value of {len} bytes exceeds the {max}-byte limit for hash \
                 fields in a cluster: hash fields replicate over the gossip tier, which \
                 [cluster.kv] small_key_threshold bounds. Store large values under string keys"
            ),
        }
    }
}

impl std::error::Error for HashWriteError {}

/// Error text Redis uses when `HINCRBY` meets a non-integer field.
pub const HASH_NOT_INTEGER: &str = "ERR hash value is not an integer";

/// Error text Redis uses when `HINCRBYFLOAT` meets a non-float field.
pub const HASH_NOT_FLOAT: &str = "ERR hash value is not a float";

/// Error text for an increment that overflows `i64`.
pub const INCR_OVERFLOW: &str = "ERR increment or decrement would overflow";

/// Error text for a float increment that leaves the finite range.
pub const INCR_NAN_OR_INF: &str = "ERR increment would produce NaN or Infinity";

/// Per-field reply codes of the field-expiry commands (`HEXPIRE`, `HPERSIST`,
/// `HTTL`, ...), as Redis defines them.
pub mod field_code {
    /// The field (or the whole hash) does not exist.
    pub const NO_FIELD: i64 = -2;
    /// `HTTL`/`HPERSIST`: the field exists but has no expiry.
    pub const NO_EXPIRY: i64 = -1;
    /// `HEXPIRE`: the `NX`/`XX`/`GT`/`LT` condition was not met.
    pub const CONDITION_NOT_MET: i64 = 0;
    /// `HEXPIRE`: the expiry was set. `HPERSIST`: the expiry was removed.
    pub const UPDATED: i64 = 1;
    /// `HEXPIRE`: the new expiry was zero or in the past, so the field was
    /// deleted.
    pub const DELETED: i64 = 2;
}

impl Store {
    // ── Internal machinery ───────────────────────────────────────

    /// Run `op` on `key`'s hash under its shard write lock, creating the hash
    /// when `create` is set. Returns `None` (without running `op`) when the
    /// hash does not exist and `create` is not set.
    ///
    /// Before `op` runs, a key-expired hash is reset to empty and expired
    /// fields are purged, so `op` only ever sees live state. Memory deltas
    /// are applied from the entry's own `mem` counter, an emptied hash is
    /// removed, and the TTL index is updated — all after the guard is
    /// dropped.
    ///
    /// Never call with a `hashes` guard held, and never from inside `op`
    /// touch `self.hashes`. Memory must have been reserved beforehand: this
    /// does not evict.
    fn hash_write<R>(
        &self,
        key: &str,
        create: bool,
        op: impl FnOnce(&mut HashEntry, Instant) -> R,
    ) -> Option<R> {
        let now = Instant::now();
        let base = hash_base_cost(key);
        let mut guard = if create {
            self.hashes.entry(key.to_string()).or_insert_with(|| {
                self.mem_add(base);
                HashEntry::new(key, self.now_nanos())
            })
        } else {
            self.hashes.get_mut(key)?
        };
        let before = guard.mem;
        if guard.key_expired(now) {
            let _ = guard.reset(base);
        } else {
            let _ = guard.purge_expired_fields(now);
        }
        if !create && guard.fields.is_empty() {
            // Nothing live to operate on: settle the accounting and drop it.
            let after = guard.mem;
            drop(guard);
            self.mem_sub(before.saturating_sub(after));
            self.drop_empty_hash(key);
            return None;
        }
        guard.touch(self.now_nanos());
        let result = op(&mut guard, now);
        let after = guard.mem;
        let empty = guard.fields.is_empty();
        let deadline = guard.earliest_deadline();
        drop(guard);
        if after > before {
            self.mem_add(after - before);
        } else {
            self.mem_sub(before - after);
        }
        if empty {
            self.drop_empty_hash(key);
        } else if let Some(at) = deadline {
            self.hash_index_note(key, at);
        }
        Some(result)
    }

    /// Remove `key`'s hash if it holds no fields (checked under the lock, so
    /// a field added concurrently keeps it).
    fn drop_empty_hash(&self, key: &str) {
        if let Some((_, old)) = self.hashes.remove_if(key, |_, h| h.fields.is_empty()) {
            self.mem_sub(old.mem);
            self.hash_index_drop(key);
        }
    }

    /// Record that `key`'s hash has something expiring at `at`, keeping the
    /// earlier of an existing hint and `at` — the hint may be early, never
    /// late.
    fn hash_index_note(&self, key: &str, at: Instant) {
        if let Some(mut hint) = self.hash_ttl_keys.get_mut(key) {
            if at < *hint {
                *hint = at;
            }
            return;
        }
        if self.hash_ttl_keys.insert(key.to_string(), at).is_none() {
            self.mem_add(ttl_index_cost(key));
        }
    }

    /// Replace `key`'s hint with exactly `at` (or drop it).
    fn hash_index_set(&self, key: &str, at: Option<Instant>) {
        match at {
            Some(at) => {
                if let Some(mut hint) = self.hash_ttl_keys.get_mut(key) {
                    *hint = at;
                } else if self.hash_ttl_keys.insert(key.to_string(), at).is_none() {
                    self.mem_add(ttl_index_cost(key));
                }
            }
            None => self.hash_index_drop(key),
        }
    }

    /// Set `key`'s hint after replacing the whole hash (a snapshot restore).
    pub(super) fn hash_index_restore(&self, key: &str, at: Option<Instant>) {
        self.hash_index_set(key, at);
    }

    /// Drop `key`'s hint.
    pub(super) fn hash_index_drop(&self, key: &str) {
        if self.hash_ttl_keys.remove(key).is_some() {
            self.mem_sub(ttl_index_cost(key));
        }
    }

    /// Reap whatever has expired in `key`'s hash (an expiry-index hint
    /// passed). Returns `true` when something was removed. Local only.
    pub(super) fn reap_hash(&self, key: &str) -> bool {
        let now = Instant::now();
        let Some(mut guard) = self.hashes.get_mut(key) else {
            self.hash_index_drop(key);
            return false;
        };
        let before = guard.mem;
        let (removed_any, whole) = if guard.key_expired(now) {
            (true, true)
        } else {
            let freed = guard.purge_expired_fields(now);
            (freed > 0, guard.fields.is_empty())
        };
        let after = guard.mem;
        let deadline = guard.earliest_deadline();
        drop(guard);
        self.mem_sub(before.saturating_sub(after));
        if whole {
            let now = Instant::now();
            if let Some((_, old)) =
                self.hashes.remove_if(key, |_, h| h.key_expired(now) || h.fields.is_empty())
            {
                self.mem_sub(old.mem);
                self.hash_index_drop(key);
                super::lazy_drop(old);
            }
        } else {
            self.hash_index_set(key, deadline);
        }
        if removed_any {
            self.notify_write(key);
        }
        removed_any
    }

    /// Run one field mutation as a replicated write.
    ///
    /// `op` performs the local mutation and reports `(result, changed)`. On a
    /// standalone store it simply runs. With a [`Replicator`](super::Replicator)
    /// installed it runs **inside** the replicator's ordering slot for this
    /// field, and when it changed something the field's resulting state is
    /// published — so the local copy and what peers receive are always the
    /// same point in the per-field order. Returns `None` when the replicator
    /// skipped the write because a newer write for the field had already
    /// landed (last-writer-wins: this one lost before it ran).
    fn replicated_field_op<R>(
        &self,
        key: &str,
        field: &str,
        op: impl FnOnce() -> (R, bool),
    ) -> Option<R> {
        let Some(rep) = self.active_replicator() else {
            return Some(op().0);
        };
        let mut op = Some(op);
        let mut out = None;
        let mut apply = || {
            let (result, changed) = (op.take()?)();
            out = Some(result);
            changed.then(|| self.hash_field_state(key, field))
        };
        rep.replicate_hash_field(key, field, &mut apply);
        out
    }

    /// The current replicable state of one field (see [`HashFieldState`]).
    fn hash_field_state(&self, key: &str, field: &str) -> HashFieldState {
        let now = Instant::now();
        let remaining = |at: Option<Instant>| at.map(|at| at.saturating_duration_since(now));
        let Some(h) = self.hashes.get(key) else {
            return HashFieldState::deleted();
        };
        if h.key_expired(now) {
            return HashFieldState::deleted();
        }
        match h.live(field, now) {
            Some(f) => HashFieldState {
                value: Some(f.value.to_vec()),
                field_ttl: remaining(f.expires_at),
                hash_ttl: remaining(h.expires_at),
            },
            None => {
                HashFieldState { value: None, field_ttl: None, hash_ttl: remaining(h.expires_at) }
            }
        }
    }

    /// Refuse values a clustered hash field cannot replicate (see
    /// [`HashWriteError::TooLargeForCluster`]). Checked before anything is
    /// written, so a multi-field write is all-or-nothing on this account.
    fn check_hash_values(
        &self,
        lens: impl IntoIterator<Item = usize>,
    ) -> Result<(), HashWriteError> {
        let Some(rep) = self.active_replicator() else {
            return Ok(());
        };
        let Some(max) = rep.hash_field_value_limit() else {
            return Ok(());
        };
        match lens.into_iter().find(|len| *len > max) {
            Some(len) => Err(HashWriteError::TooLargeForCluster { len, max }),
            None => Ok(()),
        }
    }

    /// Reserve room for one field write to `key`.
    fn reserve_field(&self, key: &str, field: &str, value_len: usize) -> bool {
        let base = if self.hashes.contains_key(key) { 0 } else { hash_base_cost(key) };
        self.reserve(key, hash_field_cost(field, value_len) + base)
    }

    /// Run `read` on `key`'s live hash under the shard read lock, touching
    /// its LRU clock and counting the lookup as a keyspace hit or miss.
    /// `None` when there is no live hash; a key-expired one is reaped.
    fn hash_read<R>(&self, key: &str, read: impl FnOnce(&HashEntry, Instant) -> R) -> Option<R> {
        let now = Instant::now();
        let Some(h) = self.hashes.get(key) else {
            self.stats.miss();
            return None;
        };
        if h.key_expired(now) {
            drop(h);
            self.reap_hash(key);
            self.stats.miss();
            return None;
        }
        if !h.is_live(now) {
            self.stats.miss();
            return None;
        }
        self.stats.hit();
        h.touch(self.now_nanos());
        Some(read(&h, now))
    }

    // ── Writes ───────────────────────────────────────────────────

    /// Set one field (`HSET key field value`), clearing any field TTL.
    ///
    /// Returns `Ok(true)` if the field was created, `Ok(false)` if an
    /// existing field was overwritten.
    ///
    /// # Errors
    ///
    /// [`HashWriteError`] when the memory budget refuses the write (the hash
    /// is left untouched) or, in a cluster, the value is too large to
    /// replicate as a hash field.
    pub fn hset(&self, key: &str, field: &str, value: Vec<u8>) -> Result<bool, HashWriteError> {
        self.hset_many(key, &[(field, value.as_slice())]).map(|added| added == 1)
    }

    /// Set several fields (`HSET key f1 v1 f2 v2 ...`), clearing their TTLs.
    /// Returns how many fields were created.
    ///
    /// Value sizes are checked against the cluster limit before anything is
    /// written. The memory budget is checked per field: a refusal stops the
    /// write with the earlier fields already stored — the same best-effort
    /// contract as `MSET`.
    ///
    /// # Errors
    ///
    /// As [`hset`](Self::hset).
    pub fn hset_many(&self, key: &str, pairs: &[(&str, &[u8])]) -> Result<usize, HashWriteError> {
        self.check_hash_values(pairs.iter().map(|(_, v)| v.len()))?;
        let mut added = 0;
        for (field, value) in pairs {
            if !self.reserve_field(key, field, value.len()) {
                return Err(HashWriteError::OutOfMemory);
            }
            let created = self.replicated_field_op(key, field, || {
                let created = self
                    .hash_write(key, true, |h, now| {
                        h.put_field(field, Bytes::copy_from_slice(value), None, now).0
                    })
                    .unwrap_or(false);
                (created, true)
            });
            if created == Some(true) {
                added += 1;
            }
            self.notify_write(key);
        }
        Ok(added)
    }

    /// `HSETNX`: set `field` only if it does not exist (or has expired).
    /// Returns whether it was set.
    ///
    /// # Errors
    ///
    /// As [`hset`](Self::hset).
    pub fn hsetnx(&self, key: &str, field: &str, value: &[u8]) -> Result<bool, HashWriteError> {
        self.check_hash_values([value.len()])?;
        if self.hexists(key, field) {
            return Ok(false);
        }
        if !self.reserve_field(key, field, value.len()) {
            return Err(HashWriteError::OutOfMemory);
        }
        let set = self.replicated_field_op(key, field, || {
            let set = self
                .hash_write(key, true, |h, now| {
                    if h.live(field, now).is_some() {
                        false
                    } else {
                        h.put_field(field, Bytes::copy_from_slice(value), None, now);
                        true
                    }
                })
                .unwrap_or(false);
            (set, set)
        });
        let set = set == Some(true);
        if set {
            self.notify_write(key);
        }
        Ok(set)
    }

    /// Delete one field. Returns `true` if a live field was removed.
    pub fn hdel(&self, key: &str, field: &str) -> bool {
        self.hdel_many(key, &[field]) == 1
    }

    /// Delete several fields (`HDEL`). Returns how many live fields were
    /// removed; an emptied hash stops existing.
    pub fn hdel_many(&self, key: &str, fields: &[&str]) -> usize {
        let mut removed = 0;
        for field in fields {
            let gone = self.replicated_field_op(key, field, || {
                let gone = self
                    .hash_write(key, false, |h, _| h.take_field(field).map(|(old, _)| old))
                    .flatten()
                    .is_some_and(|old| {
                        super::lazy_drop(old);
                        true
                    });
                (gone, gone)
            });
            if gone == Some(true) {
                removed += 1;
            }
        }
        if removed > 0 {
            self.notify_write(key);
        }
        removed
    }

    /// `HINCRBY`: add `delta` to the integer in `field` (a missing field
    /// counts as `0`). The field keeps its TTL.
    ///
    /// # Errors
    ///
    /// `ERR hash value is not an integer`, the overflow error, or a
    /// [`HashWriteError`] message.
    pub fn hincr_by(&self, key: &str, field: &str, delta: i64) -> Result<i64, String> {
        // An i64 is at most 20 bytes of text.
        self.check_hash_values([20]).map_err(|e| e.to_string())?;
        if !self.reserve_field(key, field, 20) {
            return Err(super::OOM_ERROR.to_string());
        }
        let result = self.replicated_field_op(key, field, || {
            let r = self
                .hash_write(key, true, |h, now| {
                    let current = match h.live(field, now) {
                        Some(f) => std::str::from_utf8(&f.value)
                            .ok()
                            .and_then(|s| s.parse::<i64>().ok())
                            .ok_or_else(|| HASH_NOT_INTEGER.to_string())?,
                        None => 0,
                    };
                    let next =
                        current.checked_add(delta).ok_or_else(|| INCR_OVERFLOW.to_string())?;
                    let ttl = h.live(field, now).and_then(|f| f.expires_at);
                    h.put_field(field, Bytes::from(next.to_string().into_bytes()), ttl, now);
                    Ok(next)
                })
                .unwrap_or_else(|| Err(super::OOM_ERROR.to_string()));
            let changed = r.is_ok();
            (r, changed)
        });
        match result {
            Some(r) => {
                if r.is_ok() {
                    self.notify_write(key);
                }
                r
            }
            // Overtaken by a newer write for this field: report what is there.
            None => self
                .hget(key, field)
                .and_then(|v| std::str::from_utf8(&v).ok().and_then(|s| s.parse().ok()))
                .ok_or_else(|| HASH_NOT_INTEGER.to_string()),
        }
    }

    /// `HINCRBYFLOAT`: add `delta` to the float in `field` (a missing field
    /// counts as `0`) and return the stored text. The field keeps its TTL.
    ///
    /// The result is stored the way Redis prints it: plain decimal, no
    /// exponent, no trailing zeros, rounded to the precision a long double
    /// would print (so `0.1 + 0.2` is `0.3`, as in Redis).
    ///
    /// # Errors
    ///
    /// `ERR hash value is not a float`, the NaN/Infinity error, or a
    /// [`HashWriteError`] message.
    pub fn hincr_by_float(&self, key: &str, field: &str, delta: f64) -> Result<String, String> {
        if !delta.is_finite() {
            return Err(INCR_NAN_OR_INF.to_string());
        }
        self.check_hash_values([64]).map_err(|e| e.to_string())?;
        if !self.reserve_field(key, field, 64) {
            return Err(super::OOM_ERROR.to_string());
        }
        let result = self.replicated_field_op(key, field, || {
            let r = self
                .hash_write(key, true, |h, now| {
                    let current = match h.live(field, now) {
                        Some(f) => {
                            parse_float(&f.value).ok_or_else(|| HASH_NOT_FLOAT.to_string())?
                        }
                        None => 0.0,
                    };
                    let next = current + delta;
                    if !next.is_finite() {
                        return Err(INCR_NAN_OR_INF.to_string());
                    }
                    let text = format_float(next);
                    let ttl = h.live(field, now).and_then(|f| f.expires_at);
                    h.put_field(field, Bytes::from(text.clone().into_bytes()), ttl, now);
                    Ok(text)
                })
                .unwrap_or_else(|| Err(super::OOM_ERROR.to_string()));
            let changed = r.is_ok();
            (r, changed)
        });
        match result {
            Some(r) => {
                if r.is_ok() {
                    self.notify_write(key);
                }
                r
            }
            None => self
                .hget(key, field)
                .map(|v| String::from_utf8_lossy(&v).into_owned())
                .ok_or_else(|| HASH_NOT_FLOAT.to_string()),
        }
    }

    // ── Field expiry ─────────────────────────────────────────────

    /// `HEXPIRE`/`HPEXPIRE`/`HEXPIREAT`/`HPEXPIREAT`: give each of `fields`
    /// an expiry `remaining` from now, subject to `cond`. A zero `remaining`
    /// (an expire time of `0` or in the past) deletes the field instead.
    ///
    /// Returns one [`field_code`] per field, in order.
    pub fn hexpire(
        &self,
        key: &str,
        fields: &[&str],
        remaining: Duration,
        cond: ExpireCondition,
    ) -> Vec<i64> {
        let mut codes = Vec::with_capacity(fields.len());
        let mut deleted_any = false;
        for field in fields {
            let code = self.replicated_field_op(key, field, || {
                let code = self
                    .hash_write(key, false, |h, now| {
                        let Some(current) = h.live(field, now).map(|f| f.expires_at) else {
                            return field_code::NO_FIELD;
                        };
                        let deadline = now.checked_add(remaining);
                        if !cond.allows(current, deadline) {
                            return field_code::CONDITION_NOT_MET;
                        }
                        if remaining.is_zero() {
                            if let Some((old, _)) = h.take_field(field) {
                                super::lazy_drop(old);
                            }
                            return field_code::DELETED;
                        }
                        h.set_field_expiry(field, deadline);
                        field_code::UPDATED
                    })
                    .unwrap_or(field_code::NO_FIELD);
                let changed = matches!(code, field_code::UPDATED | field_code::DELETED);
                (code, changed)
            });
            let code = code.unwrap_or(field_code::UPDATED);
            deleted_any |= code == field_code::DELETED;
            codes.push(code);
        }
        if deleted_any {
            self.notify_write(key);
        }
        codes
    }

    /// `HPERSIST`: remove the expiry of each of `fields`. Returns one
    /// [`field_code`] per field: `-2` no such field, `-1` no expiry, `1`
    /// removed.
    pub fn hpersist(&self, key: &str, fields: &[&str]) -> Vec<i64> {
        fields
            .iter()
            .map(|field| {
                self.replicated_field_op(key, field, || {
                    let code = self
                        .hash_write(key, false, |h, now| match h.live(field, now) {
                            None => field_code::NO_FIELD,
                            Some(f) if f.expires_at.is_none() => field_code::NO_EXPIRY,
                            Some(_) => {
                                h.set_field_expiry(field, None);
                                field_code::UPDATED
                            }
                        })
                        .unwrap_or(field_code::NO_FIELD);
                    (code, code == field_code::UPDATED)
                })
                .unwrap_or(field_code::UPDATED)
            })
            .collect()
    }

    /// The remaining TTL of each of `fields` in milliseconds (`HPTTL`):
    /// `-2` no such field, `-1` no expiry.
    #[must_use]
    pub fn hpttl(&self, key: &str, fields: &[&str]) -> Vec<i64> {
        self.field_expiry_codes(key, fields, |at, now| {
            i64::try_from(at.saturating_duration_since(now).as_millis()).unwrap_or(i64::MAX)
        })
    }

    /// The absolute expiry of each of `fields` as Unix milliseconds
    /// (`HPEXPIRETIME`): `-2` no such field, `-1` no expiry.
    #[must_use]
    pub fn hexpiretime_ms(&self, key: &str, fields: &[&str]) -> Vec<i64> {
        self.field_expiry_codes(key, fields, |at, _| super::instant_to_unix_ms(at))
    }

    /// Shared body of the field-TTL readers.
    fn field_expiry_codes(
        &self,
        key: &str,
        fields: &[&str],
        render: impl Fn(Instant, Instant) -> i64,
    ) -> Vec<i64> {
        self.hash_read(key, |h, now| {
            fields
                .iter()
                .map(|field| match h.live(field, now) {
                    None => field_code::NO_FIELD,
                    Some(f) => f.expires_at.map_or(field_code::NO_EXPIRY, |at| render(at, now)),
                })
                .collect()
        })
        .unwrap_or_else(|| vec![field_code::NO_FIELD; fields.len()])
    }

    // ── Whole-key expiry on hashes ───────────────────────────────

    /// Set the whole-key expiry of `key`'s hash to `at`, or clear it with
    /// `None`. Returns whether a live hash existed (and, for `None`, had an
    /// expiry to clear). Every field is re-published so peers carry the new
    /// key expiry.
    pub(super) fn hash_set_key_expiry(&self, key: &str, at: Option<Instant>) -> bool {
        let changed = self
            .hash_write(key, false, |h, _| {
                if at.is_none() && h.expires_at.is_none() {
                    return false;
                }
                h.expires_at = at;
                true
            })
            .unwrap_or(false);
        if changed {
            if at.is_none() {
                // The hint may now be the (cleared) key deadline: rebuild it.
                let deadline = self.hashes.get(key).and_then(|h| h.earliest_deadline());
                self.hash_index_set(key, deadline);
            }
            self.republish_hash(key);
        }
        changed
    }

    /// The whole-key expiry of `key`'s live hash: `Some(None)` = no expiry,
    /// `None` = no live hash.
    #[allow(clippy::option_option, reason = "outer: a live hash exists; inner: it has an expiry")]
    pub(super) fn hash_key_expiry(&self, key: &str) -> Option<Option<Instant>> {
        let now = Instant::now();
        let h = self.hashes.get(key)?;
        h.is_live(now).then_some(h.expires_at)
    }

    /// Re-publish every live field of `key`'s hash through the replicator —
    /// after a whole-key expiry change, so peers learn the new key expiry
    /// carried in each field's state.
    fn republish_hash(&self, key: &str) {
        let Some(rep) = self.active_replicator() else {
            return;
        };
        let fields: Vec<String> = self
            .hashes
            .get(key)
            .map(|h| {
                let now = Instant::now();
                h.live_fields(now).map(|(name, _)| name.clone()).collect()
            })
            .unwrap_or_default();
        for field in fields {
            let mut apply = || Some(self.hash_field_state(key, &field));
            rep.replicate_hash_field(key, &field, &mut apply);
        }
    }

    /// The live field names of `key`'s hash, for publishing per-field
    /// tombstones when the whole hash is deleted.
    pub(super) fn hash_field_names(&self, key: &str) -> Vec<String> {
        let now = Instant::now();
        self.hashes
            .get(key)
            .map(|h| {
                h.fields.iter().filter(|(_, f)| f.is_live(now)).map(|(n, _)| n.clone()).collect()
            })
            .unwrap_or_default()
    }

    // ── Replication receive side ─────────────────────────────────

    /// Apply a replicated field state from a peer: set or delete `field` in
    /// `key`'s hash and adopt the key expiry it carries. **Local only** — never
    /// re-published. Returns `false` when a value could not be stored (memory
    /// budget).
    pub fn apply_hash_field_local(&self, key: &str, field: &str, state: HashFieldState) -> bool {
        let gone = state.field_ttl.is_some_and(|d| d.is_zero())
            || state.hash_ttl.is_some_and(|d| d.is_zero());
        match state.value {
            Some(value) if !gone => {
                if !self.reserve_field(key, field, value.len()) {
                    return false;
                }
                self.hash_write(key, true, |h, now| {
                    let field_at = state.field_ttl.and_then(|d| now.checked_add(d));
                    h.put_field(field, Bytes::from(value), field_at, now);
                    h.expires_at = state.hash_ttl.and_then(|d| now.checked_add(d));
                });
                self.notify_write(key);
                true
            }
            _ => {
                let removed = self
                    .hash_write(key, false, |h, _| h.take_field(field).map(|(old, _)| old))
                    .flatten();
                if let Some(old) = removed {
                    super::lazy_drop(old);
                    self.notify_write(key);
                }
                true
            }
        }
    }

    // ── Reads ────────────────────────────────────────────────────

    /// `HGET`: one live field's value. A cheap [`Bytes`] `Arc`-clone.
    #[must_use]
    pub fn hget(&self, key: &str, field: &str) -> Option<Bytes> {
        self.hash_read(key, |h, now| h.live(field, now).map(|f| f.value.clone())).flatten()
    }

    /// `HMGET`: each of `fields`' value, in order (`None` for a missing one).
    #[must_use]
    pub fn hmget(&self, key: &str, fields: &[&str]) -> Vec<Option<Bytes>> {
        self.hash_read(key, |h, now| {
            fields.iter().map(|f| h.live(f, now).map(|f| f.value.clone())).collect()
        })
        .unwrap_or_else(|| vec![None; fields.len()])
    }

    /// `HGETALL`: every live `(field, value)` pair.
    #[must_use]
    pub fn hgetall(&self, key: &str) -> Vec<(String, Bytes)> {
        self.hash_read(key, |h, now| {
            h.live_fields(now).map(|(k, f)| (k.clone(), f.value.clone())).collect()
        })
        .unwrap_or_default()
    }

    /// `HKEYS`: every live field name.
    #[must_use]
    pub fn hkeys(&self, key: &str) -> Vec<String> {
        self.hash_read(key, |h, now| h.live_fields(now).map(|(k, _)| k.clone()).collect())
            .unwrap_or_default()
    }

    /// `HVALS`: every live field value.
    #[must_use]
    pub fn hvals(&self, key: &str) -> Vec<Bytes> {
        self.hash_read(key, |h, now| h.live_fields(now).map(|(_, f)| f.value.clone()).collect())
            .unwrap_or_default()
    }

    /// `HLEN`: the number of live fields.
    #[must_use]
    pub fn hlen(&self, key: &str) -> usize {
        self.hash_read(key, HashEntry::live_len).unwrap_or(0)
    }

    /// `HEXISTS`: whether `field` is live.
    #[must_use]
    pub fn hexists(&self, key: &str, field: &str) -> bool {
        self.hash_read(key, |h, now| h.live(field, now).is_some()).unwrap_or(false)
    }

    /// `HSTRLEN`: the length of `field`'s value, `0` if it does not exist.
    #[must_use]
    pub fn hstrlen(&self, key: &str, field: &str) -> usize {
        self.hash_read(key, |h, now| h.live(field, now).map_or(0, |f| f.value.len())).unwrap_or(0)
    }

    /// `HSCAN`: every live field whose name matches `pattern` (`None` = all),
    /// with its value.
    ///
    /// Always the whole hash in one batch (the cursor that goes with it is
    /// `0`): a field's position in the table moves when the table grows, so a
    /// resumable cursor over it could skip fields. Redis does the same for
    /// small hashes and documents `COUNT` as a hint.
    #[must_use]
    pub fn hscan(&self, key: &str, pattern: Option<&str>) -> Vec<(String, Bytes)> {
        let mut pairs = self.hgetall(key);
        if let Some(p) = pattern.filter(|p| *p != "*") {
            pairs.retain(|(name, _)| super::glob::glob_match(p.as_bytes(), name.as_bytes()));
        }
        pairs
    }

    /// Whether `key` is a live hash.
    #[must_use]
    pub fn is_hash(&self, key: &str) -> bool {
        let now = Instant::now();
        let Some(h) = self.hashes.get(key) else {
            return false;
        };
        if h.key_expired(now) {
            drop(h);
            self.reap_hash(key);
            return false;
        }
        h.is_live(now)
    }

    /// Remove `key`'s hash entirely (local). Returns whether a live hash was
    /// removed.
    pub(super) fn hash_remove(&self, key: &str) -> bool {
        let Some((_, old)) = self.hashes.remove(key) else {
            return false;
        };
        self.mem_sub(old.mem);
        self.hash_index_drop(key);
        let live = old.is_live(Instant::now());
        super::lazy_drop(old);
        live
    }
}

/// Parse a stored or supplied float the way Redis does: a finite decimal or
/// exponent form, no surrounding whitespace, no `inf`/`nan`.
#[must_use]
pub fn parse_float(bytes: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(bytes).ok()?;
    if s.is_empty() || s.trim() != s {
        return None;
    }
    let v: f64 = s.parse().ok()?;
    v.is_finite().then_some(v)
}

/// Render a float the way Redis stores an `INCRBYFLOAT`/`HINCRBYFLOAT`
/// result: plain decimal, no exponent, no trailing zeros.
///
/// Redis computes in `long double` and prints 17 significant digits, so
/// binary rounding noise below that precision never shows (`0.1 + 0.2`
/// prints `0.3`). With `f64` the same is achieved by preferring the
/// 15-significant-digit rendering whenever it is the same number within a
/// few units in the last place, and the exact shortest rendering otherwise.
#[must_use]
pub fn format_float(v: f64) -> String {
    let rounded: f64 = format!("{v:.14e}").parse().unwrap_or(v);
    let close = (rounded - v).abs() <= v.abs() * 4.0 * f64::EPSILON;
    let shown = if close { rounded } else { v };
    // `Display` for f64 never uses an exponent and prints the shortest text
    // that round-trips. `-0` is printed as `0`.
    if shown == 0.0 { "0".to_string() } else { format!("{shown}") }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::{EvictionPolicy, StoreConfig};

    fn store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    #[test]
    fn float_formatting_matches_redis() {
        assert_eq!(format_float(10.5 + 0.1), "10.6");
        assert_eq!(format_float(0.1 + 0.2), "0.3");
        assert_eq!(format_float(5.0e3), "5000");
        assert_eq!(format_float(-0.0), "0");
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(parse_float(b"5.0e3"), Some(5000.0));
        assert_eq!(parse_float(b" 1"), None);
        assert_eq!(parse_float(b"inf"), None);
        assert_eq!(parse_float(b"nan"), None);
        assert_eq!(parse_float(b"abc"), None);
    }

    #[test]
    fn field_expiry_hides_the_field_then_the_hash() {
        let s = store();
        s.hset("h", "a", b"1".to_vec()).unwrap();
        s.hset("h", "b", b"2".to_vec()).unwrap();
        assert_eq!(s.hexpire("h", &["a"], Duration::from_millis(20), ExpireCondition::Always), [1]);
        assert_eq!(s.hlen("h"), 2);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(s.hget("h", "a"), None);
        assert!(!s.hexists("h", "a"));
        assert_eq!(s.hlen("h"), 1);
        assert_eq!(s.hkeys("h"), vec!["b".to_string()]);
        assert_eq!(s.hmget("h", &["a", "b"]), vec![None, Some(Bytes::from_static(b"2"))]);
        // The last field expiring removes the hash.
        s.hexpire("h", &["b"], Duration::from_millis(10), ExpireCondition::Always);
        std::thread::sleep(Duration::from_millis(30));
        assert!(!s.exists("h"));
        assert!(s.keys("*").is_empty(), "{:?}", s.keys("*"));
    }

    #[test]
    fn active_expiry_reclaims_expired_fields_and_their_memory() {
        let s = Store::new(StoreConfig { memory_limit: 0, ..StoreConfig::default() });
        for i in 0..100 {
            s.hset("h", &format!("f{i}"), vec![b'x'; 100]).unwrap();
        }
        s.hset("keep", "f", b"v".to_vec()).unwrap();
        let fields: Vec<String> = (0..100).map(|i| format!("f{i}")).collect();
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        s.hexpire("h", &refs, Duration::from_millis(10), ExpireCondition::Always);
        let peak = s.mem_used();
        std::thread::sleep(Duration::from_millis(30));
        s.active_expire_cycle(Duration::from_millis(50));
        assert!(!s.hashes.contains_key("h"), "the emptied hash must be reclaimed");
        assert!(s.mem_used() < peak / 10, "field memory must be credited back");
        assert_eq!(s.hget("keep", "f").as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn hexpire_conditions_follow_redis() {
        let s = store();
        s.hset("h", "f", b"v".to_vec()).unwrap();
        let ten = Duration::from_secs(10);
        let twenty = Duration::from_secs(20);
        // GT on a persistent field fails; LT succeeds (persistent = infinite).
        assert_eq!(s.hexpire("h", &["f"], ten, ExpireCondition::Gt), [0]);
        assert_eq!(s.hexpire("h", &["f"], twenty, ExpireCondition::Lt), [1]);
        assert_eq!(s.hexpire("h", &["f"], ten, ExpireCondition::Nx), [0]);
        assert_eq!(s.hexpire("h", &["f"], ten, ExpireCondition::Xx), [1]);
        assert_eq!(s.hexpire("h", &["f"], twenty, ExpireCondition::Gt), [1]);
        assert_eq!(s.hexpire("h", &["f"], ten, ExpireCondition::Gt), [0]);
        assert_eq!(s.hexpire("h", &["nope"], ten, ExpireCondition::Always), [-2]);
        assert_eq!(s.hexpire("missing", &["f"], ten, ExpireCondition::Always), [-2]);
        // Zero deletes the field.
        assert_eq!(s.hexpire("h", &["f"], Duration::ZERO, ExpireCondition::Always), [2]);
        assert!(!s.exists("h"));
    }

    #[test]
    fn httl_hpersist_and_hset_clearing_ttl() {
        let s = store();
        s.hset("h", "f", b"v".to_vec()).unwrap();
        assert_eq!(s.hpttl("h", &["f", "x"]), [-1, -2]);
        s.hexpire("h", &["f"], Duration::from_secs(100), ExpireCondition::Always);
        let ms = s.hpttl("h", &["f"])[0];
        assert!(ms > 99_000 && ms <= 100_000, "{ms}");
        assert!(s.hexpiretime_ms("h", &["f"])[0] > super::super::unix_ms_now());
        assert_eq!(s.hpersist("h", &["f", "x"]), [1, -2]);
        assert_eq!(s.hpersist("h", &["f"]), [-1]);
        // HINCRBY keeps a field TTL, HSET clears it.
        s.hexpire("h", &["n"], Duration::from_secs(100), ExpireCondition::Always);
        s.hset("h", "n", b"1".to_vec()).unwrap();
        s.hexpire("h", &["n"], Duration::from_secs(100), ExpireCondition::Always);
        assert_eq!(s.hincr_by("h", "n", 2), Ok(3));
        assert!(s.hpttl("h", &["n"])[0] > 0);
        s.hset("h", "n", b"1".to_vec()).unwrap();
        assert_eq!(s.hpttl("h", &["n"]), [-1]);
    }

    #[test]
    fn hincr_by_and_float_errors() {
        let s = store();
        assert_eq!(s.hincr_by("h", "n", 5), Ok(5));
        assert_eq!(s.hincr_by("h", "n", -7), Ok(-2));
        s.hset("h", "s", b"abc".to_vec()).unwrap();
        assert_eq!(s.hincr_by("h", "s", 1), Err(HASH_NOT_INTEGER.to_string()));
        s.hset("h", "big", i64::MAX.to_string().into_bytes()).unwrap();
        assert_eq!(s.hincr_by("h", "big", 1), Err(INCR_OVERFLOW.to_string()));
        assert_eq!(s.hincr_by_float("h", "f", 10.5), Ok("10.5".to_string()));
        assert_eq!(s.hincr_by_float("h", "f", 0.1), Ok("10.6".to_string()));
        assert_eq!(s.hincr_by_float("h", "s", 1.0), Err(HASH_NOT_FLOAT.to_string()));
        s.hset("h", "huge", b"1.7976931348623157e308".to_vec()).unwrap();
        assert_eq!(s.hincr_by_float("h", "huge", 1e308), Err(INCR_NAN_OR_INF.to_string()));
        // A failed increment on a fresh hash does not leave an empty hash.
        assert_eq!(s.hincr_by("fresh", "f", 1), Ok(1));
    }

    #[test]
    fn hsetnx_hstrlen_hmget_hdel_many() {
        let s = store();
        assert_eq!(s.hsetnx("h", "f", b"one"), Ok(true));
        assert_eq!(s.hsetnx("h", "f", b"two"), Ok(false));
        assert_eq!(s.hstrlen("h", "f"), 3);
        assert_eq!(s.hstrlen("h", "x"), 0);
        assert_eq!(s.hset_many("h", &[("a", b"1"), ("b", b"2"), ("f", b"3")]), Ok(2));
        assert_eq!(s.hdel_many("h", &["a", "b", "zz"]), 2);
        assert_eq!(s.hmget("h", &["f", "a"]), vec![Some(Bytes::from_static(b"3")), None]);
        assert_eq!(s.hmget("missing", &["f"]), vec![None]);
    }

    #[test]
    fn whole_key_expiry_applies_to_hashes() {
        let s = store();
        s.hset("h", "f", b"v".to_vec()).unwrap();
        assert!(s.expire("h", Duration::from_millis(20)));
        let ms = s.pttl("h").unwrap();
        assert!(ms > 0 && ms <= 20, "{ms}");
        std::thread::sleep(Duration::from_millis(40));
        assert!(!s.exists("h"));
        assert_eq!(s.hget("h", "f"), None);
        assert_eq!(s.pttl("h"), None);

        s.hset("p", "f", b"v".to_vec()).unwrap();
        assert!(s.expire("p", Duration::from_secs(10)));
        assert!(s.persist("p"));
        assert_eq!(s.pttl("p"), Some(-1));
    }

    #[test]
    fn memory_accounting_returns_to_zero() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            ..StoreConfig::default()
        });
        for i in 0..50 {
            s.hset("h", &format!("f{i}"), vec![b'v'; 10]).unwrap();
        }
        s.hexpire("h", &["f1", "f2"], Duration::from_secs(60), ExpireCondition::Always);
        s.expire("h", Duration::from_secs(60));
        assert!(s.mem_used() > 0);
        s.hdel_many("h", &["f0", "f1"]);
        s.hincr_by("h", "n", 1).unwrap();
        assert!(s.remove("h"));
        assert_eq!(s.mem_used(), 0, "every charged byte must be credited back");
    }

    #[test]
    fn hscan_filters_by_pattern() {
        let s = store();
        s.hset_many("h", &[("user:1", b"a"), ("user:2", b"b"), ("post:1", b"c")]).unwrap();
        let mut got: Vec<String> = s.hscan("h", Some("user:*")).into_iter().map(|p| p.0).collect();
        got.sort();
        assert_eq!(got, vec!["user:1", "user:2"]);
        assert_eq!(s.hscan("h", None).len(), 3);
    }

    #[test]
    fn hash_writes_bump_watch_versions_ttl_changes_do_not() {
        let s = store();
        let (v0, _) = s.wait_for_change("h", 0, Duration::ZERO).unwrap();
        s.hset("h", "f", b"v".to_vec()).unwrap();
        let (v1, value) = s.wait_for_change("h", v0, Duration::ZERO).unwrap();
        assert!(v1 > v0);
        assert_eq!(value, None, "a hash has no string value");
        s.hexpire("h", &["f"], Duration::from_secs(10), ExpireCondition::Always);
        assert!(s.wait_for_change("h", v1, Duration::ZERO).is_none(), "TTL-only change");
        s.hdel("h", "f");
        assert!(s.wait_for_change("h", v1, Duration::ZERO).is_some());
    }

    #[test]
    fn replicated_state_round_trips_through_apply() {
        let s = store();
        s.hset("h", "f", b"v".to_vec()).unwrap();
        s.hexpire("h", &["f"], Duration::from_secs(50), ExpireCondition::Always);
        s.expire("h", Duration::from_secs(100));
        let state = s.hash_field_state("h", "f");
        assert_eq!(state.value.as_deref(), Some(&b"v"[..]));

        let peer = store();
        assert!(peer.apply_hash_field_local("h", "f", state));
        assert_eq!(peer.hget("h", "f").as_deref(), Some(&b"v"[..]));
        let field_ms = peer.hpttl("h", &["f"])[0];
        assert!(field_ms > 49_000 && field_ms <= 50_000, "{field_ms}");
        let key_ms = peer.pttl("h").unwrap();
        assert!(key_ms > 99_000 && key_ms <= 100_000, "{key_ms}");

        assert!(peer.apply_hash_field_local("h", "f", HashFieldState::deleted()));
        assert!(!peer.exists("h"));
        assert_eq!(peer.mem_used(), 0);
    }
}
