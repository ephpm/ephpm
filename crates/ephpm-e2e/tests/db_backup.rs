//! End-to-end `[db.sqlite.backup]` → object storage → `ephpm db restore`
//! round-trip against a **real S3-compatible server**.
//!
//! The unit tests in `ephpm-server` prove the snapshot copy and the retention
//! arithmetic, and the SigV4 signer is checked against an in-process mock.
//! What none of them prove is the whole feature talking to a real object
//! store: that the scheduled tick uploads an object a real `ListObjectsV2`
//! finds under the documented key layout, that `ephpm db restore` can fetch
//! it back with real credentials, and that the rows survive the trip. This
//! suite does exactly that, through the shipped binary:
//!
//! 1. Boot an S3-compatible server in a container (a **named** container on
//!    a **named** volume, both removed on every exit path — see
//!    [`ObjectStore`]), with a bucket pre-created.
//! 2. Seed a per-site database offline with `ephpm db import` (3 rows), then
//!    serve it with `[db.sqlite.backup] enabled = true, interval = "2s"` and
//!    `AWS_*` credentials in the server's environment.
//! 3. Wait for `ephpm db backup list --json` (the shipped client, real
//!    `ListObjectsV2`) to show an object, and assert its key is
//!    `<s3_path>/<site>/<YYYYMMDDTHHMMSSZ>.db.gz`.
//! 4. Write a 4th row over HTTP (`ephpm_db_execute` in the vhost's
//!    `index.php`) and wait for a snapshot taken after it; check
//!    `GET /_ephpm/db/backup` reports it.
//! 5. Stop the server, **delete the site's `.db`**, and `ephpm db restore
//!    --from s3://… --yes`: the newest object must bring back all 4 rows.
//!    Then `--at <first snapshot>` must bring back exactly the 3 seeded rows
//!    (point-in-time selection). Both read back offline through the engine.
//! 6. Serve again with `retention = "3s"`: the first object must be deleted
//!    by the sweep while the listing never empties (the newest is always
//!    kept), and `db restore` must refuse while the server is up.
//!
//! **Skips** (loudly, exit 0) when `EPHPM_BINARY` is unset, like every
//! self-managed suite, and when no container runtime answers — the ordinary
//! E2E CI legs run inside a container with no `podman`/`docker`, and a skip
//! there must not red the leg. Set `EPHPM_REQUIRE_CONTAINER_TESTS=1` to turn
//! a missing runtime into a failure (the local proof that the test ran).
//!
//! Knobs, all optional:
//!
//! * `EPHPM_E2E_CONTAINER_RUNTIME` — the runtime command (default: the first
//!   of `podman`, `docker` whose `info` succeeds). From a WSL distro driving
//!   the Windows podman machine this is `podman.exe`.
//! * `EPHPM_E2E_CONTAINER_HOST` — where the published port is reachable
//!   (default `127.0.0.1`). On a Windows host running the test natively, a
//!   podman-machine port is **not** on Windows loopback: pass the machine's
//!   IP (`wsl -d podman-machine-default -- ip -4 addr show eth0`).
//! * `EPHPM_E2E_S3_IMAGE` — the server image (default [`DEFAULT_IMAGE`]).
//!
//! The server is [versitygw](https://github.com/versity/versitygw) (Apache
//! 2.0): a plain SigV4 S3 gateway over a directory, where a bucket is a
//! directory — so the bucket is created with a `mkdir` on the volume and no
//! `CreateBucket` client is needed. MinIO was the first choice, but its
//! community images are no longer anonymously pullable from quay.io or
//! Docker Hub (both answer `unauthorized` as of 2026-11); the feature is
//! endpoint-agnostic (one SigV4 client, path-style addressing), which is the
//! point of running it against a second implementation anyway.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ephpm_e2e::{DbImportFixture, ephpm_binary_env, get_metrics};
use serde::Deserialize;

/// The S3-compatible server image. Pinned by tag; override with
/// `EPHPM_E2E_S3_IMAGE`.
const DEFAULT_IMAGE: &str = "docker.io/versity/versitygw:v1.10.0";
/// The port the gateway listens on inside the container.
const GATEWAY_PORT: u16 = 7070;
/// Named container and volume: identifiable in `podman ps -a` / `volume ls`,
/// and removed by [`ObjectStore::drop`] on every exit path.
const CONTAINER_NAME: &str = "ephpm-e2e-minio-backup";
const VOLUME_NAME: &str = "ephpm-e2e-minio-data";
/// Bucket, prefix and the credentials the gateway is started with.
const BUCKET: &str = "ephpm-e2e-backups";
const S3_PATH: &str = "e2e/db";
const ACCESS_KEY: &str = "ephpm-e2e-access-key";
const SECRET_KEY: &str = "ephpm-e2e-secret-key-not-for-production";
/// The site under test.
const SITE: &str = "shop";

/// The vhost's `index.php`: `?add=<name>` inserts a row through the
/// in-process bridge; otherwise it lists the rows.
const VHOST_INDEX_PHP: &str = r#"<?php
header('Content-Type: application/json');
try {
    if (isset($_GET['add'])) {
        $r = ephpm_db_execute('INSERT INTO customers (name, email) VALUES (?, ?)', [$_GET['add'], 'new@fake.test']);
        echo json_encode(['status' => 'ok', 'affected' => $r['affected_rows'] ?? null]);
        return;
    }
    $rows = ephpm_db_query('SELECT id, name FROM customers ORDER BY id');
    echo json_encode(['status' => 'ok', 'names' => array_map(static fn($r) => $r['name'], $rows ?: [])]);
} catch (\Throwable $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'message' => $e->getMessage()]);
}
"#;

/// Three obviously-fake rows, as a `mysqldump` the offline import takes.
fn fake_mysqldump() -> String {
    r#"-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
/*!40101 SET NAMES utf8mb4 */;
DROP TABLE IF EXISTS `customers`;
CREATE TABLE `customers` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `name` varchar(128) NOT NULL,
  `email` varchar(190) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
LOCK TABLES `customers` WRITE;
INSERT INTO `customers` VALUES (1,'Ada Fakeington','ada@fake.test'),(2,'Bram Testworth','bram@fake.test'),(3,'Cleo Mockby','cleo@fake.test');
UNLOCK TABLES;
-- Dump completed
"#
    .to_string()
}

/// The seeded names, in `id` order.
const SEEDED: [&str; 3] = ["Ada Fakeington", "Bram Testworth", "Cleo Mockby"];
/// The row written over HTTP after the first snapshot.
const LATE_ROW: &str = "Dora Newrow";

// ── container runtime ───────────────────────────────────────────────────────

/// The container runtime to use, or `None` when none answers.
fn container_runtime() -> Option<String> {
    let candidates: Vec<String> = match std::env::var("EPHPM_E2E_CONTAINER_RUNTIME") {
        Ok(v) if !v.trim().is_empty() => vec![v],
        _ => vec!["podman".to_string(), "docker".to_string()],
    };
    candidates.into_iter().find(|rt| {
        // `info` (not `--version`): a docker CLI with no daemon, or a podman
        // CLI with no machine, has a version but cannot run anything.
        Command::new(rt)
            .arg("info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Run `<runtime> <args>`; `Ok(stdout)` on success, the combined output in
/// the error otherwise.
fn rt(runtime: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(runtime)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{runtime} {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{runtime} {} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ))
    }
}

/// `GET /` over a raw socket; the HTTP status line on any answer. The e2e
/// crate's reqwest is async-only, and this runs before any runtime matters.
fn http_status_line(host: &str, port: u16) -> Result<String, String> {
    use std::io::{Read as _, Write as _};
    let addr = format!("{host}:{port}");
    let sockaddr = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| format!("{addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr}: no address"))?;
    let mut stream = std::net::TcpStream::connect_timeout(&sockaddr, Duration::from_secs(2))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .map_err(|e| format!("write {addr}: {e}"))?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read {addr}: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or_default();
    if line.starts_with("HTTP/1.") {
        Ok(line.to_string())
    } else {
        Err(format!("not HTTP: {line:?}"))
    }
}

/// A free loopback TCP port (bound and released; the race with another
/// process is the usual one).
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    l.local_addr().expect("local_addr").port()
}

/// The running S3-compatible server. **Dropping it removes the container
/// and the volume** — on success, on an assertion failure (the test harness
/// unwinds, so locals drop) and on an early `?`/panic alike.
struct ObjectStore {
    runtime: String,
    /// `http://<host>:<port>` — the `[db.sqlite.backup] endpoint`.
    endpoint: String,
}

impl ObjectStore {
    /// Start the gateway with `BUCKET` pre-created, and wait until it answers
    /// HTTP. Fails (never leaves the container behind) if it does not come up.
    fn start(runtime: &str) -> Result<Self, String> {
        let image = std::env::var("EPHPM_E2E_S3_IMAGE")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
        let host_override =
            std::env::var("EPHPM_E2E_CONTAINER_HOST").ok().filter(|v| !v.is_empty());

        // A previous run that died without unwinding (SIGKILL) may have left
        // these; the names are ours, so start clean.
        Self::remove(runtime);

        // The guard exists from here on, so every failure below cleans up.
        let port = free_port();
        let host = host_override.clone().unwrap_or_else(|| "127.0.0.1".to_string());
        let store =
            Self { runtime: runtime.to_string(), endpoint: format!("http://{host}:{port}") };

        rt(runtime, &["volume", "create", VOLUME_NAME])?;
        // A bucket is a directory under the posix backend's root; create it
        // on the volume before the gateway starts, with the image's own
        // `mkdir` (no second image, no CreateBucket client).
        let data_mount = format!("{VOLUME_NAME}:/data");
        let bucket_dir = format!("/data/{BUCKET}");
        rt(
            runtime,
            &["run", "--rm", "-v", &data_mount, "--entrypoint", "mkdir", &image, "-p", &bucket_dir],
        )?;
        // Loopback-only unless the port has to be reached from another host
        // (the Windows-native case, where the machine IP is used).
        let publish = match host_override {
            Some(_) => format!("{port}:{GATEWAY_PORT}"),
            None => format!("127.0.0.1:{port}:{GATEWAY_PORT}"),
        };
        let access = format!("ROOT_ACCESS_KEY={ACCESS_KEY}");
        let secret = format!("ROOT_SECRET_KEY={SECRET_KEY}");
        // The gateway listens on `GATEWAY_PORT` by default; `posix /data` is
        // the directory backend.
        rt(
            runtime,
            &[
                "run",
                "-d",
                "--name",
                CONTAINER_NAME,
                "-p",
                &publish,
                "-v",
                &data_mount,
                "-e",
                &access,
                "-e",
                &secret,
                &image,
                "posix",
                "/data",
            ],
        )?;

        // Ready = any HTTP answer on `/` (an unsigned request gets 403; what
        // matters is that the listener is up and reachable from here).
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = String::new();
        while Instant::now() < deadline {
            match http_status_line(&host, port) {
                Ok(line) => {
                    eprintln!(
                        "==> object store up: {} answered `{line}` ({image})",
                        store.endpoint
                    );
                    return Ok(store);
                }
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let logs = rt(runtime, &["logs", CONTAINER_NAME]).unwrap_or_else(|e| e);
        Err(format!(
            "object store never answered at {} within 60s (last error: {last}); container logs:\n{logs}",
            store.endpoint
        ))
    }

    /// Remove the container and the volume, ignoring "no such" errors.
    fn remove(runtime: &str) {
        let _ = Command::new(runtime)
            .args(["rm", "-f", CONTAINER_NAME])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new(runtime)
            .args(["volume", "rm", "-f", VOLUME_NAME])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for ObjectStore {
    fn drop(&mut self) {
        Self::remove(&self.runtime);
        eprintln!("==> object store removed (container {CONTAINER_NAME}, volume {VOLUME_NAME})");
    }
}

// ── the shipped CLI as the S3 oracle ────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct Backup {
    key: String,
    taken_at_unix: u64,
    bytes: u64,
}

#[derive(Debug, Deserialize)]
struct Listing {
    site: String,
    location: String,
    backups: Vec<Backup>,
}

/// `ephpm db backup list --json` — a real `ListObjectsV2` through the
/// shipped client, against the configured bucket.
fn list_backups(fixture: &DbImportFixture) -> Listing {
    let out = fixture.db(&["backup", "list"], &["--json"]).expect("run db backup list");
    assert!(
        out.status.success(),
        "db backup list exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse listing json: {e}\n{stdout}"))
}

/// Poll the listing until `pred` holds, or fail after `timeout` showing the
/// last listing.
fn wait_for_listing(
    fixture: &DbImportFixture,
    what: &str,
    timeout: Duration,
    pred: impl Fn(&Listing) -> bool,
) -> Listing {
    let deadline = Instant::now() + timeout;
    loop {
        let listing = list_backups(fixture);
        if pred(&listing) {
            return listing;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}; last listing: {listing:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The `YYYYMMDDTHHMMSSZ` stamp an object key carries.
fn compact_stamp(key: &str) -> &str {
    key.rsplit('/').next().and_then(|n| n.strip_suffix(".db.gz")).expect("timestamped key")
}

/// Rows read back **offline** through the engine (`ephpm db query`), the
/// same path the server serves from.
fn names_offline(fixture: &DbImportFixture) -> Vec<String> {
    let out = fixture
        .query(&["--format", "json", "SELECT name FROM customers ORDER BY id"])
        .expect("run db query");
    assert!(
        out.status.success(),
        "db query exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<serde_json::Value> = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse query json: {e}\n{stdout}"));
    rows.iter().map(|r| r["name"].as_str().unwrap_or_default().to_string()).collect()
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove {}: {e}", Path::new(&p).display()),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("build http client")
}

// ── the test ────────────────────────────────────────────────────────────────

/// Backup → object storage → `db restore` round-trip, through the shipped
/// binary against a real S3-compatible server. See the module docs for the
/// phases.
#[tokio::test]
async fn backup_round_trips_through_object_storage() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping db_backup round-trip");
        return;
    };
    let require = std::env::var("EPHPM_REQUIRE_CONTAINER_TESTS").is_ok_and(|v| v == "1");
    let Some(runtime) = container_runtime() else {
        assert!(
            !require,
            "EPHPM_REQUIRE_CONTAINER_TESTS=1 but no container runtime answered `info` \
             (tried EPHPM_E2E_CONTAINER_RUNTIME, podman, docker)"
        );
        eprintln!(
            "!!! SKIPPING db_backup round-trip: no container runtime (podman/docker) answered \
             `info` on this host, so no S3-compatible server can be started. Set \
             EPHPM_REQUIRE_CONTAINER_TESTS=1 to make this a failure."
        );
        return;
    };
    eprintln!("==> container runtime: {runtime}");
    let store = match ObjectStore::start(&runtime) {
        Ok(s) => s,
        // A runtime that answers `info` but cannot actually provision is the
        // ephemerd CI runner: its docker shim advertises itself, yet `volume
        // create`/`run` are unimplemented. That must skip the leg, not red it —
        // same contract as "no runtime answered". `EPHPM_REQUIRE_CONTAINER_TESTS=1`
        // (the local proof the test ran) still turns it into a failure.
        Err(e) if !require => {
            eprintln!(
                "!!! SKIPPING db_backup round-trip: container runtime {runtime:?} answered \
                 `info` but could not start the S3-compatible server ({e}). Set \
                 EPHPM_REQUIRE_CONTAINER_TESTS=1 to make this a failure."
            );
            return;
        }
        Err(e) => panic!("could not start the S3-compatible server: {e}"),
    };

    // ── 2. seed offline, serve with backups on ──
    let backup_toml = format!(
        "[db.sqlite.backup]\nenabled = true\ns3_bucket = \"{BUCKET}\"\ns3_path = \"{S3_PATH}\"\n\
         interval = \"2s\"\nretention = \"1h\"\nendpoint = \"{}\"\nregion = \"us-east-1\"\n\
         path_style = true\n",
        store.endpoint
    );
    let env = [
        ("AWS_ACCESS_KEY_ID", ACCESS_KEY),
        ("AWS_SECRET_ACCESS_KEY", SECRET_KEY),
        ("AWS_DEFAULT_REGION", "us-east-1"),
    ];
    let mut fixture =
        DbImportFixture::prepare_with(&binary, SITE, VHOST_INDEX_PHP, &backup_toml, &env)
            .expect("prepare fixture");
    let dump = fixture.scratch_dir().join("customers.sql");
    std::fs::write(&dump, fake_mysqldump()).expect("write dump");
    let out =
        fixture.import(&dump, &["--dialect", "mysql", "--on-error", "skip"]).expect("run import");
    assert!(
        out.status.success(),
        "seed import exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(names_offline(&fixture), SEEDED, "the seed must be in place before serving");
    let db_path = fixture.db_path();
    assert!(db_path.is_file(), "{} should exist after the import", db_path.display());

    // Nothing in the bucket yet: the listing works and is empty.
    let listing = list_backups(&fixture);
    assert_eq!(listing.site, SITE);
    assert_eq!(listing.location, format!("s3://{BUCKET}/{S3_PATH}/{SITE}/"));
    assert!(listing.backups.is_empty(), "fresh bucket: {listing:?}");

    fixture.serve().await.expect("serve");

    // ── 3. the first scheduled tick uploads an object under the documented key ──
    let listing =
        wait_for_listing(&fixture, "the first backup object", Duration::from_secs(60), |l| {
            !l.backups.is_empty()
        });
    let first = listing.backups[0].clone();
    let expected_dir = format!("{S3_PATH}/{SITE}/");
    assert!(
        first.key.starts_with(&expected_dir) && first.key.ends_with(".db.gz"),
        "key layout must be <s3_path>/<site>/<ts>.db.gz: {}",
        first.key
    );
    let stamp = compact_stamp(&first.key);
    assert!(
        stamp.len() == 16
            && stamp.as_bytes()[8] == b'T'
            && stamp.as_bytes()[15] == b'Z'
            && stamp.bytes().enumerate().all(|(i, b)| i == 8 || i == 15 || b.is_ascii_digit()),
        "object name must be YYYYMMDDTHHMMSSZ.db.gz: {}",
        first.key
    );
    assert!(first.bytes > 0, "an empty object: {first:?}");
    assert!(
        first.taken_at_unix.abs_diff(now_unix()) < 600,
        "the object's timestamp must be now-ish: {first:?}"
    );
    eprintln!("==> first backup object: s3://{BUCKET}/{} ({} bytes)", first.key, first.bytes);

    // ── 4. a live write, then a snapshot taken after it ──
    let client = http();
    let resp = client
        .get(format!("{}/index.php?add={}", fixture.base_url(), urlencoding::encode(LATE_ROW)))
        .header("Host", SITE)
        .send()
        .await
        .expect("GET /index.php?add=");
    let status = resp.status();
    let body = resp.text().await.expect("body");
    assert_eq!(status, 200, "live insert returned {status}: {body}");
    assert!(body.contains("\"status\":\"ok\""), "live insert failed: {body}");
    let written_at = now_unix();
    // A snapshot stamped ≥ 2 s after the insert returned cannot have started
    // before it (the stamp is taken after a sub-second VACUUM of a tiny file).
    let listing = wait_for_listing(
        &fixture,
        "a snapshot taken after the live write",
        Duration::from_secs(60),
        |l| l.backups.iter().any(|b| b.taken_at_unix >= written_at + 2),
    );
    let after_write = listing
        .backups
        .iter()
        .find(|b| b.taken_at_unix >= written_at + 2)
        .cloned()
        .expect("just waited for it");
    assert!(after_write.taken_at_unix > first.taken_at_unix);
    assert!(
        listing.backups.windows(2).all(|w| w[0].taken_at_unix <= w[1].taken_at_unix),
        "listing is oldest first: {listing:?}"
    );

    // The status endpoint agrees with the bucket.
    let resp = get_metrics(&format!("{}/_ephpm/db/backup", fixture.base_url()))
        .await
        .expect("GET /_ephpm/db/backup");
    let status = resp.status();
    let body = resp.text().await.expect("status body");
    assert_eq!(status, 200, "/_ephpm/db/backup returned {status}: {body}");
    let doc: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("status json: {e}\n{body}"));
    assert_eq!(doc["bucket"], BUCKET, "{body}");
    assert_eq!(doc["prefix"], S3_PATH, "{body}");
    let last_ok = &doc["sites"][SITE]["last_ok"];
    assert!(
        last_ok["object"]
            .as_str()
            .is_some_and(|o| o.starts_with(&format!("s3://{BUCKET}/{expected_dir}"))),
        "status must name the site's last object: {body}"
    );
    assert!(doc["sites"][SITE]["last_error"].is_null(), "no backup error expected: {body}");
    assert!(doc["last_tick"]["ok"].as_u64().is_some_and(|n| n >= 1), "{body}");

    // ── 5. stop, delete the database, restore from the bucket ──
    fixture.stop();
    remove_db(&db_path);
    assert!(!db_path.exists());

    let from = format!("s3://{BUCKET}/{S3_PATH}");
    let out = fixture.db(&["restore"], &["--from", &from, "--yes"]).expect("run db restore");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "db restore (newest) exited {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(stdout.contains("restored "), "restore must report what it installed:\n{stdout}");
    assert!(db_path.is_file(), "the restore must have installed {}", db_path.display());
    let mut expected: Vec<String> = SEEDED.iter().map(|s| (*s).to_string()).collect();
    expected.push(LATE_ROW.to_string());
    assert_eq!(
        names_offline(&fixture),
        expected,
        "the newest backup must carry the seeded rows and the live write"
    );
    eprintln!("==> restore (newest) brought back {} rows: {expected:?}", expected.len());

    // Point in time: the first snapshot predates the live write.
    let at = compact_stamp(&first.key).to_string();
    let out = fixture
        .db(&["restore"], &["--from", &from, "--at", &at, "--yes"])
        .expect("run db restore --at");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "db restore --at {at} exited {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(stdout.contains(&first.key), "--at {at} must select the first object:\n{stdout}");
    assert_eq!(
        names_offline(&fixture),
        SEEDED,
        "the first backup must carry exactly the seeded rows (taken before the live write)"
    );
    eprintln!("==> restore --at {at} brought back the {} seeded rows", SEEDED.len());

    // ── 6. retention sweeps the oldest; restore refuses while serving ──
    let cfg = std::fs::read_to_string(fixture.config_path()).expect("read config");
    assert!(cfg.contains("retention = \"1h\""), "config template drifted:\n{cfg}");
    std::fs::write(fixture.config_path(), cfg.replace("retention = \"1h\"", "retention = \"3s\""))
        .expect("write config");
    fixture.serve().await.expect("serve again");

    let out =
        fixture.db(&["restore"], &["--from", &from, "--yes"]).expect("run db restore (server up)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "db restore must refuse while the server is up:\n{stderr}");
    assert!(stderr.contains("offline-only"), "the refusal must say why:\n{stderr}");

    let listing = wait_for_listing(
        &fixture,
        "the retention sweep to delete the first object",
        Duration::from_secs(60),
        |l| !l.backups.iter().any(|b| b.key == first.key),
    );
    assert!(!listing.backups.is_empty(), "the newest object is always kept: {listing:?}");
    assert!(
        listing.backups.iter().all(|b| b.taken_at_unix >= first.taken_at_unix),
        "only older objects are swept: {listing:?}"
    );
    eprintln!("==> retention swept {}; {} object(s) remain", first.key, listing.backups.len());

    fixture.stop();
    drop(store);
}
