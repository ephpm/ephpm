//! Database-proxy health as seen by the HTTP readiness probe.
//!
//! # Why readiness gates on the *first* upstream connect and nothing after
//!
//! A proxy that has never reached its upstream since boot cannot serve a
//! single query. Before this existed, that state was invisible: the proxy
//! logged, gave up, and the process went on passing both `/_ephpm/health`
//! and `/_ephpm/ready` while every DB-backed page returned 500. A
//! health-checked deployment reported healthy and served errors, and a bad
//! rollout replaced healthy pods with pods that could never work.
//!
//! So: **readiness fails until every configured proxy has completed one
//! upstream handshake.** A rollout containing a pod that cannot reach the
//! database now stalls instead of completing.
//!
//! The deliberate other half — **after that first success, readiness never
//! flaps on upstream state**:
//!
//! - Gating readiness on *live* database reachability makes every replica
//!   sharing one database fail its probe at the same instant. Kubernetes
//!   then empties the Service, and a database that was merely degraded
//!   becomes a total outage — including for the static assets, cached
//!   pages, and non-DB routes the pods could still serve.
//!   Correlated dependencies do not belong in a per-pod readiness gate.
//! - It also breaks recovery: with no endpoints there is no traffic, so
//!   nothing reopens pooled connections, and external monitoring sees a
//!   black hole rather than 500s that name the failing database.
//!
//! Liveness (`/_ephpm/health`) stays green in both cases. Restarting the
//! process does not make a remote database come back; it only discards warm
//! pools and OPcache and adds a crash-loop to the incident.
//!
//! A post-startup outage is therefore reported, not routed around:
//! `ephpm_db_proxy_upstream_up` drops to 0,
//! `ephpm_db_proxy_connect_failures_total` climbs, and
//! [`ProxyHealth`](ephpm_db::health::ProxyHealth) logs it at ERROR
//! (throttled to one line per minute). Alert on the gauge.

use std::sync::Arc;

use anyhow::Context;
use ephpm_config::Config;
use ephpm_db::health::ProxyHealth;
use ephpm_db::url::DbUrl;

/// Upstream health for every SQL proxy this process was configured to run.
///
/// Built from config *before* the HTTP listeners are bound and shared with
/// both the router (readiness) and proxy startup (state transitions), so
/// there is no window where a proxy exists but has not yet registered and
/// the probe would report a premature "ready".
#[derive(Debug, Default)]
pub struct DbProxyHealth {
    /// Health of the `[db.mysql]` proxy, if configured.
    mysql: Option<Arc<ProxyHealth>>,
    /// Health of the `[db.postgres]` proxy, if configured.
    postgres: Option<Arc<ProxyHealth>>,
}

impl DbProxyHealth {
    /// Pre-register health state for each configured proxy.
    ///
    /// # Errors
    ///
    /// Returns an error if a configured proxy URL cannot be parsed — the
    /// same failure `spawn_deferred` would hit, surfaced at startup where
    /// it is actionable.
    pub fn from_config(config: &Config) -> anyhow::Result<Arc<Self>> {
        let mysql = config
            .db
            .mysql
            .as_ref()
            .map(|c| {
                let listen = c.listen.clone().unwrap_or_else(|| "127.0.0.1:3306".to_string());
                let upstream =
                    DbUrl::parse(&c.url).context("invalid [db.mysql] url").map(|u| u.addr())?;
                anyhow::Ok(ProxyHealth::new("mysql", listen, upstream))
            })
            .transpose()?;

        let postgres = config
            .db
            .postgres
            .as_ref()
            .map(|c| {
                let listen = c.listen.clone().unwrap_or_else(|| "127.0.0.1:5432".to_string());
                let upstream =
                    DbUrl::parse(&c.url).context("invalid [db.postgres] url").map(|u| u.addr())?;
                anyhow::Ok(ProxyHealth::new("postgres", listen, upstream))
            })
            .transpose()?;

        Ok(Arc::new(Self { mysql, postgres }))
    }

    /// The `[db.mysql]` proxy's health handle.
    #[must_use]
    pub fn mysql(&self) -> Option<&Arc<ProxyHealth>> {
        self.mysql.as_ref()
    }

    /// The `[db.postgres]` proxy's health handle.
    #[must_use]
    pub fn postgres(&self) -> Option<&Arc<ProxyHealth>> {
        self.postgres.as_ref()
    }

    /// Every configured proxy's health handle.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<ProxyHealth>> {
        self.mysql.iter().chain(self.postgres.iter())
    }

    /// The first proxy that has never reached its upstream since boot, if
    /// any. `Some` means the process must report **not ready**.
    ///
    /// Deliberately reads `ever_connected`, not the live `is_up` — see the
    /// module docs for why a database outage must not evict every pod from
    /// the load balancer.
    #[must_use]
    pub fn first_never_connected(&self) -> Option<&Arc<ProxyHealth>> {
        self.iter().find(|h| !h.ever_connected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(mysql: Option<&str>, postgres: Option<&str>) -> Config {
        let mut config = Config::default();
        config.db.mysql = mysql.map(|url| ephpm_config::DbBackendConfig {
            url: url.to_string(),
            ..Default::default()
        });
        config.db.postgres = postgres.map(|url| ephpm_config::DbBackendConfig {
            url: url.to_string(),
            ..Default::default()
        });
        config
    }

    #[test]
    fn no_proxies_configured_is_always_ready() {
        let health = DbProxyHealth::from_config(&Config::default()).unwrap();
        assert!(health.first_never_connected().is_none());
        assert_eq!(health.iter().count(), 0);
    }

    /// The exact hole issue #226 reported: a configured proxy that has not
    /// reached its upstream must hold readiness down.
    #[test]
    fn configured_proxy_holds_readiness_until_first_connect() {
        let health = DbProxyHealth::from_config(&config_with(
            Some("mysql://root@127.0.0.1:3307/main"),
            None,
        ))
        .unwrap();
        assert!(
            health.first_never_connected().is_some(),
            "a proxy that has never reached its upstream must fail readiness"
        );

        health.mysql().unwrap().record_up();
        assert!(health.first_never_connected().is_none(), "one handshake must clear the gate");
    }

    /// The deliberate half of the design: a *later* outage must not evict
    /// the pod. If this ever starts failing, someone changed
    /// `first_never_connected` to read live state — read the module docs
    /// before "fixing" it.
    #[test]
    fn later_outage_does_not_flap_readiness() {
        let health = DbProxyHealth::from_config(&config_with(
            Some("mysql://root@127.0.0.1:3307/main"),
            None,
        ))
        .unwrap();
        let mysql = health.mysql().unwrap();
        mysql.record_up();
        mysql.record_down(&"connection refused");

        assert!(!mysql.is_up(), "the live gauge must reflect the outage");
        assert!(
            health.first_never_connected().is_none(),
            "readiness must not flap on a post-startup database outage"
        );
    }

    #[test]
    fn every_configured_proxy_gates_readiness() {
        let health = DbProxyHealth::from_config(&config_with(
            Some("mysql://root@127.0.0.1:3307/main"),
            Some("postgres://postgres@127.0.0.1:15432/main"),
        ))
        .unwrap();
        assert_eq!(health.iter().count(), 2);

        health.mysql().unwrap().record_up();
        let pending = health.first_never_connected().expect("postgres still pending");
        assert_eq!(pending.kind(), "postgres");

        health.postgres().unwrap().record_up();
        assert!(health.first_never_connected().is_none());
    }

    #[test]
    fn malformed_url_fails_startup() {
        let err = DbProxyHealth::from_config(&config_with(Some("not-a-url"), None)).unwrap_err();
        assert!(format!("{err:#}").contains("[db.mysql] url"));
    }
}
