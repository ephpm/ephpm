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
        Frame::Simple(s) => s.to_ascii_uppercase().into(),
        _ => return Frame::error("ERR invalid command name"),
    };

    let argv: Vec<&[u8]> = args
        .iter()
        .skip(1)
        .filter_map(|f| match f {
            Frame::Bulk(b) => Some(b.as_slice()),
            _ => None,
        })
        .collect();

    debug!(cmd = %cmd, argc = argv.len(), "executing command");

    execute(store, &cmd, &argv)
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
                Ok(v) if v > 0 => v as u64,
                _ => return Frame::error("ERR invalid expire time in 'expire' command"),
            };
            let ok = store.expire(&key, Duration::from_secs(secs));
            Frame::integer(i64::from(ok))
        }
        "PEXPIRE" => {
            check_args!(cmd, argv, 2);
            let key = str_from(argv[0]);
            let ms = match parse_i64(argv[1]) {
                Ok(v) if v > 0 => v as u64,
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

        _ => Frame::error(format!("ERR unknown command '{}'", cmd)),
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
                        ttl = Some(Duration::from_secs(sec as u64));
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
                        ttl = Some(Duration::from_millis(ms as u64));
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
            "ERR wrong number of arguments for '{}' command",
            cmd
        )))
    } else {
        Ok(())
    }
}
