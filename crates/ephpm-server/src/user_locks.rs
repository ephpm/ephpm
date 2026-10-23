//! MySQL named locks (`GET_LOCK()` & co.) in ePHPm's embedded-database modes.
//!
//! litewire implements the lock functions but deliberately refuses them on
//! any connection that was not given a **lock scope** (a namespace in a
//! [`UserLockRegistry`]): a lock that does not actually exclude anything is
//! worse than an error, because applications take one precisely to stop a
//! job from running twice. ePHPm therefore decides, per database mode, what
//! the scope is — and whether this node may grant locks at all:
//!
//! | Mode | Scope | Who may grant |
//! |------|-------|---------------|
//! | single-node, one database | the node (namespace `""`) | this node |
//! | per-site (single-node) | **the site** — one namespace per virtual host, so a tenant can neither block nor observe another's locks | this node |
//! | per-site clustered | the site, **on its HRW owner** — a non-owner forwards the operation over `sql/<site>` and the lock lives with the owner-side session (`sql_forward`) | the site's owner |
//! | whole-database clustered | the node (namespace `""`) | **the primary only**: a replica refuses with an error |
//!
//! The per-site scopes are applied by [`crate::site_backends::SiteBackends`]
//! (every backend it hands out is scoped to its site key, in one registry
//! per node, so the wire listener, the `ephpm_db_*` bridge and the owner side
//! of forwarded statements all see one lock table per site). This module
//! holds the two shared-database helpers.
//!
//! # Lifetime
//!
//! A lock belongs to the litewire connection that took it and is released
//! when that connection is dropped — a `pdo_mysql` client's `QUIT`, a TCP
//! reset when its process is killed, a forwarded stream closing — which is
//! the auto-release-on-disconnect guarantee MySQL gives. The `ephpm_db_*`
//! bridge keeps one session per PHP thread *across* requests, so it releases
//! that session's locks at request end instead (`db_bridge::on_request_end`);
//! a bridge lock therefore lives at most for the request that took it.
//!
//! # Whole-database clustered mode
//!
//! There is no statement forwarding in that mode: a replica's wire frontend
//! serves its local copy, and a write made there is already documented as
//! unreplicated. A lock granted there would be node-local too — two cron
//! workers on two nodes would both "win". So a replica **errors** on every
//! lock operation ([`PrimaryOnlyUserLocks`]), and only the elected primary
//! grants locks; the primary is the single writer, so its node-local table is
//! the cluster-wide one. On failover the old primary's locks die with its
//! sessions and the new primary starts with none — the same window per-site
//! clustered mode has (see `sql_forward`): a holder whose next statement
//! lands on a demoted node fails loudly there, but one that issues no
//! statement during the move keeps believing it holds the lock while the new
//! primary can grant it to someone else.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use litewire::backend::user_lock::{UserLockOp, UserLockReply};
use litewire::backend::{
    Backend, BackendConn, BackendError, Column, ExecuteResult, ResultSet, SharedBackend,
    UserLockRegistry, UserLockScoped, Value,
};

/// The one lock namespace of a single shared database.
const SHARED_NAMESPACE: &str = "";

/// Scope a shared (single-database) backend's connections to this node's one
/// lock namespace, with a registry of their own.
pub(crate) fn scope_shared(backend: SharedBackend) -> UserLockScoped {
    UserLockScoped::new(backend, &UserLockRegistry::new(), SHARED_NAMESPACE)
}

/// What a replica answers to a lock operation in whole-database clustered
/// mode. Stable text — the compatibility docs quote it.
pub(crate) const REPLICA_REFUSAL: &str = "named locks (GET_LOCK and friends) are served by the \
                                          primary only in clustered mode and this node is a \
                                          replica — point the client at the primary (writes \
                                          made on a replica are not replicated either)";

/// A shared backend whose connections grant named locks only while this node
/// is the elected primary (`is_primary`, the `/_ephpm/primary` view the CDC
/// election flips). On a replica every lock operation is an error.
///
/// Checked per operation, not per connection: a connection opened on the
/// primary that survives a demotion must stop granting, and one opened on a
/// replica must start working after a promotion without reconnecting.
pub(crate) struct PrimaryOnlyUserLocks {
    inner: UserLockScoped,
    is_primary: Arc<AtomicBool>,
}

impl PrimaryOnlyUserLocks {
    /// Scope `backend` to this node's namespace and gate its locks on
    /// `is_primary`.
    pub(crate) fn new(backend: SharedBackend, is_primary: Arc<AtomicBool>) -> Self {
        Self { inner: scope_shared(backend), is_primary }
    }
}

#[async_trait::async_trait]
impl Backend for PrimaryOnlyUserLocks {
    async fn connect(&self) -> Result<Box<dyn BackendConn>, BackendError> {
        Ok(Box::new(PrimaryGatedConn {
            inner: self.inner.connect().await?,
            is_primary: Arc::clone(&self.is_primary),
        }))
    }

    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        self.inner.query(sql, params).await
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        self.inner.execute(sql, params).await
    }

    async fn describe_columns(&self, sql: &str) -> Result<Vec<Column>, BackendError> {
        self.inner.describe_columns(sql).await
    }
}

struct PrimaryGatedConn {
    inner: Box<dyn BackendConn>,
    is_primary: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl BackendConn for PrimaryGatedConn {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        self.inner.query(sql, params).await
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        self.inner.execute(sql, params).await
    }

    async fn describe_columns(&self, sql: &str) -> Result<Vec<Column>, BackendError> {
        self.inner.describe_columns(sql).await
    }

    async fn user_lock(&self, op: &UserLockOp) -> Result<UserLockReply, BackendError> {
        if !self.is_primary.load(Ordering::Relaxed) {
            return Err(BackendError::Other(REPLICA_REFUSAL.to_string()));
        }
        self.inner.user_lock(op).await
    }
}

#[cfg(test)]
mod tests {
    use litewire::backend::LockTimeout;

    use super::*;

    fn turso(dir: &std::path::Path) -> SharedBackend {
        let path = dir.join("locks.db");
        let rt = tokio::runtime::Handle::current();
        let backend = std::thread::scope(|s| {
            s.spawn(|| {
                rt.block_on(litewire::Turso::builder(path.to_str().unwrap()).build()).unwrap()
            })
            .join()
            .unwrap()
        });
        Arc::new(backend)
    }

    fn get(name: &str) -> UserLockOp {
        UserLockOp::Get { name: name.into(), timeout: LockTimeout::Immediate }
    }

    /// A replica refuses; the same connection grants once promoted; a demoted
    /// node stops granting without a reconnect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn locks_follow_the_primary_flag_per_operation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let is_primary = Arc::new(AtomicBool::new(false));
        let backend = PrimaryOnlyUserLocks::new(turso(tmp.path()), Arc::clone(&is_primary));
        let conn = backend.connect().await.expect("connect");

        let err = conn.user_lock(&get("job")).await.expect_err("a replica must refuse");
        assert!(err.to_string().contains("primary only"), "{err}");
        // Ordinary SQL on a replica is unaffected.
        conn.query("SELECT 1", &[]).await.expect("replica reads");

        is_primary.store(true, Ordering::Relaxed);
        assert_eq!(conn.user_lock(&get("job")).await.expect("primary grants"), Some(1));

        is_primary.store(false, Ordering::Relaxed);
        let err = conn.user_lock(&UserLockOp::IsFree { name: "job".into() }).await.unwrap_err();
        assert!(err.to_string().contains("replica"), "{err}");
    }

    /// Two connections to the shared backend exclude each other, and a
    /// dropped connection frees its lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_scope_excludes_across_connections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = scope_shared(turso(tmp.path()));
        let a = backend.connect().await.expect("a");
        let b = backend.connect().await.expect("b");
        assert_eq!(a.user_lock(&get("job")).await.unwrap(), Some(1));
        assert_eq!(b.user_lock(&get("job")).await.unwrap(), Some(0));
        drop(a);
        assert_eq!(b.user_lock(&get("job")).await.unwrap(), Some(1));
    }
}
