//! String-key commands built on the core store: `GETDEL`, `GETEX`, the
//! conditional `SET ... IFEQ` (compare-and-set), and the expiry readers
//! behind `EXPIRETIME`/`PEXPIRETIME` and `SET ... KEEPTTL`.

use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{
    Entry, KeyUnavailable, Routed, Store, decompress_value, instant_to_unix_ms, ttl_index_cost,
};

/// The expiry a conditional `SET` leaves on the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetTtl {
    /// No expiry (a plain `SET` without `EX`/`PX`/...).
    Persist,
    /// Expire this long from now.
    After(Duration),
    /// Keep whatever expiry the key already has (`KEEPTTL`).
    Keep,
}

/// What `GETEX` does to the key's expiry after reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GetExpiry {
    /// Expire this long from now (`EX`/`PX`, or `EXAT`/`PXAT` in the future).
    After(Duration),
    /// Remove the expiry (`PERSIST`).
    Persist,
    /// The requested absolute time has already passed (`EXAT`/`PXAT` in the
    /// past): delete the key, as Redis does.
    Delete,
}

/// Outcome of a compare-and-set (`SET key value IFEQ expected`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetIfEqOutcome {
    /// The key held `expected`; it now holds the new value.
    Set,
    /// The key was absent, expired, or held something else. Nothing changed.
    NotSet,
    /// The memory budget refused the write; the old value is untouched.
    OutOfMemory,
    /// Clustered only: the key's owner could not be reached; the outcome is
    /// unknown (see [`super::SetNxOutcome::OwnerUnreachable`]).
    OwnerUnreachable,
}

impl Store {
    /// The logical (decompressed) value of a live entry equals `expected`.
    fn entry_equals(&self, entry: &Entry, expected: &[u8]) -> bool {
        if entry.compressed {
            decompress_value(&entry.data, self.config.compression.algo)
                .is_some_and(|plain| plain.as_ref() == expected)
        } else {
            entry.data.as_ref() == expected
        }
    }

    /// `GETDEL`: remove a string key and return the value it held.
    ///
    /// Locally the read and the removal are one step under the key's shard
    /// lock, so two concurrent `GETDEL`s cannot both get the value. In a
    /// cluster the local copy is first made current
    /// ([`prepare_update`](Self::prepare_update)) — a node catching up after a
    /// restart takes its peers' newest copy, and a large value held only by
    /// other nodes is installed from its replica set — and the removal is
    /// then published as a tombstone. Not atomic across nodes
    /// (last-writer-wins, like every write in that tier).
    ///
    /// # Errors
    ///
    /// [`KeyUnavailable`] when the key's current value could not be
    /// established; nothing was deleted.
    pub fn getdel(&self, key: &str) -> Result<Option<Bytes>, KeyUnavailable> {
        self.prepare_update_for("getdel", key)?;
        if let Some((_, old)) = self.data.remove_if(key, |_, e| !e.is_expired()) {
            self.mem_sub(old.mem_size);
            self.ttl_index_drop(key, old.expires_at);
            self.notify_write(key);
            if let Some(rep) = self.active_replicator() {
                // Publish-only: the local copy is already gone, and a value
                // recreated since must survive here (see `del_if_eq_local`).
                rep.replicate_remove_published(key.to_string());
            }
            self.stats.hit();
            return Ok(if old.compressed {
                decompress_value(&old.data, self.config.compression.algo)
            } else {
                Some(old.data)
            });
        }
        self.stats.miss();
        Ok(None)
    }

    /// `GETEX`: read a string key and change its expiry.
    ///
    /// Returns the value, or `None` (and changes nothing) when the key does
    /// not exist. The expiry change goes through the replicated paths
    /// (`expire`, `persist`, `remove`), so peers follow it. In a cluster the
    /// local copy is first made current
    /// ([`prepare_update`](Self::prepare_update)), so a TTL change on a large
    /// value this node did not hold reaches its replica set instead of being
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`KeyUnavailable`] when the key's current value could not be
    /// established; nothing was changed.
    pub fn getex(&self, key: &str, expiry: GetExpiry) -> Result<Option<Bytes>, KeyUnavailable> {
        self.prepare_update_for("getex", key)?;
        let Some(value) = self.get(key) else {
            self.stats.miss();
            return Ok(None);
        };
        self.stats.hit();
        match expiry {
            GetExpiry::After(ttl) => {
                if let Some(rep) = self.active_replicator() {
                    rep.replicate_expire(key, ttl);
                } else {
                    self.expire_local(key, ttl);
                }
            }
            GetExpiry::Persist => {
                self.persist_string(key);
            }
            GetExpiry::Delete => {
                self.remove(key);
            }
        }
        Ok(Some(value))
    }

    /// The expiry of a live **string** key: `Some(None)` = no expiry, `None`
    /// = no live string key. Backs `SET ... KEEPTTL`.
    #[must_use]
    #[allow(
        clippy::option_option,
        reason = "outer: a live string key exists; inner: it has an expiry"
    )]
    pub fn string_expiry(&self, key: &str) -> Option<Option<Instant>> {
        let entry = self.data.get(key)?;
        (!entry.is_expired()).then_some(entry.expires_at)
    }

    /// `PEXPIRETIME`: the key's absolute expiry in Unix milliseconds,
    /// `Some(-1)` for a key with no expiry, `None` for a missing key. A hash
    /// key reports its whole-key expiry.
    #[must_use]
    pub fn expire_time_ms(&self, key: &str) -> Option<i64> {
        let at = match self.string_expiry(key) {
            Some(at) => at,
            None => self.hash_key_expiry(key)?,
        };
        Some(at.map_or(-1, instant_to_unix_ms))
    }

    /// Compare-and-set: replace `key`'s value with `value` only while it
    /// currently equals `expected` (Valkey's `SET key value IFEQ expected`).
    ///
    /// The lock-extension companion to [`set_nx`](Self::set_nx) and
    /// [`del_if_eq`](Self::del_if_eq): a holder refreshes its lock with
    /// `set_if_eq(lock, token, token, SetTtl::After(ttl))`, which fails
    /// instead of extending a lock someone else has taken over since.
    ///
    /// Clustered, it is decided by the key's owner node like `SETNX` and
    /// `DELIFEQ` ([`Replicator::route_set_if_eq`](super::Replicator::route_set_if_eq)),
    /// with the same membership-change limits.
    pub fn set_if_eq(
        &self,
        key: String,
        expected: &[u8],
        value: Vec<u8>,
        ttl: SetTtl,
    ) -> SetIfEqOutcome {
        if let Some(rep) = self.active_replicator() {
            match rep.route_set_if_eq(&key, expected, &value, ttl) {
                Routed::Local => {}
                Routed::Owner(outcome) => return outcome,
                Routed::OwnerUnreachable => return SetIfEqOutcome::OwnerUnreachable,
            }
        }
        self.set_if_eq_local(key, expected, value, ttl)
    }

    /// [`set_if_eq`](Self::set_if_eq) decided on **this node's** map — what
    /// the owner runs for a routed request. The new value is still published
    /// through the installed replicator.
    pub fn set_if_eq_local(
        &self,
        key: String,
        expected: &[u8],
        value: Vec<u8>,
        ttl: SetTtl,
    ) -> SetIfEqOutcome {
        // Peek: answer a mismatch without reserving memory, and learn what
        // the replaced entry is charged so only the growth is reserved.
        let (old_size, old_had_ttl) = match self.data.get(&key) {
            Some(e) if !e.is_expired() && self.entry_equals(&e, expected) => {
                (e.mem_size, e.expires_at.is_some())
            }
            _ => return SetIfEqOutcome::NotSet,
        };
        let published_value = self.has_replicator().then(|| value.clone());
        let (data, compressed) = self.encode_value(value);
        let mut new_entry = Entry::new(data, key.len(), compressed, self.now_nanos());
        let new_size = new_entry.mem_size;
        let index_growth =
            if matches!(ttl, SetTtl::After(_)) && !old_had_ttl { ttl_index_cost(&key) } else { 0 };
        // No shard guard held: reserving may evict from any shard.
        if !self.reserve(&key, (new_size + index_growth).saturating_sub(old_size)) {
            return SetIfEqOutcome::OutOfMemory;
        }
        // The authoritative compare and the replace, under one write lock.
        let Some(mut guard) = self.data.get_mut(&key) else {
            return SetIfEqOutcome::NotSet;
        };
        if guard.is_expired() || !self.entry_equals(&guard, expected) {
            return SetIfEqOutcome::NotSet;
        }
        new_entry.expires_at = match ttl {
            SetTtl::Persist => None,
            SetTtl::After(d) => Instant::now().checked_add(d),
            SetTtl::Keep => guard.expires_at,
        };
        let deadline = new_entry.expires_at;
        let old = std::mem::replace(&mut *guard, new_entry);
        drop(guard);
        self.mem_add(new_size);
        self.mem_sub(old.mem_size);
        if deadline != old.expires_at {
            self.ttl_index_put(&key, deadline);
        }
        self.notify_write(&key);
        if let Some(value) = published_value {
            self.replicate_published_value(&key, value, deadline);
        }
        SetIfEqOutcome::Set
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::{CompressionAlgo, CompressionConfig, StoreConfig};

    fn store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    #[test]
    fn getdel_returns_and_removes() {
        let s = store();
        s.set("k".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        assert_eq!(s.getdel("k").unwrap().as_deref(), Some(&b"v"[..]));
        assert_eq!(s.get("k"), None);
        assert_eq!(s.getdel("k"), Ok(None));
        assert_eq!(s.mem_used(), 0);
    }

    #[test]
    fn getex_changes_the_expiry() {
        let s = store();
        s.set("k".into(), b"v".to_vec(), None);
        assert_eq!(
            s.getex("k", GetExpiry::After(Duration::from_secs(30))).unwrap().as_deref(),
            Some(&b"v"[..])
        );
        let ms = s.pttl("k").unwrap();
        assert!(ms > 29_000 && ms <= 30_000);
        s.getex("k", GetExpiry::Persist).unwrap();
        assert_eq!(s.pttl("k"), Some(-1));
        assert_eq!(s.getex("k", GetExpiry::Delete).unwrap().as_deref(), Some(&b"v"[..]));
        assert_eq!(s.get("k"), None);
        assert_eq!(s.getex("missing", GetExpiry::Persist), Ok(None));
    }

    #[test]
    fn set_if_eq_compares_then_swaps() {
        let s = store();
        assert_eq!(
            s.set_if_eq("k".into(), b"a", b"b".to_vec(), SetTtl::Persist),
            SetIfEqOutcome::NotSet,
            "absent keys are never created"
        );
        s.set("k".into(), b"a".to_vec(), Some(Duration::from_secs(60)));
        assert_eq!(
            s.set_if_eq("k".into(), b"x", b"b".to_vec(), SetTtl::Persist),
            SetIfEqOutcome::NotSet
        );
        assert_eq!(s.set_if_eq("k".into(), b"a", b"b".to_vec(), SetTtl::Keep), SetIfEqOutcome::Set);
        assert_eq!(s.get("k").as_deref(), Some(&b"b"[..]));
        assert!(s.pttl("k").unwrap() > 59_000, "KEEPTTL keeps the expiry");
        assert_eq!(
            s.set_if_eq("k".into(), b"b", b"c".to_vec(), SetTtl::Persist),
            SetIfEqOutcome::Set
        );
        assert_eq!(s.pttl("k"), Some(-1));
        assert_eq!(
            s.set_if_eq("k".into(), b"c", b"d".to_vec(), SetTtl::After(Duration::from_secs(5))),
            SetIfEqOutcome::Set
        );
        assert!(s.pttl("k").unwrap() > 4_000);
    }

    #[test]
    fn set_if_eq_compares_the_logical_value_when_compressed() {
        let s = Store::new(StoreConfig {
            compression: CompressionConfig { algo: CompressionAlgo::Zstd, level: 3, min_size: 8 },
            ..StoreConfig::default()
        });
        let big = vec![b'a'; 4096];
        s.set("k".into(), big.clone(), None);
        assert_eq!(
            s.set_if_eq("k".into(), &big, b"n".to_vec(), SetTtl::Persist),
            SetIfEqOutcome::Set
        );
        assert_eq!(s.get("k").as_deref(), Some(&b"n"[..]));
    }

    #[test]
    fn expire_time_reports_absolute_unix_ms() {
        let s = store();
        assert_eq!(s.expire_time_ms("missing"), None);
        s.set("k".into(), b"v".to_vec(), None);
        assert_eq!(s.expire_time_ms("k"), Some(-1));
        s.expire("k", Duration::from_secs(100));
        let at = s.expire_time_ms("k").unwrap();
        let now = super::super::unix_ms_now();
        assert!(at > now + 99_000 && at <= now + 100_001, "{at} vs {now}");
    }
}
