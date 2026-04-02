//! RESP protocol compatibility tests.
//!
//! Spins up a real [`ephpm_kv`] TCP server on a random port and exercises
//! every supported command via the [`redis`] crate — the same client that
//! PHP's `predis` library uses under the hood. This validates the full
//! path: RESP framing → parser → command dispatch → store → serialised
//! response, as seen by a real Redis client.

use std::time::Duration;

use ephpm_kv::server;
use ephpm_kv::store::{Store, StoreConfig};
use redis::AsyncCommands;
use tokio::net::TcpListener;

// ── Test harness ─────────────────────────────────────────────────────────────

struct TestServer {
    addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener
            .local_addr()
            .expect("failed to get local addr")
            .to_string();

        let store = Store::new(StoreConfig::default());
        let handle = tokio::spawn(async move {
            server::serve_on(store, listener, 64 * 1024 * 1024, None)
                .await
                .ok();
        });

        // Give the accept loop a moment to become ready.
        tokio::time::sleep(Duration::from_millis(5)).await;

        Self { addr, handle }
    }

    async fn con(&self) -> redis::aio::MultiplexedConnection {
        let url = format!("redis://{}/", self.addr);
        redis::Client::open(url)
            .expect("invalid redis URL")
            .get_multiplexed_async_connection()
            .await
            .expect("failed to connect to test server")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ── Connection commands ───────────────────────────────────────────────────────

#[tokio::test]
async fn ping_returns_pong() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let resp: String = redis::cmd("PING").query_async(&mut con).await.unwrap();
    assert_eq!(resp, "PONG");
}

#[tokio::test]
async fn ping_with_message_echoes_back() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let resp: String = redis::cmd("PING")
        .arg("hello world")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(resp, "hello world");
}

#[tokio::test]
async fn echo_returns_argument() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let resp: String = redis::cmd("ECHO")
        .arg("testing 123")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(resp, "testing 123");
}

// ── GET / SET ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn set_and_get_round_trip() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("key", "value").await.unwrap();
    let got: String = con.get("key").await.unwrap();
    assert_eq!(got, "value");
}

#[tokio::test]
async fn get_missing_key_returns_nil() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let got: Option<String> = con.get("no_such_key").await.unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn set_overwrites_existing_value() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "first").await.unwrap();
    let _: () = con.set("k", "second").await.unwrap();
    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "second");
}

#[tokio::test]
async fn set_with_ex_applies_ttl_seconds() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    // Use SET ... EX rather than SETEX (we don't implement the SETEX alias).
    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .arg("EX")
        .arg(30)
        .query_async(&mut con)
        .await
        .unwrap();
    let ttl: i64 = con.ttl("k").await.unwrap();
    assert!(ttl > 0 && ttl <= 30, "expected TTL in (0, 30], got {ttl}");
}

#[tokio::test]
async fn set_with_px_applies_ttl_millis() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    // Use SET ... PX rather than PSETEX (we don't implement the PSETEX alias).
    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .arg("PX")
        .arg(30_000)
        .query_async(&mut con)
        .await
        .unwrap();
    let pttl: i64 = con.pttl("k").await.unwrap();
    assert!(
        pttl > 0 && pttl <= 30_000,
        "expected PTTL in (0, 30000], got {pttl}"
    );
}

#[tokio::test]
async fn set_nx_only_when_key_absent() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let first: bool = con.set_nx("k", "original").await.unwrap();
    assert!(first, "first SET NX should succeed");

    let second: bool = con.set_nx("k", "overwrite").await.unwrap();
    assert!(!second, "second SET NX should fail");

    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "original");
}

#[tokio::test]
async fn set_xx_only_when_key_present() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    // XX on absent key → Null (redis crate returns false/None).
    let absent: Option<String> = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .arg("XX")
        .query_async(&mut con)
        .await
        .unwrap();
    assert!(absent.is_none(), "SET XX on absent key should return nil");

    let _: () = con.set("k", "original").await.unwrap();

    let _present: Option<String> = redis::cmd("SET")
        .arg("k")
        .arg("updated")
        .arg("XX")
        .query_async(&mut con)
        .await
        .unwrap();

    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "updated");
}

#[tokio::test]
async fn set_get_option_returns_previous_value() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "old").await.unwrap();

    let prev: Option<String> = redis::cmd("SET")
        .arg("k")
        .arg("new")
        .arg("GET")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(prev.as_deref(), Some("old"));

    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "new");
}

// ── MGET / MSET / SETNX ──────────────────────────────────────────────────────

#[tokio::test]
async fn mset_and_mget_multiple_keys() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con
        .mset(&[("a", "1"), ("b", "2"), ("c", "3")])
        .await
        .unwrap();

    let got: Vec<Option<String>> = con.mget(&["a", "b", "c", "missing"]).await.unwrap();
    assert_eq!(got[0].as_deref(), Some("1"));
    assert_eq!(got[1].as_deref(), Some("2"));
    assert_eq!(got[2].as_deref(), Some("3"));
    assert!(got[3].is_none(), "missing key should be nil");
}

#[tokio::test]
async fn setnx_via_set_nx() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let first: i64 = redis::cmd("SETNX")
        .arg("k")
        .arg("v")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(first, 1);

    let second: i64 = redis::cmd("SETNX")
        .arg("k")
        .arg("other")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(second, 0);

    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "v");
}

// ── DEL / EXISTS ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn del_existing_key_returns_one() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let removed: i64 = con.del("k").await.unwrap();
    assert_eq!(removed, 1);

    let got: Option<String> = con.get("k").await.unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn del_missing_key_returns_zero() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let removed: i64 = con.del("nope").await.unwrap();
    assert_eq!(removed, 0);
}

#[tokio::test]
async fn del_multiple_keys_counts_removed() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.mset(&[("a", "1"), ("b", "2")]).await.unwrap();
    let removed: i64 = con.del(&["a", "b", "missing"]).await.unwrap();
    assert_eq!(removed, 2);
}

#[tokio::test]
async fn exists_present_and_absent() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let yes: i64 = con.exists("k").await.unwrap();
    assert_eq!(yes, 1);

    let no: i64 = con.exists("nope").await.unwrap();
    assert_eq!(no, 0);
}

#[tokio::test]
async fn exists_multiple_keys_returns_count() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.mset(&[("a", "1"), ("b", "2")]).await.unwrap();
    let count: i64 = con.exists(&["a", "b", "missing"]).await.unwrap();
    assert_eq!(count, 2);
}

// ── INCR / DECR / INCRBY / DECRBY ────────────────────────────────────────────

#[tokio::test]
async fn incr_creates_key_at_one() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let val: i64 = con.incr("counter", 1_i64).await.unwrap();
    assert_eq!(val, 1);
}

#[tokio::test]
async fn incr_increments_existing_value() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("n", "10").await.unwrap();
    let val: i64 = con.incr("n", 1_i64).await.unwrap();
    assert_eq!(val, 11);
}

#[tokio::test]
async fn decr_decrements_value() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("n", "5").await.unwrap();
    let val: i64 = con.decr("n", 1_i64).await.unwrap();
    assert_eq!(val, 4);
}

#[tokio::test]
async fn incrby_adds_delta() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("n", "10").await.unwrap();
    let val: i64 = con.incr("n", 5_i64).await.unwrap();
    assert_eq!(val, 15);
}

#[tokio::test]
async fn decrby_subtracts_delta() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("n", "10").await.unwrap();
    let val: i64 = con.decr("n", 3_i64).await.unwrap();
    assert_eq!(val, 7);
}

#[tokio::test]
async fn incr_on_non_integer_returns_error() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "hello").await.unwrap();
    let err = con.incr::<_, _, i64>("k", 1_i64).await;
    assert!(err.is_err(), "INCR on non-integer should error");
}

// ── APPEND / STRLEN / GETSET ──────────────────────────────────────────────────

#[tokio::test]
async fn append_creates_and_extends() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let len1: i64 = con.append("k", "hello").await.unwrap();
    assert_eq!(len1, 5);

    let len2: i64 = con.append("k", " world").await.unwrap();
    assert_eq!(len2, 11);

    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "hello world");
}

#[tokio::test]
async fn strlen_returns_byte_length() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "hello").await.unwrap();
    let len: i64 = redis::cmd("STRLEN")
        .arg("k")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(len, 5);
}

#[tokio::test]
async fn strlen_missing_key_is_zero() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let len: i64 = redis::cmd("STRLEN")
        .arg("nope")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(len, 0);
}

#[tokio::test]
async fn getset_returns_old_value_and_sets_new() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "old").await.unwrap();
    let prev: String = redis::cmd("GETSET")
        .arg("k")
        .arg("new")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(prev, "old");

    let got: String = con.get("k").await.unwrap();
    assert_eq!(got, "new");
}

// ── TTL / EXPIRE / PERSIST / TYPE ────────────────────────────────────────────

#[tokio::test]
async fn ttl_without_expiry_returns_minus_one() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let ttl: i64 = con.ttl("k").await.unwrap();
    assert_eq!(ttl, -1);
}

#[tokio::test]
async fn ttl_missing_key_returns_minus_two() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let ttl: i64 = con.ttl("nope").await.unwrap();
    assert_eq!(ttl, -2);
}

#[tokio::test]
async fn expire_sets_ttl_and_ttl_reads_it_back() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let set: bool = con.expire("k", 30_i64).await.unwrap();
    assert!(set);

    let ttl: i64 = con.ttl("k").await.unwrap();
    assert!(ttl > 0 && ttl <= 30, "expected TTL in (0, 30], got {ttl}");
}

#[tokio::test]
async fn expire_on_missing_key_returns_false() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let set: bool = con.expire("nope", 10_i64).await.unwrap();
    assert!(!set);
}

#[tokio::test]
async fn pexpire_sets_millisecond_ttl() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let set: bool = con.pexpire("k", 30_000_i64).await.unwrap();
    assert!(set);

    let pttl: i64 = con.pttl("k").await.unwrap();
    assert!(
        pttl > 0 && pttl <= 30_000,
        "expected PTTL in (0, 30000], got {pttl}"
    );
}

#[tokio::test]
async fn persist_removes_expiry() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .arg("EX")
        .arg(30)
        .query_async(&mut con)
        .await
        .unwrap();
    let removed: bool = con.persist("k").await.unwrap();
    assert!(removed);

    let ttl: i64 = con.ttl("k").await.unwrap();
    assert_eq!(ttl, -1, "TTL should be -1 after PERSIST");
}

#[tokio::test]
async fn persist_on_key_without_ttl_returns_false() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let removed: bool = con.persist("k").await.unwrap();
    assert!(!removed);
}

#[tokio::test]
async fn type_string_key() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.set("k", "v").await.unwrap();
    let t: String = redis::cmd("TYPE")
        .arg("k")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(t, "string");
}

#[tokio::test]
async fn type_missing_key_returns_none_string() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let t: String = redis::cmd("TYPE")
        .arg("nope")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(t, "none");
}

// ── KEYS / DBSIZE / FLUSHDB / FLUSHALL ───────────────────────────────────────

#[tokio::test]
async fn keys_wildcard_returns_all_keys() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.mset(&[("x", "1"), ("y", "2"), ("z", "3")]).await.unwrap();
    let mut keys: Vec<String> = con.keys("*").await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["x", "y", "z"]);
}

#[tokio::test]
async fn keys_prefix_pattern_filters() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con
        .mset(&[("user:1", "a"), ("user:2", "b"), ("post:1", "c")])
        .await
        .unwrap();

    let mut keys: Vec<String> = con.keys("user:*").await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["user:1", "user:2"]);
}

#[tokio::test]
async fn dbsize_counts_all_keys() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.mset(&[("a", "1"), ("b", "2"), ("c", "3")]).await.unwrap();
    let size: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
    assert_eq!(size, 3);
}

#[tokio::test]
async fn flushdb_removes_all_keys() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.mset(&[("a", "1"), ("b", "2")]).await.unwrap();
    let _: () = redis::cmd("FLUSHDB").query_async(&mut con).await.unwrap();

    let size: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
    assert_eq!(size, 0);
}

#[tokio::test]
async fn flushall_removes_all_keys() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let _: () = con.mset(&[("a", "1"), ("b", "2")]).await.unwrap();
    let _: () = redis::cmd("FLUSHALL").query_async(&mut con).await.unwrap();

    let size: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
    assert_eq!(size, 0);
}

// ── INFO ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn info_contains_server_section_and_memory() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let info: String = redis::cmd("INFO").query_async(&mut con).await.unwrap();
    assert!(info.contains("redis_version:"), "INFO missing redis_version");
    assert!(info.contains("used_memory:"), "INFO missing used_memory");
}

// ── Binary-safe values ────────────────────────────────────────────────────────

#[tokio::test]
async fn set_and_get_binary_value() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    let data: Vec<u8> = vec![0x00, 0x01, 0xFF, 0xFE, 0x80];
    let _: () = con.set("bin", data.clone()).await.unwrap();
    let got: Vec<u8> = con.get("bin").await.unwrap();
    assert_eq!(got, data);
}

// ── Multiple connections / pipelining ────────────────────────────────────────

#[tokio::test]
async fn two_connections_see_same_data() {
    let srv = TestServer::start().await;
    let mut con1 = srv.con().await;
    let mut con2 = srv.con().await;

    let _: () = con1.set("shared", "hello").await.unwrap();
    let got: String = con2.get("shared").await.unwrap();
    assert_eq!(got, "hello");
}

#[tokio::test]
async fn pipeline_set_then_get() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    // Pipeline: SET pk pv  (ignored), GET pk, SET pk2 pv2 (ignored).
    // Non-ignored responses are collected in order: just GET pk → "pv".
    let (got,): (String,) = redis::pipe()
        .cmd("SET").arg("pk").arg("pv").ignore()
        .cmd("GET").arg("pk")
        .cmd("SET").arg("pk2").arg("pv2").ignore()
        .query_async(&mut con)
        .await
        .unwrap();

    assert_eq!(got, "pv");
}

// ── Error responses ───────────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_command_returns_error() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let err = redis::cmd("NOTACOMMAND")
        .query_async::<String>(&mut con)
        .await;
    assert!(err.is_err(), "unknown command should return an error");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("NOTACOMMAND"),
        "error message should name the command, got: {msg}"
    );
}

#[tokio::test]
async fn wrong_arg_count_returns_error() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;
    let err = redis::cmd("GET").query_async::<String>(&mut con).await;
    assert!(err.is_err(), "GET with no args should return an error");
}

// ── AUTH tests ───────────────────────────────────────────────────────────────

/// Start a server with a password configured.
struct AuthTestServer {
    addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl AuthTestServer {
    async fn start(password: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener
            .local_addr()
            .expect("failed to get local addr")
            .to_string();

        let store = Store::new(StoreConfig::default());
        let pw = Some(password.to_string());
        let handle = tokio::spawn(async move {
            server::serve_on(store, listener, 64 * 1024 * 1024, pw)
                .await
                .ok();
        });

        tokio::time::sleep(Duration::from_millis(5)).await;

        Self { addr, handle }
    }
}

impl Drop for AuthTestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[tokio::test]
async fn auth_required_blocks_commands() {
    let srv = AuthTestServer::start("secret123").await;

    // Connect without auth — commands should be rejected with NOAUTH.
    let client = redis::Client::open(format!("redis://{}/", srv.addr)).unwrap();
    let mut con = client.get_multiplexed_async_connection().await.unwrap();

    let err = redis::cmd("PING").query_async::<String>(&mut con).await;
    assert!(err.is_err(), "PING without AUTH should fail");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("NOAUTH"),
        "expected NOAUTH error, got: {msg}"
    );
}

#[tokio::test]
async fn auth_correct_password_allows_commands() {
    let srv = AuthTestServer::start("secret123").await;

    // Connect with the correct password via URL.
    let url = format!("redis://:secret123@{}/", srv.addr);
    let client = redis::Client::open(url).unwrap();
    let mut con = client.get_multiplexed_async_connection().await.unwrap();

    let resp: String = redis::cmd("PING").query_async(&mut con).await.unwrap();
    assert_eq!(resp, "PONG");
}

#[tokio::test]
async fn auth_wrong_password_rejected() {
    let srv = AuthTestServer::start("secret123").await;

    // Connect with a wrong password — the redis crate sends AUTH during connect.
    let url = format!("redis://:wrongpass@{}/", srv.addr);
    let client = redis::Client::open(url).unwrap();
    let result = client.get_multiplexed_async_connection().await;

    assert!(result.is_err(), "wrong password should fail to connect");
}

#[tokio::test]
async fn auth_no_password_configured_accepts_anything() {
    // Use the normal TestServer (no password).
    let srv = TestServer::start().await;

    // AUTH with any password should succeed when no password is configured.
    let mut con = srv.con().await;
    let resp: String = redis::cmd("AUTH")
        .arg("anything")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(resp, "OK");
}
