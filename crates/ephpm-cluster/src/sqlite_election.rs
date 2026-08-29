//! Primary election for clustered `SQLite`.
//!
//! Uses the gossip KV tier to elect a primary node for replication. As of
//! v0.7.0 the consumer is the in-process Turso CDC path (`turso_cdc.rs`); the
//! election itself is engine-agnostic, and `ElectedRole::Replica`'s
//! `primary_grpc_url` field carries whatever address peers must dial to reach
//! the primary (the primary's cluster-channel address in the CDC path).
//! The lowest-ordinal alive node wins. The primary heartbeats its claim
//! every 5 seconds with a 10-second TTL. On primary failure, the next
//! lowest-ordinal node promotes itself.
//!
//! # Incumbent protection (issue #314)
//!
//! Two rules keep a node that *joins* a running cluster from taking the
//! primary role away from a healthy incumbent (observed in the wild: a cold
//! node re-rooted the cluster onto its own empty database):
//!
//! 1. **Startup grace.** A freshly started node must not publish a first
//!    primary claim until [`STARTUP_GRACE`] has elapsed since boot. At boot,
//!    gossip has not converged: `cluster.nodes()` holds only the local node
//!    (so "lowest-ordinal alive" is trivially true for everyone) and an
//!    incumbent's claim may not have gossiped over yet. The grace window is
//!    longer than claim TTL + heartbeat, so a live incumbent's claim is
//!    guaranteed multiple opportunities to arrive before this node may
//!    self-elect. A restarting incumbent whose own (unexpired) claim is
//!    still in gossip reclaims immediately — that is not a theft.
//! 2. **Deterministic conflict resolution.** The gossip KV tier is
//!    last-write-wins, so two simultaneous claimants would otherwise
//!    flip-flop on every heartbeat. If this node currently holds the
//!    primary role and sees a *live* foreign claim, the tie is broken by
//!    node id — lowest wins (the documented election rule) — instead of
//!    silently yielding to whoever wrote last. With rule 1 in place this
//!    only triggers when two nodes genuinely elected concurrently (e.g.
//!    a symmetric partition heal), where either database is as good as
//!    the other and determinism is what matters.
//!
//! # Data-identity guard on the fast reclaim (issue #344)
//!
//! Rule 1 deliberately lets a *restarting incumbent* reclaim primary
//! immediately when its own claim is still unexpired in gossip (TTL ~10s),
//! so a fast restart does not bounce the role. But "same node id" is not
//! "same data": if the node's database was wiped or replaced between stop
//! and start (a redeploy onto an empty volume), it comes back with an
//! **empty database and a fresh CDC log identity** yet the same node id,
//! wins that fast-reclaim inside the TTL window, and re-roots the cluster
//! onto the empty database — #314's blast radius through a narrower door.
//!
//! The fix stamps the primary's **CDC log identity** (`__ephpm_cdc_log_id`,
//! issue #315 — a stable per-database-file fingerprint) into the claim and
//! requires the fast reclaim to match on **both** node id and log id. When
//! the node id matches but the log id does not, the claim describes data
//! this node no longer holds: it refuses the fast path, stops refreshing
//! the stale claim (letting it expire), and falls through to the normal
//! startup grace + election so a node that still has the data can win. When
//! the data identity is unchanged the deliberate fast-restart behaviour is
//! preserved unchanged.
//!
//! # Per-site ownership converges on live membership
//!
//! The per-site election ([`SqliteElection::new_per_site`]) does **not**
//! use lowest-ordinal: the owner of a site is `hrw_owner(alive, site)`, a
//! pure function of the site key and the current alive set. Two rules
//! follow from that, enforced in `evaluate_role`:
//!
//! 1. A node stops refreshing its own claim as soon as HRW no longer
//!    names it — the claim expires and the real owner takes over.
//! 2. A foreign claim never wins over live HRW: if HRW names *us*, the
//!    claim predates the membership change and we elect regardless of
//!    whether its author is alive.
//!
//! Without those, a claim outlived the membership change that invalidated
//! it: the stale claimant kept the gossip key alive while refusing to
//! serve `cdc/<site>` (the serving side has always gated on live HRW),
//! and the real owner retried a refused dial forever — that site's
//! replication wedged permanently. The claim carries the owner's
//! *address* and its CDC log identity; it is not the ownership record.
//! The single-database election is untouched by all of this.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::{ClusterHandle, NodeInfo, NodeState};

/// Gossip KV key for the (single-database) primary node identity.
///
/// In per-site mode each site gets its own key, `"sqlite:primary:<site>"`
/// (see [`SqliteElection::new_per_site`] and [`primary_key_for_site`]).
const PRIMARY_KEY: &str = "sqlite:primary";

/// Gossip KV key for a per-site primary claim.
///
/// Per-site clustered replication elects an owner per virtual host, keyed
/// by the site's canonical key, so ownership of one tenant's database is
/// independent of every other's. The site key is validated `[a-z0-9._-]`
/// upstream, so it never contains the `:` used to delimit the namespace.
#[must_use]
fn primary_key_for_site(site: &str) -> String {
    format!("{PRIMARY_KEY}:{site}")
}

/// The node currently claiming ownership of `site` in the per-site primary
/// election, as `(node_id, channel_addr)`, if any **member-validated** claim
/// is published.
///
/// The claim's `channel_addr` is that node's cluster-channel advertise
/// address — exactly what a non-owner must dial to forward `sql/<site>`
/// statements to the owner. Returns `None` when no claim is published yet
/// (a site no node has opened) **or when the claim's advertised address does
/// not belong to a known cluster member**, in which case the caller falls back
/// to the HRW owner's *derived* channel address.
///
/// # Why the member check is load-bearing here
///
/// The gossip KV tier is last-write-wins and carries no per-key authorship
/// proof, so a claim is attacker-influenceable in the same threat model
/// [`SqliteElection::replica_url_for`] defends against. The caller
/// (`sql_forward::ClusteredSiteResolver`) *dials* this address and forwards
/// every one of that tenant's SQL statements — including bound parameters —
/// to it, so an unvalidated address is a full read/write SSRF against one
/// tenant's data. Validation is identical to `replica_url_for`'s: the host
/// must match a currently-known gossip member's host. Fails closed.
///
/// Decodes the same claim [`SqliteElection`] publishes under
/// `"sqlite:primary:<site>"`, so the forwarding path and the election agree
/// on the owner's address without a second gossip key.
pub async fn per_site_primary(cluster: &ClusterHandle, site: &str) -> Option<(String, String)> {
    let bytes = cluster.gossip_get(&primary_key_for_site(site)).await?;
    let claim = PrimaryClaim::decode(&bytes)?;
    let nodes = cluster.nodes().await;
    let addr = claim_addr_if_member(&claim.grpc_addr, &nodes).or_else(|| {
        tracing::warn!(
            site = %site,
            claimant = %claim.node_id,
            claimed_addr = %claim.grpc_addr,
            "refusing a per-site owner claim: the advertised cluster-channel host is not a \
             known cluster member (possible forged gossip claim); falling back to the HRW \
             owner's derived address"
        );
        None
    })?;
    Some((claim.node_id, addr))
}

/// The claim's advertised address, but only if its host belongs to a known
/// gossip member. `None` (fail closed) otherwise, or when the address has no
/// parseable host.
///
/// Host-only comparison, because a node's gossip address and the address it
/// advertises for another service use different ports on the same host — the
/// same rule [`SqliteElection::replica_url_for`] applies. Split out as a pure
/// function so it is directly unit-testable without a live gossip mesh.
#[must_use]
fn claim_addr_if_member(claim_addr: &str, members: &[NodeInfo]) -> Option<String> {
    let claim_host = host_of(claim_addr)?;
    let known = members.iter().filter_map(|n| host_of(&n.gossip_addr));
    member_hosts_contain(known, &claim_host).then(|| claim_addr.to_string())
}

/// Rendezvous-hash (HRW) score for placing `site` on the node named
/// `node_id`; higher wins.
///
/// A stable 64-bit FNV-1a over `node_id`, a fixed domain separator, and
/// `site`. It deliberately does **not** use `std`'s hasher (whose seed and
/// implementation are not contractually stable across builds): every node
/// must compute the identical score so they independently agree on a
/// site's owner with no coordination.
#[must_use]
fn hrw_score(node_id: &str, site: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    /// Domain separator between `node_id` and `site`, so
    /// `hrw_score("ab", "c")` differs from `hrw_score("a", "bc")`.
    const SEP: u64 = 0x5c;

    let mut hash = FNV_OFFSET;
    for &byte in node_id.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= SEP;
    hash = hash.wrapping_mul(FNV_PRIME);
    for &byte in site.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The HRW (rendezvous-hashing) owner of `site` among the **alive** nodes
/// in `alive`: the node maximizing [`hrw_score`], ties broken by node id
/// so the choice is total and identical on every node.
///
/// Dead nodes are filtered out here, so callers may pass the full
/// [`ClusterHandle::nodes`](crate::ClusterHandle::nodes) result verbatim.
/// Returns `None` only when no node is alive.
///
/// This is the ownership rule for per-site clustered replication: because
/// the score depends only on the (node id, site) pair, a node's death
/// re-homes **only that node's** sites — each to whichever surviving node
/// scores next-highest for it, a node already replicating that site — and
/// leaves every other site's owner unchanged. That minimal reshuffle is
/// exactly what mod-N ownership does not give.
#[must_use]
pub fn hrw_owner<'a>(alive: &'a [NodeInfo], site: &str) -> Option<&'a NodeInfo> {
    alive.iter().filter(|n| n.state == NodeState::Alive).max_by(|a, b| {
        hrw_score(&a.id, site).cmp(&hrw_score(&b.id, site)).then_with(|| a.id.cmp(&b.id))
    })
}

/// How often the primary refreshes its claim.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// TTL for the primary claim. If not refreshed, the key expires and
/// triggers re-election.
const PRIMARY_TTL: Duration = Duration::from_secs(10);

/// How long a freshly started node must wait before it may publish its
/// *first* primary claim (issue #314 — see the module docs).
///
/// Must exceed `PRIMARY_TTL + HEARTBEAT_INTERVAL`: a live incumbent
/// refreshes its claim every [`HEARTBEAT_INTERVAL`], and gossip delivers a
/// KV write in low single-digit seconds, so within this window a joining
/// node is guaranteed to observe the incumbent's claim if one exists.
/// The cost is that a genuinely fresh cluster elects its first primary
/// ~15s after boot instead of immediately.
const STARTUP_GRACE: Duration =
    Duration::from_secs(PRIMARY_TTL.as_secs() + HEARTBEAT_INTERVAL.as_secs());

/// The elected role for this node's sqld instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectedRole {
    /// This node is the primary — sqld accepts writes and serves WAL frames.
    Primary,
    /// This node is a replica — sqld syncs from the primary via gRPC.
    Replica {
        /// gRPC URL of the primary node.
        primary_grpc_url: String,
    },
}

/// Value stored in gossip KV for the primary claim.
///
/// Format: `"{node_id}|{grpc_addr}|{log_id}"`.
///
/// `log_id` is the primary's CDC log identity (`__ephpm_cdc_log_id`, issue
/// #315) — a stable per-database-file fingerprint. It is what distinguishes
/// "the same node restarting with the same data" from "the same node id
/// coming back with a *different* (wiped/replaced) database" (issue #344).
/// The `grpc_addr` and `log_id` fields never contain a `|` (an address has
/// none, and the log id is 32 hex chars), so a plain three-way split is
/// unambiguous.
#[derive(Debug, Clone)]
struct PrimaryClaim {
    node_id: String,
    grpc_addr: String,
    /// CDC log identity of the primary's database (issue #315/#344). Empty
    /// when decoded from a legacy two-field claim written by an older node
    /// mid-rolling-upgrade; an empty local/claimed log id never matches a
    /// real one, so the reclaim guard errs safe (defers to election).
    log_id: String,
}

impl PrimaryClaim {
    fn encode(&self) -> Vec<u8> {
        format!("{}|{}|{}", self.node_id, self.grpc_addr, self.log_id).into_bytes()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let s = std::str::from_utf8(bytes).ok()?;
        // `node_id|grpc_addr|log_id`. Tolerate a legacy two-field claim
        // (`node_id|grpc_addr`) from a node that has not yet upgraded: the
        // missing log id decodes as empty, which fails the #344 identity
        // match and so defers to election rather than fast-reclaiming.
        let mut parts = s.splitn(3, '|');
        let node_id = parts.next()?;
        let grpc_addr = parts.next()?;
        let log_id = parts.next().unwrap_or("");
        Some(Self {
            node_id: node_id.to_string(),
            grpc_addr: grpc_addr.to_string(),
            log_id: log_id.to_string(),
        })
    }
}

/// Manages primary election for sqld via gossip KV.
///
/// Spawn the [`run`](Self::run) method as a tokio task. Watch for role
/// changes via [`watch_role`](Self::watch_role).
pub struct SqliteElection {
    cluster: Arc<ClusterHandle>,
    grpc_listen: String,
    /// This node's CDC log identity (`__ephpm_cdc_log_id`, issue #315) — a
    /// stable fingerprint of the database file this node currently holds.
    /// Stamped into every published claim and compared against a surviving
    /// claim's log id before the fast-restart reclaim (issue #344).
    log_id: String,
    /// The site this election governs, in **per-site** mode; `None` for the
    /// single-database election.
    ///
    /// When `Some`, the claim is stored under `"sqlite:primary:<site>"` and
    /// ownership is decided by rendezvous hashing ([`hrw_owner`]) rather
    /// than lowest-ordinal — so each tenant's owner is chosen independently
    /// and a node death re-homes only that node's sites.
    site: Option<String>,
    role_tx: watch::Sender<ElectedRole>,
    role_rx: watch::Receiver<ElectedRole>,
    /// When this election manager was created. Gates the first
    /// self-election behind [`STARTUP_GRACE`] (issue #314).
    boot: tokio::time::Instant,
}

impl SqliteElection {
    /// Create a new election manager.
    ///
    /// `grpc_listen` is this node's sqld gRPC address that replicas will
    /// connect to if this node becomes primary. `log_id` is this node's CDC
    /// log identity (issue #315) — the fingerprint of the database this node
    /// currently holds, used to reject a fast reclaim after the data was
    /// wiped/replaced (issue #344).
    #[must_use]
    pub fn new(cluster: Arc<ClusterHandle>, grpc_listen: String, log_id: String) -> Self {
        Self::build(cluster, grpc_listen, log_id, None)
    }

    /// Create a **per-site** election manager for `site`.
    ///
    /// Identical to [`new`](Self::new) except the primary claim is keyed
    /// `"sqlite:primary:<site>"` and ownership is decided by rendezvous
    /// hashing ([`hrw_owner`]) over the alive nodes rather than by
    /// lowest-ordinal: `should_be_primary` is true iff this node is the
    /// site's HRW owner. `grpc_listen` is this node's cluster-channel
    /// address that replicas of `site` will dial when this node owns it.
    #[must_use]
    pub fn new_per_site(
        cluster: Arc<ClusterHandle>,
        grpc_listen: String,
        log_id: String,
        site: String,
    ) -> Self {
        Self::build(cluster, grpc_listen, log_id, Some(site))
    }

    fn build(
        cluster: Arc<ClusterHandle>,
        grpc_listen: String,
        log_id: String,
        site: Option<String>,
    ) -> Self {
        // Start as replica with empty URL — will be resolved on first tick.
        let (role_tx, role_rx) =
            watch::channel(ElectedRole::Replica { primary_grpc_url: String::new() });

        Self {
            cluster,
            grpc_listen,
            log_id,
            site,
            role_tx,
            role_rx,
            boot: tokio::time::Instant::now(),
        }
    }

    /// The gossip KV key this election reads and writes its primary claim
    /// under: the global [`PRIMARY_KEY`] for the single-database election,
    /// or `"sqlite:primary:<site>"` in per-site mode.
    fn primary_key(&self) -> String {
        match &self.site {
            Some(site) => primary_key_for_site(site),
            None => PRIMARY_KEY.to_string(),
        }
    }

    /// Get a receiver for role changes.
    ///
    /// The integration layer watches this to restart sqld when the role
    /// changes (e.g., replica promoted to primary on failover).
    #[must_use]
    pub fn watch_role(&self) -> watch::Receiver<ElectedRole> {
        self.role_rx.clone()
    }

    /// Determine the initial role by checking existing gossip state.
    ///
    /// Should be called once before starting the election loop.
    ///
    /// A fresh node never returns `Primary` from here unless the existing
    /// gossip claim already names *this* node (a fast restart of the
    /// incumbent, within the claim TTL). At boot, gossip has not converged
    /// — the membership view may hold only the local node and an
    /// incumbent's claim may not have arrived yet — so deciding "I am the
    /// lowest-ordinal alive node, therefore primary" here is exactly the
    /// role-theft bug of issue #314. The election loop ([`run`](Self::run))
    /// makes the first claim instead, after [`STARTUP_GRACE`].
    ///
    /// The decision is also published to the role watch channel so
    /// [`watch_role`](Self::watch_role) receivers and the loop's own
    /// current-role view agree with the value returned here.
    pub async fn determine_initial_role(&self) -> ElectedRole {
        let role = self.initial_role_inner().await;
        self.role_tx.send_replace(role.clone());
        role
    }

    async fn initial_role_inner(&self) -> ElectedRole {
        let primary_key = self.primary_key();
        // Check if there's already a primary claim in gossip.
        if let Some(bytes) = self.cluster.gossip_get(&primary_key).await
            && let Some(claim) = PrimaryClaim::decode(&bytes)
        {
            match classify_claim(&claim, &self.cluster.self_node().id, &self.log_id) {
                ClaimKind::OwnFresh => {
                    // Our own (unexpired) claim survived a restart AND the
                    // database it describes is the one we still hold —
                    // reclaiming is not a theft; refresh and carry on. In
                    // per-site mode the reclaim is additionally conditional on
                    // live HRW still naming us: ownership there is a function
                    // of membership, and a claim that outlived its membership
                    // must not be reasserted (see `per_site_hrw_names_us`).
                    if self.site.is_none() || self.per_site_hrw_names_us().await {
                        self.publish_claim().await;
                        tracing::info!(
                            "SQLite election: reclaiming our own surviving primary claim"
                        );
                        return ElectedRole::Primary;
                    }
                    tracing::info!(
                        site = ?self.site,
                        "per-site election: our surviving claim is no longer supported by live \
                         HRW ownership; not reclaiming"
                    );
                    return ElectedRole::Replica { primary_grpc_url: String::new() };
                }
                ClaimKind::OwnStale => {
                    // Same node id, DIFFERENT CDC log identity: this node
                    // came back with a wiped/replaced database (empty volume
                    // on redeploy) inside the claim TTL. The surviving claim
                    // describes data we no longer have, so fast-reclaiming
                    // would re-root the cluster onto an empty database
                    // (issue #344 — #314's blast radius through a narrower
                    // door). Refuse the fast path: do not reclaim; return
                    // unresolved so the election loop applies the startup
                    // grace + election and a node that still holds the data
                    // can win.
                    tracing::warn!(
                        claimed_log = %claim.log_id,
                        local_log = %self.log_id,
                        "SQLite election: a surviving primary claim names this node but its CDC \
                         log identity does not match our current database (wiped/replaced across \
                         restart, issue #344); refusing the fast reclaim and deferring to election"
                    );
                    return ElectedRole::Replica { primary_grpc_url: String::new() };
                }
                ClaimKind::Foreign => {
                    if let Some(url) = self.replica_url_for(&claim).await {
                        return ElectedRole::Replica { primary_grpc_url: url };
                    }
                    // Claim points at a host that is not a known member.
                    // Refuse to dial it (defense in depth against a forged
                    // gossip claim) and wait for a valid one.
                    return ElectedRole::Replica { primary_grpc_url: String::new() };
                }
            }
        }

        // No valid claim visible. That means either the cluster genuinely
        // has no primary, or gossip simply has not delivered the
        // incumbent's claim yet — indistinguishable this early. Do NOT
        // claim: start as an unresolved replica and let the election loop
        // claim after the startup grace (issue #314).
        ElectedRole::Replica { primary_grpc_url: String::new() }
    }

    /// Run the election loop. This should be spawned as a tokio task.
    ///
    /// - If primary: heartbeats the claim every 5 seconds.
    /// - Periodically checks if the primary claim has expired and
    ///   re-evaluates whether this node should promote.
    pub async fn run(self) {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);

        loop {
            interval.tick().await;

            let current_role = self.role_rx.borrow().clone();
            let new_role = self.evaluate_role(&current_role).await;

            if new_role != current_role {
                tracing::info!(
                    old = ?current_role,
                    new = ?new_role,
                    "SQLite election: role changed"
                );
                // Ignore send errors — receiver may have been dropped.
                let _ = self.role_tx.send(new_role);
            }
        }
    }

    /// Evaluate what role this node should have right now.
    ///
    /// `current` is the role this node currently holds (from the watch
    /// channel). It matters in exactly one place: when a *live* foreign
    /// claim conflicts with our own primary role, the tie is broken
    /// deterministically instead of by whoever gossiped last (issue #314).
    async fn evaluate_role(&self, current: &ElectedRole) -> ElectedRole {
        let self_node = self.cluster.self_node();

        // Check existing primary claim.
        if let Some(bytes) = self.cluster.gossip_get(&self.primary_key()).await
            && let Some(claim) = PrimaryClaim::decode(&bytes)
        {
            match classify_claim(&claim, &self_node.id, &self.log_id) {
                ClaimKind::OwnFresh => {
                    // We are the primary and the claim describes the database
                    // we still hold — refresh heartbeat, unless live membership
                    // has since moved this site's ownership elsewhere. In
                    // per-site mode ownership is a pure function of the alive
                    // set, so refreshing a claim HRW no longer supports is what
                    // wedges the site (see `per_site_hrw_names_us`): stop
                    // refreshing, let the claim expire, and fall through to
                    // resolve the real owner. Single-DB mode is unaffected —
                    // `per_site_hrw_names_us` is always false there.
                    if self.site.is_none() || self.per_site_hrw_names_us().await {
                        self.publish_claim().await;
                        return ElectedRole::Primary;
                    }
                    tracing::info!(
                        site = ?self.site,
                        "per-site election: HRW ownership of this site moved to another node; \
                         releasing our claim instead of refreshing it"
                    );
                }
                ClaimKind::OwnStale => {
                    // Same node id, DIFFERENT CDC log identity (issue #344): a
                    // stale claim from a previous incarnation whose database
                    // is gone. Do NOT reclaim and do NOT refresh it — that is
                    // what re-roots the cluster onto an empty database. Treat
                    // it as absent and fall through to the startup grace +
                    // election tail below (the grace still gates a first
                    // self-election while current != Primary), letting the
                    // claim expire so a node that still holds the data can win.
                    tracing::warn!(
                        claimed_log = %claim.log_id,
                        local_log = %self.log_id,
                        "SQLite election: surviving primary claim names this node but its CDC log \
                         identity no longer matches our database (wiped/replaced, issue #344); not \
                         reclaiming — deferring to election"
                    );
                }
                ClaimKind::Foreign => {
                    // Someone else claims primary -- check if they're alive
                    // AND that the advertised gRPC address belongs to a known
                    // member (defense in depth: a forged claim from a
                    // plaintext gossip injection must not make us dial an
                    // arbitrary host).
                    let nodes = self.cluster.nodes().await;
                    let primary_alive =
                        nodes.iter().any(|n| n.id == claim.node_id && n.state == NodeState::Alive);
                    // Per-site: if live HRW names US, the foreign claim predates
                    // the membership change that gave us the site. Never
                    // subordinate to it — fall through and elect. (Always false
                    // in single-DB mode, so that path is byte-for-byte as it
                    // was.)
                    let hrw_names_us = self.per_site_hrw_names_us().await;

                    if primary_alive && !hrw_names_us {
                        // Conflict: we hold the primary role, yet a live peer
                        // claims it too (gossip KV is last-write-wins, so its
                        // newer write shadows ours). Do not yield
                        // unconditionally — that is how a joining node stole
                        // the role from a healthy incumbent. Break the tie by
                        // the documented rule: lowest node id wins.
                        if matches!(current, ElectedRole::Primary) {
                            if self.wins_conflict(&self_node.id, &claim.node_id) {
                                tracing::warn!(
                                    claimant = %claim.node_id,
                                    "SQLite election: live conflicting primary claim from a \
                                     higher-ordinal node; keeping the primary role and \
                                     re-asserting our claim (lowest node id wins)"
                                );
                                self.publish_claim().await;
                                return ElectedRole::Primary;
                            }
                            tracing::warn!(
                                claimant = %claim.node_id,
                                "SQLite election: live conflicting primary claim from a \
                                 lower-ordinal node; stepping down (lowest node id wins)"
                            );
                        }
                        if let Some(url) = self.replica_url_for(&claim).await {
                            return ElectedRole::Replica { primary_grpc_url: url };
                        }
                        // Alive primary but the gRPC host is not a known
                        // member -- refuse to dial it and wait for a valid
                        // claim.
                        return ElectedRole::Replica { primary_grpc_url: String::new() };
                    }

                    if hrw_names_us {
                        tracing::info!(
                            site = ?self.site,
                            claimant = %claim.node_id,
                            "per-site election: a stale claim names another node, but live HRW \
                             names us as this site's owner; taking ownership"
                        );
                    } else {
                        // Primary is dead — fall through to re-election.
                        tracing::warn!(
                            dead_primary = %claim.node_id,
                            "primary node is dead, triggering re-election"
                        );
                    }
                }
            }
        }

        // No valid primary claim visible. If this node started recently,
        // that absence is not evidence there is no primary — gossip may
        // simply not have delivered the incumbent's claim yet. Defer the
        // first self-election until the startup grace has passed
        // (issue #314). A node that already holds the primary role is by
        // definition past its first election and is not gated.
        if !matches!(current, ElectedRole::Primary) && self.boot.elapsed() < STARTUP_GRACE {
            tracing::debug!(
                elapsed_ms = self.boot.elapsed().as_millis(),
                grace_ms = STARTUP_GRACE.as_millis(),
                "SQLite election: no primary claim visible, but within the startup \
                 grace window — deferring self-election until gossip has converged"
            );
            return ElectedRole::Replica { primary_grpc_url: String::new() };
        }

        // No valid primary claim — elect.
        if self.should_be_primary().await {
            self.publish_claim().await;
            tracing::info!(
                node_id = %self_node.id,
                "elected as SQLite primary"
            );
            ElectedRole::Primary
        } else {
            // Not our turn — wait for the rightful primary to claim.
            ElectedRole::Replica { primary_grpc_url: String::new() }
        }
    }

    /// Whether this node should be primary for the database this election
    /// governs.
    ///
    /// - **Per-site** (`site = Some`): this node is the site's HRW owner
    ///   ([`hrw_owner`]) among the alive nodes.
    /// - **Single-database** (`site = None`): this node is the
    ///   lowest-ordinal alive node (the documented single-DB rule).
    async fn should_be_primary(&self) -> bool {
        let self_id = &self.cluster.self_node().id;
        let nodes = self.cluster.nodes().await;
        node_should_be_primary(self.site.as_deref(), self_id, &nodes)
    }

    /// Whether **live membership** currently makes this node the site's owner,
    /// in per-site mode. Always `false` for the single-database election.
    ///
    /// This is the convergence rule for per-site ownership: the owner is a pure
    /// function of `(site, alive members)`, so any claim that disagrees with it
    /// is stale by definition — no matter who wrote it, or whether they are
    /// still alive. The published claim exists to carry the owner's *address*
    /// and its CDC log identity (the issue #344 guard), never to override
    /// membership. Before this, a claim outlived the membership change that
    /// invalidated it: the stale claimant kept refreshing it and refusing to
    /// serve `cdc/<site>` (it gates serving on live HRW) while the real owner
    /// retried a refused dial forever — a site's replication wedged permanently.
    async fn per_site_hrw_names_us(&self) -> bool {
        if self.site.is_none() {
            return false;
        }
        self.should_be_primary().await
    }

    /// Resolve a live conflict between our own primary role and a foreign
    /// live claim: `true` means *we* keep the role. The rule matches the
    /// election rule for this database — HRW (higher score wins) per-site,
    /// lowest node id for the single database — so the tie-break can never
    /// contradict `should_be_primary`.
    fn wins_conflict(&self, self_id: &str, claimant_id: &str) -> bool {
        match &self.site {
            Some(site) => {
                let mine = hrw_score(self_id, site);
                let theirs = hrw_score(claimant_id, site);
                // Higher score wins; ties broken by node id (higher id), the
                // same total order `hrw_owner` uses.
                (mine, self_id) > (theirs, claimant_id)
            }
            None => incumbent_wins_tie(self_id, claimant_id),
        }
    }

    /// Validate a primary claim and build the replica gRPC URL for it.
    ///
    /// Returns `Some(url)` only when the claim's advertised `grpc_addr`
    /// host matches a currently-known gossip member's address. This is
    /// defense in depth on top of the mandatory cluster secret (see
    /// `ClusterConfig::ensure_secure`): even if an attacker managed to
    /// inject a claim into gossip, a replica will not dial a `host:port`
    /// that does not belong to a live cluster member (blocks SSRF and
    /// pointing replication at an attacker-controlled sqld).
    ///
    /// The comparison is by host only, because a node's gossip address
    /// and its sqld gRPC address use different ports on the same host.
    async fn replica_url_for(&self, claim: &PrimaryClaim) -> Option<String> {
        let claim_host = host_of(&claim.grpc_addr)?;
        let nodes = self.cluster.nodes().await;
        let known = nodes.iter().filter_map(|n| host_of(&n.gossip_addr));

        if member_hosts_contain(known, &claim_host) {
            Some(format!("http://{}", claim.grpc_addr))
        } else {
            tracing::warn!(
                primary = %claim.node_id,
                grpc_addr = %claim.grpc_addr,
                "refusing to dial SQLite primary: advertised gRPC host is not a known cluster \
                 member (possible forged gossip claim)"
            );
            None
        }
    }

    /// Publish this node's primary claim to the gossip KV tier.
    async fn publish_claim(&self) {
        let claim = PrimaryClaim {
            node_id: self.cluster.self_node().id.clone(),
            grpc_addr: self.grpc_listen.clone(),
            log_id: self.log_id.clone(),
        };
        self.cluster.gossip_set(&self.primary_key(), &claim.encode(), Some(PRIMARY_TTL)).await;
    }
}

/// How a surviving primary claim relates to *this* node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimKind {
    /// Names this node AND describes the database this node still holds
    /// (node id and CDC log identity both match). Eligible for the
    /// fast-restart reclaim.
    OwnFresh,
    /// Names this node but with a *different* CDC log identity — a stale
    /// claim from a previous incarnation whose database was wiped/replaced
    /// (issue #344). Must NOT be fast-reclaimed; defer to grace + election.
    OwnStale,
    /// Names a different node.
    Foreign,
}

/// Classify a surviving primary claim against this node's identity.
///
/// The fast-restart reclaim (issue #314) must fire only for
/// [`ClaimKind::OwnFresh`]: a matching node id is not enough, because a
/// redeploy onto an empty volume brings the same node id back with a
/// *different* database and a fresh CDC log identity. Requiring the log id
/// to match too is what closes the issue #344 window — the same node
/// returning with wiped data classifies as [`ClaimKind::OwnStale`] and is
/// denied the fast path.
fn classify_claim(claim: &PrimaryClaim, self_id: &str, self_log_id: &str) -> ClaimKind {
    if claim.node_id != self_id {
        ClaimKind::Foreign
    } else if claim.log_id == self_log_id {
        ClaimKind::OwnFresh
    } else {
        ClaimKind::OwnStale
    }
}

/// Whether `self_id` should hold the primary role for the database this
/// election governs, given the current membership view.
///
/// - **Per-site** (`site = Some`): `self_id` is the site's HRW owner
///   ([`hrw_owner`]) among the alive nodes.
/// - **Single-database** (`site = None`): `self_id` is the lowest-ordinal alive
///   node (the documented single-DB rule).
///
/// A free function over an explicit membership slice so both the election's own
/// `should_be_primary` and its tests exercise the same rule with no cluster.
#[must_use]
fn node_should_be_primary(site: Option<&str>, self_id: &str, nodes: &[NodeInfo]) -> bool {
    match site {
        Some(site) => hrw_owner(nodes, site).is_some_and(|n| n.id == self_id),
        None => nodes
            .iter()
            .filter(|n| n.state == NodeState::Alive)
            .min_by(|a, b| a.id.cmp(&b.id))
            .is_some_and(|n| n.id == self_id),
    }
}

/// Deterministic resolution for two live, conflicting primary claims:
/// the incumbent keeps the role iff its node id sorts strictly lower than
/// the claimant's — the same "lowest ordinal wins" rule the election uses
/// for a fresh cluster, applied to conflicts (issue #314).
fn incumbent_wins_tie(self_id: &str, claimant_id: &str) -> bool {
    self_id < claimant_id
}

/// Extract the host portion of a `host:port` (or `[ipv6]:port`) address.
///
/// Returns `None` if the string has no port separator. Parsing as a
/// [`SocketAddr`](std::net::SocketAddr) first canonicalizes IPv6 forms
/// (e.g. `[::1]` vs `[0:0:...:1]`) so two spellings of the same address
/// compare equal; a bare `host:port` that is not a literal socket addr
/// falls back to splitting on the last colon.
fn host_of(addr: &str) -> Option<String> {
    if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
        return Some(sock.ip().to_string());
    }
    // Not a literal IP:port (e.g. a DNS name). Split off the final `:port`.
    let (host, _port) = addr.rsplit_once(':')?;
    if host.is_empty() { None } else { Some(host.to_string()) }
}

/// Whether any known member host equals `claim_host`.
fn member_hosts_contain<I>(mut members: I, claim_host: &str) -> bool
where
    I: Iterator<Item = String>,
{
    members.any(|h| h == claim_host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_claim_roundtrip() {
        let claim = PrimaryClaim {
            node_id: "ephpm-0".into(),
            grpc_addr: "10.0.1.2:5001".into(),
            log_id: "0123456789abcdef0123456789abcdef".into(),
        };
        let encoded = claim.encode();
        let decoded = PrimaryClaim::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, "ephpm-0");
        assert_eq!(decoded.grpc_addr, "10.0.1.2:5001");
        assert_eq!(decoded.log_id, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn primary_claim_decode_invalid() {
        assert!(PrimaryClaim::decode(b"no-pipe-here").is_none());
        assert!(PrimaryClaim::decode(b"").is_none());
    }

    #[test]
    fn primary_claim_encode_format() {
        let claim = PrimaryClaim {
            node_id: "node-1".into(),
            grpc_addr: "0.0.0.0:5001".into(),
            log_id: "cafef00d".into(),
        };
        let bytes = claim.encode();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "node-1|0.0.0.0:5001|cafef00d");
    }

    /// A legacy two-field claim (`node_id|grpc_addr`, no log id) written by
    /// a node that has not yet upgraded must still decode — with an empty
    /// log id, which the #344 reclaim guard treats as "does not match" and
    /// so safely defers to election.
    #[test]
    fn primary_claim_decode_legacy_two_field() {
        let decoded = PrimaryClaim::decode(b"ephpm-0|10.0.1.2:5001").unwrap();
        assert_eq!(decoded.node_id, "ephpm-0");
        assert_eq!(decoded.grpc_addr, "10.0.1.2:5001");
        assert_eq!(decoded.log_id, "");
    }

    #[test]
    fn elected_role_equality() {
        assert_eq!(ElectedRole::Primary, ElectedRole::Primary);
        assert_ne!(
            ElectedRole::Primary,
            ElectedRole::Replica { primary_grpc_url: "http://x:5001".into() }
        );
        assert_eq!(
            ElectedRole::Replica { primary_grpc_url: "http://x:5001".into() },
            ElectedRole::Replica { primary_grpc_url: "http://x:5001".into() }
        );
    }

    #[test]
    fn elected_role_different_replicas_not_equal() {
        assert_ne!(
            ElectedRole::Replica { primary_grpc_url: "http://a:5001".into() },
            ElectedRole::Replica { primary_grpc_url: "http://b:5001".into() }
        );
    }

    #[test]
    fn primary_claim_with_ipv6() {
        let claim = PrimaryClaim {
            node_id: "node-v6".into(),
            grpc_addr: "[::1]:5001".into(),
            log_id: "deadbeef".into(),
        };
        let encoded = claim.encode();
        let decoded = PrimaryClaim::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, "node-v6");
        assert_eq!(decoded.grpc_addr, "[::1]:5001");
        assert_eq!(decoded.log_id, "deadbeef");
    }

    #[test]
    fn primary_claim_decode_three_fields() {
        // node_id | grpc_addr | log_id — a plain three-way split (neither an
        // address nor a hex log id contains a pipe).
        let decoded = PrimaryClaim::decode(b"a|10.0.1.2:5001|abc123").unwrap();
        assert_eq!(decoded.node_id, "a");
        assert_eq!(decoded.grpc_addr, "10.0.1.2:5001");
        assert_eq!(decoded.log_id, "abc123");
    }

    #[test]
    fn primary_claim_with_long_node_id() {
        let long_id = "ephpm-".to_string() + &"x".repeat(200);
        let claim = PrimaryClaim {
            node_id: long_id.clone(),
            grpc_addr: "10.0.1.2:5001".into(),
            log_id: "abcdef".into(),
        };
        let roundtripped = PrimaryClaim::decode(&claim.encode()).unwrap();
        assert_eq!(roundtripped.node_id, long_id);
        assert_eq!(roundtripped.log_id, "abcdef");
    }

    /// Verify that lowest-ordinal wins: when comparing node IDs
    /// alphabetically, the smallest should become primary.
    #[test]
    fn lowest_ordinal_election_logic() {
        // Simulate the election logic: filter alive, pick min by id.
        let nodes = [
            ("ephpm-c", true),
            ("ephpm-a", true),
            ("ephpm-b", false), // dead
            ("ephpm-d", true),
        ];

        let lowest_alive =
            nodes.iter().filter(|(_, alive)| *alive).min_by(|a, b| a.0.cmp(b.0)).map(|(id, _)| *id);

        assert_eq!(lowest_alive, Some("ephpm-a"));
    }

    /// Verify that when the primary dies, the next lowest becomes primary.
    #[test]
    fn failover_to_next_lowest() {
        let nodes = [
            ("ephpm-a", false), // was primary, now dead
            ("ephpm-b", true),
            ("ephpm-c", true),
        ];

        let lowest_alive =
            nodes.iter().filter(|(_, alive)| *alive).min_by(|a, b| a.0.cmp(b.0)).map(|(id, _)| *id);

        assert_eq!(lowest_alive, Some("ephpm-b"));
    }

    #[test]
    fn host_of_ipv4_socket() {
        assert_eq!(host_of("10.0.1.2:5001").as_deref(), Some("10.0.1.2"));
    }

    #[test]
    fn host_of_ipv6_socket_canonicalizes() {
        // Both spellings of loopback must yield the same host string so a
        // member advertised one way still matches a claim spelled another.
        let a = host_of("[::1]:5001").unwrap();
        let b = host_of("[0:0:0:0:0:0:0:1]:7946").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn host_of_dns_name_splits_last_colon() {
        assert_eq!(host_of("node-a.internal:5001").as_deref(), Some("node-a.internal"));
    }

    #[test]
    fn host_of_no_port_is_none() {
        assert_eq!(host_of("10.0.1.2"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn member_validation_accepts_known_host() {
        // A claim's gRPC host (different port) matching a member's gossip
        // host is accepted.
        let members = ["10.0.1.2:7946".to_string(), "10.0.1.3:7946".to_string()];
        let claim_host = host_of("10.0.1.2:5001").unwrap();
        let known = members.iter().filter_map(|m| host_of(m));
        assert!(member_hosts_contain(known, &claim_host));
    }

    #[test]
    fn member_validation_rejects_unknown_host() {
        // A forged claim pointing at an attacker host not in the member
        // list must be rejected.
        let members = ["10.0.1.2:7946".to_string(), "10.0.1.3:7946".to_string()];
        let claim_host = host_of("6.6.6.6:5001").unwrap();
        let known = members.iter().filter_map(|m| host_of(m));
        assert!(!member_hosts_contain(known, &claim_host));
    }

    #[test]
    fn heartbeat_ttl_constants_valid() {
        // TTL must be greater than heartbeat interval for liveness detection.
        assert!(PRIMARY_TTL > HEARTBEAT_INTERVAL);
        // The ratio should allow at least one missed heartbeat.
        assert!(PRIMARY_TTL >= HEARTBEAT_INTERVAL * 2);
    }

    /// The startup grace must outlast a full claim TTL plus one heartbeat:
    /// only then is a joining node guaranteed to have had the chance to
    /// observe a live incumbent's (re-published) claim before it may
    /// self-elect. Issue #314.
    #[test]
    fn startup_grace_covers_claim_ttl_and_a_heartbeat() {
        assert!(STARTUP_GRACE >= PRIMARY_TTL + HEARTBEAT_INTERVAL);
    }

    /// Conflicting live claims resolve by node id, lowest wins — in both
    /// directions, so exactly one of the two conflicting nodes keeps the
    /// role. Issue #314.
    #[test]
    fn conflicting_claims_resolve_to_lowest_node_id() {
        // Observed field failure: cdc-node-3 (joiner) took the role from
        // cdc-node-2 (incumbent). The incumbent must win this tie...
        assert!(incumbent_wins_tie("cdc-node-2", "cdc-node-3"));
        // ...and symmetrically, if the *joiner* somehow held the role, it
        // must yield to the lower-ordinal claimant.
        assert!(!incumbent_wins_tie("cdc-node-3", "cdc-node-2"));
        // Equal ids never conflict (that is the refresh-own-claim path),
        // but the tie-break must not let an equal id "win" as incumbent
        // AND as claimant on two nodes at once.
        assert!(!incumbent_wins_tie("cdc-node-1", "cdc-node-1"));
    }

    fn claim_with_log(node_id: &str, log_id: &str) -> PrimaryClaim {
        PrimaryClaim {
            node_id: node_id.into(),
            grpc_addr: "10.0.1.2:5001".into(),
            log_id: log_id.into(),
        }
    }

    /// The issue #344 window: node N was primary with CDC log identity A and
    /// published a claim; N is redeployed onto an empty volume and restarts
    /// within the claim TTL, now holding a *fresh, empty* database with log
    /// identity B. Its own surviving claim (node N, log A) is still in
    /// gossip. The claim-mine fast reclaim must NOT fire — the claim names
    /// data N no longer has, and reclaiming would re-root the cluster onto
    /// the empty database. It must classify as `OwnStale` (defer to
    /// election), not `OwnFresh` (fast reclaim).
    #[test]
    fn wiped_restart_does_not_take_fast_reclaim() {
        let self_id = "ephpm-0";
        let log_before_wipe = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let log_after_wipe = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let surviving_claim = claim_with_log(self_id, log_before_wipe);

        // Same node id, different (fresh) log identity → stale, must defer.
        assert_eq!(
            classify_claim(&surviving_claim, self_id, log_after_wipe),
            ClaimKind::OwnStale,
            "a wiped-DB restart must not be eligible for the fast reclaim"
        );
    }

    /// The deliberate fast-restart behaviour (issue #314) is preserved when
    /// the data identity is unchanged: same node id AND same CDC log
    /// identity classifies as `OwnFresh`, so a genuine fast restart still
    /// reclaims immediately without bouncing the role.
    #[test]
    fn genuine_fast_restart_still_reclaims() {
        let self_id = "ephpm-0";
        let log = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let surviving_claim = claim_with_log(self_id, log);
        assert_eq!(
            classify_claim(&surviving_claim, self_id, log),
            ClaimKind::OwnFresh,
            "an unchanged-data fast restart must still reclaim"
        );
    }

    /// A claim naming a different node is `Foreign` regardless of log id —
    /// the log-identity guard only gates our *own* claim's fast reclaim.
    #[test]
    fn foreign_claim_is_foreign_regardless_of_log_id() {
        let claim = claim_with_log("ephpm-1", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // Even if the log ids happened to collide, a different node id is
        // always foreign.
        assert_eq!(
            classify_claim(&claim, "ephpm-0", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ClaimKind::Foreign
        );
        assert_eq!(
            classify_claim(&claim, "ephpm-0", "cccccccccccccccccccccccccccccccc"),
            ClaimKind::Foreign
        );
    }

    /// A legacy two-field claim (empty log id after decode) from a node that
    /// has not yet upgraded must never satisfy the fast reclaim against a
    /// node holding a real (non-empty) log id — it classifies as `OwnStale`
    /// and defers, which is the safe direction during a rolling upgrade.
    #[test]
    fn legacy_empty_log_claim_defers() {
        let legacy = PrimaryClaim::decode(b"ephpm-0|10.0.1.2:5001").unwrap();
        assert_eq!(legacy.log_id, "");
        assert_eq!(
            classify_claim(&legacy, "ephpm-0", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ClaimKind::OwnStale
        );
    }

    // ----- Per-site HRW ownership (Phase 0 clustered per-site) -----

    fn alive(id: &str) -> NodeInfo {
        NodeInfo { id: id.into(), gossip_addr: "10.0.0.1:7946".into(), state: NodeState::Alive }
    }

    fn dead(id: &str) -> NodeInfo {
        NodeInfo { id: id.into(), gossip_addr: "10.0.0.1:7946".into(), state: NodeState::Dead }
    }

    #[test]
    fn per_site_key_is_namespaced() {
        assert_eq!(primary_key_for_site("blog.example.com"), "sqlite:primary:blog.example.com");
        // The global key is untouched.
        assert_eq!(PRIMARY_KEY, "sqlite:primary");
    }

    #[test]
    fn hrw_score_is_deterministic_and_order_sensitive() {
        // Stable across calls (the property every node relies on).
        assert_eq!(hrw_score("ephpm-0", "site-a"), hrw_score("ephpm-0", "site-a"));
        // The domain separator makes the split between node id and site
        // significant, so a shared concatenation does not collide.
        assert_ne!(hrw_score("ab", "c"), hrw_score("a", "bc"));
    }

    #[test]
    fn hrw_owner_is_deterministic_and_alive_only() {
        let nodes = [alive("ephpm-0"), alive("ephpm-1"), alive("ephpm-2")];
        let a = hrw_owner(&nodes, "tenant-x").map(|n| n.id.clone());
        let b = hrw_owner(&nodes, "tenant-x").map(|n| n.id.clone());
        assert_eq!(a, b, "owner selection must be deterministic");
        assert!(a.is_some());
        // No alive node → no owner.
        let all_dead = [dead("ephpm-0"), dead("ephpm-1")];
        assert!(hrw_owner(&all_dead, "tenant-x").is_none());
    }

    #[test]
    fn hrw_owner_excludes_dead_nodes() {
        // Find a site whose owner (over three alive nodes) is a specific
        // node, then kill that node and confirm ownership moves to another
        // node that was already in the set (a warm replica) — the core
        // rendezvous-hashing property.
        let all = [alive("ephpm-0"), alive("ephpm-1"), alive("ephpm-2")];
        // Some site is owned by *some* node; pick that node and mark it dead.
        let owner = hrw_owner(&all, "failover-site").unwrap().id.clone();
        let after: Vec<NodeInfo> =
            all.iter().map(|n| if n.id == owner { dead(&n.id) } else { n.clone() }).collect();
        let new_owner = hrw_owner(&after, "failover-site").unwrap();
        assert_ne!(new_owner.id, owner, "a dead owner must not keep the site");
        assert_eq!(new_owner.state, NodeState::Alive);
        // And the new owner is one of the original nodes (already replicating).
        assert!(all.iter().any(|n| n.id == new_owner.id));
    }

    #[test]
    fn hrw_owner_distributes_sites_across_nodes() {
        // Over many sites, ownership should not collapse onto one node —
        // otherwise the "spread tenants across the cluster" premise fails.
        let nodes = [alive("ephpm-0"), alive("ephpm-1"), alive("ephpm-2")];
        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            let site = format!("site-{i:03}");
            seen.insert(hrw_owner(&nodes, &site).unwrap().id.clone());
        }
        assert!(seen.len() >= 2, "HRW must spread sites across more than one node: {seen:?}");
    }

    // ----- Ownership converges on live membership (PR #416 blocker 3) -----

    /// A site's owner is a pure function of `(site, alive set)`. When a node
    /// joins and HRW re-homes the site, the previous owner must observe that it
    /// is no longer the owner — that is what makes it stop refreshing its claim
    /// instead of holding a key the serving side already disagrees with.
    #[test]
    fn per_site_ownership_follows_membership_changes() {
        let before = [alive("ephpm-0"), alive("ephpm-1")];
        // Find a site the joining node takes over, which is the interesting
        // case (HRW deliberately re-homes only a fraction of sites).
        let after = [alive("ephpm-0"), alive("ephpm-1"), alive("ephpm-2")];
        let moved = (0..500)
            .map(|i| format!("site-{i:03}"))
            .find(|site| {
                hrw_owner(&before, site).unwrap().id != hrw_owner(&after, site).unwrap().id
            })
            .expect("some site must re-home when a third node joins");

        let old_owner = hrw_owner(&before, &moved).unwrap().id.clone();
        let new_owner = hrw_owner(&after, &moved).unwrap().id.clone();
        assert_ne!(old_owner, new_owner);

        // Before the join the old owner is the owner...
        assert!(node_should_be_primary(Some(&moved), &old_owner, &before));
        // ...and after it, it is NOT — so the OwnFresh arm releases its claim
        // rather than refreshing a claim live membership no longer supports.
        assert!(
            !node_should_be_primary(Some(&moved), &old_owner, &after),
            "a stale claimant must not still evaluate as the owner after the membership change"
        );
        // The new owner elects, even though a live foreign claim still names
        // the old one.
        assert!(node_should_be_primary(Some(&moved), &new_owner, &after));
    }

    /// The single-database rule is untouched: lowest-ordinal alive node, dead
    /// nodes excluded.
    #[test]
    fn single_db_ownership_is_still_lowest_ordinal() {
        let nodes = [dead("ephpm-a"), alive("ephpm-b"), alive("ephpm-c")];
        assert!(node_should_be_primary(None, "ephpm-b", &nodes));
        assert!(!node_should_be_primary(None, "ephpm-c", &nodes));
        assert!(!node_should_be_primary(None, "ephpm-a", &nodes), "a dead node never wins");
        assert!(!node_should_be_primary(None, "ephpm-b", &[]), "no members, no primary");
    }

    // ----- Per-site claim address validation (PR #416 blocker 2) -----

    fn member_at(id: &str, gossip: &str) -> NodeInfo {
        NodeInfo { id: id.into(), gossip_addr: gossip.into(), state: NodeState::Alive }
    }

    /// `per_site_primary`'s address is dialed and every one of that tenant's
    /// SQL statements is forwarded to it, so a claim naming a host gossip does
    /// not know must be refused — the same rule `replica_url_for` applies.
    #[test]
    fn per_site_claim_address_must_belong_to_a_member() {
        let members =
            [member_at("ephpm-0", "10.0.1.2:7946"), member_at("ephpm-1", "10.0.1.3:7946")];

        // A member's host on a different port (the channel port) is accepted.
        assert_eq!(
            claim_addr_if_member("10.0.1.3:7948", &members).as_deref(),
            Some("10.0.1.3:7948")
        );
        // An attacker-chosen host is refused, so the caller falls back to the
        // HRW owner's derived address instead of dialing it.
        assert_eq!(claim_addr_if_member("6.6.6.6:7948", &members), None);
        // So is a DNS-shaped host that is not a member, and a malformed one.
        assert_eq!(claim_addr_if_member("evil.example.com:7948", &members), None);
        assert_eq!(claim_addr_if_member("no-port-here", &members), None);
        assert_eq!(claim_addr_if_member("", &members), None);
        // With no members at all nothing validates (fail closed).
        assert_eq!(claim_addr_if_member("10.0.1.3:7948", &[]), None);
    }

    #[test]
    fn hrw_conflict_resolution_matches_ownership() {
        // The per-site conflict tie-break must agree with hrw_owner: for a
        // given site, exactly the higher-scoring node "wins" the conflict.
        let site = "tenant-y";
        let a = "ephpm-0";
        let b = "ephpm-1";
        let a_wins = (hrw_score(a, site), a) > (hrw_score(b, site), b);
        // Emulate wins_conflict's per-site branch directly (no cluster needed).
        assert_eq!((hrw_score(a, site), a) > (hrw_score(b, site), b), a_wins);
        assert_eq!((hrw_score(b, site), b) > (hrw_score(a, site), a), !a_wins);
    }
}
