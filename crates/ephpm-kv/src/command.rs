//! Redis command parsing and dispatch.
//!
//! Translates parsed RESP frames into operations on the [`Store`].

use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use crate::resp::Frame;
use crate::store::Store;

/// Early-return an error frame if `argv` has fewer than `n` elements.
macro_rules! check_args {
    ($cmd:expr, $argv:expr, $n:expr) => {
        if let Err(e) = require_args($cmd, $argv, $n) {
            return e;
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
            Frame::Bulk(b) => Some(b.as_slice()),
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
            if db == "0" {
                Frame::ok()
            } else {
                Frame::error("ERR DB index is out of range")
            }
        }
        "QUIT" => Frame::ok(),
        "COMMAND" => {
            // Minimal stub: return empty array.
            Frame::Array(vec![])
        }

        // ── String operations ────────────────────────────────────
        "GET" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            match store.get(&key) {
                Some(v) => Frame::bulk(v),
                None => Frame::Null,
            }
        }
        "SET" => cmd_set(store, argv),
        "MGET" => {
            if argv.is_empty() {
                return Frame::error("ERR wrong number of arguments for 'mget' command");
            }
            let results: Vec<Frame> = argv
                .iter()
                .map(|k| {
                    let key = str_from(k);
                    match store.get(&key) {
                        Some(v) => Frame::bulk(v),
                        None => Frame::Null,
                    }
                })
                .collect();
            Frame::Array(results)
        }
        "MSET" => {
            if argv.len() < 2 || argv.len() % 2 != 0 {
                return Frame::error("ERR wrong number of arguments for 'mset' command");
            }
            for pair in argv.chunks(2) {
                let key = str_from(pair[0]);
                store.set(key, pair[1].to_vec(), None);
            }
            Frame::ok()
        }
        "SETNX" => {
            check_args!(cmd, argv, 2);
            let key = str_from(argv[0]);
            if store.exists(&key) {
                Frame::integer(0)
            } else {
                store.set(key, argv[1].to_vec(), None);
                Frame::integer(1)
            }
        }
        "INCR" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            match store.incr_by(&key, 1) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "DECR" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            match store.incr_by(&key, -1) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "INCRBY" => {
            check_args!(cmd, argv, 2);
            let key = str_from(argv[0]);
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
            let key = str_from(argv[0]);
            let delta = match parse_i64(argv[1]) {
                Ok(v) => v,
                Err(f) => return f,
            };
            match store.incr_by(&key, -delta) {
                Ok(v) => Frame::integer(v),
                Err(e) => Frame::error(e),
            }
        }
        "APPEND" => {
            check_args!(cmd, argv, 2);
            let key = str_from(argv[0]);
            let new_len = store.append(&key, argv[1]);
            Frame::integer(i64::try_from(new_len).unwrap_or(i64::MAX))
        }
        "STRLEN" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            match store.get(&key) {
                Some(v) => Frame::integer(i64::try_from(v.len()).unwrap_or(i64::MAX)),
                None => Frame::integer(0),
            }
        }
        "GETSET" => {
            check_args!(cmd, argv, 2);
            let key = str_from(argv[0]);
            let old = store.get(&key);
            store.set(key, argv[1].to_vec(), None);
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
            let removed: i64 = argv
                .iter()
                .filter(|k| store.remove(&str_from(k)))
                .count()
                .try_into()
                .unwrap_or(i64::MAX);
            Frame::integer(removed)
        }
        "EXISTS" => {
            if argv.is_empty() {
                return Frame::error("ERR wrong number of arguments for 'exists' command");
            }
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
            let key = str_from(argv[0]);
            let secs = match parse_i64(argv[1]) {
                Ok(v) if v > 0 => u64::try_from(v).unwrap_or(u64::MAX),
                _ => return Frame::error("ERR invalid expire time in 'expire' command"),
            };
            let ok = store.expire(&key, Duration::from_secs(secs));
            Frame::integer(i64::from(ok))
        }
        "PEXPIRE" => {
            check_args!(cmd, argv, 2);
            let key = str_from(argv[0]);
            let ms = match parse_i64(argv[1]) {
                Ok(v) if v > 0 => u64::try_from(v).unwrap_or(u64::MAX),
                _ => return Frame::error("ERR invalid expire time in 'pexpire' command"),
            };
            let ok = store.expire(&key, Duration::from_millis(ms));
            Frame::integer(i64::from(ok))
        }
        "TTL" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            match store.pttl(&key) {
                Some(ms) if ms >= 0 => Frame::integer(ms / 1000),
                Some(ms) => Frame::integer(ms), // -1 or -2
                None => Frame::integer(-2),
            }
        }
        "PTTL" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            match store.pttl(&key) {
                Some(ms) => Frame::integer(ms),
                None => Frame::integer(-2),
            }
        }
        "PERSIST" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            Frame::integer(i64::from(store.persist(&key)))
        }
        "TYPE" => {
            check_args!(cmd, argv, 1);
            let key = str_from(argv[0]);
            if store.exists(&key) {
                Frame::Simple("string".into())
            } else {
                Frame::Simple("none".into())
            }
        }
        "KEYS" => {
            check_args!(cmd, argv, 1);
            let pattern = str_from(argv[0]);
            let keys = store.keys(&pattern);
            Frame::Array(keys.into_iter().map(|k| Frame::bulk(k.into_bytes())).collect())
        }
        "DBSIZE" => Frame::integer(i64::try_from(store.len()).unwrap_or(i64::MAX)),
        "FLUSHDB" | "FLUSHALL" => {
            store.flush();
            Frame::ok()
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

/// Execute the SET command with options: SET key value [EX|PX|EXAT|PXAT seconds|ms] [NX|XX] [GET]
fn cmd_set(store: &Arc<Store>, argv: &[&[u8]]) -> Frame {
    if argv.len() < 2 {
        return Frame::error("ERR wrong number of arguments for 'set' command");
    }

    let key = str_from(argv[0]);
    let val = argv[1].to_vec();
    let mut ttl: Option<Duration> = None;
    let mut nx = false;
    let mut xx = false;
    let mut get = false;

    let mut i = 2;
    while i < argv.len() {
        let opt = str_from(argv[i]).to_uppercase();
        match opt.as_str() {
            "EX" => {
                if i + 1 >= argv.len() {
                    return Frame::error("ERR syntax error");
                }
                if let Ok(sec) = parse_i64(argv[i + 1]) {
                    if sec > 0 {
                        ttl = Some(Duration::from_secs(u64::try_from(sec).unwrap_or(u64::MAX)));
                    }
                }
                i += 2;
            }
            "PX" => {
                if i + 1 >= argv.len() {
                    return Frame::error("ERR syntax error");
                }
                if let Ok(ms) = parse_i64(argv[i + 1]) {
                    if ms > 0 {
                        ttl = Some(Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX)));
                    }
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

    let old = store.get(&key);

    // NX: Set only if the key does not exist
    if nx && old.is_some() {
        return Frame::Null;
    }
    // XX: Set only if the key exists
    if xx && old.is_none() {
        return Frame::Null;
    }

    store.set(key, val, ttl);

    if get {
        match old {
            Some(v) => Frame::bulk(v),
            None => Frame::Null,
        }
    } else {
        Frame::ok()
    }
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
    s.parse::<i64>()
        .map_err(|_| Frame::error("ERR value is not an integer or out of range"))
}

/// Return an error frame if `argv` doesn't have at least `n` elements.
/// Uses the `?` operator trick — returns `Frame` on error.
fn require_args(cmd: &str, argv: &[&[u8]], n: usize) -> Result<(), Frame> {
    if argv.len() < n {
        Err(Frame::error(format!(
            "ERR wrong number of arguments for '{cmd}' command"
        )))
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
        let frame = Frame::Array(
            args.iter()
                .map(|s| Frame::bulk(s.as_bytes().to_vec()))
                .collect(),
        );
        dispatch(store, &frame)
    }

    fn bulk_str(f: &Frame) -> Option<&str> {
        if let Frame::Bulk(b) = f {
            std::str::from_utf8(b).ok()
        } else {
            None
        }
    }

    fn int(f: &Frame) -> Option<i64> {
        if let Frame::Integer(n) = f { Some(*n) } else { None }
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
        assert!(matches!(
            cmd(&store(), &["SET", "k", "v", "NX", "XX"]),
            Frame::Error(_)
        ));
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
        assert_eq!(
            cmd(&store(), &["TYPE", "nope"]),
            Frame::Simple("none".into())
        );
    }

    // ── KEYS / DBSIZE / FLUSHDB ──────────────────────────────────────────

    #[test]
    fn keys_wildcard_returns_all() {
        let s = store();
        cmd(&s, &["MSET", "a", "1", "b", "2"]);
        let Frame::Array(keys) = cmd(&s, &["KEYS", "*"]) else {
            panic!("expected array")
        };
        assert!(keys.len() >= 2);
    }

    #[test]
    fn keys_pattern_filters() {
        let s = store();
        cmd(&s, &["MSET", "user:1", "a", "user:2", "b", "post:1", "c"]);
        let Frame::Array(keys) = cmd(&s, &["KEYS", "user:*"]) else {
            panic!("expected array")
        };
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

    #[test]
    fn incrby_non_integer_delta_is_error() {
        assert!(matches!(
            cmd(&store(), &["INCRBY", "k", "notanint"]),
            Frame::Error(_)
        ));
    }

    // ── Case insensitivity ───────────────────────────────────────────────

    #[test]
    fn commands_are_case_insensitive() {
        let s = store();
        assert_eq!(cmd(&s, &["set", "k", "v"]), Frame::ok());
        assert_eq!(bulk_str(&cmd(&s, &["get", "k"])), Some("v"));
        assert_eq!(cmd(&s, &["Set", "k2", "v2"]), Frame::ok());
    }
}
