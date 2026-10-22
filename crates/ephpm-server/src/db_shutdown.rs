//! Final checkpoint of every open embedded database at shutdown.
//!
//! # Why this exists
//!
//! The Turso Rust SDK has no `close()`: dropping its handles only releases
//! locks and memory maps. The engine's shutdown checkpoint (`checkpoint_shutdown`,
//! a `TRUNCATE` of the write-ahead log) runs only from an explicit close of a
//! database's *last* connection, which the SDK never performs. So without this
//! module every exit — graceful or not — left each database's `-wal` holding
//! every frame written since the WAL gate's last truncation, and the next start
//! had to recover it. That is the hot-WAL state `CLAUDE.md` warns about for
//! upgrades.
//!
//! [`checkpoint_all`] closes the gap: after the HTTP drain, while the runtime
//! is still running, it truncates the log of every database the process has
//! open (the same set the WAL-gate sampler walks), retrying a checkpoint that
//! reports `busy` until the readers that blocked it (CDC tailers, head
//! samplers, a forwarded statement) let go. A log is done when the checkpoint
//! reports nothing left and the `-wal` file is empty.
//!
//! # Bounds
//!
//! This is deliberately **not** bound by `[server.timeouts] shutdown`, which
//! covers request draining: a final checkpoint should finish, however long the
//! log is. Progress is logged at INFO once it runs past [`PROGRESS_AFTER`]. The
//! only bound is a last-resort [`GIVE_UP_AFTER`] for a checkpoint that can
//! genuinely never succeed (a reader that never ends); past it the process exits
//! anyway with a WARN naming each site whose log was left un-truncated — the
//! engine recovers such a log on the next open, the same as after a crash.
//!
//! Writes that land after a database's checkpoint (a replicated batch or a
//! forwarded statement from a peer that is still running) stay in its log and
//! are recovered on the next open like any other WAL frames; nothing is lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::wal_gate;

/// Pause between checkpoint attempts on a database whose last attempt was
/// blocked by a reader.
const RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// How many databases are checkpointed at once. A truncate is mostly
/// waiting on `fsync`, so a few in parallel overlap that wait; more would
/// only queue on the same disk.
const CHECKPOINT_CONCURRENCY: usize = 6;

/// After this long the pass logs which databases it is still waiting on.
const PROGRESS_AFTER: Duration = Duration::from_secs(3);

/// How many times a log that was refilled after its checkpoint is
/// checkpointed again before the pass accepts it.
const MAX_VERIFY_ROUNDS: u32 = 5;

/// How often the progress line repeats.
const PROGRESS_EVERY: Duration = Duration::from_secs(5);

/// Last-resort bound on the whole pass. Generous on purpose — it exists only
/// so a reader that never finishes cannot keep a stopped server alive forever.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10 * 60);

/// One database the pass still has to truncate.
#[derive(Debug, Clone)]
struct Pending {
    site: String,
    db_path: PathBuf,
}

/// Outcome of one checkpoint attempt.
enum Attempt {
    /// The log is empty: nothing left to recover on the next open.
    Done,
    /// A reader blocked the checkpoint, or the log is not empty yet: retry.
    Retry,
}

/// What [`checkpoint_all`] did, for logging and tests.
#[derive(Debug, Default)]
pub struct CheckpointReport {
    /// Databases whose log was truncated.
    pub truncated: usize,
    /// Sites whose log could not be truncated before the last-resort bound.
    pub abandoned: Vec<String>,
}

/// Truncate the write-ahead log of every embedded database this process has
/// open. Call once, after request draining, while the runtime still runs.
pub async fn checkpoint_all() -> CheckpointReport {
    let pending: Vec<Pending> = wal_gate::sample_all()
        .into_iter()
        .map(|s| Pending { site: s.site, db_path: s.db_path })
        .collect();
    checkpoint_databases(pending, GIVE_UP_AFTER).await
}

/// Truncate one database's write-ahead log, for a short-lived process that
/// opened `db_path` directly and is about to exit (`ephpm php --site` in its
/// offline mode). Same retry loop as [`checkpoint_all`], bounded by
/// `give_up_after`; the caller drops its own handles on the file first, so
/// nothing blocks the truncate and it normally finishes in one attempt.
pub async fn checkpoint_database(
    site: &str,
    db_path: &Path,
    give_up_after: Duration,
) -> CheckpointReport {
    let pending = vec![Pending { site: site.to_string(), db_path: db_path.to_path_buf() }];
    checkpoint_databases(pending, give_up_after).await
}

async fn checkpoint_databases(
    mut pending: Vec<Pending>,
    give_up_after: Duration,
) -> CheckpointReport {
    let mut report = CheckpointReport::default();
    if pending.is_empty() {
        return report;
    }
    let total = pending.len();
    let started = Instant::now();
    let mut next_progress = started + PROGRESS_AFTER;
    tracing::debug!(databases = total, "final WAL checkpoint of the embedded databases");

    let mut done: Vec<Pending> = Vec::with_capacity(total);
    let mut verify_rounds = 0;
    loop {
        if pending.is_empty() {
            // Verify: a straggling writer (a replicated batch that was
            // already being applied) can refill a log after its checkpoint.
            // Re-checkpoint any that did, a bounded number of times.
            let refilled: Vec<Pending> =
                done.iter().filter(|d| wal_gate::wal_file_size(&d.db_path) > 0).cloned().collect();
            if refilled.is_empty() || verify_rounds >= MAX_VERIFY_ROUNDS {
                break;
            }
            verify_rounds += 1;
            done.retain(|d| wal_gate::wal_file_size(&d.db_path) == 0);
            report.truncated = done.len();
            pending = refilled;
        }
        let mut still = Vec::with_capacity(pending.len());
        for (db, outcome) in checkpoint_round(pending).await {
            match outcome {
                Ok(Attempt::Done) => {
                    report.truncated += 1;
                    done.push(db);
                }
                Ok(Attempt::Retry) => still.push(db),
                Err(e) => {
                    // A database that cannot even be opened has nothing this
                    // pass can do for it; report it rather than spin.
                    tracing::warn!(
                        site = %db.site,
                        "final WAL checkpoint failed; its log is recovered on the next open: {e:#}"
                    );
                    report.abandoned.push(db.site);
                }
            }
        }
        pending = still;
        if pending.is_empty() {
            continue;
        }
        let now = Instant::now();
        if now.duration_since(started) >= give_up_after {
            let sites: Vec<&str> = pending.iter().map(|p| p.site.as_str()).collect();
            tracing::warn!(
                sites = ?sites,
                waited_secs = now.duration_since(started).as_secs(),
                "giving up on the final WAL checkpoint of these databases (a reader never \
                 finished); exiting with their logs un-truncated — the engine recovers them on \
                 the next open"
            );
            report.abandoned.extend(pending.into_iter().map(|p| p.site));
            break;
        }
        if now >= next_progress {
            let sites: Vec<&str> = pending.iter().map(|p| p.site.as_str()).take(10).collect();
            tracing::info!(
                closing = pending.len(),
                of = total,
                elapsed_secs = now.duration_since(started).as_secs(),
                waiting_on = ?sites,
                "closing embedded databases: final WAL checkpoint still waiting for readers"
            );
            next_progress = now + PROGRESS_EVERY;
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }

    let elapsed = started.elapsed();
    if elapsed >= PROGRESS_AFTER {
        tracing::info!(
            truncated = report.truncated,
            abandoned = report.abandoned.len(),
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "closing embedded databases: final WAL checkpoint finished"
        );
    } else {
        tracing::debug!(truncated = report.truncated, "final WAL checkpoint finished");
    }
    report
}

/// One checkpoint attempt on each of `pending`, at most
/// [`CHECKPOINT_CONCURRENCY`] at a time, returned in completion order.
///
/// Each attempt is its own task: an attempt runs its engine work under
/// `offload::engine`, whose nested `block_on` holds the polling task for
/// the whole attempt, so attempts polled together on one task would still
/// run one after another. A failed (panicked) task reports as an error for
/// its database rather than losing it.
async fn checkpoint_round(pending: Vec<Pending>) -> Vec<(Pending, anyhow::Result<Attempt>)> {
    let permits = Arc::new(Semaphore::new(CHECKPOINT_CONCURRENCY));
    let mut tasks = JoinSet::new();
    for db in pending.iter().cloned() {
        let permits = Arc::clone(&permits);
        tasks.spawn(async move {
            // The semaphore is never closed, so `acquire_owned` cannot fail.
            let _permit = permits.acquire_owned().await;
            let outcome = checkpoint_once(&db.db_path).await;
            (db, outcome)
        });
    }
    let mut out = Vec::with_capacity(pending.len());
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(result) => out.push(result),
            Err(e) => tracing::warn!("final WAL checkpoint task failed: {e}"),
        }
    }
    // A task that panicked returned nothing: report its database as failed.
    for db in pending {
        if !out.iter().any(|(done, _)| done.db_path == db.db_path) {
            out.push((db, Err(anyhow::anyhow!("checkpoint task failed"))));
        }
    }
    out
}

/// One `PRAGMA wal_checkpoint(TRUNCATE)` against `db_path`. Opens its own
/// engine handle: the engine coordinates every in-process handle on one file
/// through a shared WAL index, so a truncate here empties the log the serving
/// factories write to.
///
/// The open and the truncate copy and fsync up to the whole log, so they run
/// off the async workers (litewire's `offload::engine`).
async fn checkpoint_once(db_path: &Path) -> anyhow::Result<Attempt> {
    use anyhow::Context as _;

    if wal_gate::wal_file_size(db_path) == 0 {
        return Ok(Attempt::Done);
    }
    let path = db_path.to_str().context("database path is not UTF-8")?;
    let busy = litewire::litewire_turso::offload::engine(async {
        let db = turso::Builder::new_local(path).build().await.context("open for checkpoint")?;
        let conn = db.connect().context("connect for checkpoint")?;
        let mut rows = conn
            .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
            .await
            .context("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let busy = match rows.next().await.context("read checkpoint result")? {
            Some(row) => matches!(row.get_value(0)?, turso::Value::Integer(1)),
            None => false,
        };
        anyhow::Ok(busy)
    })
    .await?;
    if !busy && wal_gate::wal_file_size(db_path) == 0 {
        Ok(Attempt::Done)
    } else {
        Ok(Attempt::Retry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exec(conn: &turso::Connection, sql: &str) {
        conn.execute(sql, ()).await.unwrap();
    }

    /// The property shutdown relies on: a database the serving side still
    /// has open, with frames in its log, ends the pass with an empty `-wal`.
    #[tokio::test]
    async fn the_final_checkpoint_empties_a_live_databases_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.db");
        let serving = turso::Builder::new_local(path.to_str().unwrap()).build().await.unwrap();
        let conn = serving.connect().unwrap();
        exec(&conn, "CREATE TABLE t (x INTEGER)").await;
        for i in 0..50 {
            exec(&conn, &format!("INSERT INTO t VALUES ({i})")).await;
        }
        assert!(wal_gate::wal_file_size(&path) > 0, "precondition: the log has frames");

        let report = checkpoint_databases(
            vec![Pending { site: "s1".into(), db_path: path.clone() }],
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(report.truncated, 1);
        assert_eq!(report.abandoned, Vec::<String>::new());
        assert_eq!(wal_gate::wal_file_size(&path), 0, "the log must be empty at exit");

        // And nothing was lost: the rows are in the main file now.
        let mut rows = conn.query("SELECT COUNT(*) FROM t", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), turso::Value::Integer(50)));
    }

    /// A reader that holds the log open delays the checkpoint, never skips
    /// it: the pass keeps retrying and finishes once the reader lets go.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_busy_checkpoint_is_retried_until_the_reader_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s2.db");
        let serving = turso::Builder::new_local(path.to_str().unwrap()).build().await.unwrap();
        let writer = serving.connect().unwrap();
        exec(&writer, "CREATE TABLE t (x INTEGER)").await;
        exec(&writer, "INSERT INTO t VALUES (1)").await;

        let reader = serving.connect().unwrap();
        exec(&reader, "BEGIN").await;
        let mut rows = reader.query("SELECT x FROM t", ()).await.unwrap();
        rows.next().await.unwrap();
        drop(rows);

        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            exec(&reader, "COMMIT").await;
            reader
        });
        let started = std::time::Instant::now();
        let report = checkpoint_databases(
            vec![Pending { site: "s2".into(), db_path: path.clone() }],
            Duration::from_secs(30),
        )
        .await;
        let _reader = release.await.unwrap();
        assert_eq!(report.truncated, 1, "{report:?}");
        assert_eq!(wal_gate::wal_file_size(&path), 0);
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    /// The last-resort bound reports which site it gave up on instead of
    /// keeping the process alive forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_that_never_finishes_is_abandoned_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s3.db");
        let serving = turso::Builder::new_local(path.to_str().unwrap()).build().await.unwrap();
        let writer = serving.connect().unwrap();
        exec(&writer, "CREATE TABLE t (x INTEGER)").await;
        exec(&writer, "INSERT INTO t VALUES (1)").await;
        let reader = serving.connect().unwrap();
        exec(&reader, "BEGIN").await;
        let mut rows = reader.query("SELECT x FROM t", ()).await.unwrap();
        rows.next().await.unwrap();
        drop(rows);

        let report = checkpoint_databases(
            vec![Pending { site: "stuck".into(), db_path: path.clone() }],
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(report.truncated, 0);
        assert_eq!(report.abandoned, vec!["stuck".to_string()]);
        drop(reader);
    }
}
