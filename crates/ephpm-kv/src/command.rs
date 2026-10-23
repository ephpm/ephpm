//! Redis command parsing and dispatch.
//!
//! Translates parsed RESP frames into operations on the [`Store`].

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use crate::resp::Frame;
use crate::store::{
    ExpireCondition, GetExpiry, KEY_UNAVAILABLE_ERROR, KeyKind, OOM_ERROR, OWNER_UNREACHABLE_ERROR,
    SetIfEqOutcome, SetNxOutcome, SetTtl, Store,
};

/// Error frame for a write refused by the memory budget (Redis `-OOM`).
fn oom() -> Frame {
    Frame::error(OOM_ERROR)
}

/// Error frame for an operation refused because this cluster node could not
/// establish the key's current value (`-TRYAGAIN`, nothing changed). See
/// [`crate::store::Resolved`].
fn unavailable() -> Frame {
    Frame::error(KEY_UNAVAILABLE_ERROR)
}

/// Early-return an error frame if `argv` has fewer than `n` elements.
macro_rules! check_args {
    ($cmd:expr, $argv:expr, $n:expr) => {
        if let Err(e) = require_args($cmd, $argv, $n) {
            return e;
        }
    };
}

/// Early-return an arity error unless `argv` has exactly `n` elements.
macro_rules! check_args_exact {
    ($cmd:expr, $argv:expr, $n:expr) => {
        if $argv.len() != $n {
            let name = $cmd.to_ascii_lowercase();
            return Frame::error(format!("ERR wrong number of arguments for '{name}' command"));
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
            match store.try_append(&key, argv[1]) {
                Ok(new_len) => Frame::integer(i64::try_from(new_len).unwrap_or(i64::MAX)),
                Err(e) => Frame::error(e.to_string()),
            }
        }
        "STRLEN" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            Frame::integer(i64::try_from(store.strlen_through(&key)).unwrap_or(i64::MAX))
        }
        "GETSET" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            // The reply is the value replaced: make the local copy current
            // first (a cluster node catching up, or outside a large value's
            // replica set, may hold a stale copy or none).
            if store.prepare_update(&key).is_err() {
                return unavailable();
            }
            let old = store.get(&key);
            if !store.set(key, argv[1].to_vec(), None) {
                return oom();
            }
            match old {
                Some(v) => Frame::bulk(v),
                None => Frame::Null,
            }
        }

        "GETDEL" => {
            check_args_exact!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.getdel(&key) {
                Ok(value) => value.map_or(Frame::Null, Frame::bulk),
                Err(_) => unavailable(),
            }
        }
        "GETEX" => cmd_getex(store, argv),

        // ── Key operations ───────────────────────────────────────
        // `UNLINK` is `DEL`: removing a large hash frees it off the calling
        // thread in both (Valkey 8's `lazyfree-lazy-user-del yes` default).
        "DEL" | "UNLINK" => {
            if argv.is_empty() {
                let name = cmd.to_ascii_lowercase();
                return Frame::error(format!("ERR wrong number of arguments for '{name}' command"));
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
                .filter(|k| store.exists_through(&str_from(k)))
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
            match store.try_expire(&key, ttl) {
                Ok(ok) => Frame::integer(i64::from(ok)),
                Err(_) => unavailable(),
            }
        }
        "PEXPIRE" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let ttl = match expire_arg(argv[1], TtlUnit::Millis, "pexpire") {
                Ok(ttl) => ttl,
                Err(f) => return f,
            };
            match store.try_expire(&key, ttl) {
                Ok(ok) => Frame::integer(i64::from(ok)),
                Err(_) => unavailable(),
            }
        }
        "EXPIREAT" | "PEXPIREAT" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let unit = if cmd == "EXPIREAT" { TtlUnit::Seconds } else { TtlUnit::Millis };
            let name = cmd.to_ascii_lowercase();
            let at = match parse_i64(argv[1]).map(|v| absolute_ttl(v, unit)) {
                Ok(Some(at)) => at,
                Ok(None) => {
                    return Frame::error(format!("ERR invalid expire time in '{name}' command"));
                }
                Err(f) => return f,
            };
            match at {
                // A time already past deletes the key, as Redis does.
                AbsoluteTtl::Passed => {
                    let existed = store.exists_through(&key);
                    if existed {
                        store.remove(&key);
                    }
                    Frame::integer(i64::from(existed))
                }
                AbsoluteTtl::After(ttl) => match store.try_expire(&key, ttl) {
                    Ok(ok) => Frame::integer(i64::from(ok)),
                    Err(_) => unavailable(),
                },
            }
        }
        "EXPIRETIME" | "PEXPIRETIME" => {
            check_args_exact!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.expire_time_ms_through(&key) {
                None => Frame::integer(-2),
                Some(-1) => Frame::integer(-1),
                Some(ms) if cmd == "PEXPIRETIME" => Frame::integer(ms),
                Some(ms) => Frame::integer(ms / 1000),
            }
        }
        "TTL" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.pttl_through(&key) {
                Some(ms) if ms >= 0 => Frame::integer(ms / 1000),
                Some(ms) => Frame::integer(ms), // -1 or -2
                None => Frame::integer(-2),
            }
        }
        "PTTL" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.pttl_through(&key) {
                Some(ms) => Frame::integer(ms),
                None => Frame::integer(-2),
            }
        }
        "PERSIST" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            match store.try_persist(&key) {
                Ok(removed) => Frame::integer(i64::from(removed)),
                Err(_) => unavailable(),
            }
        }
        "TYPE" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            if store.is_hash(&key) {
                Frame::Simple("hash".into())
            } else if store.exists_through(&key) {
                Frame::Simple("string".into())
            } else {
                Frame::Simple("none".into())
            }
        }
        "RENAME" => {
            check_args!(cmd, argv, 2);
            let old_key = key!(argv[0]);
            let new_key = key!(argv[1]);
            // Copies the value: make the local copy of the source current.
            if store.prepare_update(&old_key).is_err() {
                return unavailable();
            }
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
        "SCAN" => cmd_scan(store, argv),
        "DBSIZE" => Frame::integer(i64::try_from(store.len()).unwrap_or(i64::MAX)),
        "FLUSHDB" | "FLUSHALL" => {
            store.flush();
            Frame::ok()
        }

        // ── Hash operations ──────────────────────────────────────
        "HSET" | "HMSET" => {
            if argv.len() < 3 || argv.len().is_multiple_of(2) {
                let name = cmd.to_ascii_lowercase();
                return Frame::error(format!("ERR wrong number of arguments for '{name}' command"));
            }
            let key = key!(argv[0]);
            check_keys!(argv[1..].iter().step_by(2));
            let pairs: Vec<(String, &[u8])> =
                argv[1..].chunks(2).map(|pair| (str_from(pair[0]), pair[1])).collect();
            let pairs: Vec<(&str, &[u8])> = pairs.iter().map(|(f, v)| (f.as_str(), *v)).collect();
            let names: Vec<&str> = pairs.iter().map(|(f, _)| *f).collect();
            if store.prepare_hash_fields(&key, &names).is_err() {
                return unavailable();
            }
            // Under memory pressure a refused field returns `-OOM` and stops,
            // so earlier fields may already be stored — the same best-effort
            // contract as MSET. A value too large for a clustered hash field
            // is refused before anything is written.
            match store.hset_many(&key, &pairs) {
                Ok(added) if cmd == "HSET" => {
                    Frame::integer(i64::try_from(added).unwrap_or(i64::MAX))
                }
                Ok(_) => Frame::ok(),
                Err(e) => Frame::error(e.to_string()),
            }
        }
        "HSETNX" => {
            check_args_exact!(cmd, argv, 3);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            if store.prepare_hash_fields(&key, &[&field]).is_err() {
                return unavailable();
            }
            match store.hsetnx(&key, &field, argv[2]) {
                Ok(set) => Frame::integer(i64::from(set)),
                Err(e) => Frame::error(e.to_string()),
            }
        }
        "HMGET" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            check_keys!(argv[1..].iter());
            let fields: Vec<String> = argv[1..].iter().map(|f| str_from(f)).collect();
            let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
            // A read: fields whose current value cannot be established are
            // answered as misses, as `GET` answers then.
            if store.prepare_hash_fields(&key, &fields).is_err() {
                return Frame::Array(vec![Frame::Null; fields.len()]);
            }
            Frame::Array(
                store
                    .hmget(&key, &fields)
                    .into_iter()
                    .map(|v| v.map_or(Frame::Null, Frame::bulk))
                    .collect(),
            )
        }
        "HINCRBY" => {
            check_args_exact!(cmd, argv, 3);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            let delta = match parse_i64(argv[2]) {
                Ok(v) => v,
                Err(f) => return f,
            };
            if store.prepare_hash_fields(&key, &[&field]).is_err() {
                return unavailable();
            }
            match store.hincr_by(&key, &field, delta) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "HINCRBYFLOAT" => {
            check_args_exact!(cmd, argv, 3);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            let Some(delta) = crate::store::parse_float(argv[2]) else {
                return Frame::error("ERR value is not a valid float");
            };
            if store.prepare_hash_fields(&key, &[&field]).is_err() {
                return unavailable();
            }
            match store.hincr_by_float(&key, &field, delta) {
                Ok(v) => Frame::bulk(v.into_bytes()),
                Err(e) => Frame::error(e),
            }
        }
        "HSTRLEN" => {
            check_args_exact!(cmd, argv, 2);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            if store.prepare_hash_fields(&key, &[&field]).is_err() {
                return Frame::integer(0);
            }
            Frame::integer(i64::try_from(store.hstrlen(&key, &field)).unwrap_or(i64::MAX))
        }
        "HEXPIRE" | "HPEXPIRE" | "HEXPIREAT" | "HPEXPIREAT" => cmd_hexpire(store, cmd, argv),
        "HTTL" | "HPTTL" | "HEXPIRETIME" | "HPEXPIRETIME" | "HPERSIST" => {
            check_args!(cmd, argv, 4);
            let key = key!(argv[0]);
            let fields = match parse_fields(argv, 1) {
                Ok(f) => f,
                Err(e) => return e,
            };
            let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
            if store.prepare_hash_fields(&key, &fields).is_err() {
                // HPERSIST writes; the others read, and answer "no such
                // field" (-2) for a field they cannot establish.
                return if cmd == "HPERSIST" {
                    unavailable()
                } else {
                    Frame::Array(vec![Frame::integer(-2); fields.len()])
                };
            }
            let codes = match cmd {
                "HPERSIST" => store.hpersist(&key, &fields),
                "HPTTL" => store.hpttl(&key, &fields),
                "HPEXPIRETIME" => store.hexpiretime_ms(&key, &fields),
                // Seconds: only the non-negative values are times.
                "HTTL" => store.hpttl(&key, &fields).into_iter().map(ms_to_secs).collect(),
                _ => store.hexpiretime_ms(&key, &fields).into_iter().map(ms_to_secs).collect(),
            };
            Frame::Array(codes.into_iter().map(Frame::integer).collect())
        }
        "HSCAN" => cmd_hscan(store, argv),
        "HGET" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            if store.prepare_hash_fields(&key, &[&field]).is_err() {
                return Frame::Null;
            }
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
            let fields: Vec<String> = argv[1..].iter().map(|f| str_from(f)).collect();
            let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
            // Cluster-aware: never refused (a delete does not depend on the
            // old value), and never skips a field this node has not received.
            Frame::integer(i64::try_from(store.hdel_through(&key, &fields)).unwrap_or(i64::MAX))
        }
        "HGETALL" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            if store.prepare_hash_whole(&key).is_err() {
                return unavailable();
            }
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
            if store.prepare_hash_whole(&key).is_err() {
                return unavailable();
            }
            let keys = store.hkeys(&key);
            Frame::Array(keys.into_iter().map(|k| Frame::bulk(k.into_bytes())).collect())
        }
        "HVALS" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            if store.prepare_hash_whole(&key).is_err() {
                return unavailable();
            }
            let vals = store.hvals(&key);
            Frame::Array(vals.into_iter().map(Frame::bulk).collect())
        }
        "HLEN" => {
            check_args!(cmd, argv, 1);
            let key = key!(argv[0]);
            if store.prepare_hash_whole(&key).is_err() {
                return unavailable();
            }
            Frame::integer(i64::try_from(store.hlen(&key)).unwrap_or(i64::MAX))
        }
        "HEXISTS" => {
            check_args!(cmd, argv, 2);
            let key = key!(argv[0]);
            let field = key!(argv[1]);
            if store.prepare_hash_fields(&key, &[&field]).is_err() {
                return Frame::integer(0);
            }
            Frame::integer(i64::from(store.hexists(&key, &field)))
        }

        // ── Server info ──────────────────────────────────────────
        "INFO" => {
            let (keys, expires, hits, misses) = store.keyspace_info();
            let mut info = format!(
                "# Server\r\nredis_version:7.0.0\r\nredis_mode:embedded\r\ntcp_port:6379\r\n\
                 # Memory\r\nused_memory:{}\r\n\
                 # Stats\r\nkeyspace_hits:{hits}\r\nkeyspace_misses:{misses}\r\n\
                 # Keyspace\r\n",
                store.mem_used()
            );
            // Redis omits a database with no keys.
            if keys > 0 {
                let _ = write!(info, "db0:keys={keys},expires={expires},avg_ttl=0\r\n");
            }
            Frame::bulk(info.into_bytes())
        }

        _ => Frame::error(format!("ERR unknown command '{cmd}'")),
    }
}

// ── Helpers ──────────────────────────────────────────────────────

/// The expiry option of a `SET` (`EX`/`PX`/`EXAT`/`PXAT`/`KEEPTTL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetExpiry {
    /// No expiry option: the key is stored without one.
    None,
    /// Expire this long from now.
    After(Duration),
    /// `EXAT`/`PXAT` naming a time already past: the write happens and the
    /// key is immediately gone, as in Redis.
    Passed,
    /// `KEEPTTL`.
    Keep,
}

/// The condition of a `SET` (`NX`/`XX`/`IFEQ`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SetCondition {
    /// Unconditional.
    None,
    /// Only if absent.
    Nx,
    /// Only if present.
    Xx,
    /// Only while the current value equals this (Valkey 8.1 `IFEQ`).
    IfEq(Vec<u8>),
}

/// Execute `SET key value [NX | XX | IFEQ comparison] [GET]
/// [EX s | PX ms | EXAT unix-s | PXAT unix-ms | KEEPTTL]`.
///
/// As in Redis/Valkey: the expiry options are mutually exclusive, as are
/// `NX`/`XX`/`IFEQ` (`ERR syntax error`, except the historical `NX`+`XX`
/// message); a non-integer time is `ERR value is not an integer or out of
/// range` and a non-positive or overflowing one `ERR invalid expire time in
/// 'set' command`. `IFEQ` sets only while the current value equals
/// `comparison` and never creates the key; with `GET` it replies with the
/// value the key held before.
fn cmd_set(store: &Arc<Store>, argv: &[&[u8]]) -> Frame {
    if argv.len() < 2 {
        return Frame::error("ERR wrong number of arguments for 'set' command");
    }

    let key = key!(argv[0]);
    let val = argv[1].to_vec();
    let mut expiry = SetExpiry::None;
    let mut cond = SetCondition::None;
    let mut nx_and_xx = false;
    let mut get = false;

    let mut i = 2;
    while i < argv.len() {
        let opt = str_from(argv[i]).to_ascii_uppercase();
        match opt.as_str() {
            "EX" | "PX" | "EXAT" | "PXAT" => {
                if i + 1 >= argv.len() || expiry != SetExpiry::None {
                    return Frame::error("ERR syntax error");
                }
                let unit = if opt.starts_with('E') { TtlUnit::Seconds } else { TtlUnit::Millis };
                expiry = if opt.ends_with("AT") {
                    match parse_i64(argv[i + 1]).map(|v| positive_absolute_ttl(v, unit)) {
                        Ok(Some(AbsoluteTtl::After(d))) => SetExpiry::After(d),
                        Ok(Some(AbsoluteTtl::Passed)) => SetExpiry::Passed,
                        Ok(None) => {
                            return Frame::error("ERR invalid expire time in 'set' command");
                        }
                        Err(f) => return f,
                    }
                } else {
                    match expire_arg(argv[i + 1], unit, "set") {
                        Ok(d) => SetExpiry::After(d),
                        Err(f) => return f,
                    }
                };
                i += 2;
            }
            "KEEPTTL" => {
                if expiry != SetExpiry::None {
                    return Frame::error("ERR syntax error");
                }
                expiry = SetExpiry::Keep;
                i += 1;
            }
            "NX" | "XX" => {
                cond = match (&cond, opt.as_str()) {
                    (SetCondition::None, "NX") => SetCondition::Nx,
                    (SetCondition::None, _) => SetCondition::Xx,
                    (SetCondition::Nx, "XX") | (SetCondition::Xx, "NX") => {
                        nx_and_xx = true;
                        cond
                    }
                    _ => return Frame::error("ERR syntax error"),
                };
                i += 1;
            }
            "IFEQ" => {
                if i + 1 >= argv.len() || cond != SetCondition::None {
                    return Frame::error("ERR syntax error");
                }
                cond = SetCondition::IfEq(argv[i + 1].to_vec());
                i += 2;
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

    if nx_and_xx {
        return Frame::error("ERR NX and XX options at the same time are not compatible");
    }

    match cond {
        SetCondition::Nx => set_nx(store, key, val, expiry, get),
        SetCondition::IfEq(expected) => set_if_eq(store, key, &expected, val, expiry, get),
        SetCondition::None | SetCondition::Xx => {
            // GET, XX and KEEPTTL read the key before writing it: make the
            // local copy current first. A plain SET is a blind write.
            if (get || cond == SetCondition::Xx || expiry == SetExpiry::Keep)
                && store.prepare_update(&key).is_err()
            {
                return unavailable();
            }
            let old = store.get(&key);
            // XX: set only if the key exists.
            if cond == SetCondition::Xx && old.is_none() {
                return Frame::Null;
            }
            let ttl = match expiry {
                SetExpiry::None => None,
                SetExpiry::After(d) => Some(d),
                SetExpiry::Keep => keep_ttl(store, &key),
                SetExpiry::Passed => {
                    // Written and immediately expired: the key is gone.
                    store.remove(&key);
                    return reply_set(get, old);
                }
            };
            if !store.set(key, val, ttl) {
                return oom();
            }
            reply_set(get, old)
        }
    }
}

/// `+OK`, or with `GET` the value the key held before (`nil` if none).
fn reply_set(get: bool, old: Option<bytes::Bytes>) -> Frame {
    if get { old.map_or(Frame::Null, Frame::bulk) } else { Frame::ok() }
}

/// The remaining TTL of `key` for `KEEPTTL` (`None` = no expiry to keep).
fn keep_ttl(store: &Store, key: &str) -> Option<Duration> {
    store
        .string_expiry(key)
        .flatten()
        .map(|at| at.saturating_duration_since(std::time::Instant::now()))
        .filter(|d| !d.is_zero())
}

/// `SET ... NX`, routed through the atomic primitive so concurrent callers
/// see exactly one winner.
///
/// With `GET`, a lost race re-reads the existing value: Redis returns the
/// value as it was before the attempt, which needs a set-and-return-old
/// primitive this store does not have. The value fetched is consistent with
/// what is stored after the call.
fn set_nx(store: &Store, key: String, val: Vec<u8>, expiry: SetExpiry, get: bool) -> Frame {
    let ttl = match expiry {
        SetExpiry::After(d) => Some(d),
        // An absent key has no TTL to keep.
        SetExpiry::None | SetExpiry::Keep => None,
        SetExpiry::Passed => {
            // Written and immediately gone: only the reply is observable.
            return match (store.get(&key), get) {
                (Some(v), true) => Frame::bulk(v),
                (Some(_), false) | (None, true) => Frame::Null,
                (None, false) => Frame::ok(),
            };
        }
    };
    match store.set_nx_outcome(key.clone(), val, ttl) {
        // A memory refusal is `-OOM`, distinct from the `nil` that means
        // "the key already existed" (#477).
        SetNxOutcome::OutOfMemory => oom(),
        // Clustered: the key's owner could not be reached, so nothing was
        // written anywhere. Not `nil` — that would claim the key exists.
        SetNxOutcome::OwnerUnreachable => Frame::error(OWNER_UNREACHABLE_ERROR),
        SetNxOutcome::Inserted => {
            if get {
                Frame::Null
            } else {
                Frame::ok()
            }
        }
        SetNxOutcome::Exists => {
            if get {
                store.get(&key).map_or(Frame::Null, Frame::bulk)
            } else {
                Frame::Null
            }
        }
    }
}

/// `SET ... IFEQ comparison` — compare-and-set, decided by the key's owner
/// node in a cluster.
fn set_if_eq(
    store: &Store,
    key: String,
    expected: &[u8],
    val: Vec<u8>,
    expiry: SetExpiry,
    get: bool,
) -> Frame {
    let ttl = match expiry {
        SetExpiry::None | SetExpiry::Passed => SetTtl::Persist,
        SetExpiry::After(d) => SetTtl::After(d),
        SetExpiry::Keep => SetTtl::Keep,
    };
    match store.set_if_eq(key.clone(), expected, val, ttl) {
        SetIfEqOutcome::Set => {
            if expiry == SetExpiry::Passed {
                store.remove(&key);
            }
            if get { Frame::bulk(expected.to_vec()) } else { Frame::ok() }
        }
        SetIfEqOutcome::NotSet => {
            if get {
                store.get_through(&key).map_or(Frame::Null, Frame::bulk)
            } else {
                Frame::Null
            }
        }
        SetIfEqOutcome::OutOfMemory => oom(),
        SetIfEqOutcome::OwnerUnreachable => Frame::error(OWNER_UNREACHABLE_ERROR),
    }
}

/// `GETEX key [EX s | PX ms | EXAT unix-s | PXAT unix-ms | PERSIST]`.
fn cmd_getex(store: &Arc<Store>, argv: &[&[u8]]) -> Frame {
    check_args!("GETEX", argv, 1);
    let key = key!(argv[0]);
    let expiry = match argv.len() {
        1 => None,
        2 if str_from(argv[1]).eq_ignore_ascii_case("PERSIST") => Some(GetExpiry::Persist),
        3 => {
            let opt = str_from(argv[1]).to_ascii_uppercase();
            let unit = match opt.as_str() {
                "EX" | "EXAT" => TtlUnit::Seconds,
                "PX" | "PXAT" => TtlUnit::Millis,
                _ => return Frame::error("ERR syntax error"),
            };
            if opt.ends_with("AT") {
                match parse_i64(argv[2]).map(|v| positive_absolute_ttl(v, unit)) {
                    Ok(Some(AbsoluteTtl::After(d))) => Some(GetExpiry::After(d)),
                    Ok(Some(AbsoluteTtl::Passed)) => Some(GetExpiry::Delete),
                    Ok(None) => return Frame::error("ERR invalid expire time in 'getex' command"),
                    Err(f) => return f,
                }
            } else {
                match expire_arg(argv[2], unit, "getex") {
                    Ok(d) => Some(GetExpiry::After(d)),
                    Err(f) => return f,
                }
            }
        }
        _ => return Frame::error("ERR syntax error"),
    };
    let value = match expiry {
        None => Ok(store.get_through(&key)),
        Some(expiry) => store.getex(&key, expiry),
    };
    match value {
        Ok(value) => value.map_or(Frame::Null, Frame::bulk),
        Err(_) => unavailable(),
    }
}

/// `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]`. See
/// [`Store::scan`] for the cursor's guarantee.
fn cmd_scan(store: &Arc<Store>, argv: &[&[u8]]) -> Frame {
    check_args!("SCAN", argv, 1);
    let Some(cursor) = std::str::from_utf8(argv[0]).ok().and_then(|s| s.parse::<u64>().ok()) else {
        return Frame::error("ERR invalid cursor");
    };
    let (pattern, count, kind) = match scan_options(&argv[1..], true) {
        Ok(o) => o,
        Err(f) => return f,
    };
    let page = store.scan(cursor, pattern.as_deref(), count, kind);
    Frame::Array(vec![
        Frame::bulk(page.cursor.to_string().into_bytes()),
        Frame::Array(page.keys.into_iter().map(|k| Frame::bulk(k.into_bytes())).collect()),
    ])
}

/// `HSCAN key cursor [MATCH pattern] [COUNT count] [NOVALUES]`.
///
/// Returns the whole hash in one page with cursor `0` (see
/// [`Store::hscan`]); a non-zero cursor — which this server never hands out —
/// gets an empty final page.
fn cmd_hscan(store: &Arc<Store>, argv: &[&[u8]]) -> Frame {
    check_args!("HSCAN", argv, 2);
    let key = key!(argv[0]);
    let Some(cursor) = std::str::from_utf8(argv[1]).ok().and_then(|s| s.parse::<u64>().ok()) else {
        return Frame::error("ERR invalid cursor");
    };
    let mut novalues = false;
    let mut rest: Vec<&[u8]> = Vec::with_capacity(argv.len());
    for arg in &argv[2..] {
        if str_from(arg).eq_ignore_ascii_case("NOVALUES") {
            novalues = true;
        } else {
            rest.push(arg);
        }
    }
    let (pattern, _count, _) = match scan_options(&rest, false) {
        Ok(o) => o,
        Err(f) => return f,
    };
    if store.prepare_hash_whole(&key).is_err() {
        return unavailable();
    }
    let mut items = Vec::new();
    if cursor == 0 {
        for (field, value) in store.hscan(&key, pattern.as_deref()) {
            items.push(Frame::bulk(field.into_bytes()));
            if !novalues {
                items.push(Frame::bulk(value));
            }
        }
    }
    Frame::Array(vec![Frame::bulk(b"0".to_vec()), Frame::Array(items)])
}

/// The parsed `[MATCH pattern] [COUNT count] [TYPE type]` of a scan.
type ScanOptions = (Option<String>, usize, Option<KeyKind>);

/// Parse `[MATCH pattern] [COUNT count]` (and `[TYPE type]` when `with_type`).
fn scan_options(args: &[&[u8]], with_type: bool) -> Result<ScanOptions, Frame> {
    let mut pattern = None;
    let mut count = 10usize;
    let mut kind = None;
    let mut i = 0;
    while i < args.len() {
        let opt = str_from(args[i]).to_ascii_uppercase();
        let Some(value) = args.get(i + 1) else {
            return Err(Frame::error("ERR syntax error"));
        };
        match opt.as_str() {
            "MATCH" => match std::str::from_utf8(value) {
                Ok(p) => pattern = Some(p.to_owned()),
                Err(_) => return Err(Frame::error(NON_UTF8_KEY_ERROR)),
            },
            "COUNT" => {
                let n = parse_i64(value)?;
                if n < 1 {
                    return Err(Frame::error("ERR syntax error"));
                }
                count = usize::try_from(n).unwrap_or(usize::MAX);
            }
            "TYPE" if with_type => {
                kind = Some(match str_from(value).to_ascii_lowercase().as_str() {
                    "string" => KeyKind::String,
                    "hash" => KeyKind::Hash,
                    _ => KeyKind::Other,
                });
            }
            _ => return Err(Frame::error("ERR syntax error")),
        }
        i += 2;
    }
    Ok((pattern, count, kind))
}

/// Largest absolute field-expiry time Redis accepts, in Unix milliseconds
/// (`HFE_MAX_ABS_TIME_MSEC`, 2^46 - 1: year 4199).
const HFE_MAX_ABS_TIME_MS: i64 = (1 << 46) - 1;

/// `HEXPIRE`/`HPEXPIRE`/`HEXPIREAT`/`HPEXPIREAT key time [NX|XX|GT|LT]
/// FIELDS numfields field...` (Redis 7.4).
fn cmd_hexpire(store: &Arc<Store>, cmd: &str, argv: &[&[u8]]) -> Frame {
    check_args!(cmd, argv, 4);
    let key = key!(argv[0]);
    let name = cmd.to_ascii_lowercase();
    let time = match parse_i64(argv[1]) {
        Ok(t) => t,
        Err(f) => return f,
    };
    if time < 0 {
        return Frame::error("ERR invalid expire time, must be >= 0");
    }
    let unit_ms = if cmd.starts_with("HP") { 1 } else { 1000 };
    let now_ms = crate::store::unix_ms_now();
    let absolute_ms = time
        .checked_mul(unit_ms)
        .and_then(|t| if cmd.ends_with("AT") { Some(t) } else { now_ms.checked_add(t) });
    let Some(absolute_ms) = absolute_ms.filter(|ms| *ms <= HFE_MAX_ABS_TIME_MS) else {
        return Frame::error(format!("ERR invalid expire time in '{name}' command"));
    };
    let remaining =
        Duration::from_millis(u64::try_from(absolute_ms.saturating_sub(now_ms)).unwrap_or(0));

    let (cond, fields_at) = match str_from(argv[2]).to_ascii_uppercase().as_str() {
        "NX" => (ExpireCondition::Nx, 3),
        "XX" => (ExpireCondition::Xx, 3),
        "GT" => (ExpireCondition::Gt, 3),
        "LT" => (ExpireCondition::Lt, 3),
        _ => (ExpireCondition::Always, 2),
    };
    let fields = match parse_fields(argv, fields_at) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    if store.prepare_hash_fields(&key, &fields).is_err() {
        return unavailable();
    }
    let codes = store.hexpire(&key, &fields, remaining, cond);
    Frame::Array(codes.into_iter().map(Frame::integer).collect())
}

/// Parse `FIELDS numfields field...` starting at `argv[at]`, with Redis'
/// error replies.
fn parse_fields(argv: &[&[u8]], at: usize) -> Result<Vec<String>, Frame> {
    let keyword_ok = argv.get(at).is_some_and(|a| str_from(a).eq_ignore_ascii_case("FIELDS"));
    if !keyword_ok {
        return Err(Frame::error(
            "ERR Mandatory argument FIELDS is missing or not at the right position",
        ));
    }
    let Some(count) = argv.get(at + 1) else {
        return Err(Frame::error("ERR wrong number of arguments"));
    };
    let n = match parse_i64(count) {
        Ok(n) if n >= 1 => n,
        Ok(_) => return Err(Frame::error("ERR Parameter `numFields` should be greater than 0")),
        Err(f) => return Err(f),
    };
    let fields = &argv[at + 2..];
    if usize::try_from(n).ok() != Some(fields.len()) {
        return Err(Frame::error(
            "ERR The `numfields` parameter must match the number of arguments",
        ));
    }
    fields
        .iter()
        .map(|f| {
            std::str::from_utf8(f).map(str::to_owned).map_err(|_| Frame::error(NON_UTF8_KEY_ERROR))
        })
        .collect()
}

/// A field-expiry reply in milliseconds as seconds; the negative codes pass
/// through.
fn ms_to_secs(ms: i64) -> i64 {
    if ms < 0 { ms } else { ms / 1000 }
}

/// An absolute expire time (`EXAT`, `PXAT`, `EXPIREAT`, ...) relative to now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbsoluteTtl {
    /// In the future: expire this long from now.
    After(Duration),
    /// At or before now: already expired.
    Passed,
}

/// Turn an absolute Unix time into an [`AbsoluteTtl`], or `None` when it is
/// out of range (the seconds overflow milliseconds, or the deadline cannot be
/// represented).
fn absolute_ttl(when: i64, unit: TtlUnit) -> Option<AbsoluteTtl> {
    let when_ms = match unit {
        TtlUnit::Seconds => when.checked_mul(1000)?,
        TtlUnit::Millis => when,
    };
    let remaining = when_ms.checked_sub(crate::store::unix_ms_now())?;
    if remaining <= 0 {
        return Some(AbsoluteTtl::Passed);
    }
    checked_ttl(remaining, TtlUnit::Millis).map(AbsoluteTtl::After)
}

/// [`absolute_ttl`] for `SET`/`GETEX`'s `EXAT`/`PXAT`, which — unlike
/// `EXPIREAT` — Redis refuses for a non-positive time (`ERR invalid expire
/// time`).
fn positive_absolute_ttl(when: i64, unit: TtlUnit) -> Option<AbsoluteTtl> {
    if when <= 0 { None } else { absolute_ttl(when, unit) }
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
        fn replicate_hash_field(
            &self,
            _key: &str,
            _field: &str,
            apply: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
        ) -> bool {
            let _ = apply();
            true
        }
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

    // ── v0.13 command additions ──────────────────────────────────────────

    fn ints(f: &Frame) -> Vec<i64> {
        match f {
            Frame::Array(items) => items.iter().map(|i| int(i).expect("integer")).collect(),
            other => panic!("expected an array, got {other:?}"),
        }
    }

    fn strs(f: &Frame) -> Vec<Option<String>> {
        match f {
            Frame::Array(items) => items.iter().map(|i| bulk_str(i).map(str::to_owned)).collect(),
            other => panic!("expected an array, got {other:?}"),
        }
    }

    fn future_secs(delta: i64) -> String {
        (crate::store::unix_ms_now() / 1000 + delta).to_string()
    }

    #[test]
    fn getdel_and_getex() {
        let s = store();
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(bulk_str(&cmd(&s, &["GETDEL", "k"])), Some("v"));
        assert_eq!(cmd(&s, &["GETDEL", "k"]), Frame::Null);
        assert!(matches!(cmd(&s, &["GETDEL"]), Frame::Error(_)));

        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(bulk_str(&cmd(&s, &["GETEX", "k", "EX", "100"])), Some("v"));
        assert!(int(&cmd(&s, &["TTL", "k"])).unwrap() > 98);
        assert_eq!(bulk_str(&cmd(&s, &["GETEX", "k", "PERSIST"])), Some("v"));
        assert_eq!(int(&cmd(&s, &["TTL", "k"])), Some(-1));
        let pxat = (crate::store::unix_ms_now() + 50_000).to_string();
        assert_eq!(bulk_str(&cmd(&s, &["GETEX", "k", "PXAT", &pxat])), Some("v"));
        assert!(int(&cmd(&s, &["PTTL", "k"])).unwrap() > 48_000);
        assert_eq!(bulk_str(&cmd(&s, &["GETEX", "k"])), Some("v"));
        // An absolute time in the past deletes the key.
        assert_eq!(bulk_str(&cmd(&s, &["GETEX", "k", "EXAT", "1"])), Some("v"));
        assert_eq!(cmd(&s, &["EXISTS", "k"]), Frame::integer(0));
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(
            err_text(&cmd(&s, &["GETEX", "k", "EX", "0"])),
            "ERR invalid expire time in 'getex' command"
        );
        assert_eq!(err_text(&cmd(&s, &["GETEX", "k", "FOO", "1"])), "ERR syntax error");
        assert_eq!(cmd(&s, &["GETEX", "missing", "EX", "10"]), Frame::Null);
    }

    #[test]
    fn set_exat_pxat_keepttl() {
        let s = store();
        assert_eq!(cmd(&s, &["SET", "k", "v", "EXAT", &future_secs(100)]), Frame::ok());
        let ttl = int(&cmd(&s, &["TTL", "k"])).unwrap();
        assert!(ttl > 97 && ttl <= 100, "{ttl}");
        assert_eq!(cmd(&s, &["SET", "k", "v2", "KEEPTTL"]), Frame::ok());
        assert!(int(&cmd(&s, &["TTL", "k"])).unwrap() > 97, "KEEPTTL kept the expiry");
        assert_eq!(cmd(&s, &["SET", "k", "v3"]), Frame::ok());
        assert_eq!(int(&cmd(&s, &["TTL", "k"])), Some(-1), "a plain SET clears it");
        let pxat = (crate::store::unix_ms_now() + 30_000).to_string();
        assert_eq!(cmd(&s, &["SET", "k", "v", "PXAT", &pxat]), Frame::ok());
        assert!(int(&cmd(&s, &["PTTL", "k"])).unwrap() > 28_000);
        // A past absolute time: the write happens and the key is gone.
        assert_eq!(bulk_str(&cmd(&s, &["SET", "k", "v", "EXAT", "1", "GET"])), Some("v"));
        assert_eq!(cmd(&s, &["GET", "k"]), Frame::Null);
        for bad in [
            &["SET", "k", "v", "EX", "10", "KEEPTTL"][..],
            &["SET", "k", "v", "KEEPTTL", "PX", "10"],
            &["SET", "k", "v", "EXAT", "10", "PXAT", "10"],
            &["SET", "k", "v", "NX", "IFEQ", "x"],
            &["SET", "k", "v", "IFEQ"],
        ] {
            assert_eq!(err_text(&cmd(&s, bad)), "ERR syntax error", "{bad:?}");
        }
        assert_eq!(
            err_text(&cmd(&s, &["SET", "k", "v", "EXAT", "0"])),
            "ERR invalid expire time in 'set' command"
        );
    }

    #[test]
    fn set_ifeq_is_compare_and_set() {
        let s = store();
        assert_eq!(cmd(&s, &["SET", "lock", "tok", "IFEQ", "tok"]), Frame::Null, "never creates");
        assert_eq!(cmd(&s, &["GET", "lock"]), Frame::Null);
        cmd(&s, &["SET", "lock", "tok", "EX", "10"]);
        assert_eq!(cmd(&s, &["SET", "lock", "new", "IFEQ", "other"]), Frame::Null);
        assert_eq!(bulk_str(&cmd(&s, &["GET", "lock"])), Some("tok"));
        // Lock extension: same value, fresh TTL.
        assert_eq!(cmd(&s, &["SET", "lock", "tok", "IFEQ", "tok", "EX", "100"]), Frame::ok());
        assert!(int(&cmd(&s, &["TTL", "lock"])).unwrap() > 98);
        assert_eq!(bulk_str(&cmd(&s, &["SET", "lock", "b", "IFEQ", "tok", "GET"])), Some("tok"));
        assert_eq!(bulk_str(&cmd(&s, &["SET", "lock", "c", "IFEQ", "zzz", "GET"])), Some("b"));
        assert_eq!(bulk_str(&cmd(&s, &["GET", "lock"])), Some("b"));
    }

    #[test]
    fn expiretime_expireat_and_unlink() {
        let s = store();
        assert_eq!(cmd(&s, &["EXPIRETIME", "nope"]), Frame::integer(-2));
        cmd(&s, &["SET", "k", "v"]);
        assert_eq!(cmd(&s, &["PEXPIRETIME", "k"]), Frame::integer(-1));
        let at = future_secs(500);
        assert_eq!(cmd(&s, &["EXPIREAT", "k", &at]), Frame::integer(1));
        assert_eq!(int(&cmd(&s, &["EXPIRETIME", "k"])).map(|v| v.to_string()), Some(at.clone()));
        let pms = int(&cmd(&s, &["PEXPIRETIME", "k"])).unwrap();
        assert_eq!(pms / 1000, at.parse::<i64>().unwrap());
        assert_eq!(cmd(&s, &["PEXPIREAT", "k", "1"]), Frame::integer(1), "a past time deletes");
        assert_eq!(cmd(&s, &["EXISTS", "k"]), Frame::integer(0));
        assert_eq!(cmd(&s, &["EXPIREAT", "k", &at]), Frame::integer(0));
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        cmd(&s, &["HSET", "h", "f", "v"]);
        assert_eq!(cmd(&s, &["UNLINK", "a", "b", "h", "zz"]), Frame::integer(3));
        assert_eq!(cmd(&s, &["DBSIZE"]), Frame::integer(0));
    }

    #[test]
    fn hash_command_family() {
        let s = store();
        assert_eq!(cmd(&s, &["HMSET", "h", "a", "1", "b", "2"]), Frame::ok());
        assert_eq!(
            strs(&cmd(&s, &["HMGET", "h", "a", "zz", "b"])),
            vec![Some("1".into()), None, Some("2".into())]
        );
        assert_eq!(cmd(&s, &["HSETNX", "h", "a", "x"]), Frame::integer(0));
        assert_eq!(cmd(&s, &["HSETNX", "h", "c", "3"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["HINCRBY", "h", "a", "41"]), Frame::integer(42));
        assert_eq!(
            err_text(&cmd(&s, &["HINCRBY", "h", "a", "x"])),
            "ERR value is not an integer or out of range"
        );
        cmd(&s, &["HSET", "h", "s", "text"]);
        assert_eq!(
            err_text(&cmd(&s, &["HINCRBY", "h", "s", "1"])),
            "ERR hash value is not an integer"
        );
        assert_eq!(bulk_str(&cmd(&s, &["HINCRBYFLOAT", "h", "f", "10.50"])), Some("10.5"));
        assert_eq!(bulk_str(&cmd(&s, &["HINCRBYFLOAT", "h", "f", "0.1"])), Some("10.6"));
        assert_eq!(bulk_str(&cmd(&s, &["HINCRBYFLOAT", "h", "f", "-5.0e3"])), Some("-4989.4"));
        assert_eq!(
            err_text(&cmd(&s, &["HINCRBYFLOAT", "h", "s", "1"])),
            "ERR hash value is not a float"
        );
        assert_eq!(
            err_text(&cmd(&s, &["HINCRBYFLOAT", "h", "f", "nan"])),
            "ERR value is not a valid float"
        );
        assert_eq!(cmd(&s, &["HSTRLEN", "h", "s"]), Frame::integer(4));
        assert_eq!(cmd(&s, &["HSTRLEN", "h", "nope"]), Frame::integer(0));
        assert!(matches!(cmd(&s, &["HSETNX", "h", "a"]), Frame::Error(_)));
        assert!(matches!(cmd(&s, &["HMSET", "h", "a"]), Frame::Error(_)));
        assert_eq!(cmd(&s, &["TYPE", "h"]), Frame::Simple("hash".into()));
    }

    #[test]
    fn hash_field_expiry_commands() {
        let s = store();
        cmd(&s, &["HSET", "h", "a", "1", "b", "2", "c", "3"]);
        assert_eq!(ints(&cmd(&s, &["HEXPIRE", "h", "100", "FIELDS", "2", "a", "zz"])), [1, -2]);
        assert_eq!(ints(&cmd(&s, &["HEXPIRE", "h", "100", "NX", "FIELDS", "2", "a", "b"])), [0, 1]);
        assert_eq!(ints(&cmd(&s, &["HEXPIRE", "h", "50", "GT", "FIELDS", "1", "a"])), [0]);
        assert_eq!(ints(&cmd(&s, &["HEXPIRE", "h", "50", "LT", "FIELDS", "1", "a"])), [1]);
        let ttl = ints(&cmd(&s, &["HTTL", "h", "FIELDS", "3", "a", "c", "zz"]));
        assert!(ttl[0] > 48 && ttl[0] <= 50, "{ttl:?}");
        assert_eq!(&ttl[1..], [-1, -2]);
        let pttl = ints(&cmd(&s, &["HPTTL", "h", "FIELDS", "1", "b"]));
        assert!(pttl[0] > 98_000);
        let at = ints(&cmd(&s, &["HPEXPIRETIME", "h", "FIELDS", "1", "b"]))[0];
        assert!(at > crate::store::unix_ms_now() + 98_000);
        let at_s = ints(&cmd(&s, &["HEXPIRETIME", "h", "FIELDS", "1", "b"]))[0];
        assert!((at_s - at / 1000).abs() <= 1);
        assert_eq!(ints(&cmd(&s, &["HPERSIST", "h", "FIELDS", "3", "a", "c", "zz"])), [1, -1, -2]);
        assert_eq!(ints(&cmd(&s, &["HPEXPIRE", "h", "0", "FIELDS", "1", "c"])), [2]);
        assert_eq!(cmd(&s, &["HEXISTS", "h", "c"]), Frame::integer(0));
        let past = (crate::store::unix_ms_now() / 1000 - 10).to_string();
        assert_eq!(ints(&cmd(&s, &["HEXPIREAT", "h", &past, "FIELDS", "1", "a"])), [2]);
        assert_eq!(ints(&cmd(&s, &["HTTL", "nokey", "FIELDS", "1", "a"])), [-2]);
        // Argument errors, with Redis' wording.
        assert_eq!(
            err_text(&cmd(&s, &["HEXPIRE", "h", "10", "FIELDS", "2", "a"])),
            "ERR The `numfields` parameter must match the number of arguments"
        );
        assert_eq!(
            err_text(&cmd(&s, &["HEXPIRE", "h", "10", "FIELDS", "0", "a"])),
            "ERR Parameter `numFields` should be greater than 0"
        );
        assert_eq!(
            err_text(&cmd(&s, &["HEXPIRE", "h", "10", "NX", "XX", "FIELDS", "1", "a"])),
            "ERR Mandatory argument FIELDS is missing or not at the right position"
        );
        assert_eq!(
            err_text(&cmd(&s, &["HEXPIRE", "h", "-1", "FIELDS", "1", "a"])),
            "ERR invalid expire time, must be >= 0"
        );
        assert_eq!(
            err_text(&cmd(&s, &["HPEXPIREAT", "h", &i64::MAX.to_string(), "FIELDS", "1", "a"])),
            "ERR invalid expire time in 'hpexpireat' command"
        );
    }

    #[test]
    fn hash_key_level_expiry_via_expire_and_ttl() {
        let s = store();
        cmd(&s, &["HSET", "h", "f", "v"]);
        assert_eq!(cmd(&s, &["EXPIRE", "h", "100"]), Frame::integer(1));
        assert!(int(&cmd(&s, &["TTL", "h"])).unwrap() > 98);
        assert_eq!(cmd(&s, &["PERSIST", "h"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["TTL", "h"]), Frame::integer(-1));
        assert_eq!(cmd(&s, &["TTL", "missing"]), Frame::integer(-2));
    }

    #[test]
    fn scan_and_hscan() {
        let s = store();
        for i in 0..200 {
            cmd(&s, &["SET", &format!("user:{i}"), "v"]);
        }
        cmd(&s, &["SET", "other", "v"]);
        cmd(&s, &["HSET", "user:h", "f1", "a", "f2", "b"]);
        let mut cursor = "0".to_string();
        let mut seen = std::collections::HashSet::new();
        loop {
            let Frame::Array(reply) = cmd(&s, &["SCAN", &cursor, "MATCH", "user:*", "COUNT", "5"])
            else {
                panic!("SCAN must reply with an array");
            };
            cursor = bulk_str(&reply[0]).unwrap().to_string();
            for k in strs(&reply[1]) {
                seen.insert(k.unwrap());
            }
            if cursor == "0" {
                break;
            }
        }
        assert_eq!(seen.len(), 201);
        assert!(!seen.contains("other"));
        let Frame::Array(reply) = cmd(&s, &["SCAN", "0", "TYPE", "hash", "COUNT", "1000"]) else {
            panic!("SCAN must reply with an array")
        };
        assert!(strs(&reply[1]).contains(&Some("user:h".into())));
        assert_eq!(err_text(&cmd(&s, &["SCAN", "abc"])), "ERR invalid cursor");
        assert_eq!(err_text(&cmd(&s, &["SCAN", "0", "COUNT", "0"])), "ERR syntax error");
        assert_eq!(err_text(&cmd(&s, &["SCAN", "0", "MATCH"])), "ERR syntax error");

        let Frame::Array(reply) = cmd(&s, &["HSCAN", "user:h", "0", "MATCH", "f1"]) else {
            panic!("HSCAN must reply with an array")
        };
        assert_eq!(bulk_str(&reply[0]), Some("0"));
        assert_eq!(strs(&reply[1]), vec![Some("f1".into()), Some("a".into())]);
        let Frame::Array(reply) = cmd(&s, &["HSCAN", "user:h", "0", "NOVALUES"]) else {
            panic!("HSCAN must reply with an array")
        };
        assert_eq!(strs(&reply[1]).len(), 2);
    }

    #[test]
    fn info_reports_keyspace_and_hit_stats() {
        let s = store();
        cmd(&s, &["SET", "k", "v", "EX", "100"]);
        cmd(&s, &["GET", "k"]);
        cmd(&s, &["GET", "missing"]);
        let info = cmd(&s, &["INFO"]);
        let text = bulk_str(&info).unwrap();
        assert!(text.contains("db0:keys=1,expires=1"), "{text}");
        assert!(text.contains("keyspace_hits:1"), "{text}");
        assert!(text.contains("keyspace_misses:1"), "{text}");
    }

    /// A clustered hash field larger than the gossip payload is refused with
    /// a clear error — never stored node-locally.
    #[test]
    fn oversized_hash_field_in_a_cluster_is_an_error() {
        #[derive(Debug)]
        struct Limited;
        impl crate::store::Replicator for Limited {
            fn replicate_set(&self, _: String, _: Vec<u8>, _: Option<Duration>) -> bool {
                true
            }
            fn replicate_remove(&self, _: &str) -> bool {
                true
            }
            fn replicate_expire(&self, _: &str, _: Duration) -> bool {
                true
            }
            fn replicate_published(&self, _: String, _: Vec<u8>, _: Option<Duration>) {}
            fn replicate_remove_published(&self, _: String) {}
            fn replicate_hash_field(
                &self,
                _: &str,
                _: &str,
                apply: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
            ) -> bool {
                let _ = apply();
                true
            }
            fn hash_field_value_limit(&self) -> Option<usize> {
                Some(8)
            }
        }
        let s = store();
        s.set_replicator(Some(Arc::new(Limited)));
        let reply = cmd(&s, &["HSET", "h", "small", "ok", "big", "0123456789"]);
        assert!(err_text(&reply).contains("exceeds the 8-byte limit"), "{reply:?}");
        assert_eq!(cmd(&s, &["HLEN", "h"]), Frame::integer(0), "nothing written");
        assert_eq!(cmd(&s, &["HSET", "h", "small", "ok"]), Frame::integer(1));
    }

    /// A replicator standing in for a cluster node that cannot establish any
    /// key's current value: every resolution answers `Unavailable`, and the
    /// node is warm for plain reads.
    #[derive(Debug)]
    struct Unresolvable;
    impl crate::store::Replicator for Unresolvable {
        fn replicate_set(&self, _: String, _: Vec<u8>, _: Option<Duration>) -> bool {
            panic!("a refused operation must not write");
        }
        fn replicate_remove(&self, _: &str) -> bool {
            true
        }
        fn replicate_expire(&self, _: &str, _: Duration) -> bool {
            panic!("a refused operation must not change an expiry");
        }
        fn replicate_published(&self, _: String, _: Vec<u8>, _: Option<Duration>) {
            panic!("a refused operation must not publish");
        }
        fn replicate_remove_published(&self, _: String) {}
        fn replicate_hash_field(
            &self,
            _: &str,
            _: &str,
            _: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
        ) -> bool {
            panic!("a refused hash operation must not write a field");
        }
        fn resolve_for_update(&self, _: &str, _: bool) -> crate::store::Resolved {
            crate::store::Resolved::Unavailable
        }
        fn resolve_hash_fields(&self, _: &str, _: &[&str]) -> crate::store::Resolved {
            crate::store::Resolved::Unavailable
        }
        fn resolve_hash_whole(&self, _: &str) -> crate::store::Resolved {
            crate::store::Resolved::Unavailable
        }
    }

    /// When a cluster node cannot establish a key's current value, every
    /// read-modify-write is refused with `TRYAGAIN` and changes nothing —
    /// computing from its own copy would publish a result that overwrites
    /// newer data everywhere (a counter reset, a large value replaced by an
    /// appended tail, a TTL change dropping the value). Blind writes still
    /// run, and per-field hash reads answer as misses, the way `GET` does.
    #[test]
    fn unresolvable_keys_refuse_read_modify_writes_and_miss_on_field_reads() {
        let s = store();
        // Seed through the local paths, before the replicator is installed.
        s.set("n".into(), b"5".to_vec(), None);
        s.set("s".into(), b"abc".to_vec(), None);
        s.hset("h", "f", b"v".to_vec()).unwrap();
        s.set_replicator(Some(Arc::new(Unresolvable)));

        for args in [
            &["INCR", "n"][..],
            &["DECRBY", "n", "2"],
            &["APPEND", "s", "x"],
            &["GETSET", "s", "y"],
            &["GETDEL", "s"],
            &["GETEX", "s", "EX", "10"],
            &["GETEX", "s", "PERSIST"],
            &["SET", "s", "z", "GET"],
            &["SET", "s", "z", "XX"],
            &["SET", "s", "z", "KEEPTTL"],
            &["EXPIRE", "s", "10"],
            &["PEXPIREAT", "s", "99999999999999"],
            &["PERSIST", "s"],
            &["RENAME", "s", "t"],
            &["HSET", "h", "f", "w"],
            &["HSETNX", "h", "g", "w"],
            &["HINCRBY", "h", "c", "1"],
            &["HINCRBYFLOAT", "h", "c", "1.5"],
            &["HEXPIRE", "h", "10", "FIELDS", "1", "f"],
            &["HPERSIST", "h", "FIELDS", "1", "f"],
            &["HGETALL", "h"],
            &["HKEYS", "h"],
            &["HVALS", "h"],
            &["HLEN", "h"],
            &["HSCAN", "h", "0"],
        ] {
            let reply = cmd(&s, args);
            assert_eq!(err_text(&reply), crate::store::KEY_UNAVAILABLE_ERROR, "{args:?}");
        }
        assert_eq!(s.get("n").as_deref(), Some(&b"5"[..]), "nothing changed");
        assert_eq!(s.get("s").as_deref(), Some(&b"abc"[..]), "nothing changed");
        assert_eq!(s.pttl("s"), Some(-1), "no expiry was set");
        assert_eq!(s.hget("h", "f").as_deref(), Some(&b"v"[..]), "nothing changed");
        assert_eq!(s.get("t"), None, "RENAME wrote nothing");

        // Per-field reads answer as misses rather than serve a field of
        // unknown freshness.
        assert_eq!(cmd(&s, &["HGET", "h", "f"]), Frame::Null);
        assert_eq!(cmd(&s, &["HMGET", "h", "f", "g"]), Frame::Array(vec![Frame::Null; 2]));
        assert_eq!(cmd(&s, &["HEXISTS", "h", "f"]), Frame::integer(0));
        assert_eq!(cmd(&s, &["HSTRLEN", "h", "f"]), Frame::integer(0));
        assert_eq!(
            cmd(&s, &["HPTTL", "h", "FIELDS", "1", "f"]),
            Frame::Array(vec![Frame::integer(-2)])
        );
        // Blind writes are not read-modify-writes and still run.
        assert_eq!(cmd(&s, &["DEL", "n"]), Frame::integer(1));
    }

    /// A resolution that answers `Current` (the default, and a warm cluster
    /// node holding the key) changes nothing about the commands' results.
    #[test]
    fn resolvable_keys_run_as_before() {
        #[derive(Debug)]
        struct Local(Arc<std::sync::atomic::AtomicUsize>);
        impl crate::store::Replicator for Local {
            fn replicate_set(&self, _: String, _: Vec<u8>, _: Option<Duration>) -> bool {
                true
            }
            fn replicate_remove(&self, _: &str) -> bool {
                true
            }
            fn replicate_expire(&self, _: &str, _: Duration) -> bool {
                true
            }
            fn replicate_published(&self, _: String, _: Vec<u8>, _: Option<Duration>) {}
            fn replicate_remove_published(&self, _: String) {}
            fn replicate_hash_field(
                &self,
                _: &str,
                _: &str,
                apply: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
            ) -> bool {
                let _ = apply();
                true
            }
            fn resolve_for_update(&self, _: &str, _: bool) -> crate::store::Resolved {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::store::Resolved::Current
            }
        }
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = store();
        s.set_replicator(Some(Arc::new(Local(Arc::clone(&asked)))));
        assert_eq!(cmd(&s, &["INCR", "n"]), Frame::integer(1));
        assert_eq!(cmd(&s, &["APPEND", "n", "0"]), Frame::integer(2));
        assert_eq!(bulk_str(&cmd(&s, &["GETSET", "n", "x"])), Some("10"));
        // (This fake publishes nowhere and materializes nothing, so the GETSET
        // above left the local copy at "10".)
        assert_eq!(bulk_str(&cmd(&s, &["SET", "n", "y", "GET"])), Some("10"));
        // A plain SET is blind: it never asks.
        let before = asked.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(cmd(&s, &["SET", "n", "z"]), Frame::ok());
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), before);
        assert_eq!(before, 4, "every read-modify-write asked once");
    }
}
