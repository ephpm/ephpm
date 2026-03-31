//! Query-stats-tracking wrapper for litewire backends.
//!
//! `TrackedBackend` implements the litewire [`Backend`] trait by delegating
//! to an inner backend while recording query timing and row counts in
//! [`QueryStats`].

use std::time::Instant;

use ephpm_query_stats::QueryStats;
use litewire::backend::{Backend, BackendError, ExecuteResult, ResultSet, Value};

/// A backend decorator that records query stats.
///
/// Wraps any litewire [`Backend`] and calls [`QueryStats::record_query`] or
/// [`QueryStats::record_mutation`] after every operation.
pub struct TrackedBackend<B> {
    inner: B,
    stats: QueryStats,
}

impl<B> TrackedBackend<B> {
    /// Create a new tracked backend wrapping the given inner backend.
    pub fn new(inner: B, stats: QueryStats) -> Self {
        Self { inner, stats }
    }
}

#[async_trait::async_trait]
impl<B: Backend> Backend for TrackedBackend<B> {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        let start = Instant::now();
        let result = self.inner.query(sql, params).await;
        let duration = start.elapsed();
        let rows = result.as_ref().map_or(0, |rs| rs.rows.len() as u64);
        self.stats.record_query(sql, duration, result.is_ok(), rows);
        result
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        let start = Instant::now();
        let result = self.inner.execute(sql, params).await;
        let duration = start.elapsed();
        let rows = result.as_ref().map_or(0, |r| r.affected_rows);
        self.stats.record_mutation(sql, duration, result.is_ok(), rows);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A minimal test backend that always succeeds.
    struct StubBackend;

    #[async_trait::async_trait]
    impl Backend for StubBackend {
        async fn query(&self, _sql: &str, _params: &[Value]) -> Result<ResultSet, BackendError> {
            Ok(ResultSet {
                columns: vec![],
                rows: vec![vec![Value::Integer(1)]],
            })
        }

        async fn execute(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> Result<ExecuteResult, BackendError> {
            Ok(ExecuteResult {
                affected_rows: 1,
                last_insert_rowid: Some(1),
            })
        }
    }

    #[tokio::test]
    async fn query_records_stats() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let backend = TrackedBackend::new(StubBackend, stats.clone());

        backend.query("SELECT * FROM t WHERE id = 1", &[]).await.unwrap();
        backend.query("SELECT * FROM t WHERE id = 2", &[]).await.unwrap();

        assert_eq!(stats.digest_count(), 1);
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 2);
        assert_eq!(top[0].total_rows, 2);
    }

    #[tokio::test]
    async fn execute_records_stats() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let backend = TrackedBackend::new(StubBackend, stats.clone());

        backend
            .execute("INSERT INTO t VALUES (1, 'hello')", &[])
            .await
            .unwrap();

        assert_eq!(stats.digest_count(), 1);
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].total_rows, 1);
    }

    /// A backend that always fails.
    struct FailBackend;

    #[async_trait::async_trait]
    impl Backend for FailBackend {
        async fn query(&self, _sql: &str, _params: &[Value]) -> Result<ResultSet, BackendError> {
            Err(BackendError::Other("fail".into()))
        }

        async fn execute(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> Result<ExecuteResult, BackendError> {
            Err(BackendError::Other("fail".into()))
        }
    }

    #[tokio::test]
    async fn error_records_stats() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let backend = TrackedBackend::new(FailBackend, stats.clone());

        let _ = backend.query("SELECT 1", &[]).await;

        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].error_count, 1);
    }

    #[tokio::test]
    async fn slow_query_tracked() {
        let config = ephpm_query_stats::StatsConfig {
            slow_query_threshold: Duration::from_millis(1),
            ..Default::default()
        };
        let stats = QueryStats::new(config);

        // StubBackend is fast, but we can verify the stats work
        let backend = TrackedBackend::new(StubBackend, stats.clone());
        backend.query("SELECT 1", &[]).await.unwrap();

        assert_eq!(stats.digest_count(), 1);
    }
}
