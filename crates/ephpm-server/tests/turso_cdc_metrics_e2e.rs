//! Instrumentation proof for CDC-native Turso replication: the
//! `ephpm_cdc_*` series must appear, with correct values, in a **real
//! Prometheus scrape** produced by the production recorder.
//!
//! # Why this is not a mock test
//!
//! Every assertion below observes a metric recorded by production code:
//! the primary side runs [`ephpm_server::turso_cdc::serve_subscriber`],
//! the replica side runs
//! [`ephpm_server::turso_cdc::subscribe_and_consume`], and the scrape is
//! rendered by [`ephpm_server::metrics`] — the same `init()` the binary
//! calls and the same `render()` that answers `/metrics`. A test that
//! called `counter!` itself and then asserted the counter moved would
//! prove only that the `metrics` crate works.
//!
//! # Single-process caveat
//!
//! Both "nodes" live in one process, so they share one global metrics
//! registry: the primary-side and replica-side series are all present in
//! the same scrape. That is fine because the two sides use disjoint
//! metric names (`*_shipped_*` vs `*_applied_*`). It does mean the tests
//! here must not run concurrently with each other — they are serialized
//! by [`SERIAL`], since counters are process-global and a parallel test
//! would make a delta assertion meaningless.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, IncomingStream, maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig};
use ephpm_server::turso_cdc::{
    fetch_and_apply_snapshot, run_replica, serve_snapshot, serve_subscriber, subscribe_and_consume,
};
use ephpm_server::turso_cdc_metrics as cdc_metrics;
use litewire::backend::Backend;
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::read_watermark;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

const CDC_STREAM_TYPE: &str = "cdc/default";

/// Serializes the tests in this binary. Counters are process-global, so
/// two tests recording at once would break every delta assertion. An
/// async mutex because every test holds it across `.await` points.
static SERIAL: Mutex<()> = Mutex::const_new(());

/// The production Prometheus recorder, installed exactly once. `init()`
/// installs a *global* recorder and fails on a second call, so every test
/// in this binary shares this handle.
fn scrape_handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let handle = ephpm_server::metrics::init().expect("install prometheus recorder");
        // Seed the zero-valued CDC series exactly as
        // `start_clustered_turso_cdc` does at startup.
        cdc_metrics::init();
        handle
    })
}

// ---------------------------------------------------------------------------
// Scrape parsing. Deliberately a dumb text parser over the rendered
// payload rather than a peek at internal recorder state: if the series
// does not survive rendering into the Prometheus exposition format, it
// does not exist as far as an operator is concerned.
// ---------------------------------------------------------------------------

/// Value of `name{labels}` in a rendered scrape, or `None` if the series
/// is absent. `labels` are matched as a subset, so a caller need not
/// reproduce the exporter's label ordering.
fn metric_value(scrape: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    for line in scrape.lines() {
        if line.starts_with('#') {
            continue;
        }
        // A line with no space is not a sample; skip it rather than
        // ending the search (`?` here would abandon the whole scrape).
        let Some((series, value)) = line.rsplit_once(' ') else { continue };
        let (series_name, series_labels) = match series.split_once('{') {
            Some((n, rest)) => (n, rest.trim_end_matches('}')),
            None => (series, ""),
        };
        if series_name != name {
            continue;
        }
        if labels.iter().all(|(k, v)| series_labels.contains(&format!("{k}=\"{v}\""))) {
            return value.parse().ok();
        }
    }
    None
}

/// Like [`metric_value`] but fails the test with the full scrape attached
/// when the series is missing — "the metric never rendered" is the most
/// likely failure here and the least useful to debug from a bare `None`.
#[track_caller]
fn require_metric(scrape: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    metric_value(scrape, name, labels).unwrap_or_else(|| {
        let cdc_lines: Vec<&str> =
            scrape.lines().filter(|l| l.contains("ephpm_cdc") && !l.starts_with('#')).collect();
        panic!(
            "metric {name}{labels:?} did not appear in the scrape.\n\
             ephpm_cdc_* series present:\n{}",
            cdc_lines.join("\n")
        )
    })
}

/// [`require_metric`] as an integer. Every `ephpm_cdc_*` value is a
/// count or a `change_id`, so the cast is exact and equality assertions
/// can be written without float comparison.
#[track_caller]
fn require_metric_i64(scrape: &str, name: &str, labels: &[(&str, &str)]) -> i64 {
    require_metric(scrape, name, labels) as i64
}

// ---------------------------------------------------------------------------
// Two-node bring-up (mirrors turso_cdc_e2e.rs).
// ---------------------------------------------------------------------------

async fn start_channel(node_id: &str) -> (Arc<ephpm_cluster::ClusterHandle>, ChannelHandle) {
    let gossip_bind = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().to_string()
    };
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: gossip_bind,
        secret: "cdc-metrics-secret".to_string(),
        node_id: node_id.to_string(),
        cluster_id: "cdc-metrics".to_string(),
        ..ClusterConfig::default()
    };
    let cluster = Arc::new(start_gossip(&cluster_cfg).await.expect("gossip start"));
    let channel = maybe_start_cluster_channel(
        &ClusterChannelConfig { listen: Some("127.0.0.1:0".to_string()), secret: None },
        &cluster_cfg.secret,
        &cluster,
        ChannelFeatureFlags { cdc: true },
    )
    .await
    .expect("channel start")
    .expect("channel bound");
    (cluster, channel)
}

/// Primary side: dispatch every inbound stream into the production
/// `serve_subscriber`, which is what records the shipping metrics.
fn spawn_primary(mgmt: Arc<Turso>, channel: &ChannelHandle) -> std::net::SocketAddr {
    let mut cdc_streams = channel.register_exact(CDC_STREAM_TYPE);
    tokio::spawn(async move {
        while let Some(incoming) = cdc_streams.recv().await {
            let IncomingStream { stream, .. } = incoming;
            let mgmt = Arc::clone(&mgmt);
            tokio::spawn(async move {
                if let Err(e) = serve_subscriber(stream, &mgmt).await {
                    eprintln!("serve subscriber ended: {e:#}");
                }
            });
        }
    });
    channel.listen_addr()
}

/// The primary's CDC write head: `MAX(turso_cdc.change_id)`.
///
/// Read straight from the database rather than from
/// `ephpm_cdc_primary_head_change_id`, so the quiesce gate below is
/// anchored to a fact about the data and never to the instrumentation it
/// is about to assert on. Answers `0` while the log does not exist yet.
async fn primary_head(conn: &turso::Connection) -> i64 {
    let Ok(mut stmt) = conn.prepare("SELECT COALESCE(MAX(change_id), 0) FROM turso_cdc").await
    else {
        return 0;
    };
    let Ok(mut rows) = stmt.query(()).await else { return 0 };
    match rows.next().await {
        Ok(Some(row)) => match row.get_value(0) {
            Ok(turso::Value::Integer(i)) => i,
            _ => 0,
        },
        _ => 0,
    }
}

/// Replica side: dial once and run the production consume loop, which is
/// what records the apply metrics and the watermark gauge.
fn spawn_replica(
    mgmt: Arc<Turso>,
    primary_addr: std::net::SocketAddr,
    channel: ChannelHandle,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let apply_conn = mgmt.raw_connection().unwrap();
        loop {
            if let Ok(mut stream) = channel.dial(primary_addr, CDC_STREAM_TYPE).await {
                let wm = read_watermark(&apply_conn).await.unwrap_or(0);
                if let Err(e) = subscribe_and_consume(&mut stream, &apply_conn, wm).await {
                    eprintln!("replica stream ended: {e:#}");
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

async fn eventually<F, Fut>(mut check: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check().await
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// **HEADLINE**: a real two-node CDC flow moves every ship-side and
/// apply-side series, the watermark gauge equals the watermark actually
/// stored in the replica's database, and the lag gauge closes to zero
/// once the replica has caught up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_node_cdc_flow_renders_every_metric_in_a_real_scrape() {
    let _serial = SERIAL.lock().await;
    let handle = scrape_handle();

    // Counters are process-global and this binary may have run another
    // test first, so every counter assertion below is a delta.
    let before = handle.render();
    let shipped_before = require_metric(&before, cdc_metrics::METRIC_BATCHES_SHIPPED, &[]);
    let applied_before = require_metric(&before, cdc_metrics::METRIC_BATCHES_APPLIED, &[]);
    // The error counters too: another test in this binary deliberately
    // provokes an apply failure, so "no errors" here means "no *new*
    // errors", not an absolute zero.
    let apply_errors_before = require_metric_i64(&before, cdc_metrics::METRIC_APPLY_ERRORS, &[]);
    let tail_errors_before = require_metric_i64(&before, cdc_metrics::METRIC_TAIL_POLL_ERRORS, &[]);

    let primary_file = tempfile::NamedTempFile::new().unwrap();
    let replica_file = tempfile::NamedTempFile::new().unwrap();

    let (_pc, primary_channel) = start_channel("m-primary").await;
    let (_rc, replica_channel) = start_channel("m-replica").await;

    let primary_wire = Arc::new(
        Turso::builder(primary_file.path().to_str().unwrap())
            .enable_cdc_on_connect(true)
            .build()
            .await
            .unwrap(),
    );
    let primary_mgmt = Arc::new(Turso::open(primary_file.path().to_str().unwrap()).await.unwrap());
    let replica_mgmt = Arc::new(Turso::open(replica_file.path().to_str().unwrap()).await.unwrap());

    let primary_addr = spawn_primary(Arc::clone(&primary_mgmt), &primary_channel);
    let _replica = spawn_replica(Arc::clone(&replica_mgmt), primary_addr, replica_channel);

    // The subscriber gauge must reach 1 purely from the production
    // attach path — nothing in this test touches it.
    let attached = eventually(
        || async {
            metric_value(&handle.render(), cdc_metrics::METRIC_SUBSCRIBERS, &[]).map(|v| v as i64)
                == Some(1)
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(attached, "ephpm_cdc_subscribers never reached 1 after the replica subscribed");

    let session = primary_wire.connect().await.unwrap();
    session.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)", &[]).await.unwrap();
    for i in 1..=5 {
        session.execute(&format!("INSERT INTO posts VALUES ({i}, 'post-{i}')"), &[]).await.unwrap();
    }

    // Quiesce before sampling anything.
    //
    // This gate used to be `read_watermark(...) > 0`, which is satisfied
    // by the FIRST of the six batches this test produces — so every
    // assertion below raced the remaining five. That is what made the
    // suite flaky (observed both as "batches applied did not advance:
    // 0 -> 0" and as "applied_change_id gauge (10) disagrees with the
    // watermark actually stored in the replica database (12)"): the
    // scrape was rendered at one instant and compared against a database
    // read taken a few milliseconds later, with replication still moving
    // in between.
    //
    // The fix is to wait for the terminal state rather than to relax the
    // comparison. No writes are issued after this point, so once the
    // replica's durable watermark equals the primary's CDC head, nothing
    // further can arrive and every series below is stable.
    //
    // The gauge is included in the gate on purpose: `apply_batch` commits
    // the watermark row *inside* its own transaction and
    // `record_batch_applied` runs immediately after it returns, so the
    // gauge legitimately trails the database by microseconds. Waiting for
    // it to settle is synchronisation, not a weakened assertion — if the
    // gauge never agrees, this gate times out and fails with all three
    // values named.
    let primary_conn = primary_mgmt.raw_connection().unwrap();
    let replica_conn = replica_mgmt.raw_connection().unwrap();
    let converged = eventually(
        || async {
            let head = primary_head(&primary_conn).await;
            head > 0
                && read_watermark(&replica_conn).await.unwrap_or(-1) == head
                && metric_value(&handle.render(), cdc_metrics::METRIC_APPLIED_CHANGE_ID, &[])
                    .map(|v| v as i64)
                    == Some(head)
                && metric_value(&handle.render(), cdc_metrics::METRIC_SUBSCRIBERS, &[])
                    .map(|v| v as i64)
                    == Some(1)
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(
        converged,
        "replica never reached the primary's CDC head: primary head {}, replica watermark {}, \
         applied_change_id gauge {:?}, subscribers {:?}",
        primary_head(&primary_conn).await,
        read_watermark(&replica_conn).await.unwrap_or(-1),
        metric_value(&handle.render(), cdc_metrics::METRIC_APPLIED_CHANGE_ID, &[]),
        metric_value(&handle.render(), cdc_metrics::METRIC_SUBSCRIBERS, &[]),
    );

    // Let the ship-side idle path run at least once so the head gauge is
    // published from a caught-up tailer.
    let lag_closed = eventually(
        || async {
            metric_value(&handle.render(), cdc_metrics::METRIC_LAG_CHANGES, &[]).map(|v| v as i64)
                == Some(0)
        },
        Duration::from_secs(15),
    )
    .await;

    let scrape = handle.render();

    // --- ship side ---------------------------------------------------
    let shipped = require_metric(&scrape, cdc_metrics::METRIC_BATCHES_SHIPPED, &[]);
    assert!(
        shipped > shipped_before,
        "batches shipped did not advance: {shipped_before} -> {shipped}"
    );
    let rows_shipped = require_metric(&scrape, cdc_metrics::METRIC_ROWS_SHIPPED, &[]);
    assert!(rows_shipped > 0.0, "no CDC rows recorded as shipped");
    assert_eq!(require_metric_i64(&scrape, cdc_metrics::METRIC_SUBSCRIBERS, &[]), 1);
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_TAIL_POLL_ERRORS, &[]),
        tail_errors_before,
        "a healthy flow must record no new tail-poll errors"
    );

    // --- apply side --------------------------------------------------
    let applied = require_metric(&scrape, cdc_metrics::METRIC_BATCHES_APPLIED, &[]);
    assert!(
        applied > applied_before,
        "batches applied did not advance: {applied_before} -> {applied}"
    );
    let rows_applied = require_metric(&scrape, cdc_metrics::METRIC_ROWS_APPLIED, &[]);
    assert!(rows_applied > 0.0, "no CDC rows recorded as applied");
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_APPLY_ERRORS, &[]),
        apply_errors_before,
        "a healthy flow must record no new apply errors"
    );

    // The apply-duration histogram must have observations, not just a
    // registered name — a bucket set with a zero count is a phantom.
    let apply_count =
        require_metric(&scrape, &format!("{}_count", cdc_metrics::METRIC_APPLY_DURATION), &[]);
    assert!(apply_count > 0.0, "apply duration histogram recorded no observations");

    // --- the watermark gauge must equal reality ----------------------
    //
    // Three-way, not two: the gauge, the replica's own database, and the
    // primary's CDC head must all agree. Comparing the gauge only against
    // the database would be satisfiable by a gauge that is merely
    // self-consistent; pinning both to the primary's head proves the
    // replica actually finished the work the primary produced.
    let real_watermark = read_watermark(&replica_conn).await.unwrap();
    let expected_head = primary_head(&primary_conn).await;
    let gauge_watermark = require_metric_i64(&scrape, cdc_metrics::METRIC_APPLIED_CHANGE_ID, &[]);
    assert_eq!(
        gauge_watermark, real_watermark,
        "applied_change_id gauge ({gauge_watermark}) disagrees with the watermark actually \
         stored in the replica database ({real_watermark})"
    );
    assert_eq!(
        real_watermark, expected_head,
        "replica watermark ({real_watermark}) is not the primary's CDC head ({expected_head}) \
         even though the quiesce gate passed"
    );

    // --- lag ---------------------------------------------------------
    let head = require_metric_i64(&scrape, cdc_metrics::METRIC_HEAD_CHANGE_ID, &[]);
    let shipped_cursor = require_metric_i64(&scrape, cdc_metrics::METRIC_SHIPPED_CHANGE_ID, &[]);
    let lag = require_metric_i64(&scrape, cdc_metrics::METRIC_LAG_CHANGES, &[]);
    assert_eq!(
        head, expected_head,
        "head_change_id gauge ({head}) is not the primary's actual CDC head ({expected_head})"
    );
    // The documented identity: lag is a subtraction of two change_ids,
    // not an independently maintained number that could drift from them.
    assert_eq!(
        lag,
        head - shipped_cursor,
        "lag ({lag}) is not head ({head}) - shipped ({shipped_cursor})"
    );
    assert!(lag_closed && lag == 0, "lag did not close to 0 after convergence: {lag}");

    // The lag is measured in change-log rows, and the primary emitted at
    // least one row change per statement, so a converged replica's
    // shipped cursor must be at least as high as its applied watermark.
    assert!(
        shipped_cursor >= gauge_watermark,
        "shipped cursor {shipped_cursor} is behind the applied watermark {gauge_watermark}"
    );

    println!("--- ephpm_cdc_* series after a two-node flow ---");
    for line in scrape.lines().filter(|l| l.contains("ephpm_cdc") && !l.starts_with('#')) {
        println!("{line}");
    }
}

/// A failing `apply_batch` must increment the error counter **and leave
/// the watermark gauge alone** — a gauge that advanced past a failed
/// apply would report a convergence that did not happen.
///
/// Drives the production consume loop over a duplex pair, hand-writing
/// the wire frame as raw JSON so the test does not depend on the private
/// `Frame` type. `change_type = 99` is rejected by litewire's
/// `apply_batch_inner` as an unknown change type, which fails
/// deterministically without depending on SQL-layer behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_apply_increments_the_error_counter_and_freezes_the_watermark() {
    let _serial = SERIAL.lock().await;
    let handle = scrape_handle();

    let before = handle.render();
    let errors_before = require_metric_i64(&before, cdc_metrics::METRIC_APPLY_ERRORS, &[]);
    let applied_before = require_metric_i64(&before, cdc_metrics::METRIC_BATCHES_APPLIED, &[]);

    let replica_file = tempfile::NamedTempFile::new().unwrap();
    let replica = Turso::open(replica_file.path().to_str().unwrap()).await.unwrap();
    let apply_conn = replica.raw_connection().unwrap();

    let (mut fake_primary, mut replica_side) = tokio::io::duplex(1 << 20);

    // Production consume loop under test.
    let consumer = tokio::spawn(async move {
        let conn = apply_conn;
        subscribe_and_consume(&mut replica_side, &conn, 0).await
    });

    // Drain the Subscribe frame the consumer sends first.
    {
        use tokio::io::AsyncReadExt;
        let mut len = [0u8; 4];
        fake_primary.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        fake_primary.read_exact(&mut body).await.unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("Subscribe"), "first frame from the replica was {text}");
    }

    // A batch litewire cannot apply.
    let poison = br#"{"Batch":{"rows":[{"change_id":4242,"change_txn_id":null,"change_type":99,"table_name":"posts","id":1,"before":null,"after":null,"updates":null}]}}"#;
    let len = u32::try_from(poison.len()).unwrap();
    fake_primary.write_all(&len.to_be_bytes()).await.unwrap();
    fake_primary.write_all(poison).await.unwrap();
    fake_primary.flush().await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), consumer)
        .await
        .expect("consume loop hung on a bad batch")
        .expect("consume task panicked");
    let err = result.expect_err("an unknown change_type must fail the stream, not be skipped");
    assert!(
        err.to_string().contains("4242"),
        "error should name the failing change_id, got: {err:#}"
    );

    let scrape = handle.render();
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_APPLY_ERRORS, &[]),
        errors_before + 1,
        "apply error counter did not move"
    );
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_BATCHES_APPLIED, &[]),
        applied_before,
        "a failed batch must not count as applied"
    );
    // Subscribe published watermark 0; the failed apply must not have
    // advanced it to 4242.
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_APPLIED_CHANGE_ID, &[]),
        0,
        "the watermark gauge advanced past a batch that failed to apply"
    );
}

/// `init()` must register every zero-valued series, so a freshly booted
/// node's scrape distinguishes "replication is on and idle" from "this
/// build has no CDC instrumentation". Absent counters cannot say which.
#[tokio::test]
async fn startup_seeds_every_zero_valued_series() {
    let _serial = SERIAL.lock().await;
    let scrape = scrape_handle().render();

    for name in [
        cdc_metrics::METRIC_BATCHES_SHIPPED,
        cdc_metrics::METRIC_ROWS_SHIPPED,
        cdc_metrics::METRIC_TAIL_POLL_ERRORS,
        cdc_metrics::METRIC_SNAPSHOT_BYTES_SERVED,
        cdc_metrics::METRIC_BATCHES_APPLIED,
        cdc_metrics::METRIC_ROWS_APPLIED,
        cdc_metrics::METRIC_APPLY_ERRORS,
        cdc_metrics::METRIC_SNAPSHOT_BYTES_RECEIVED,
        cdc_metrics::METRIC_SUBSCRIBERS,
    ] {
        require_metric(&scrape, name, &[]);
    }
    for stream in ["cdc", "snapshot"] {
        require_metric(&scrape, cdc_metrics::METRIC_STREAMS_REFUSED, &[("stream", stream)]);
    }
    for status in ["ok", "error"] {
        require_metric(&scrape, cdc_metrics::METRIC_SNAPSHOTS_SERVED, &[("status", status)]);
    }
    for outcome in ["closed", "dial_error", "stream_error", "watermark_error"] {
        require_metric(&scrape, cdc_metrics::METRIC_REPLICA_CONNECTS, &[("outcome", outcome)]);
    }
    for outcome in ["ok", "skipped", "failed"] {
        require_metric(&scrape, cdc_metrics::METRIC_BOOTSTRAP, &[("outcome", outcome)]);
    }

    // The lag/head/shipped gauges are deliberately NOT seeded: a node
    // that has never replicated anything must not publish a lag of 0,
    // which reads as "fully caught up". They appear on first attach.
    // (If a prior test in this binary already attached a subscriber they
    // will be present, so this only asserts the seeding list above.)
}

/// A replica that cannot reach its primary must be visible as such. This
/// is the failure an operator most needs the counter for — the replica
/// logs the dial failure at `debug`, so without the metric a silently
/// disconnected replica looks identical to an idle one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_primary_increments_the_dial_error_counter() {
    let _serial = SERIAL.lock().await;
    let handle = scrape_handle();

    let before = require_metric_i64(
        &handle.render(),
        cdc_metrics::METRIC_REPLICA_CONNECTS,
        &[("outcome", "dial_error")],
    );

    let db = tempfile::NamedTempFile::new().unwrap();
    let mgmt = Arc::new(Turso::open(db.path().to_str().unwrap()).await.unwrap());
    let (_c, channel) = start_channel("m-orphan").await;

    // A port nothing is listening on: bind, read the address, drop.
    let dead_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };

    // The production replica driver, pointed at nothing.
    let driver = tokio::spawn(run_replica(mgmt, dead_addr, channel));

    let observed = eventually(
        || async {
            metric_value(
                &handle.render(),
                cdc_metrics::METRIC_REPLICA_CONNECTS,
                &[("outcome", "dial_error")],
            )
            .map(|v| v as i64)
            .is_some_and(|v| v > before)
        },
        Duration::from_secs(15),
    )
    .await;
    driver.abort();

    assert!(observed, "a replica dialing a dead primary recorded no dial_error");
}

/// The snapshot bootstrap path: a cold replica fetching a base snapshot
/// must move the served/received byte counters, the outcome counter, and
/// the transfer histogram — and land the watermark gauge on the
/// snapshot's watermark before any CDC batch arrives.
///
/// Drives the production [`serve_snapshot`] and
/// [`fetch_and_apply_snapshot`] over a real cluster channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_bootstrap_records_bytes_outcome_and_watermark() {
    let _serial = SERIAL.lock().await;
    let handle = scrape_handle();

    let before = handle.render();
    let served_before =
        require_metric_i64(&before, cdc_metrics::METRIC_SNAPSHOTS_SERVED, &[("status", "ok")]);
    let bytes_served_before =
        require_metric_i64(&before, cdc_metrics::METRIC_SNAPSHOT_BYTES_SERVED, &[]);
    let bytes_recv_before =
        require_metric_i64(&before, cdc_metrics::METRIC_SNAPSHOT_BYTES_RECEIVED, &[]);

    let primary_file = tempfile::NamedTempFile::new().unwrap();
    let replica_file = tempfile::NamedTempFile::new().unwrap();
    let (_pc, primary_channel) = start_channel("m-snap-primary").await;
    let (_rc, replica_channel) = start_channel("m-snap-replica").await;

    // Populate the primary BEFORE any replica exists, so the rows can
    // only reach the replica through the snapshot.
    let primary_wire = Turso::builder(primary_file.path().to_str().unwrap())
        .enable_cdc_on_connect(true)
        .build()
        .await
        .unwrap();
    let session = primary_wire.connect().await.unwrap();
    session.execute("CREATE TABLE seeded (id INTEGER PRIMARY KEY, v TEXT)", &[]).await.unwrap();
    for i in 1..=10 {
        session.execute(&format!("INSERT INTO seeded VALUES ({i}, 'v{i}')"), &[]).await.unwrap();
    }

    // Primary-side snapshot handler, running production `serve_snapshot`.
    let primary_mgmt = Arc::new(Turso::open(primary_file.path().to_str().unwrap()).await.unwrap());
    let mut snapshot_streams = primary_channel.register_exact("snapshot/default");
    tokio::spawn(async move {
        while let Some(incoming) = snapshot_streams.recv().await {
            let mgmt = Arc::clone(&primary_mgmt);
            tokio::spawn(async move {
                if let Err(e) = serve_snapshot(incoming.stream, &mgmt).await {
                    eprintln!("serve snapshot: {e:#}");
                }
            });
        }
    });

    // Replica side, running production `fetch_and_apply_snapshot`.
    let replica_mgmt = Turso::open(replica_file.path().to_str().unwrap()).await.unwrap();
    let replica_conn = replica_mgmt.raw_connection().unwrap();
    let watermark = fetch_and_apply_snapshot(
        &replica_conn,
        primary_channel.listen_addr(),
        &replica_channel,
        1 << 30,
    )
    .await
    .expect("snapshot bootstrap");

    let scrape = handle.render();
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_SNAPSHOTS_SERVED, &[("status", "ok")]),
        served_before + 1,
        "a successful snapshot was not counted"
    );
    assert!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_SNAPSHOT_BYTES_SERVED, &[])
            > bytes_served_before,
        "snapshot bytes served did not advance"
    );
    assert!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_SNAPSHOT_BYTES_RECEIVED, &[])
            > bytes_recv_before,
        "snapshot bytes received did not advance"
    );
    // Both sides of one transfer measured the same body.
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_SNAPSHOT_BYTES_SERVED, &[])
            - bytes_served_before,
        require_metric_i64(&scrape, cdc_metrics::METRIC_SNAPSHOT_BYTES_RECEIVED, &[])
            - bytes_recv_before,
        "served and received byte counts disagree for the same snapshot"
    );
    for role in ["serve", "fetch"] {
        let count = require_metric(
            &scrape,
            &format!("{}_count", cdc_metrics::METRIC_SNAPSHOT_DURATION),
            &[("role", role)],
        );
        assert!(count > 0.0, "snapshot duration histogram has no {role} observations");
    }
    // The gauge must reflect the seeded watermark, before CDC tails.
    assert_eq!(
        require_metric_i64(&scrape, cdc_metrics::METRIC_APPLIED_CHANGE_ID, &[]),
        watermark,
        "watermark gauge does not match the watermark the snapshot seeded"
    );
    assert_eq!(read_watermark(&replica_conn).await.unwrap(), watermark);
}
