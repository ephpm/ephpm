//! End-to-end: the cross-tenant database exploit (#274 / pentest C1), over the
//! **wire** path, against a real MySQL client.
//!
//! `site_wire_auth`'s unit tests drive the authenticator directly. This drives
//! it the way a tenant does — through `mysql_async` over TCP, with a real
//! handshake — because the thing being asserted is a property of the deployed
//! surface, not of a function call. `pdo_mysql` produces the same handshake.
//!
//! The shape under test is the one multi-site mode actually runs: ONE listener,
//! two databases, and per-site credentials deciding which one a connection
//! reaches.

use ephpm_server::site_backends::SiteBackends;
use ephpm_server::site_wire_auth::SiteWireAuth;
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder};

const SITE_A: &str = "site-a.test";
const SITE_B: &str = "site-b.test";

fn stats() -> ephpm_query_stats::QueryStats {
    ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
        enabled: false,
        slow_query_threshold: std::time::Duration::from_secs(1),
        max_digests: 16,
        metric_label_series_max: 16,
    })
}

/// Bring up exactly what `serve()` brings up in multi-site mode: a per-site
/// registry over `dir`, per-site credentials over it, and one authenticating
/// MySQL listener in front.
async fn start(dir: std::path::PathBuf) -> (SiteWireAuth, u16) {
    let backends = SiteBackends::new(dir, 8, stats(), tokio::runtime::Handle::current())
        .expect("build registry");
    let auth = SiteWireAuth::new(backends).expect("mint credentials");

    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("pick a port")
        .local_addr()
        .expect("local addr")
        .port();

    let server = litewire::LiteWire::with_authenticator(auth.as_authenticator())
        .mysql(&format!("127.0.0.1:{port}"));
    tokio::spawn(async move {
        let _ = server.serve().await;
    });

    (auth, port)
}

/// Connect as `user` with `password`. Retries only transport errors, so an
/// access-denied answer comes back immediately.
async fn connect(port: u16, user: &str, password: &str) -> Result<Conn, mysql_async::Error> {
    let opts: Opts = OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some(user))
        .pass(Some(password))
        .into();

    let mut last = None;
    for _ in 0..40 {
        match Conn::new(opts.clone()).await {
            Ok(conn) => return Ok(conn),
            Err(e) if e.to_string().contains("Authenticate failed") => return Err(e),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

fn assert_denied(err: &mysql_async::Error, what: &str) {
    let msg = err.to_string();
    assert!(
        msg.contains("28000") && msg.contains("Authenticate failed"),
        "{what}: expected an access-denied handshake error, got: {msg}"
    );
}

/// The C1 exploit, over the wire.
///
/// Site A writes a secret through `pdo_mysql`. Site B, using its own valid
/// credentials, must not be able to read it — it is in a different database
/// file, so the table does not even exist for B.
#[tokio::test]
async fn site_b_cannot_read_site_as_secret_over_the_wire() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (auth, port) = start(tmp.path().to_path_buf()).await;

    let mut a = connect(port, SITE_A, &auth.password_for(SITE_A))
        .await
        .expect("site-a's own credentials must work");
    a.query_drop("CREATE TABLE secrets (v TEXT)").await.expect("create");
    a.query_drop("INSERT INTO secrets (v) VALUES ('a-only')").await.expect("insert");

    // Site B, fully authenticated as itself, looks for A's table.
    let mut b = connect(port, SITE_B, &auth.password_for(SITE_B))
        .await
        .expect("site-b's own credentials must work");
    let err = b
        .query::<String, _>("SELECT v FROM secrets")
        .await
        .expect_err("site-b must not see site-a's table");
    assert!(
        err.to_string().to_ascii_lowercase().contains("no such table"),
        "expected a missing-table error (separate databases), got: {err}"
    );

    // And the two really are different files on disk.
    assert!(tmp.path().join("site-a.test.db").exists());
    let mut b2 = connect(port, SITE_B, &auth.password_for(SITE_B)).await.expect("site-b again");
    b2.query_drop("CREATE TABLE secrets (v TEXT)").await.expect("b creates its own");
    b2.query_drop("INSERT INTO secrets (v) VALUES ('b-only')").await.expect("insert");
    let b_rows: Vec<String> = b2.query("SELECT v FROM secrets").await.expect("select");
    assert_eq!(b_rows, vec!["b-only".to_string()], "site-b must see only its own row");

    let mut a2 = connect(port, SITE_A, &auth.password_for(SITE_A)).await.expect("site-a again");
    let a_rows: Vec<String> = a2.query("SELECT v FROM secrets").await.expect("select");
    assert_eq!(a_rows, vec!["a-only".to_string()], "site-a must be unaffected by site-b's writes");
}

/// **The security test.** Site B does not merely fail to see A's data by
/// accident — it actively tries to reach A's database and is refused.
///
/// This is the case that separates a real boundary from an accident of
/// routing: B knows A's site name (names are public, they are hostnames) and
/// deliberately claims it.
#[tokio::test]
async fn site_b_impersonating_site_a_is_denied() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (auth, port) = start(tmp.path().to_path_buf()).await;

    // Seed A so there is something worth stealing.
    let mut a = connect(port, SITE_A, &auth.password_for(SITE_A)).await.expect("site-a");
    a.query_drop("CREATE TABLE secrets (v TEXT)").await.expect("create");
    a.query_drop("INSERT INTO secrets (v) VALUES ('a-only')").await.expect("insert");
    drop(a);

    // 1. Claim A's username with the only password B has.
    let err = connect(port, SITE_A, &auth.password_for(SITE_B))
        .await
        .expect_err("site-a's name with site-b's password must be refused");
    assert_denied(&err, "impersonation with a neighbour's password");

    // 2. Claim A's username with no password.
    let err = connect(port, SITE_A, "")
        .await
        .expect_err("site-a's name with no password must be refused");
    assert_denied(&err, "impersonation with an empty password");

    // 3. Claim A's username with a guessed password. The derivation is HMAC
    //    over a secret the tenant never sees, so guessing is the only option
    //    available and it does not work.
    for guess in ["password", SITE_A, "0", &"f".repeat(64)] {
        match connect(port, SITE_A, guess).await {
            Ok(_) => panic!("guessed password {guess:?} authenticated as {SITE_A}"),
            Err(e) => assert_denied(&e, "guessed password"),
        }
    }
}

/// A tenant cannot invent a *new* database by naming one, nor escape `dir`.
#[tokio::test]
async fn unknown_and_traversal_usernames_are_denied() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (auth, port) = start(tmp.path().to_path_buf()).await;

    // An unregistered-but-valid site name: refused, because the caller cannot
    // produce a password it has never been given. (Note the derivation is
    // defined for any key — what stops this is the secret, not a registry.)
    let err = connect(port, "not-a-site.test", &auth.password_for(SITE_A))
        .await
        .expect_err("a site name with the wrong password must be refused");
    assert_denied(&err, "unknown site");

    // Traversal-shaped names are refused before any path is derived from them,
    // even when the client somehow holds the correctly-derived password.
    for user in ["../../etc/passwd", "a/b"] {
        let err = connect(port, user, &auth.password_for(user))
            .await
            .expect_err("a traversal-shaped username must be refused");
        assert_denied(&err, "traversal username");
    }

    // Nothing was created outside the per-site directory.
    let created: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert!(
        created.iter().all(|n| n.starts_with("site-") || n.is_empty()),
        "a refused connection created files: {created:?}"
    );
}

/// A failed authentication must not open (or create) the target's database.
///
/// Verification runs before resolution precisely so that an unauthenticated
/// caller cannot make the server touch files by naming them — otherwise the
/// handshake becomes an oracle for which sites exist, and a way to churn the
/// open-database LRU.
#[tokio::test]
async fn a_refused_connection_does_not_open_the_targets_database() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (auth, port) = start(tmp.path().to_path_buf()).await;

    let err = connect(port, SITE_A, &auth.password_for(SITE_B)).await.expect_err("denied");
    assert_denied(&err, "pre-resolution rejection");

    assert!(
        !tmp.path().join("site-a.test.db").exists(),
        "a refused connection created the target's database file"
    );
}
