//! Per-site credentials for the multi-tenant MySQL wire listener.
//!
//! # What this is for
//!
//! In multi-site mode every virtual host has its own database file (see
//! [`crate::site_backends`]). The `ephpm_db_*` bridge routes to the right one
//! because the router tells it which site the *request* belongs to. A
//! `pdo_mysql` connection has no such context: PHP opens a TCP socket to
//! `127.0.0.1:3306` and the MySQL handshake carries no virtual-host name.
//!
//! This module supplies the missing identity, and — more importantly — makes it
//! one the tenant cannot forge.
//!
//! # Why a credential, and not a port or a socket
//!
//! The obvious designs both fail against this threat model, for the same
//! reason: **every tenant's PHP runs in one process as one OS user.**
//!
//! * *A listener per site, on its own port.* Ports are enumerable. Tenant B
//!   opens `fsockopen('127.0.0.1', $p)` in a loop and finds tenant A's.
//!   Nothing distinguishes the two callers.
//! * *A listener per site, on its own unix socket.* Same OS user means
//!   identical filesystem permissions, so file modes cannot separate tenants
//!   either. `open_basedir` restricts the plain-files wrapper, not the
//!   `unix://` transport or PDO's `unix_socket` DSN parameter, so it does not
//!   help; and an unguessable path is obscurity, not a permission.
//!
//! There is no socket-level primitive that can tell tenant A's PHP from tenant
//! B's PHP inside one process. The boundary therefore has to be **a secret one
//! tenant holds and its neighbour does not** — which is exactly how a real
//! database separates accounts, and exactly what ePHPm already does for the
//! multi-tenant KV listener ([`ephpm_kv::auth`], added in #283).
//!
//! So: one listener, and the connection's tenant is its *authenticated*
//! username.
//!
//! # Why the username is safe to route on here
//!
//! The username in a MySQL handshake is client-asserted — a hostile tenant will
//! happily type its neighbour's. On its own it is a claim, not an identity, and
//! routing on it alone would be the same shared-database hole with extra steps.
//!
//! It becomes an identity once it is *paired with a password only that tenant
//! can produce*. [`SiteWireAuth::authenticate`] verifies the
//! `mysql_native_password` response **before** it touches the backend registry:
//! a caller that cannot answer the challenge for `site-a.test` never causes
//! `site-a.test.db` to be opened, let alone read. Claiming another site's name
//! gets `ER_ACCESS_DENIED` and a closed connection.
//!
//! # Where the passwords come from
//!
//! `password = HMAC-SHA256(master_secret, site_key)`, hex-encoded — the same
//! derivation, and the same function, the KV listener uses
//! ([`ephpm_kv::auth::derive_site_password`]). The master secret is 32 random
//! bytes generated at startup and never leaves the process: it is not written
//! to disk, not exposed to PHP, and not derivable from any site's password
//! (that is what makes HMAC the right primitive rather than a hash of
//! secret+key).
//!
//! Each site's *own* password is injected into that site's requests as
//! `$_SERVER['DB_PASSWORD']` by the router, which builds it per request from
//! the site key it already validated. `$_SERVER` is per-request and
//! thread-local in ePHPm's SAPI, so site B's PHP never observes site A's
//! credential.
//!
//! Consequence worth knowing: **the credentials rotate on every restart.** A
//! tenant must read them from `$_SERVER`, not hard-code them in
//! `wp-config.php`. That is a deliberate trade — a stable password would have
//! to be persisted somewhere, and a per-tenant secret at rest is a much larger
//! surface than one that lives only in memory.
//!
//! # Which node serves the connection (clustered)
//!
//! Authentication settles *which tenant* is connecting. It does not settle
//! *where that tenant's data lives* — and in per-site clustered mode those are
//! different questions, because a site's writes are only captured into the CDC
//! log (and therefore only replicated) on the node that **owns** it by
//! rendezvous hashing.
//!
//! So the verified site key is handed to a [`SiteWireRoute`], the same routing
//! the `ephpm_db_*` bridge uses:
//!
//! * single-node per-site → always this node's local database file;
//! * per-site clustered → the local file when this node is the site's HRW
//!   owner, otherwise a forwarding proxy to the owner
//!   (`sql_forward::ClusteredSiteResolver`).
//!
//! Before this, the wire listener always resolved locally, so a stock
//! `pdo_mysql` write that landed on a non-owner committed to a replica whose
//! writes nothing ships — unreplicated, invisible to the owner, and discarded
//! the next time that replica re-bootstrapped. It diverged silently, with every
//! health check green.
//!
//! ## Resolve-at-connect, and what an ownership move does
//!
//! The route is consulted **once per connection**, at authentication, and the
//! resulting backend is held for that connection's life. Ownership is
//! recomputed from live membership, so it can move while a connection is open.
//! When it does, the pinned owner refuses the forwarded stream (it HRW-gates
//! every stream — see [`crate::sql_forward`]) and the tenant's statements fail
//! with an ordinary connection error until it reconnects.
//!
//! That is deliberate, and it is the same lifetime the `ephpm_db_*` bridge
//! already has: the bridge resolves once per (thread, site) session and pins
//! the result too, recovering by recycling the session on a connection-shaped
//! error. Both paths trade "an ownership move breaks open sessions, loudly" for
//! never writing to a node whose writes do not replicate. A failed statement is
//! recoverable; a silently unreplicated write is not.
//!
//! Non-persistent `pdo_mysql` (the default) opens a connection per request, so
//! the blast radius of a move is one request. `PDO::ATTR_PERSISTENT` widens it
//! to "until PHP drops the handle".
//!
//! # What this does not defend against
//!
//! One listener is shared, so `[db.sqlite.proxy] max_connections` is a
//! *global* cap: a tenant that opens connections greedily can crowd out its
//! neighbours. That is a noisy-neighbour availability problem, not a
//! confidentiality one — no tenant reaches another's data — but it is real and
//! per-tenant connection caps are not implemented.

use std::sync::Arc;

use litewire::backend::{AuthRequest, ConnectionAuthenticator, SharedBackend};
use litewire::litewire_mysql::native_password;

use crate::site_backends::SiteBackends;

/// Length of the per-process master secret, in bytes. 256 bits — the HMAC-SHA256
/// key size, and far past anything guessable by a tenant that gets one attempt
/// per TCP connection.
const MASTER_SECRET_BYTES: usize = 32;

/// Per-site MySQL credentials, and the authenticator that checks them.
///
/// Cloneable and cheap: one instance is held by the wire listener (to verify)
/// and one by the router (to mint each site's password for injection), and both
/// must agree, so they share a secret rather than deriving two.
#[derive(Clone)]
pub struct SiteWireAuth {
    inner: Arc<Inner>,
}

struct Inner {
    /// Hex-encoded per-process master secret. Never persisted, never sent to
    /// PHP; only per-site derivations of it are.
    secret: String,
    /// How an authenticated site's backend is found — the *same* routing the
    /// `ephpm_db_*` bridge uses, so a site's `pdo_mysql` connections and its
    /// bridge queries agree about which database (and which node) serves it.
    route: Arc<dyn SiteWireRoute>,
}

/// Resolves an authenticated site to the backend its connection should use.
///
/// # Why this is a seam and not just `SiteBackends`
///
/// There are two per-site modes, and they answer "where does this site's data
/// live?" differently:
///
/// * **Single-node per-site** — always this node's local database file. The
///   registry ([`SiteBackends`]) *is* the answer.
/// * **Per-site clustered** — the site's data lives on its HRW **owner**, and
///   only writes made on the owner are captured into the CDC log that
///   replicates. A non-owner must forward, not write locally.
///
/// The `ephpm_db_*` bridge has always made that distinction (via
/// `sql_forward::ClusteredSiteResolver`). The wire listener did not: it went
/// straight to `SiteBackends::get_or_open`, so a stock `pdo_mysql` write that
/// landed on a non-owner committed to that node's replica, was never
/// replicated, and was discarded the next time the replica re-bootstrapped from
/// the owner. This trait is what lets both paths share one answer.
///
/// # Async on purpose
///
/// [`ConnectionAuthenticator::authenticate`] runs on a tokio worker, so the
/// bridge's synchronous `block_on` resolver would panic here. Implementations
/// must be genuinely async.
#[litewire::async_trait]
pub trait SiteWireRoute: Send + Sync + 'static {
    /// The backend serving `site_key`, opening the site's database if this
    /// mode and this node serve it locally.
    ///
    /// `site_key` has already been normalized, validated, **and
    /// password-verified** by [`SiteWireAuth::authenticate`] — an implementation
    /// may treat it as an established tenant identity.
    ///
    /// # Errors
    ///
    /// Returns an error if the site's database cannot be opened, or (clustered)
    /// if its owner cannot be addressed. Either way the connection is refused;
    /// there is never a fallback to another site's data.
    async fn backend_for(&self, site_key: &str) -> anyhow::Result<SharedBackend>;
}

/// Single-node per-site: the site's database is always this node's local file.
#[litewire::async_trait]
impl SiteWireRoute for SiteBackends {
    async fn backend_for(&self, site_key: &str) -> anyhow::Result<SharedBackend> {
        self.get_or_open(site_key).await
    }
}

impl SiteWireAuth {
    /// Generate a fresh master secret and bind it to `backends`.
    ///
    /// # Errors
    ///
    /// Fails if the OS entropy source is unavailable. This is deliberately
    /// fatal rather than falling back to a weaker secret: a predictable master
    /// secret makes every tenant's password derivable from its (public) site
    /// name, which is worse than not starting.
    pub fn new(backends: SiteBackends) -> anyhow::Result<Self> {
        Self::with_route(Arc::new(backends))
    }

    /// Generate a fresh master secret and resolve authenticated sites through
    /// `route`.
    ///
    /// Per-site **clustered** mode passes the forwarding resolver here, so a
    /// wire connection on a non-owner forwards to the site's owner exactly as
    /// the `ephpm_db_*` bridge does, instead of writing to an unreplicated
    /// local replica.
    ///
    /// # Errors
    ///
    /// Fails if the OS entropy source is unavailable — see [`Self::new`].
    pub fn with_route(route: Arc<dyn SiteWireRoute>) -> anyhow::Result<Self> {
        let mut raw = [0u8; MASTER_SECRET_BYTES];
        getrandom::fill(&mut raw).map_err(|e| {
            anyhow::anyhow!(
                "failed to generate the per-site database master secret from OS entropy: {e}. \
                 Refusing to start rather than derive tenant credentials from a predictable \
                 secret."
            )
        })?;
        let secret = raw.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
        Ok(Self { inner: Arc::new(Inner { secret, route }) })
    }

    /// The MySQL password for `site_key`.
    ///
    /// Deterministic for the life of the process, so the router can mint it per
    /// request without coordinating with the listener. Callers must only ever
    /// hand a site its *own* password.
    #[must_use]
    pub fn password_for(&self, site_key: &str) -> String {
        ephpm_kv::auth::derive_site_password(&self.inner.secret, site_key)
    }

    /// Hand this to litewire as the MySQL frontend's authenticator.
    #[must_use]
    pub fn as_authenticator(&self) -> litewire::backend::SharedAuthenticator {
        Arc::new(self.clone())
    }
}

#[litewire::async_trait]
impl ConnectionAuthenticator for SiteWireAuth {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Option<SharedBackend> {
        // MySQL usernames are bytes; a site key is always ASCII, so anything
        // that is not valid UTF-8 cannot be one.
        let Ok(username) = std::str::from_utf8(req.username) else {
            tracing::warn!("per-site MySQL auth: rejected a non-UTF-8 username");
            return None;
        };

        // Normalize exactly as the router does when it mints the password and
        // when it picks the request's database, so the three can never disagree
        // about what a site is called. `is_valid_site_key` then bounds it to
        // `[a-z0-9._-]`, which is what makes deriving a file path from it safe.
        let site_key = crate::router::normalize_host_key(username);
        if !crate::router::is_valid_site_key(&site_key) {
            tracing::warn!(
                user = %username,
                "per-site MySQL auth: rejected a username that is not a valid site key"
            );
            return None;
        }

        // Verify BEFORE resolving. Order matters: resolving first would let an
        // unauthenticated caller make the server open (and create) database
        // files by naming them, which is both an information leak and a way to
        // churn the LRU.
        let expected = native_password::password_hash(self.password_for(&site_key).as_bytes());
        if !native_password::verify(&expected, req.salt, req.auth_response) {
            tracing::warn!(
                site = %site_key,
                peer = %req.peer_addr,
                "per-site MySQL auth: password verification failed — refusing the connection"
            );
            return None;
        }

        // Authenticated. Only now is this connection entitled to a database,
        // and only to this one.
        //
        // Where that database lives is the route's call, not ours: local in
        // single-node per-site mode, and in per-site CLUSTERED mode local only
        // when this node is the site's HRW owner — otherwise a forwarding proxy
        // to the owner, so the write is captured into CDC and replicates. See
        // `SiteWireRoute`.
        match self.inner.route.backend_for(&site_key).await {
            Ok(backend) => {
                tracing::debug!(site = %site_key, "per-site MySQL connection authenticated");
                Some(backend)
            }
            Err(e) => {
                // Fail closed: a site whose database cannot be opened gets no
                // connection, never a fallback to some other site's.
                tracing::error!(
                    site = %site_key,
                    "per-site MySQL auth: authenticated but could not open the site's database, \
                     refusing the connection: {e:#}"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use super::*;

    fn stats() -> ephpm_query_stats::QueryStats {
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
            enabled: false,
            slow_query_threshold: std::time::Duration::from_secs(1),
            max_digests: 16,
            metric_label_series_max: 16,
        })
    }

    fn auth(dir: PathBuf) -> SiteWireAuth {
        let backends = SiteBackends::new(dir, 8, stats(), tokio::runtime::Handle::current())
            .expect("registry");
        SiteWireAuth::new(backends).expect("secret")
    }

    /// Build the response a real client would send for `password`.
    fn client_response(password: &str, salt: &[u8]) -> Vec<u8> {
        use sha1::{Digest, Sha1};
        let stage1: [u8; 20] = Sha1::digest(password.as_bytes()).into();
        let stage2: [u8; 20] = Sha1::digest(stage1).into();
        let mut h = Sha1::new();
        h.update(salt);
        h.update(stage2);
        let mask = h.finalize();
        stage1.iter().zip(mask).map(|(s, m)| s ^ m).collect()
    }

    const SALT: &[u8] = b"abcdefghijklmnopqrst";

    fn request<'a>(user: &'a str, response: &'a [u8]) -> AuthRequest<'a> {
        AuthRequest {
            auth_plugin: "mysql_native_password",
            username: user.as_bytes(),
            salt: SALT,
            auth_response: response,
            local_addr: "127.0.0.1:3306".parse::<SocketAddr>().unwrap(),
            peer_addr: "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        }
    }

    /// A site's own credentials open a connection to its own database.
    #[tokio::test]
    async fn a_site_authenticates_with_its_own_password() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let auth = auth(tmp.path().to_path_buf());

        let pw = auth.password_for("site-a.test");
        let backend = auth
            .authenticate(&request("site-a.test", &client_response(&pw, SALT)))
            .await
            .expect("a site's own password must authenticate");

        // And it is that site's database, not some shared one.
        let conn = backend.connect().await.expect("connect");
        conn.execute("CREATE TABLE t (v TEXT)", &[]).await.expect("write");
        assert!(tmp.path().join("site-a.test.db").exists());
    }

    /// The C1 exploit over the wire path: site B claims site A's name, using
    /// the only password B has. Denied — and A's database is never opened.
    #[tokio::test]
    async fn claiming_another_sites_name_is_denied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let auth = auth(tmp.path().to_path_buf());

        let b_password = auth.password_for("site-b.test");
        let denied =
            auth.authenticate(&request("site-a.test", &client_response(&b_password, SALT))).await;

        assert!(denied.is_none(), "site-b's password must not open site-a's database");
        assert!(
            !tmp.path().join("site-a.test.db").exists(),
            "a failed authentication must not open (or create) the target's database file"
        );
    }

    /// Every site gets a different password, so knowing one tells you nothing
    /// about another.
    #[test]
    fn passwords_are_distinct_per_site() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let auth = rt.block_on(async { auth(tmp.path().to_path_buf()) });

        assert_ne!(auth.password_for("site-a.test"), auth.password_for("site-b.test"));
        assert_eq!(
            auth.password_for("site-a.test"),
            auth.password_for("site-a.test"),
            "the derivation must be stable within a process, or injected credentials would \
             stop matching the listener"
        );
    }

    /// Two servers do not share credentials: the master secret is per process.
    #[test]
    fn master_secret_is_per_process() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let (a, b) =
            rt.block_on(async { (auth(tmp.path().to_path_buf()), auth(tmp.path().to_path_buf())) });
        assert_ne!(
            a.password_for("site-a.test"),
            b.password_for("site-a.test"),
            "two independently-started servers must not derive the same tenant password"
        );
    }

    /// An empty auth response — a client that sent no password — is refused.
    #[tokio::test]
    async fn no_password_is_denied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let auth = auth(tmp.path().to_path_buf());
        assert!(auth.authenticate(&request("site-a.test", b"")).await.is_none());
    }

    // -----------------------------------------------------------------------
    // Routing (per-site clustered): the wire path must resolve through the
    // SAME decision as the `ephpm_db_*` bridge, not straight to the local
    // registry.
    // -----------------------------------------------------------------------

    /// A backend that answers nothing — a stand-in for "whatever the route
    /// returned", so a test can assert on *which* backend came back.
    struct MarkerBackend;

    #[litewire::async_trait]
    impl litewire::backend::Backend for MarkerBackend {
        async fn connect(
            &self,
        ) -> Result<Box<dyn litewire::backend::BackendConn>, litewire::backend::BackendError>
        {
            Err(litewire::backend::BackendError::Other("marker".into()))
        }
    }

    /// A route that records every site it was asked for and hands back one
    /// fixed backend.
    struct RecordingRoute {
        asked: std::sync::Mutex<Vec<String>>,
        backend: SharedBackend,
    }

    impl RecordingRoute {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                asked: std::sync::Mutex::new(Vec::new()),
                backend: Arc::new(MarkerBackend) as SharedBackend,
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[litewire::async_trait]
    impl SiteWireRoute for RecordingRoute {
        async fn backend_for(&self, site_key: &str) -> anyhow::Result<SharedBackend> {
            self.asked.lock().unwrap().push(site_key.to_string());
            Ok(Arc::clone(&self.backend))
        }
    }

    /// The connection is served by whatever the **route** returns, not by the
    /// local registry.
    ///
    /// This is the fix for the per-site clustered `pdo_mysql` gap stated as a
    /// test: in that mode the route is the forwarding resolver, so a wire
    /// connection on a non-owner gets a proxy to the site's owner instead of
    /// this node's unreplicated local replica.
    #[tokio::test]
    async fn the_route_decides_which_backend_serves_the_connection() {
        let route = RecordingRoute::new();
        let auth =
            SiteWireAuth::with_route(Arc::clone(&route) as Arc<dyn SiteWireRoute>).expect("secret");

        let pw = auth.password_for("site-a.test");
        let got = auth
            .authenticate(&request("site-a.test", &client_response(&pw, SALT)))
            .await
            .expect("a site's own password must authenticate");

        assert!(
            Arc::ptr_eq(&got, &route.backend),
            "authenticate must hand back the route's backend; resolving the local registry \
             directly is what let a non-owner's pdo_mysql write to an unreplicated replica"
        );
        assert_eq!(
            route.asked(),
            ["site-a.test".to_string()],
            "the route must be consulted with the normalized, verified site key"
        );
    }

    /// Verification still happens **before** routing.
    ///
    /// The ordering is the security property (an unauthenticated caller must
    /// not be able to make the server open, create, or reach out for a database
    /// by naming it), and indirecting the resolution through a trait is exactly
    /// the kind of change that can quietly invert it. In clustered mode a
    /// premature route call would also dial another node for the named tenant.
    #[tokio::test]
    async fn a_failed_password_never_reaches_the_route() {
        let route = RecordingRoute::new();
        let auth =
            SiteWireAuth::with_route(Arc::clone(&route) as Arc<dyn SiteWireRoute>).expect("secret");

        let b_password = auth.password_for("site-b.test");
        let denied =
            auth.authenticate(&request("site-a.test", &client_response(&b_password, SALT))).await;

        assert!(denied.is_none(), "site-b's password must not open site-a's database");
        assert!(
            route.asked().is_empty(),
            "a failed authentication must not consult the route at all, let alone dial \
             site-a's owner"
        );
    }

    /// An invalid site key is rejected before routing too, so no path (and no
    /// `sql/<site>` stream type) is ever derived from it.
    #[tokio::test]
    async fn an_invalid_site_key_never_reaches_the_route() {
        let route = RecordingRoute::new();
        let auth =
            SiteWireAuth::with_route(Arc::clone(&route) as Arc<dyn SiteWireRoute>).expect("secret");

        for user in ["../etc/passwd", "a/b", "", "site a"] {
            let pw = auth.password_for(user);
            assert!(auth.authenticate(&request(user, &client_response(&pw, SALT))).await.is_none());
        }
        assert!(route.asked().is_empty());
    }

    /// A route failure fails the connection closed — never a fallback to some
    /// other site's data. In clustered mode this is the "owner is unreachable
    /// or mid-election" case.
    #[tokio::test]
    async fn a_route_failure_refuses_the_connection() {
        struct FailingRoute;

        #[litewire::async_trait]
        impl SiteWireRoute for FailingRoute {
            async fn backend_for(&self, _site_key: &str) -> anyhow::Result<SharedBackend> {
                Err(anyhow::anyhow!("owner unreachable"))
            }
        }

        let auth = SiteWireAuth::with_route(Arc::new(FailingRoute)).expect("secret");
        let pw = auth.password_for("site-a.test");
        assert!(
            auth.authenticate(&request("site-a.test", &client_response(&pw, SALT))).await.is_none(),
            "a site whose backend cannot be resolved gets no connection"
        );
    }

    /// The forwarding resolver **is** a wire route.
    ///
    /// Type-level statement of the fix: `wire_per_site_clustered_db` can hand
    /// the multi-tenant MySQL listener the very same
    /// `ClusteredSiteResolver` the `ephpm_db_*` bridge resolves through, so the
    /// two tenant routes cannot disagree about which node serves a site. If
    /// this stops compiling, the wire listener has been reverted to
    /// local-only resolution and per-site clustered `pdo_mysql` writes silently
    /// diverge again.
    #[test]
    fn the_forwarding_resolver_can_serve_the_wire_listener() {
        fn assert_is_wire_route<T: SiteWireRoute>() {}
        assert_is_wire_route::<crate::sql_forward::ClusteredSiteResolver>();
        // And the registry remains one, for single-node per-site mode.
        assert_is_wire_route::<SiteBackends>();
    }

    /// A username that is not a valid site key is refused before any path is
    /// derived from it.
    #[tokio::test]
    async fn traversal_usernames_are_denied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let auth = auth(tmp.path().to_path_buf());

        for user in ["../etc/passwd", "a/b", "", "site a", "café.test"] {
            let pw = auth.password_for(user);
            let denied = auth.authenticate(&request(user, &client_response(&pw, SALT))).await;
            assert!(
                denied.is_none(),
                "username {user:?} must be rejected even with a correctly derived password"
            );
        }
    }
}
