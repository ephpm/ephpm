//! Regression test for Turso 0.8.3's page-1 allocation race
//! (tursodatabase/turso#10117).
//!
//! When several connections make the first write to a fresh database file at
//! once, unpatched 0.8.3 can let two pagers both allocate page 1 and the engine
//! panics (`turso_assert!(!self.db_initialized())` in
//! `Pager::maybe_allocate_page1`). In ePHPm that is a new per-site database
//! receiving concurrent traffic; `catch_engine_panic` keeps the process alive,
//! but the write fails. The workspace `[patch.crates-io]` builds Turso from a
//! fork carrying the fix; this test fails if that patch is ever dropped before
//! a fixed Turso release is pinned.
//!
//! Errors such as `Busy` are acceptable outcomes for a racing first write —
//! only an engine panic fails the test.
//!
//! The race is probabilistic. On Linux, unpatched 0.8.3 hit it in 1-3 of the
//! 1000 rounds on every run tried (about 8 s per run); Windows rarely opens the
//! window, so there the test is a smoke check. It cannot pass falsely once
//! the race is fixed, only fail to catch a regression on an unlucky run.

use std::sync::{Arc, Barrier};
use std::time::Duration;

/// Fresh database files to race on. Each is a new chance to hit the window.
const ROUNDS: usize = 1000;
/// OS threads making their first write at the same moment.
const WRITERS: usize = 32;

#[test]
fn concurrent_first_writes_to_a_fresh_file_never_panic_the_engine() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut panics = Vec::new();

    for round in 0..ROUNDS {
        let path = dir.path().join(format!("fresh-{round}.db"));
        let db = rt
            .block_on(turso::Builder::new_local(path.to_str().expect("utf-8 path")).build())
            .expect("open fresh database");
        let barrier = Arc::new(Barrier::new(WRITERS));

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let conn = db.connect().expect("connect");
                    // No busy wait: a writer that loses the write lock gets
                    // `Busy` straight away, which keeps a round short.
                    conn.busy_timeout(Duration::ZERO).expect("busy_timeout");
                    let barrier = Arc::clone(&barrier);
                    let handle = rt.handle().clone();
                    s.spawn(move || {
                        barrier.wait();
                        // Every writer's first statement is a write to the empty
                        // file, so each one reaches page-1 allocation.
                        let _ = handle
                            .block_on(conn.execute(format!("CREATE TABLE t{writer} (x)"), ()));
                    })
                })
                .collect();
            for h in handles {
                if let Err(payload) = h.join() {
                    let msg = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                        .unwrap_or_else(|| "<non-string panic>".to_string());
                    panics.push(format!("round {round}: {msg}"));
                }
            }
        });
    }

    assert!(
        panics.is_empty(),
        "{} engine panic(s) on concurrent first writes (page-1 allocation race, \
         tursodatabase/turso#10117 — is the [patch.crates-io] turso fork still in place?):\n{}",
        panics.len(),
        panics.join("\n")
    );
}
