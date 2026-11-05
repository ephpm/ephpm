//! Memory harness for the embedded-database backup capture
//! (`[db.sqlite.backup]`, `crates/ephpm-server/src/db_backup.rs`).
//!
//! A backup of a large database must run in a bounded amount of resident
//! memory — a small node has to be able to back up a dataset many times
//! larger than its RAM budget. This example makes that measurable outside
//! the test harness, as one process per step so the peak reflects the
//! step alone:
//!
//! ```text
//! db_backup_memtest make   <db> <mb>     # build a ~<mb> MB database
//! db_backup_memtest vacuum <src> <dst>   # the engine's VACUUM INTO
//! db_backup_memtest copy   <src> <dst>   # db_backup::snapshot_into (the shipped path)
//! db_backup_memtest verify <db>          # PRAGMA integrity_check, as `ephpm db restore` runs it
//! ```
//!
//! Every subcommand prints `peak_rss_mb=` (Linux: `VmHWM`, the kernel's
//! high-water mark for the process) and `sampled_peak_rss_mb=` (a 20 ms
//! sampler of `VmRSS`, which also catches a peak the step's own
//! allocations hide). Run it under `/usr/bin/time -v` or a cgroup with a
//! memory limit for an independent number.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;

/// Bytes of random payload per generated row.
const PAYLOAD_BYTES: usize = 1000;
/// Rows per generator transaction.
const ROWS_PER_TXN: u64 = 1000;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sampler = RssSampler::start();
    let started = Instant::now();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let result = match args.get(1).map(String::as_str) {
        Some("make") if args.len() == 4 => {
            let mb: u64 = args[3].parse().context("<mb> must be an integer")?;
            rt.block_on(make(Path::new(&args[2]), mb))
        }
        Some("vacuum") if args.len() == 4 => {
            rt.block_on(vacuum_into(Path::new(&args[2]), Path::new(&args[3])))
        }
        Some("copy") if args.len() == 4 => rt.block_on(ephpm_server::db_backup::snapshot_into(
            Path::new(&args[2]),
            Path::new(&args[3]),
        )),
        // `PRAGMA integrity_check` on an artifact, as `ephpm db restore` runs it.
        Some("verify") if args.len() == 3 => {
            rt.block_on(ephpm_server::db_backup::verify_integrity(Path::new(&args[2])))
        }
        _ => {
            eprintln!(
                "usage: db_backup_memtest make <db> <mb> | vacuum <src> <dst> | copy <src> <dst> \
                 | verify <db>"
            );
            std::process::exit(2);
        }
    };
    let sampled_peak = sampler.stop();
    println!(
        "step={} elapsed_s={:.1} peak_rss_mb={} sampled_peak_rss_mb={}",
        args[1],
        started.elapsed().as_secs_f64(),
        vm_hwm_mb().map_or_else(|| "n/a".to_owned(), |mb| mb.to_string()),
        sampled_peak.map_or_else(|| "n/a".to_owned(), |mb| mb.to_string()),
    );
    result
}

/// Build a database of roughly `mb` MB: one table of 1 KB random blobs
/// with a secondary index, plus a trigger and its log table so a copy's
/// faithfulness (triggers travel, indexes rebuild) is checkable afterwards.
async fn make(path: &Path, mb: u64) -> anyhow::Result<()> {
    let db = turso::Builder::new_local(path.to_str().context("path is not UTF-8")?)
        .build()
        .await
        .context("open")?;
    let conn = db.connect().context("connect")?;
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT NOT NULL, v BLOB NOT NULL);
         CREATE INDEX t_k ON t (k);
         CREATE TABLE log (id INTEGER, at TEXT);
         CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.id, 'x'); END;",
    )
    .await
    .context("schema")?;
    // The trigger would double the row count during generation; it is there
    // for the copy to carry, not to run now.
    conn.execute("DROP TRIGGER t_ai", ()).await?;
    let rows = mb * 1024 * 1024 / (PAYLOAD_BYTES as u64 + 64);
    let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
    let mut payload = vec![0u8; PAYLOAD_BYTES];
    let mut insert = conn.prepare("INSERT INTO t (k, v) VALUES (?, ?)").await?;
    for i in 0..rows {
        if i % ROWS_PER_TXN == 0 {
            conn.execute("BEGIN", ()).await?;
        }
        for byte in &mut payload {
            // xorshift64: cheap, incompressible, deterministic.
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *byte = (rng & 0xff) as u8;
        }
        insert
            .execute((format!("key-{i:012}"), payload.clone()))
            .await
            .with_context(|| format!("insert row {i}"))?;
        if (i + 1) % ROWS_PER_TXN == 0 || i + 1 == rows {
            conn.execute("COMMIT", ()).await?;
        }
    }
    conn.execute(
        "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.id, 'x'); END",
        (),
    )
    .await?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
    let size = std::fs::metadata(path)?.len();
    println!("made rows={rows} bytes={size}");
    Ok(())
}

/// The engine's own `VACUUM INTO`, as `db_backup::snapshot_into` ran it
/// before the batched copy (kept as the comparison baseline).
async fn vacuum_into(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let db = turso::Builder::new_local(src.to_str().context("path is not UTF-8")?)
        .build()
        .await
        .context("open")?;
    let conn = db.connect().context("connect")?;
    let sql = format!("VACUUM INTO '{}'", dst.to_str().context("dst")?.replace('\'', "''"));
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(conn.execute(&sql, ()))
    })
    .context("VACUUM INTO")?;
    Ok(())
}

/// `VmHWM` from `/proc/self/status`, in MB.
fn vm_hwm_mb() -> Option<u64> {
    proc_status_kb("VmHWM:").map(|kb| kb / 1024)
}

fn proc_status_kb(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with(key))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Samples `VmRSS` every 20 ms on a thread and keeps the maximum.
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak_kb: Arc<AtomicU64>,
    thread: std::thread::JoinHandle<()>,
}

impl RssSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_kb = Arc::new(AtomicU64::new(0));
        let thread = {
            let stop = Arc::clone(&stop);
            let peak_kb = Arc::clone(&peak_kb);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(kb) = proc_status_kb("VmRSS:") {
                        peak_kb.fetch_max(kb, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        Self { stop, peak_kb, thread }
    }

    fn stop(self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
        match self.peak_kb.load(Ordering::Relaxed) {
            0 => None,
            kb => Some(kb / 1024),
        }
    }
}
