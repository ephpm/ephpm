//! Backend decorator that refuses cross-database SQL on the tenant path.
//!
//! # Why this exists
//!
//! `ATTACH DATABASE '/path/to/other-tenant.db'` is *the* cross-tenant primitive
//! for a file-per-tenant database (issue #274 / pentest finding C1): one
//! statement and a tenant is reading — or writing a PHP shell into — its
//! neighbour's data. `DETACH`, `VACUUM INTO`, and the path-bearing `PRAGMA`s
//! (`data_store_directory`, `temp_store_directory`) are variations on it.
//!
//! The `ephpm_db_*` bridge already screens for these before a statement reaches
//! the engine, deliberately: Turso refuses `ATTACH` today, but making the
//! refusal *ePHPm's own* keeps the property if a future engine bump ever flips
//! that default.
//!
//! Opening the multi-tenant MySQL wire listener added a second route to the
//! same databases, and that argument applies to it identically. This decorator
//! puts the same screen on it, so both routes fail for ePHPm's reasons rather
//! than the engine's.
//!
//! # It is the *same* screen, not a copy
//!
//! This calls [`ephpm_php::db_bridge::screen_sql`] directly rather than
//! reimplementing the denylist. That is load-bearing: the screen is exactly the
//! kind of code that grows a bypass and then grows a fix, and a second copy
//! would receive the bypass and miss the fix. Anything that hardens the bridge's
//! screen hardens this one on the same commit, with nothing to remember.
//!
//! Confirmed against a live example — #287 teaches `screen_sql` to see through
//! a leading `EXPLAIN` / `EXPLAIN QUERY PLAN` wrapper. Before it, `EXPLAIN
//! ATTACH …` reached the engine here and was refused only by Turso's own
//! message; after it (test-merged and re-run), it is refused as
//! "`ATTACH` is not permitted on the tenant database path" through this
//! decorator, with no change to this file.
//!
//! # Where it sits, and why innermost
//!
//! It wraps the raw engine, *inside* the query-stats wrapper:
//!
//! ```text
//! TrackedBackend( ScreenedBackend( Turso ) )
//! ```
//!
//! Innermost on purpose. The wire path hands the backend SQL that litewire has
//! already translated from the MySQL dialect, so screening here inspects
//! exactly the text the engine would execute — not a form the translator might
//! still rewrite. A screen further out could pass something translation then
//! turns into `ATTACH`.
//!
//! # Cost
//!
//! One quote- and comment-aware scan of the statement per query. It is a linear
//! pass over bytes with no allocation and no parse, on a path that is about to
//! do file I/O.

use ephpm_php::db_bridge::screen_sql;
use litewire::backend::{
    Backend, BackendConn, BackendError, Column, ExecuteResult, ResultSet, Value,
};

/// Wraps a backend so every statement is screened before it reaches the engine.
pub struct ScreenedBackend<B> {
    inner: B,
}

impl<B: Backend> ScreenedBackend<B> {
    /// Screen every statement executed against `inner`.
    pub const fn new(inner: B) -> Self {
        Self { inner }
    }
}

/// Turn a rejected statement into the error the caller sees.
///
/// [`BackendError::Sqlite`] rather than `Other` so the MySQL frontend's error
/// mapping treats it like any other statement-level failure: the client gets a
/// clean SQL error and its connection stays usable, instead of a transport
/// error that would drop the session.
fn refuse(offending: &str) -> BackendError {
    BackendError::Sqlite(format!(
        "statement type `{offending}` is not permitted on the tenant database path"
    ))
}

/// A screened connection. Mirrors the decorator at the session level, because
/// every statement arrives through a `BackendConn`, not through the factory.
struct ScreenedConn {
    inner: Box<dyn BackendConn>,
}

#[litewire::async_trait]
impl BackendConn for ScreenedConn {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        screen_sql(sql).map_err(|o| refuse(&o))?;
        self.inner.query(sql, params).await
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        screen_sql(sql).map_err(|o| refuse(&o))?;
        self.inner.execute(sql, params).await
    }

    async fn describe_columns(&self, sql: &str) -> Result<Vec<Column>, BackendError> {
        // Describing does not execute, but it does hand the statement to the
        // engine's parser/planner. Screen it too rather than reason about what
        // a planner does with a path it was not supposed to see.
        screen_sql(sql).map_err(|o| refuse(&o))?;
        self.inner.describe_columns(sql).await
    }
}

#[litewire::async_trait]
impl<B: Backend> Backend for ScreenedBackend<B> {
    async fn connect(&self) -> Result<Box<dyn BackendConn>, BackendError> {
        Ok(Box::new(ScreenedConn { inner: self.inner.connect().await? }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn screened() -> ScreenedBackend<litewire::Turso> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        // Leak the tempdir for the test's lifetime: the backend holds the file
        // open, and dropping the guard here would delete it underneath.
        std::mem::forget(dir);
        ScreenedBackend::new(
            litewire::Turso::open(path.to_str().expect("utf-8 path")).await.expect("open"),
        )
    }

    /// The C1 primitive is refused by *this* layer, not by whatever the engine
    /// happens to do — the whole point of the decorator.
    #[tokio::test]
    async fn attach_is_refused_with_our_own_error() {
        let conn = screened().await.connect().await.expect("connect");
        let err = conn
            .execute("ATTACH DATABASE '/tmp/other-tenant.db' AS stolen", &[])
            .await
            .expect_err("ATTACH must be refused");
        assert!(
            format!("{err}").contains("not permitted on the tenant database path"),
            "the refusal must be ours, not the engine's: {err}"
        );
    }

    #[tokio::test]
    async fn detach_vacuum_and_path_pragmas_are_refused() {
        let conn = screened().await.connect().await.expect("connect");
        for sql in [
            "DETACH DATABASE stolen",
            "VACUUM INTO '/tmp/copy.db'",
            "PRAGMA data_store_directory = '/tmp'",
            "PRAGMA temp_store_directory = '/tmp'",
        ] {
            let err = conn.execute(sql, &[]).await.expect_err("must be refused");
            assert!(
                format!("{err}").contains("not permitted"),
                "expected our refusal for {sql:?}, got: {err}"
            );
        }
    }

    /// An `EXPLAIN`-wrapped `ATTACH` must not execute on the wire path either.
    ///
    /// Asserts only that it *fails*, deliberately, because which layer refuses
    /// it is mid-flight: today the leading keyword parses as `EXPLAIN` so our
    /// screen waves it through and the engine refuses it; once #287 lands,
    /// `screen_sql` sees through the wrapper and the refusal becomes ours.
    /// Verified by test-merging #287 locally — all three forms below come back
    /// as "not permitted on the tenant database path" with no change here,
    /// because this decorator calls the very function #287 fixes.
    ///
    /// Pinning the message would make this test fail on one side of that merge
    /// or the other. The security property — an `EXPLAIN` wrapper never buys a
    /// tenant a cross-database `ATTACH` — holds on both sides, so that is what
    /// is asserted.
    #[tokio::test]
    async fn explain_wrapped_attach_does_not_execute() {
        let conn = screened().await.connect().await.expect("connect");
        for sql in [
            "EXPLAIN ATTACH DATABASE '/tmp/other-tenant.db' AS stolen",
            "EXPLAIN QUERY PLAN ATTACH DATABASE '/tmp/other-tenant.db' AS stolen",
            "explain /* c */ attach database '/tmp/other-tenant.db' as stolen",
        ] {
            assert!(
                conn.execute(sql, &[]).await.is_err(),
                "an EXPLAIN wrapper must not carry an ATTACH through: {sql:?}"
            );
        }
    }

    /// Hiding it behind a leading statement does not help: the screen is
    /// statement-aware, not a prefix check.
    #[tokio::test]
    async fn attach_after_a_benign_statement_is_refused() {
        let conn = screened().await.connect().await.expect("connect");
        let err = conn
            .execute("SELECT 1; ATTACH DATABASE '/tmp/other.db' AS x", &[])
            .await
            .expect_err("must be refused");
        assert!(format!("{err}").contains("not permitted"), "got: {err}");
    }

    /// Ordinary SQL is untouched — a screen that broke real queries would just
    /// get turned off.
    #[tokio::test]
    async fn ordinary_sql_passes_through() {
        let backend = screened().await;
        let conn = backend.connect().await.expect("connect");
        conn.execute("CREATE TABLE t (v TEXT)", &[]).await.expect("create");
        conn.execute("INSERT INTO t (v) VALUES ('hello')", &[]).await.expect("insert");
        let rs = conn.query("SELECT v FROM t", &[]).await.expect("select");
        assert_eq!(rs.rows.len(), 1);
        // A string literal that merely *contains* the keyword is data, not a
        // statement.
        conn.execute("INSERT INTO t (v) VALUES ('ATTACH DATABASE evil')", &[])
            .await
            .expect("a keyword inside a literal must not be screened out");
    }
}
