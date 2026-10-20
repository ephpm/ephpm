//! Redis command parsing and dispatch.
//!
//! Translates parsed RESP frames into operations on the [`Store`].

use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use crate::resp::Frame;
use crate::store::{OOM_ERROR, OWNER_UNREACHABLE_ERROR, SetNxOutcome, Store};

/// Error frame for a write refused by the memory budget (Redis `-OOM`).
fn oom() -> Frame {
    Frame::error(OOM_ERROR)
}

/// Early-return an error frame if `argv` has fewer than `n` elements.
macro_rules! check_args {
    ($cmd:expr, $argv:expr, $n:expr) => {
        if let Err(e) = require_args($cmd, $argv, $n) {
            return e;
        }
    };
}

/// Reply for a key, hash field or `KEYS` pattern that is not valid UTF-8.
///
/// The store keys entries by `String`, so a non-UTF-8 name cannot be held
/// faithfully. Converting it lossily (as this module once did) maps every
/// invalid byte to U+FFFD, so distinct binary keys silently shared one entry
/// (KV stress report F-18). Refusing them is the honest alternative to
/// storing them wrong. Values stay binary-safe.
pub const NON_UTF8_KEY_ERROR: &str =
    "ERR invalid key: ePHPm KV keys and hash fields must be valid UTF-8";

/// Convert a key argument, early-returning [`NON_UTF8_KEY_ERROR`] if it is
/// not UTF-8.
macro_rules! key {
    ($arg:expr) => {
        match std::str::from_utf8($arg) {
            Ok(k) => k.to_owned(),
            Err(_) => return Frame::error(NON_UTF8_KEY_ERROR),
        }
    };
}

/// Early-return [`NON_UTF8_KEY_ERROR`] if any of the given key arguments is
/// not UTF-8. Checked before a multi-key command touches the store, so a bad
/// key changes nothing.
macro_rules! check_keys {
    ($args:expr) => {
        let mut args = $args;
        if args.any(|k: &&[u8]| std::str::from_utf8(k).is_err()) {
            return Frame::error(NON_UTF8_KEY_ERROR);
        }
    };
}

/// Execute a command frame against the store and return the response frame.
pub fn dispatch(store: &Arc<Store>, frame: &Frame) -> Frame {
    let args = match frame {
        Frame::Array(items) => items,
        // Inline commands: single simple string like "PING".
        Frame::Simple(s) => {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.is_empty() {
                return Frame::error("ERR empty command");
            }
            return dispatch_inline(store, &parts);
        }
        _ => return Frame::error("ERR invalid command format"),
    };

    if args.is_empty() {
        return Frame::error("ERR empty command");
    }

    let cmd = match &args[0] {
        Frame::Bulk(b) => String::from_utf8_lossy(b).to_ascii_uppercase(),
        Frame::Simple(s) => s.to_ascii_uppercase(),
        _ => return Frame::error("ERR invalid command name"),
    };

    let params: Vec<&[u8]> = args
        .iter()
        .skip(1)
        .filter_map(|f| match f {
            Frame::Bulk(b) => Some(b.as_ref()),
            _ => None,
        })
        .collect();

    debug!(cmd = %cmd, argc = params.len(), "executing command");

    execute(store, &cmd, &params)
}

/// Handle inline (non-RESP) commands.
fn dispatch_inline(store: &Arc<Store>, parts: &[&str]) -> Frame {
    let cmd = parts[0].to_ascii_uppercase();
    let argv: Vec<&[u8]> = parts[1..].iter().map(|s| s.as_bytes()).collect();
    execute(store, &cmd, &argv)
}

/// Core command router.
#[allow(clippy::too_many_lines)]
fn execute(store: &Arc<Store>, cmd: &str, argv: &[&[u8]]) -> Frame {
    match cmd {
        // ── Connection ───────────────────────────────────────────
        "PING" => {
            if argv.is_empty() {
                Frame::Simple("PONG".into())
            } else {
                Frame::bulk(argv[0].to_vec())
            }
        }
        "ECHO" => {
            if argv.is_empty() {
                return Frame::error("ERR wrong number of arguments for 'echo' command");
            }
            Frame::bulk(argv[0].to_vec())
        }
        "SELECT" => {
            // Single-server: only DB 0 supported.
            let db = str_arg(argv, 0).unwrap_or_default();
            if db == "0" { Frame::ok() } else { Frame::error("ERR DB index is out of range") }
        }
        "QUIT" => Frame::ok(),
        "COMMAND" => {
            // Minimal stub: return empty array.
            Frame::Array(vec![])
        }

        // ── String operations ────────────────────────────────────
        "GET" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            // Through the cluster: a large value held by other nodes only is
            // fetched from them rather than answered `nil`.
            match store.get_through(&key) {
                Some(v) => Frame::bulk(v),
                None => Frame::Null,
            }
        }
        "SET" => cmd_set(store, argv),
        "SETEX" => {
            check_args!(cmd, argv, 3);
            let key = key!(argv[0]);
            let ttl = match expire_arg(argv[1], TtlUnit::Seconds, "setex") {
                Ok(ttl) => ttl,
                Err(f) => return f,
            };
            let val = argv[2].to_vec();
            if store.set(key, val, Some(ttl)) { Frame::ok() } else { oom() }
        }
        "MGET" => {
            if argv.is_empty() {
                return Frame::error("ERR wrong number of arguments for 'mget' command");
            }
            check_keys!(argv.iter());
            let results: Vec<Frame> = argv
                .iter()
                .map(|k| {
                    let key = str_from(k);
                    match store.get_through(&key) {
                        Some(v) => Frame::bulk(v),
                        None => Frame::Null,
                    }
                })
                .collect();
            Frame::Array(results)
        }
        "MSET" => {
            if argv.len() < 2 || !argv.len().is_multiple_of(2) {
                return Frame::error("ERR wrong number of arguments for 'mset' command");
            }
            // Best-effort under memory pressure: a refused pair returns `-OOM`
            // and stops, so earlier pairs may already be stored. Redis rejects
            // the whole MSET up front; we do not pre-check total size, and this
            // only bites under `noeviction` at the limit.
            //
            // Keys are checked up front, so a bad key writes nothing at all.
            check_keys!(argv.iter().step_by(2));
            for pair in argv.chunks(2) {
                let key = str_from(pair[0]);
                if !store.set(key, pair[1].to_vec(), None) {
                    return oom();
                }
            }
            Frame::ok()
        }
        "SETNX" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            // Atomic check-and-set under the per-key shard lock — the
            // exists/set pair this used to do was racy under concurrent
            // callers. The outcome distinguishes "key exists" (`:0`) from a
            // memory refusal (`-OOM`) — the plain boolean conflated them (#477).
            match store.set_nx_outcome(key, argv[1].to_vec(), None) {
                SetNxOutcome::Inserted => Frame::integer(1),
                SetNxOutcome::Exists => Frame::integer(0),
                SetNxOutcome::OutOfMemory => oom(),
                // Clustered: the key's owner could not be reached, so nothing
                // was written anywhere. Not `:0` — that would claim it exists.
                SetNxOutcome::OwnerUnreachable => Frame::error(OWNER_UNREACHABLE_ERROR),
            }
        }
        "INCR" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.incr_by(&key, 1) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "DECR" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.incr_by(&key, -1) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "INCRBY" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let delta = match parse_i64(argv[1]) {
                Ok(v) => v,
                Err(f) => return f,
            };
            match store.incr_by(&key, delta) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "DECRBY" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            // `-i64::MIN` does not exist: negating it would panic in a debug
            // build and wrap to `i64::MIN` (an *increment*) in release.
            let delta = match parse_i64(argv[1]).map(i64::checked_neg) {
                Ok(Some(v)) => v,
                Ok(None) => return Frame::error("ERR decrement would overflow"),
                Err(f) => return f,
            };
            match store.incr_by(&key, delta) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "APPEND" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            match store.append(&key, argv[1]) {
                Some(new_len) => Frame::integer(i64::try_from(new_len).unwrap_or(i64::MAX)),
                None => oom(),
            }
        }
        "STRLEN" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.get(&key) {
                Some(v) => Frame::integer(i64::try_from(v.len()).unwrap_or(i64::MAX)),
                None => Frame::integer(0),
            }
        }
        "GETSET" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let old = store.get(&key);
            if !store.set(key, argv[1].to_vec(), None) {
                return oom();
            }
            match old {
                Some(v) => Frame::bulk(v),
                None => Frame::Null,
            }
        }

        // ── Key operations ───────────────────────────────────────
        "DEL" => {
            if argv.is_empty() {
                return Frame::error("ERR wrong number of arguments for 'del' command");
            }
            check_keys!(argv.iter());
            let removed: i64 = argv
                .iter()
                .filter(|k| store.remove(&str_from(k)))
                .count()
                .try_into()
                .unwrap_or(i64::MAX);
            Frame::integer(removed)
        }
        "DELIFEQ" => {
            // Compare-and-delete: `DELIFEQ key token` deletes `key` only while
            // its current value equals `token`, returning :1 if it did and :0
            // otherwise. This is the atomic release primitive for KV locks —
            // the delete-side companion to SETNX. It is deliberately its own
            // command rather than a `DEL key IFEQ token` variant, because DEL
            // is variadic over keys and a positional `IFEQ` marker would be
            // ambiguous with a key literally named "IFEQ".
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            // Clustered: decided by the key's owner node. An unreachable owner is
            // an error, not :0 — :0 would claim the token did not match.
            match store.try_del_if_eq(&key, argv[1]) {
                Ok(deleted) => Frame::integer(i64::from(deleted)),
                Err(_) => Frame::error(OWNER_UNREACHABLE_ERROR),
            }
        }
        "EXISTS" => {
            if argv.is_empty() {
                return Frame::error("ERR wrong number of arguments for 'exists' command");
            }
            check_keys!(argv.iter());
            let count: i64 = argv
                .iter()
                .filter(|k| store.exists(&str_from(k)))
                .count()
                .try_into()
                .unwrap_or(i64::MAX);
            Frame::integer(count)
        }
        "EXPIRE" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let ttl = match expire_arg(argv[1], TtlUnit::Seconds, "expire") {
                Ok(ttl) => ttl,
                Err(f) => return f,
            };
            let ok = store.expire(&key, ttl);
            Frame::integer(i64::from(ok))
        }
        "PEXPIRE" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let ttl = match expire_arg(argv[1], TtlUnit::Millis, "pexpire") {
                Ok(ttl) => ttl,
                Err(f) => return f,
            };
            let ok = store.expire(&key, ttl);
            Frame::integer(i64::from(ok))
        }
        "TTL" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.pttl(&key) {
                Some(ms) if ms >= 0 => Frame::integer(ms / 1000),
                Some(ms) => Frame::integer(ms), // -1 or -2
                None => Frame::integer(-2),
            }
        }
        "PTTL" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.pttl(&key) {
                Some(ms) => Frame::integer(ms),
                None => Frame::integer(-2),
            }
        }
        "PERSIST" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            Frame::integer(i64::from(store.persist(&key)))
        }
        "TYPE" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            if store.is_hash(&key) {
                Frame::Simple("hash".into())
            } else if store.exists(&key) {
                Frame::Simple("string".into())
            } else {
                Frame::Simple("none".into())
            }
        }
        "RENAME" => {
            check_args!(cmd, argv, 2);
            let old_key = key!(argv[0]);
            let new_key = key!(argv[1]);
            match store.get(&old_key) {
                Some(val) => {
                    // Preserve TTL if present
                    let ttl = store.pttl(&old_key).and_then(|ms| {
                        if ms > 0 {
                            Some(Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX)))
                        } else {
                            None
                        }
                    });
                    // Convert Bytes → Vec<u8> at the write boundary
                    // (Store::set still takes an owned Vec). RENAME is
                    // cold enough that this extra copy is fine.
                    if !store.set(new_key, val.to_vec(), ttl) {
                        // The old key is still intact — the set that would
                        // replace it was refused, so leave RENAME a no-op.
                        return oom();
                    }
                    store.remove(&old_key);
                    Frame::ok()
                }
                None => Frame::error("ERR no such key"),
            }
        }
        "KEYS" => {
            check_args!(cmd, argv, 1);
            let pattern = key!(argv[0]);
            let keys = store.keys(&pattern);
            Frame::Array(keys.into_iter().map(|k| Frame::bulk(k.into_bytes())).collect())
        }
        "DBSIZE" => Frame::integer(i64::try_from(store.len()).unwrap_or(i64::MAX)),
        "FLUSHDB" | "FLUSHALL" => {
            store.flush();
            Frame::ok()
        }

        // ── Hash operations ──────────────────────────────────────
        "HSET" => {
            if argv.len() < 3 || argv.len().is_multiple_of(2) {
                return Frame::error("ERR wrong number of arguments for 'hset' command");
            }
            let key = key!(argv[0]);
            check_keys!(argv[1..].iter().step_by(2));
            let mut added = 0i64;
            // Under memory pressure a refused field returns `-OOM` and stops,
            // so earlier fields may already be stored — the same best-effort
            // contract as MSET.
            for pair in argv[1..].chunks(2) {
                let field = str_from(pair[0]);
                let value = pair[1].to_vec();
                match store.hset(&key, &field, value) {
                    Some(true) => added += 1,
                    Some(false) => {}
                    None => return oom(),
                }
            }
            Frame::integer(added)
        }
        "HGET" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            match store.hget(&key, &field) {
                Some(v) => Frame::bulk(v),
                None => Frame::Null,
            }
        }
        "HDEL" => {
            if argv.len() < 2 {
                return Frame::error("ERR wrong number of arguments for 'hdel' command");
            }
            let key = key!(argv[0]);
            check_keys!(argv[1..].iter());
            let removed: i64 = argv[1..]
                .iter()
                .filter(|f| store.hdel(&key, &str_from(f)))
                .count()
                .try_into()
                .unwrap_or(i64::MAX);
            Frame::integer(removed)
        }
        "HGETALL" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            let pairs = store.hgetall(&key);
            let mut frames = Vec::with_capacity(pairs.len() * 2);
            for (field, value) in pairs {
                frames.push(Frame::bulk(field.into_bytes()));
                frames.push(Frame::bulk(value));
            }
            Frame::Array(frames)
        }
        "HKEYS" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            let keys = store.hkeys(&key);
            Frame::Array(keys.into_iter().map(|k| Frame::bulk(k.into_bytes())).collect())
        }
        "HVALS" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            let vals = store.hvals(&key);
            Frame::Array(vals.into_iter().map(Frame::bulk).collect())
        }
        "HLEN" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            Frame::integer(i64::try_from(store.hlen(&key)).unwrap_or(i64::MAX))
        }
        "HEXISTS" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            Frame::integer(i64::from(store.hexists(&key, &field)))
        }

        // ── Server info ──────────────────────────────────────────
        "INFO" => {
            let info = format!(
                "# Server\r\nredis_version:7.0.0\r\nredis_mode:embedded\r\ntcp_port:6379\r\n# Memory\r\nused_memory:{}\r\n",
                store.mem_used()
            );
            Frame::bulk(info.into_bytes())
        }

        _ => Frame::error(format!("ERR unknown command '{cmd}'")),
    }
}

// ── Helpers ──────────────────────────────────────────────────────

/// Execute the SET command with options: SET key value [EX seconds|PX ms] [NX|XX] [GET].
/// `EXAT`/`PXAT` are not implemented (they return `ERR syntax error`). As in
/// Redis, a non-integer `EX`/`PX` value is `ERR value is not an integer or out
/// of range`, and a non-positive or overflowing one is `ERR invalid expire time
/// in 'set' command`; giving both `EX` and `PX` is `ERR syntax error`.
fn cmd_set(store: &Arc<Store>, argv: &[&[u8]]) -> Frame {
    if argv.len() < 2 {
        return Frame::error("ERR wrong number of arguments for 'set' command");
    }

    let key = key!(argv[0]);
    let val = argv[1].to_vec();
    let mut ttl: Option<Duration> = None;
    let mut nx = false;
    let mut xx = false;
    let mut get = false;

    let mut i = 2;
    while i < argv.len() {
        let opt = str_from(argv[i]).to_uppercase();
        match opt.as_str() {
            "EX" | "PX" => {
                if i + 1 >= argv.len() || ttl.is_some() {
                    return Frame::error("ERR syntax error");
                }
                let unit = if opt == "EX" { TtlUnit::Seconds } else { TtlUnit::Millis };
                match expire_arg(argv[i + 1], unit, "set") {
                    Ok(d) => ttl = Some(d),
                    Err(f) => return f,
                }
                i += 2;
            }
            "NX" => {
                nx = true;
                i += 1;
            }
            "XX" => {
                xx = true;
                i += 1;
            }
            "GET" => {
                get = true;
                i += 1;
            }
            _ => {
                return Frame::error("ERR syntax error");
            }
        }
    }

    if nx && xx {
        return Frame::error("ERR NX and XX options at the same time are not compatible");
    }

    // NX path: route through the atomic primitive so concurrent
    // SET ... NX callers see exactly one winner. Note: the NX + GET
    // combo still has a small window (we re-fetch the existing value
    // after losing the race) — Redis itself returns the value as it
    // was *before* our attempt, but exposing that requires a
    // set-and-return-old primitive we don't have. Acceptable: NX+GET
    // is rarely used and the value fetched here is still consistent
    // with what's stored after the call.
    if nx {
        return match store.set_nx_outcome(key.clone(), val, ttl) {
            // A memory refusal is `-OOM`, distinct from the `nil` that means
            // "the key already existed" (#477).
            SetNxOutcome::OutOfMemory => oom(),
            // Clustered: the key's owner could not be reached, so nothing was
            // written anywhere. Not `nil` — that would claim the key exists.
            SetNxOutcome::OwnerUnreachable => Frame::error(OWNER_UNREACHABLE_ERROR),
            SetNxOutcome::Inserted => {
                if get {
                    // NX+GET on a successful insert: no prior value.
                    Frame::Null
                } else {
                    Frame::ok()
                }
            }
            SetNxOutcome::Exists => {
                if get {
                    match store.get(&key) {
                        Some(v) => Frame::bulk(v),
                        None => Frame::Null,
                    }
                } else {
                    Frame::Null
                }
            }
        };
    }

    let old = store.get(&key);

    // XX: Set only if the key exists
    if xx && old.is_none() {
        return Frame::Null;
    }

    if !store.set(key, val, ttl) {
        return oom();
    }

    if get {
        match old {
            Some(v) => Frame::bulk(v),
            None => Frame::Null,
        }
    } else {
        Frame::ok()
    }
}

/// The unit of an expire argument (`EX`/`EXPIRE` vs `PX`/`PEXPIRE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlUnit {
    /// Seconds (`EX`, `SETEX`, `EXPIRE`).
    Seconds,
    /// Milliseconds (`PX`, `PEXPIRE`).
    Millis,
}

/// Turn a relative expire `amount` into a TTL the store can hold, or `None`
/// when it is out of range.
///
/// Mirrors Redis's `getExpireMillisecondsOrReply`: the amount must be
/// positive, converting it to milliseconds must not overflow, and the
/// resulting absolute deadline (Unix milliseconds) must fit in an `i64`. On
/// top of that the deadline must be representable as an [`Instant`] — the
/// store computes `Instant::now() + ttl`, which **panics** on overflow (KV
/// stress report F-14: `SET k v EX 9223372036854775807` killed the
/// connection's task). Every entry point that takes a caller-supplied TTL
/// (RESP and the PHP bridge) must go through this before reaching the store.
///
/// [`Instant`]: std::time::Instant
#[must_use]
pub fn checked_ttl(amount: i64, unit: TtlUnit) -> Option<Duration> {
    if amount <= 0 {
        return None;
    }
    let ms = match unit {
        TtlUnit::Seconds => amount.checked_mul(1000)?,
        TtlUnit::Millis => amount,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    now_ms.checked_add(ms)?;
    let ttl = Duration::from_millis(u64::try_from(ms).ok()?);
    std::time::Instant::now().checked_add(ttl)?;
    Some(ttl)
}

/// Parse an expire argument for `cmd` with Redis's error replies: a
/// non-integer is `ERR value is not an integer or out of range`, a
/// non-positive or overflowing one `ERR invalid expire time in '<cmd>'
/// command`.
fn expire_arg(arg: &[u8], unit: TtlUnit, cmd: &str) -> Result<Duration, Frame> {
    let amount = parse_i64(arg)?;
    checked_ttl(amount, unit)
        .ok_or_else(|| Frame::error(format!("ERR invalid expire time in '{cmd}' command")))
}

/// Parse a byte slice as a string (lossy UTF-8).
fn str_from(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Get argument at index as a string, or default to "0".
fn str_arg(argv: &[&[u8]], idx: usize) -> Option<String> {
    argv.get(idx).map(|b| str_from(b))
}

/// Parse a byte slice as i64.
fn parse_i64(b: &[u8]) -> Result<i64, Frame> {
    let s = str_from(b);
    s.parse::<i64>().map_err(|_| Frame::error("ERR value is not an integer or out of range"))
}

/// Return an error frame if `argv` doesn't have at least `n` elements.
/// Uses the `?` operator trick — returns `Frame` on error.
fn require_args(cmd: &str, argv: &[&[u8]], n: usize) -> Result<(), Frame> {
    if argv.len() < n {
        Err(Frame::error(format!("ERR wrong number of arguments for '{cmd}' command")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::{Store, StoreConfig};

    fn store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    /// Build a command frame and dispatch it.
    fn cmd(store: &Arc<Store>, args: &[&str]) -> Frame {
        let frame = Frame::Array(args.iter().map(|s| Frame::bulk(s.as_bytes().to_vec())).collect());
        dispatch(store, &frame)
    }

    fn bulk_str(f: &Frame) -> Option<&str> {
        if let Frame::Bulk(b) = f { std::str::from_utf8(b).ok() } else { None }
    }

    fn int(f: &Frame) -> Option<i64> {
        if let Frame::Integer(n) = f { Some(*n) } else { None }
    }

    /// F-19: HSET bypassed the memory limit and always answered with a field
    /// count. Over budget under `noeviction` it must answer `-OOM`, like SET.
    #[test]
    fn hset_over_the_memory_limit_is_oom() {
        let store = Store::new(StoreConfig {
            memory_limit: 4 * 1024,
            eviction_policy: crate::store::EvictionPolicy::NoEviction,
            ..StoreConfig::default()
        });
        let big = "x".repeat(8 * 1024);
        let reply = cmd(&store, &["HSET", "h", "f", &big]);
        assert_eq!(reply, Frame::error(OOM_ERROR));
        assert_eq!(int(&cmd(&store, &["HLEN", "h"])), Some(0));
        assert_eq!(int(&cmd(&store, &["HSET", "h", "f", "small"])), Some(1));
    }

    // ── Owner-routed conditional writes (clustered) ──────────────────────

    /// A replicator whose key owner is always unreachable.
    #[derive(Debug)]
    struct DownOwner;

    impl crate::store::Replicator for DownOwner {
        fn replicate_set(&self, _: String, _: Vec<u8>, _: Option<std::time::Duration>) -> bool {
            true
        }
        fn replicate_remove(&self, _: &str) -> bool {
            true
        }
        fn replicate_expire(&self, _: &str, _: std::time::Duration) -> bool {
            true
        }
        fn replicate_published(&self, _: String, _: Vec<u8>, _: Option<std::time::Duration>) {}
        fn replicate_remove_published(&self, _: String) {}
        fn route_set_nx(
            &self,
            _: &str,
            _: &[u8],
            _: Option<std::time::Duration>,
        ) -> crate::store::Routed<SetNxOutcome> {
            crate::store::Routed::OwnerUnreachable
        }
        fn route_del_if_eq(&self, _: &str, _: &[u8]) -> crate::store::Routed<bool> {
            crate::store::Routed::OwnerUnreachable
        }
    }

    /// An unreachable owner is a `-CLUSTERDOWN` error on every conditional
    /// command — never `:0` / `nil`, which would tell a lock client "someone
    /// else holds it" (or "your token did not match") when nothing was decided.
    #[test]
    fn unreachable_owner_is_clusterdown_not_a_normal_miss() {
        let s = store();
        s.set_replicator(Some(Arc::new(DownOwner)));
        let is_clusterdown =
            |f: &Frame| matches!(f, Frame::Error(e) if e.starts_with("CLUSTERDOWN"));

        assert!(is_clusterdown(&cmd(&s, &["SETNX", "k", "v"])));
        assert!(is_clusterdown(&cmd(&s, &["SET", "k", "v", "NX"])));
        assert!(is_clusterdown(&cmd(&s, &["SET", "k", "v", "NX", "PX", "500"])));
        assert!(is_clusterdown(&cmd(&s, &["DELIFEQ", "k", "v"])));
    }

    // ── Connection commands ──────────────────────────────────────────────

    #[test]
    fn ping_bare() {
        assert_eq!(cmd(&store(), &["PING"]), Frame::Simple("PONG".into()));
    }

    #[test]
    fn ping_with_message() {
        let f = cmd(&store(), &["PING", "hello"]);
        assert_eq!(bulk_str(&f), Some("hello"));
    }

    #[test]
    fn echo_returns_arg() {
        let f = cmd(&store(), &["ECHO", "test"]);
        assert_eq!(bulk_str(&f), Some("test"));
    }

    #[test]
    fn echo_missing_arg_is_error() {
        assert!(matches!(cmd(&store(), &["ECHO"]), Frame::Error(_)));
    }

    #[test]
    fn select_zero_ok() {
        assert_eq!(cmd(&store(), &["SELECT", "0"]), Frame::ok());
    }

    #[test]
    fn select_nonzero_is_error() {
        assert!(matches!(cmd(&store(), &["SELECT", "1"]), Frame::Error(_)));
    }

    #[test]
    fn quit_returns_ok() {
        assert_eq!(cmd(&store(), &["QUIT"]), Frame::ok());
    }

    #[test]
    fn command_returns_empty_array() {
        assert_eq!(cmd(&store(), &["COMMAND"]), Frame::Array(vec![]));
    }

    // ── GET / SET ────────────────────────────────────────────────────────

    #[test]
    fn get_missing_returns_null() {
        assert_eq!(cmd(&store(), &["GET", "no_such_key"]), Frame::Null);
    }

    #[test]
    fn set_and_get_round_trip() {
        let s = store();
        assert_eq!(cmd(&s, &["SET", "k", "v"]), Frame::ok());
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("v"));
    }

    #[test]
    fn set_overwrites_existing() {
        let s = store();
        cmd(&s, &["SET", "k", "old"]);
        cmd(&s, &["SET", "k", "new"]);
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("new"));
    }

    #[test]
    fn set_with_ex_sets_ttl() {
        let s = store();
        cmd(&s, &["SET", "k", "v", "EX", "60"]);
        // Key exists with a TTL.
        let ttl = int(&cmd(&s, &["TTL", "k"])).unwrap();
        assert!(ttl > 0 && ttl <= 60, "expected TTL in (0, 60], got {ttl}");
    }

    #[test]
    fn set_with_px_sets_ttl_millis() {
        let s = store();
        cmd(&s, &["SET", "k", "v", "PX", "60000"]);
        let pttl = int(&cmd(&s, &["PTTL", "k"])).unwrap();
        assert!(pttl > 0 && pttl <= 60_000, "expected PTTL in (0, 60000], got {pttl}");
    }

    #[test]
    fn set_nx_only_when_absent() {
        let s = store();
        // First call sets the key.
        assert_eq!(cmd(&s, &["SET", "k", "first", "NX"]), Frame::ok());
        // Second call with NX returns Null (key already exists).
        assert_eq!(cmd(&s, &["SET", "k", "second", "NX"]), Frame::Null);
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("first"));
    }

    #[test]
    fn set_xx_only_when_present() {
        let s = store();
        // XX on missing key returns Null.
        assert_eq!(cmd(&s, &["SET", "k", "v", "XX"]), Frame::Null);
        cmd(&s, &["SET", "k", "v"]);
        // XX on existing key succeeds.
        assert_eq!(cmd(&s, &["SET", "k", "new", "XX"]), Frame::ok());
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("new"));
    }

    #[test]
    fn set_get_option_returns_old_value() {
        let s = store();
        cmd(&s, &["SET", "k", "old"]);
        let prev = cmd(&s, &["SET", "k", "new", "GET"]);
        assert_eq!(bulk_str(&prev), Some("old"));
    }

    #[test]
    fn set_get_option_returns_null_when_absent() {
        let s = store();
        assert_eq!(cmd(&s, &["SET", "k", "v", "GET"]), Frame::Null);
    }

    #[test]
    fn set_nx_and_xx_together_is_error() {
        assert!(matches!(cmd(&store(), &["SET", "k", "v", "NX", "XX"]), Frame::Error(_)));
    }

    #[test]
    fn setex_sets_key_with_ttl() {
        let s = store();
        assert_eq!(cmd(&s, &["SETEX", "k", "30", "value"]), Frame::ok());
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("value"));
        let ttl = int(&cmd(&s, &["TTL", "k"])).unwrap();
        assert!(ttl > 0 && ttl <= 30);
    }

    #[test]
    fn setex_invalid_ttl_is_error() {
        assert!(matches!(cmd(&store(), &["SETEX", "k", "-1", "v"]), Frame::Error(_)));
    }

    #[test]
    fn setex_wrong_args_is_error() {
        assert!(matches!(cmd(&store(), &["SETEX", "k"]), Frame::Error(_)));
    }

    // ── MGET / MSET / SETNX ─────────────────────────────────────────────

    #[test]
    fn mset_and_mget() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2", "c", "3"]);
        let f = cmd(&s, &["MGET", "a", "b", "c", "missing"]);
        let Frame::Array(items) = f else { panic!("expected array") };
        assert_eq!(bulk_str(&items[0]), Some("1"));
        assert_eq!(bulk_str(&items[1]), Some("2"));
        assert_eq!(bulk_str(&items[2]), Some("3"));
        assert_eq!(items[3], Frame::Null);
    }

    #[test]
    fn setnx_only_when_absent() {
        let s = store();
        assert_eq!(cmd(&s, &["SETNX", "k", "v"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["SETNX", "k", "v2"]), Frame::integer(0));
    }

    // ── DEL / EXISTS ─────────────────────────────────────────────────────

    #[test]
    fn del_existing_key() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["DEL", "k"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["GET", "k"]), Frame::Null);
    }

    #[test]
    fn del_missing_key() {
        assert_eq!(cmd(&store(), &["DEL", "nope"]), Frame::integer(0));
    }

    #[test]
    fn del_multiple_keys_counts_removed() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        assert_eq!(cmd(&s, &["DEL", "a", "b", "missing"]), Frame::integer(2));
    }

    // ── DELIFEQ (compare-and-delete) ────────────────────────────────────

    #[test]
    fn delifeq_deletes_on_matching_token() {
        let s = store();
        cmd(&s, &["SET", "lock", "tok"]);
        assert_eq!(cmd(&s, &["DELIFEQ", "lock", "tok"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["GET", "lock"]), Frame::Null);
    }

    #[test]
    fn delifeq_is_noop_on_mismatch() {
        let s = store();
        cmd(&s, &["SET", "lock", "tok"]);
        assert_eq!(cmd(&s, &["DELIFEQ", "lock", "other"]), Frame::integer(0));
        assert_eq!(bulk_str(&cmd(&s, &["GET", "lock"])), Some("tok"), "value must survive");
    }

    #[test]
    fn delifeq_is_noop_on_absent_key() {
        assert_eq!(cmd(&store(), &["DELIFEQ", "missing", "tok"]), Frame::integer(0));
    }

    #[test]
    fn delifeq_missing_token_is_error() {
        assert!(matches!(cmd(&store(), &["DELIFEQ", "lock"]), Frame::Error(_)));
    }

    #[test]
    fn exists_present_and_absent() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["EXISTS", "k"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["EXISTS", "nope"]), Frame::integer(0));
    }

    #[test]
    fn exists_multiple_keys() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        assert_eq!(cmd(&s, &["EXISTS", "a", "b", "nope"]), Frame::integer(2));
    }

    // ── INCR / DECR / INCRBY / DECRBY ───────────────────────────────────

    #[test]
    fn incr_creates_key_at_one() {
        let s = store();
        assert_eq!(cmd(&s, &["INCR", "counter"]), Frame::integer(1));
    }

    #[test]
    fn incr_increments_existing() {
        let s = store();
        cmd(&s, &["SET", "n", "10"]);
        assert_eq!(cmd(&s, &["INCR", "n"]), Frame::integer(11));
    }

    #[test]
    fn decr_decrements() {
        let s = store();
        cmd(&s, &["SET", "n", "5"]);
        assert_eq!(cmd(&s, &["DECR", "n"]), Frame::integer(4));
    }

    #[test]
    fn incrby_adds_delta() {
        let s = store();
        cmd(&s, &["SET", "n", "10"]);
        assert_eq!(cmd(&s, &["INCRBY", "n", "5"]), Frame::integer(15));
    }

    #[test]
    fn decrby_subtracts_delta() {
        let s = store();
        cmd(&s, &["SET", "n", "10"]);
        assert_eq!(cmd(&s, &["DECRBY", "n", "3"]), Frame::integer(7));
    }

    #[test]
    fn incr_on_non_integer_is_error() {
        let s = store();
        cmd(&s, &["SET", "k", "hello"]);
        assert!(matches!(cmd(&s, &["INCR", "k"]), Frame::Error(_)));
    }

    // ── APPEND / STRLEN / GETSET ─────────────────────────────────────────

    #[test]
    fn append_creates_key() {
        let s = store();
        assert_eq!(cmd(&s, &["APPEND", "k", "hello"]), Frame::integer(5));
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("hello"));
    }

    #[test]
    fn append_extends_existing() {
        let s = store();
        cmd(&s, &["SET", "k", "hello"]);
        cmd(&s, &["APPEND", "k", " world"]);
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("hello world"));
    }

    #[test]
    fn strlen_existing() {
        let s = store();
        cmd(&s, &["SET", "k", "hello"]);
        assert_eq!(cmd(&s, &["STRLEN", "k"]), Frame::integer(5));
    }

    #[test]
    fn strlen_missing_is_zero() {
        assert_eq!(cmd(&store(), &["STRLEN", "nope"]), Frame::integer(0));
    }

    #[test]
    fn getset_returns_old_and_sets_new() {
        let s = store();
        cmd(&s, &["SET", "k", "old"]);
        let prev = cmd(&s, &["GETSET", "k", "new"]);
        assert_eq!(bulk_str(&prev), Some("old"));
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("new"));
    }

    #[test]
    fn getset_missing_key_returns_null() {
        let s = store();
        assert_eq!(cmd(&s, &["GETSET", "k", "v"]), Frame::Null);
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("v"));
    }

    // ── TTL / EXPIRE / PERSIST / TYPE ────────────────────────────────────

    #[test]
    fn ttl_no_expiry_returns_minus_one() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["TTL", "k"]), Frame::integer(-1));
    }

    #[test]
    fn ttl_missing_key_returns_minus_two() {
        assert_eq!(cmd(&store(), &["TTL", "nope"]), Frame::integer(-2));
    }

    #[test]
    fn expire_sets_ttl() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["EXPIRE", "k", "30"]), Frame::integer(1));
        let ttl = int(&cmd(&s, &["TTL", "k"])).unwrap();
        assert!(ttl > 0 && ttl <= 30);
    }

    #[test]
    fn expire_missing_key_returns_zero() {
        assert_eq!(cmd(&store(), &["EXPIRE", "nope", "10"]), Frame::integer(0));
    }

    #[test]
    fn pexpire_sets_ttl_millis() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["PEXPIRE", "k", "30000"]), Frame::integer(1));
        let pttl = int(&cmd(&s, &["PTTL", "k"])).unwrap();
        assert!(pttl > 0 && pttl <= 30_000);
    }

    #[test]
    fn persist_removes_ttl() {
        let s = store();
        cmd(&s, &["SET", "k", "v", "EX", "30"]);
        assert_eq!(cmd(&s, &["PERSIST", "k"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["TTL", "k"]), Frame::integer(-1));
    }

    #[test]
    fn persist_on_key_without_ttl_returns_zero() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["PERSIST", "k"]), Frame::integer(0));
    }

    #[test]
    fn type_existing_key_is_string() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["TYPE", "k"]), Frame::Simple("string".into()));
    }

    #[test]
    fn type_missing_key_is_none() {
        assert_eq!(cmd(&store(), &["TYPE", "nope"]), Frame::Simple("none".into()));
    }

    #[test]
    fn rename_existing_key() {
        let s = store();
        cmd(&s, &["SET", "old", "value"]);
        assert_eq!(cmd(&s, &["RENAME", "old", "new"]), Frame::ok());
        assert_eq!(cmd(&s, &["GET", "old"]), Frame::Null);
        assert_eq!(bulk_str(&cmd(&s, &["GET", "new"])), Some("value"));
    }

    #[test]
    fn rename_missing_key_is_error() {
        assert!(matches!(cmd(&store(), &["RENAME", "nope", "new"]), Frame::Error(_)));
    }

    #[test]
    fn rename_preserves_ttl() {
        let s = store();
        cmd(&s, &["SET", "old", "value", "EX", "30"]);
        cmd(&s, &["RENAME", "old", "new"]);
        let ttl = int(&cmd(&s, &["TTL", "new"])).unwrap();
        assert!(ttl > 0 && ttl <= 30);
    }

    // ── KEYS / DBSIZE / FLUSHDB ──────────────────────────────────────────

    #[test]
    fn keys_wildcard_returns_all() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        let Frame::Array(keys) = cmd(&s, &["KEYS", "*"]) else { panic!("expected array") };
        assert!(keys.len() >= 2);
    }

    #[test]
    fn keys_pattern_filters() {
        let s = store();
        cmd(&s, &["MSET", "user:1", "a", "user:2", "b", "post:1", "c"]);
        let Frame::Array(keys) = cmd(&s, &["KEYS", "user:*"]) else { panic!("expected array") };
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn dbsize_counts_keys() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2", "c", "3"]);
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(3));
    }

    #[test]
    fn flushdb_removes_all_keys() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        assert_eq!(cmd(&s, &["FLUSHDB"]), Frame::ok());
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(0));
    }

    #[test]
    fn flushall_removes_all_keys() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        assert_eq!(cmd(&s, &["FLUSHALL"]), Frame::ok());
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(0));
    }

    // ── INFO ─────────────────────────────────────────────────────────────

    #[test]
    fn info_returns_bulk_with_redis_version() {
        let f = cmd(&store(), &["INFO"]);
        let s = bulk_str(&f).expect("INFO should return a bulk string");
        assert!(s.contains("redis_version:"), "missing redis_version in INFO");
        assert!(s.contains("used_memory:"), "missing used_memory in INFO");
    }

    // ── Error cases ──────────────────────────────────────────────────────

    #[test]
    fn unknown_command_is_error() {
        let f = cmd(&store(), &["NOTACOMMAND"]);
        let Frame::Error(msg) = f else { panic!("expected error frame") };
        assert!(msg.contains("NOTACOMMAND"));
    }

    #[test]
    fn get_missing_arg_is_error() {
        assert!(matches!(cmd(&store(), &["GET"]), Frame::Error(_)));
    }

    #[test]
    fn set_missing_value_is_error() {
        assert!(matches!(cmd(&store(), &["SET", "k"]), Frame::Error(_)));
    }

    #[test]
    fn del_no_args_is_error() {
        assert!(matches!(cmd(&store(), &["DEL"]), Frame::Error(_)));
    }

    #[test]
    fn mget_no_args_is_error() {
        assert!(matches!(cmd(&store(), &["MGET"]), Frame::Error(_)));
    }

    #[test]
    fn mset_odd_args_is_error() {
        assert!(matches!(cmd(&store(), &["MSET", "k"]), Frame::Error(_)));
    }

    #[test]
    fn expire_invalid_time_is_error() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert!(matches!(cmd(&s, &["EXPIRE", "k", "-5"]), Frame::Error(_)));
    }

    // ── Out-of-range numeric arguments (KV stress report F-14) ───────────

    fn err_text(f: &Frame) -> &str {
        match f {
            Frame::Error(e) => e,
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    /// `SET k v EX i64::MAX` used to panic inside the store
    /// (`Instant + Duration` overflow), dropping the connection with no reply.
    /// Every expire entry point now answers Redis's error instead.
    #[test]
    fn huge_expire_times_are_errors_not_panics() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        let max = i64::MAX.to_string();
        let cases: [(&[&str], &str); 6] = [
            (&["SET", "k", "v", "EX", &max], "set"),
            (&["SET", "k", "v", "PX", &max], "set"),
            (&["SETEX", "k", &max, "v"], "setex"),
            (&["EXPIRE", "k", &max], "expire"),
            (&["PEXPIRE", "k", &max], "pexpire"),
            // Seconds that overflow only once multiplied by 1000.
            (&["EXPIRE", "k", "9223372036854776"], "expire"),
        ];
        for (args, name) in cases {
            let reply = cmd(&s, args);
            assert_eq!(
                err_text(&reply),
                format!("ERR invalid expire time in '{name}' command"),
                "{args:?}"
            );
        }
        // Nothing was changed by the refused calls.
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("v"));
        assert_eq!(int(&cmd(&s, &["PTTL", "k"])), Some(-1));
    }

    #[test]
    fn set_rejects_non_positive_and_non_integer_expire_like_redis() {
        let s = store();
        for bad in ["0", "-1"] {
            let reply = cmd(&s, &["SET", "k", "v", "EX", bad]);
            assert_eq!(err_text(&reply), "ERR invalid expire time in 'set' command");
        }
        let reply = cmd(&s, &["SET", "k", "v", "PX", "soon"]);
        assert_eq!(err_text(&reply), "ERR value is not an integer or out of range");
        let reply = cmd(&s, &["SET", "k", "v", "EX", "10", "PX", "10"]);
        assert_eq!(err_text(&reply), "ERR syntax error");
        // None of the refused SETs wrote the key.
        assert_eq!(cmd(&s, &["GET", "k"]), Frame::Null);
    }

    #[test]
    fn large_but_valid_expire_is_accepted() {
        let s = store();
        // 100 years: far beyond any real TTL, well inside every bound.
        assert_eq!(cmd(&s, &["SET", "k", "v", "EX", "3153600000"]), Frame::ok());
        assert!(int(&cmd(&s, &["TTL", "k"])).unwrap() > 3_153_599_000);
    }

    /// `DECRBY k i64::MIN` negated the delta unchecked: a debug-build panic,
    /// and in release a wrap to `i64::MIN` that *incremented*.
    #[test]
    fn decrby_min_is_an_error_not_an_overflow() {
        let s = store();
        cmd(&s, &["SET", "k", "5"]);
        let reply = cmd(&s, &["DECRBY", "k", &i64::MIN.to_string()]);
        assert_eq!(err_text(&reply), "ERR decrement would overflow");
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("5"));
    }

    #[test]
    fn incrby_overflow_is_an_error() {
        let s = store();
        cmd(&s, &["SET", "k", &i64::MAX.to_string()]);
        assert!(matches!(cmd(&s, &["INCRBY", "k", "1"]), Frame::Error(_)));
        assert!(matches!(cmd(&s, &["INCR", "k"]), Frame::Error(_)));
    }

    // ── Binary keys (KV stress report F-18) ──────────────────────────────

    fn cmd_bytes(store: &Arc<Store>, args: &[&[u8]]) -> Frame {
        let frame = Frame::Array(args.iter().map(|s| Frame::bulk(s.to_vec())).collect());
        dispatch(store, &frame)
    }

    fn is_bad_key(f: &Frame) -> bool {
        matches!(f, Frame::Error(e) if e == NON_UTF8_KEY_ERROR)
    }

    /// Two distinct non-UTF-8 keys used to be converted lossily to the same
    /// `"\u{FFFD}"` string and silently share one entry. They are refused.
    #[test]
    fn non_utf8_keys_are_refused_not_collapsed() {
        let s = store();
        assert!(is_bad_key(&cmd_bytes(&s, &[b"SET", b"\xff", b"A"])));
        assert!(is_bad_key(&cmd_bytes(&s, &[b"SET", b"\xfe", b"B"])));
        assert!(is_bad_key(&cmd_bytes(&s, &[b"GET", b"\xff"])));
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(0), "nothing may have been written");
    }

    #[test]
    fn every_key_position_is_checked() {
        let s = store();
        let bad: &[u8] = b"k\xc3";
        let cases: &[&[&[u8]]] = &[
            &[b"GET", bad],
            &[b"SETEX", bad, b"10", b"v"],
            &[b"SETNX", bad, b"v"],
            &[b"INCR", bad],
            &[b"DECRBY", bad, b"1"],
            &[b"APPEND", bad, b"v"],
            &[b"GETSET", bad, b"v"],
            &[b"DELIFEQ", bad, b"t"],
            &[b"EXPIRE", bad, b"10"],
            &[b"PTTL", bad],
            &[b"RENAME", b"ok", bad],
            &[b"KEYS", bad],
            &[b"MGET", b"ok", bad],
            &[b"MSET", b"ok", b"1", bad, b"2"],
            &[b"DEL", b"ok", bad],
            &[b"EXISTS", bad],
            &[b"HSET", b"h", b"f", b"v", bad, b"v"],
            &[b"HGET", b"h", bad],
            &[b"HDEL", b"h", bad],
            &[b"HEXISTS", bad, b"f"],
        ];
        for args in cases {
            assert!(is_bad_key(&cmd_bytes(&s, args)), "{args:?}");
        }
        // The partially valid MSET / HSET wrote nothing.
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(0));
    }

    /// Values stay binary-safe, and so do keys with a NUL in them.
    #[test]
    fn binary_values_and_nul_keys_round_trip() {
        let s = store();
        assert_eq!(cmd_bytes(&s, &[b"SET", b"nul\0A", b"\xff\x00a"]), Frame::ok());
        assert_eq!(cmd_bytes(&s, &[b"SET", b"nul\0B", b"b"]), Frame::ok());
        assert_eq!(cmd_bytes(&s, &[b"GET", b"nul\0A"]), Frame::bulk(b"\xff\x00a".to_vec()));
        assert_eq!(cmd_bytes(&s, &[b"GET", b"nul\0B"]), Frame::bulk(b"b".to_vec()));
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(2));
    }

    #[test]
    fn incrby_non_integer_delta_is_error() {
        assert!(matches!(cmd(&store(), &["INCRBY", "k", "notanint"]), Frame::Error(_)));
    }

    // ── Case insensitivity ───────────────────────────────────────────────

    #[test]
    fn commands_are_case_insensitive() {
        let s = store();
        assert_eq!(cmd(&s, &["set", "k", "v"]), Frame::ok());
        assert_eq!(bulk_str(&cmd(&s, &["get", "k"])), Some("v"));
        assert_eq!(cmd(&s, &["Set", "k2", "v2"]), Frame::ok());
    }

    // ── Out-of-memory refusal is surfaced on the wire (#477) ──────────────

    /// A store with a tiny budget and `NoEviction`, so a large value cannot be
    /// stored and small ones can. `memory_limit: 200` fits a `k=v`-sized entry
    /// but not a kilobyte (same sizing the store's `noeviction_rejects_writes`
    /// test relies on).
    fn tiny_store() -> Arc<Store> {
        Store::new(StoreConfig {
            memory_limit: 200,
            eviction_policy: crate::store::EvictionPolicy::NoEviction,
            compression: crate::store::CompressionConfig::default(),
        })
    }

    /// A value guaranteed not to fit `tiny_store`'s budget.
    fn too_big() -> String {
        "x".repeat(1024)
    }

    fn is_oom(f: &Frame) -> bool {
        matches!(f, Frame::Error(m) if m.starts_with("OOM"))
    }

    #[test]
    fn set_within_budget_still_succeeds() {
        let s = tiny_store();
        assert_eq!(cmd(&s, &["SET", "k", "v"]), Frame::ok());
        assert_eq!(bulk_str(&cmd(&s, &["GET", "k"])), Some("v"));
    }

    #[test]
    fn set_over_budget_returns_oom_not_ok() {
        let s = tiny_store();
        let big = too_big();
        let f = cmd(&s, &["SET", "big", &big]);
        assert!(is_oom(&f), "expected -OOM, got {f:?}");
        // The refused write must not have been stored.
        assert_eq!(cmd(&s, &["GET", "big"]), Frame::Null);
    }

    #[test]
    fn set_nx_option_over_budget_returns_oom() {
        let s = tiny_store();
        let big = too_big();
        assert!(is_oom(&cmd(&s, &["SET", "big", &big, "NX"])));
    }

    #[test]
    fn setex_over_budget_returns_oom() {
        let s = tiny_store();
        let big = too_big();
        assert!(is_oom(&cmd(&s, &["SETEX", "big", "60", &big])));
    }

    #[test]
    fn setnx_over_budget_returns_oom_not_zero() {
        let s = tiny_store();
        let big = too_big();
        // A fresh key that does not fit is `-OOM`, distinct from the `:0` that
        // means "key already exists".
        assert!(is_oom(&cmd(&s, &["SETNX", "big", &big])));
    }

    #[test]
    fn setnx_existing_key_is_zero_not_oom() {
        let s = tiny_store();
        assert_eq!(cmd(&s, &["SETNX", "k", "v"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["SETNX", "k", "v2"]), Frame::integer(0));
    }

    #[test]
    fn append_create_over_budget_returns_oom() {
        let s = tiny_store();
        let big = too_big();
        assert!(is_oom(&cmd(&s, &["APPEND", "big", &big])));
    }

    #[test]
    fn getset_over_budget_returns_oom() {
        let s = tiny_store();
        let big = too_big();
        assert!(is_oom(&cmd(&s, &["GETSET", "big", &big])));
    }

    #[test]
    fn mset_over_budget_returns_oom() {
        let s = tiny_store();
        let big = too_big();
        assert!(is_oom(&cmd(&s, &["MSET", "big", &big])));
    }

    #[test]
    fn incr_create_over_budget_returns_oom() {
        // A budget too small for any entry: the INCR create path reserves
        // memory and must be refused with `-OOM` rather than creating a
        // counter that does not fit.
        let s = Store::new(StoreConfig {
            memory_limit: 8,
            eviction_policy: crate::store::EvictionPolicy::NoEviction,
            compression: crate::store::CompressionConfig::default(),
        });
        let f = cmd(&s, &["INCR", "counter"]);
        assert!(is_oom(&f), "expected -OOM, got {f:?}");
    }
}
