//! The connection + channel table.
//!
//! Two `DashMap`s and an atomic counter:
//!
//! * `conns`: connection ID → [`ConnEntry`]. Globally keyed by ID (IDs are
//!   128-bit random, so they never collide across sites), with the owning site
//!   stored **inside** the entry. Every scoped operation looks the ID up and
//!   then compares the entry's site against the caller's scope — the site is a
//!   property of the connection, never of the argument the caller supplied.
//! * `channels`: `(site, channel)` → set of member IDs. The site is part of the
//!   key, so `chat` under site `a` and `chat` under site `b` are different
//!   channels that cannot see each other.
//!
//! # Lock discipline
//!
//! No `DashMap` reference is ever held across an `await`, across a PHP call, or
//! while taking a second reference into the *same* map. `broadcast` clones the
//! member ID list out of the channel shard and drops the reference before it
//! sends anything, so a fan-out never pins a shard for the duration of N sends.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use bytes::Bytes;
use dashmap::DashMap;
use metrics::{counter, gauge};
use parking_lot::Mutex;
use tokio::sync::{Notify, mpsc};

use crate::{CLOSE_QUEUE_OVERFLOW, Limits, OutFrame, RegisterError};

/// Maximum channels one connection may be subscribed to at once.
///
/// A fixed ceiling rather than a config knob: it exists to bound the memory a
/// single misbehaving script can pin (one `HashSet` entry per subscription, in
/// both directions), and no deployment has a reason to tune it. Subscribing
/// past this returns `false`.
pub const MAX_CHANNELS_PER_CONNECTION: usize = 64;

/// Maximum channel-name length in bytes. Same rationale as
/// [`MAX_CHANNELS_PER_CONNECTION`] — channel names come from userland and are
/// stored twice (once per membership direction), so they are capped rather than
/// trusted.
pub const MAX_CHANNEL_NAME_LEN: usize = 256;

/// The `(site, channel)` composite key.
///
/// A struct rather than a joined string: there is no separator byte to reason
/// about, so no channel name can be crafted to collide with another site's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChannelKey {
    site: Box<str>,
    channel: Box<str>,
}

impl ChannelKey {
    fn new(site: &str, channel: &str) -> Self {
        Self { site: Box::from(site), channel: Box::from(channel) }
    }
}

/// Out-of-band control for one connection: "stop, and close with this code".
///
/// Separate from the outbound frame queue on purpose. A close must be
/// deliverable *precisely when the queue is full* — that is the overflow case —
/// so it cannot travel through the queue it is reacting to.
#[derive(Debug)]
pub struct Control {
    notify: Notify,
    /// The close code to send, or `0` when no close has been requested. Only
    /// the first request wins: an overflow shed and a userland
    /// `ephpm_ws_close` racing each other must not produce two close frames.
    close_code: AtomicU16,
}

impl Control {
    fn new() -> Self {
        Self { notify: Notify::new(), close_code: AtomicU16::new(0) }
    }

    /// Request that this connection be closed with `code`. First caller wins.
    ///
    /// Returns `true` if this call is the one that scheduled the close.
    fn request_close(&self, code: u16) -> bool {
        let won = self.close_code.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
        // Notify regardless: if another caller won the CAS but its notification
        // was consumed already, a second wakeup is harmless (the session task
        // re-reads the code), whereas a missed wakeup would leak the socket.
        self.notify.notify_one();
        won.is_ok()
    }

    /// Wait until a close is requested, then yield the code to send.
    ///
    /// Cancel-safe: intended to be one arm of the session task's `select!`.
    pub async fn closed(&self) -> u16 {
        loop {
            let code = self.close_code.load(Ordering::SeqCst);
            if code != 0 {
                return code;
            }
            self.notify.notified().await;
        }
    }
}

/// One registered connection.
struct ConnEntry {
    /// The site scope this connection belongs to. Set once at registration
    /// from the upgrade request's canonical site key and never mutated — this
    /// is the value every scoped operation compares against.
    site: Box<str>,
    /// Bounded outbound queue. Producers `try_send` only.
    tx: mpsc::Sender<OutFrame>,
    control: Arc<Control>,
    /// Channels this connection is subscribed to, so `unregister` can clean up
    /// the reverse index without scanning every channel.
    channels: Mutex<HashSet<Box<str>>>,
}

/// What [`Registry::register`] hands back to the session task that will own the
/// socket.
#[derive(Debug)]
pub struct Registered {
    /// The minted, unguessable connection ID. This is the value PHP sees as
    /// `$_SERVER['WS_CONNECTION_ID']`.
    pub id: String,
    /// Receiving end of this connection's bounded outbound queue.
    pub rx: mpsc::Receiver<OutFrame>,
    /// Out-of-band close signal.
    pub control: Arc<Control>,
}

/// A point-in-time snapshot of registry occupancy. Used by tests and by the
/// server's startup/shutdown logging; the live numbers are exported as metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryStats {
    /// Live connections across all sites.
    pub connections: usize,
    /// Distinct sites with at least one live connection.
    pub sites: usize,
    /// Distinct `(site, channel)` pairs with at least one member.
    pub channels: usize,
}

/// The site-scoped WebSocket connection registry.
///
/// Cheap to share: wrap in an `Arc` once at startup and clone the handle.
pub struct Registry {
    conns: DashMap<Box<str>, Arc<ConnEntry>>,
    channels: DashMap<ChannelKey, HashSet<Box<str>>>,
    site_counts: DashMap<Box<str>, usize>,
    total: AtomicUsize,
    limits: Limits,
}

impl Registry {
    /// Build a registry with the given bounds.
    ///
    /// [`Limits::send_queue`] is normalized to at least 1: a zero-capacity
    /// `mpsc` channel would make every `try_send` fail, silently turning the
    /// feature off.
    #[must_use]
    pub fn new(mut limits: Limits) -> Self {
        if limits.send_queue == 0 {
            tracing::warn!("[server.websocket] send_queue = 0 is not a valid queue depth; using 1");
            limits.send_queue = 1;
        }
        Self {
            conns: DashMap::new(),
            channels: DashMap::new(),
            site_counts: DashMap::new(),
            total: AtomicUsize::new(0),
            limits,
        }
    }

    /// The bounds this registry was built with.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Admit a new connection for `site` and mint its ID.
    ///
    /// The caller owns the returned [`Registered`] and **must** call
    /// [`Registry::unregister`] with the same ID when the socket ends, however
    /// it ends.
    ///
    /// The ID is minted **here**, from OS entropy, rather than accepted from
    /// the caller: that makes "every live connection has an unguessable,
    /// unique ID" a property of this function instead of a convention every
    /// call site has to honour.
    ///
    /// # Errors
    ///
    /// [`RegisterError::ServerFull`] or [`RegisterError::SiteFull`] when a
    /// connection cap is reached, [`RegisterError::Entropy`] when no ID could
    /// be drawn. The caller should refuse the upgrade rather than complete a
    /// handshake onto a socket nothing will service.
    pub fn register(&self, site: &str) -> Result<Registered, RegisterError> {
        let id = crate::new_connection_id().map_err(|e| {
            tracing::error!(%e, "refusing a websocket upgrade: OS entropy unavailable");
            RegisterError::Entropy
        })?;

        // Reserve against the global cap first, so a per-site rejection cannot
        // leak a global slot.
        self.reserve_global()?;
        if let Err(e) = self.reserve_site(site) {
            self.total.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }

        let (tx, rx) = mpsc::channel(self.limits.send_queue);
        let control = Arc::new(Control::new());
        let entry = Arc::new(ConnEntry {
            site: Box::from(site),
            tx,
            control: Arc::clone(&control),
            channels: Mutex::new(HashSet::new()),
        });
        self.conns.insert(Box::from(id.as_str()), entry);

        counter!("ephpm_ws_connections_total").increment(1);
        self.publish_active();
        Ok(Registered { id, rx, control })
    }

    /// Take a slot against the global cap.
    fn reserve_global(&self) -> Result<(), RegisterError> {
        if self.limits.max_connections == 0 {
            self.total.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        // CAS loop rather than fetch_add-then-check: an over-limit fetch_add
        // would be briefly visible to a concurrent registration and could admit
        // past the cap under a burst.
        let mut current = self.total.load(Ordering::SeqCst);
        loop {
            if current >= self.limits.max_connections {
                counter!("ephpm_ws_connections_rejected_total", "reason" => "server_full")
                    .increment(1);
                return Err(RegisterError::ServerFull);
            }
            match self.total.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    /// Take a slot against this site's cap.
    fn reserve_site(&self, site: &str) -> Result<(), RegisterError> {
        // `entry` holds the shard write lock for the whole check-and-increment,
        // so two concurrent upgrades for one site cannot both see `n - 1`.
        let mut slot = self.site_counts.entry(Box::from(site)).or_insert(0);
        if self.limits.max_connections_per_site != 0
            && *slot >= self.limits.max_connections_per_site
        {
            counter!("ephpm_ws_connections_rejected_total", "reason" => "site_full").increment(1);
            return Err(RegisterError::SiteFull);
        }
        *slot += 1;
        Ok(())
    }

    /// Remove a connection and every channel membership it held.
    ///
    /// Idempotent — a double unregister (session task teardown racing an
    /// explicit close) is a no-op.
    pub fn unregister(&self, id: &str) {
        let Some((_, entry)) = self.conns.remove(id) else {
            return;
        };

        let subscribed: Vec<Box<str>> = {
            let mut held = entry.channels.lock();
            held.drain().collect()
        };
        for channel in subscribed {
            let key = ChannelKey { site: entry.site.clone(), channel };
            self.remove_member(&key, id);
        }

        self.site_counts.remove_if_mut(entry.site.as_ref(), |_, count| {
            *count = count.saturating_sub(1);
            *count == 0
        });
        self.total.fetch_sub(1, Ordering::SeqCst);
        self.publish_active();
    }

    /// Drop `id` from a channel, removing the channel entirely once empty so an
    /// application that churns channel names cannot grow the map forever.
    fn remove_member(&self, key: &ChannelKey, id: &str) {
        self.channels.remove_if_mut(key, |_, members| {
            members.remove(id);
            members.is_empty()
        });
    }

    /// Look up a connection **within a site scope**.
    ///
    /// Returns `None` both when the ID is unknown and when it belongs to a
    /// different site: the caller cannot distinguish the two, so this is not an
    /// existence oracle for other tenants' connection IDs.
    fn scoped(&self, site: &str, id: &str) -> Option<Arc<ConnEntry>> {
        let entry = self.conns.get(id)?;
        if entry.site.as_ref() != site {
            // Worth a counter: in a correct deployment this is always zero, so
            // any nonzero value is either a bug or an attempt.
            counter!("ephpm_ws_cross_site_denied_total").increment(1);
            tracing::warn!(
                caller_site = site,
                owner_site = %entry.site,
                "rejected cross-site websocket operation"
            );
            return None;
        }
        Some(Arc::clone(entry.value()))
    }

    /// Queue one frame to one connection.
    ///
    /// Returns `false` when the connection is unknown to this site, when its
    /// session task has already gone, or when its outbound queue is full — the
    /// last case also schedules the connection for a
    /// [`CLOSE_QUEUE_OVERFLOW`] close.
    ///
    /// Never blocks. This runs on a PHP execution thread; parking it on a
    /// slow client's socket would hold an execution-pool slot hostage.
    pub fn send(&self, site: &str, id: &str, frame: OutFrame) -> bool {
        if !self.within_size_limit(frame.len()) {
            return false;
        }
        let Some(entry) = self.scoped(site, id) else {
            return false;
        };
        self.enqueue(&entry, frame)
    }

    /// Reject an oversized payload before it is queued anywhere.
    fn within_size_limit(&self, len: usize) -> bool {
        if self.limits.max_message_size == 0 || len <= self.limits.max_message_size {
            return true;
        }
        counter!("ephpm_ws_frames_rejected_total", "reason" => "too_large").increment(1);
        tracing::warn!(
            len,
            max = self.limits.max_message_size,
            "refused an outbound websocket frame over [server.websocket] max_message_size"
        );
        false
    }

    /// The shared tail of [`Registry::send`] and [`Registry::broadcast`].
    fn enqueue(&self, entry: &ConnEntry, frame: OutFrame) -> bool {
        match entry.tx.try_send(frame) {
            Ok(()) => {
                counter!("ephpm_ws_frames_queued_total").increment(1);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                counter!("ephpm_ws_send_queue_overflow_total").increment(1);
                if entry.control.request_close(CLOSE_QUEUE_OVERFLOW) {
                    tracing::warn!(
                        site = %entry.site,
                        depth = self.limits.send_queue,
                        "websocket outbound queue full — shedding connection with 1013"
                    );
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Subscribe a connection to a channel within its own site.
    ///
    /// Returns `false` for an unknown/foreign connection, an empty or
    /// over-long channel name, or a connection already at
    /// [`MAX_CHANNELS_PER_CONNECTION`].
    pub fn subscribe(&self, site: &str, id: &str, channel: &str) -> bool {
        if !valid_channel(channel) {
            return false;
        }
        let Some(entry) = self.scoped(site, id) else {
            return false;
        };

        {
            let mut held = entry.channels.lock();
            if !held.contains(channel) && held.len() >= MAX_CHANNELS_PER_CONNECTION {
                tracing::warn!(
                    site,
                    max = MAX_CHANNELS_PER_CONNECTION,
                    "websocket connection refused a subscription at the per-connection cap"
                );
                return false;
            }
            held.insert(Box::from(channel));
        }

        self.channels.entry(ChannelKey::new(site, channel)).or_default().insert(Box::from(id));
        true
    }

    /// Remove a connection from a channel within its own site.
    ///
    /// Returns `false` for an unknown/foreign connection or a name that was
    /// never subscribed.
    pub fn unsubscribe(&self, site: &str, id: &str, channel: &str) -> bool {
        let Some(entry) = self.scoped(site, id) else {
            return false;
        };
        let was_member = entry.channels.lock().remove(channel);
        if was_member {
            self.remove_member(&ChannelKey::new(site, channel), id);
        }
        was_member
    }

    /// Queue one frame to every member of `channel` within `site`.
    ///
    /// Returns the number of connections the frame was **queued** to. A
    /// subscriber whose queue was full is not counted (and is shed).
    pub fn broadcast(&self, site: &str, channel: &str, payload: Bytes, binary: bool) -> usize {
        if !valid_channel(channel) || !self.within_size_limit(payload.len()) {
            return 0;
        }
        // Clone the membership list out and release the shard before sending:
        // the sends below touch `conns`, and a fan-out to thousands of members
        // must not pin this channel's shard for the whole walk.
        let members: Vec<Box<str>> = {
            let Some(set) = self.channels.get(&ChannelKey::new(site, channel)) else {
                return 0;
            };
            set.iter().cloned().collect()
        };

        let frame = OutFrame::new(payload, binary);
        let mut delivered = 0;
        for id in members {
            // Re-scope every member: membership is the reverse index, `conns`
            // is the authority. A connection that vanished mid-broadcast, or
            // (impossibly, but checked anyway) one whose site no longer
            // matches, is skipped rather than trusted.
            if let Some(entry) = self.scoped(site, &id)
                && self.enqueue(&entry, frame.clone())
            {
                delivered += 1;
            }
        }
        delivered
    }

    /// Ask a connection's session task to close with `code`.
    ///
    /// Returns `false` for an unknown/foreign connection. The socket is not
    /// closed synchronously: the session task drains what it can and then
    /// sends the close frame.
    pub fn close(&self, site: &str, id: &str, code: u16) -> bool {
        let Some(entry) = self.scoped(site, id) else {
            return false;
        };
        entry.control.request_close(code);
        true
    }

    /// Live connections in one site.
    #[must_use]
    pub fn site_connections(&self, site: &str) -> usize {
        self.site_counts.get(site).map_or(0, |n| *n)
    }

    /// Members of one channel within a site.
    #[must_use]
    pub fn channel_members(&self, site: &str, channel: &str) -> usize {
        self.channels.get(&ChannelKey::new(site, channel)).map_or(0, |m| m.len())
    }

    /// Occupancy snapshot.
    #[must_use]
    pub fn stats(&self) -> RegistryStats {
        RegistryStats {
            connections: self.total.load(Ordering::SeqCst),
            sites: self.site_counts.len(),
            channels: self.channels.len(),
        }
    }

    /// Publish the active-connection gauge after a membership change.
    fn publish_active(&self) {
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_ws_connections_active").set(self.total.load(Ordering::SeqCst) as f64);
    }
}

/// Channel names are userland strings that get stored in two maps, so they are
/// length-capped and may not be empty.
fn valid_channel(channel: &str) -> bool {
    !channel.is_empty() && channel.len() <= MAX_CHANNEL_NAME_LEN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new(Limits {
            max_connections: 0,
            max_connections_per_site: 0,
            send_queue: 4,
            max_message_size: 0,
        })
    }

    fn admit(reg: &Registry, site: &str) -> Registered {
        reg.register(site).expect("register")
    }

    fn text(s: &str) -> OutFrame {
        OutFrame::Text(Bytes::from(s.to_owned()))
    }

    #[test]
    fn send_reaches_a_connection_in_the_same_site() {
        let reg = registry();
        let mut c = admit(&reg, "alpha");
        assert!(reg.send("alpha", &c.id, text("hi")));
        match c.rx.try_recv().expect("frame queued") {
            OutFrame::Text(b) => assert_eq!(&b[..], b"hi"),
            OutFrame::Binary(_) => panic!("expected a text frame"),
        }
    }

    #[test]
    fn a_site_cannot_send_to_another_sites_connection_even_with_a_valid_id() {
        // The whole point of the crate: site B holds A's exact connection ID
        // (leaked, logged, guessed — it does not matter) and still cannot
        // reach it.
        let reg = registry();
        let mut victim = admit(&reg, "alpha");
        let leaked_id = victim.id.clone();

        assert!(!reg.send("beta", &leaked_id, text("pwned")), "cross-site send must fail");
        assert!(
            victim.rx.try_recv().is_err(),
            "no frame may reach a connection from outside its site"
        );
    }

    #[test]
    fn a_site_cannot_close_or_subscribe_another_sites_connection() {
        let reg = registry();
        let victim = admit(&reg, "alpha");

        assert!(!reg.close("beta", &victim.id, 1000), "cross-site close must fail");
        assert!(!reg.subscribe("beta", &victim.id, "room"), "cross-site subscribe must fail");
        assert!(!reg.unsubscribe("beta", &victim.id, "room"), "cross-site unsubscribe must fail");

        // And the victim was not scheduled for closure by the attempt.
        assert_eq!(victim.control.close_code.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn identically_named_channels_in_two_sites_are_separate() {
        let reg = registry();
        let mut a = admit(&reg, "alpha");
        let mut b = admit(&reg, "beta");
        assert!(reg.subscribe("alpha", &a.id, "chat"));
        assert!(reg.subscribe("beta", &b.id, "chat"));

        assert_eq!(reg.broadcast("alpha", "chat", Bytes::from_static(b"a-only"), false), 1);
        assert!(a.rx.try_recv().is_ok());
        assert!(b.rx.try_recv().is_err(), "site beta must not see site alpha's channel traffic");
    }

    #[test]
    fn broadcast_counts_only_connections_it_queued_to() {
        let reg = registry();
        let mut one = admit(&reg, "alpha");
        let mut two = admit(&reg, "alpha");
        let _three = admit(&reg, "alpha"); // subscribed to nothing
        assert!(reg.subscribe("alpha", &one.id, "room"));
        assert!(reg.subscribe("alpha", &two.id, "room"));

        assert_eq!(reg.broadcast("alpha", "room", Bytes::from_static(b"hello"), false), 2);
        assert!(one.rx.try_recv().is_ok());
        assert!(two.rx.try_recv().is_ok());
        assert_eq!(reg.channel_members("alpha", "room"), 2);
        assert_eq!(reg.broadcast("alpha", "no-such-channel", Bytes::new(), false), 0);
    }

    #[test]
    fn unsubscribe_removes_the_member_and_drops_an_empty_channel() {
        let reg = registry();
        let c = admit(&reg, "alpha");
        assert!(reg.subscribe("alpha", &c.id, "room"));
        assert_eq!(reg.stats().channels, 1);

        assert!(reg.unsubscribe("alpha", &c.id, "room"));
        assert_eq!(reg.channel_members("alpha", "room"), 0);
        assert_eq!(reg.stats().channels, 0, "an empty channel must not linger");
        assert!(!reg.unsubscribe("alpha", &c.id, "room"), "second unsubscribe is a no-op");
    }

    #[test]
    fn a_full_queue_sheds_the_connection_with_1013() {
        // send_queue = 4, and nothing is draining.
        let reg = registry();
        let c = admit(&reg, "alpha");
        for i in 0..4 {
            assert!(reg.send("alpha", &c.id, text("x")), "frame {i} should fit");
        }
        assert!(!reg.send("alpha", &c.id, text("overflow")), "the 5th frame must be refused");
        assert_eq!(
            c.control.close_code.load(Ordering::SeqCst),
            CLOSE_QUEUE_OVERFLOW,
            "overflow must schedule a 1013 close, not buffer"
        );
    }

    #[test]
    fn the_first_close_code_wins() {
        let reg = registry();
        let c = admit(&reg, "alpha");
        assert!(reg.close("alpha", &c.id, 4001));
        assert!(reg.close("alpha", &c.id, 1000));
        assert_eq!(c.control.close_code.load(Ordering::SeqCst), 4001);
    }

    #[test]
    fn unregister_clears_channels_and_counts() {
        let reg = registry();
        let c = admit(&reg, "alpha");
        assert!(reg.subscribe("alpha", &c.id, "room"));
        assert_eq!(reg.stats(), RegistryStats { connections: 1, sites: 1, channels: 1 });

        reg.unregister(&c.id);
        assert_eq!(reg.stats(), RegistryStats { connections: 0, sites: 0, channels: 0 });
        assert!(!reg.send("alpha", &c.id, text("gone")));
        reg.unregister(&c.id); // idempotent
        assert_eq!(reg.stats().connections, 0);
    }

    #[test]
    fn caps_are_enforced_globally_and_per_site() {
        let reg = Registry::new(Limits {
            max_connections: 3,
            max_connections_per_site: 2,
            send_queue: 4,
            max_message_size: 0,
        });
        let a1 = admit(&reg, "alpha");
        let _a2 = admit(&reg, "alpha");
        assert_eq!(reg.register("alpha").unwrap_err(), RegisterError::SiteFull);
        // A per-site rejection must not have consumed a global slot.
        let _b1 = admit(&reg, "beta");
        assert_eq!(reg.stats().connections, 3);
        assert_eq!(reg.register("beta").unwrap_err(), RegisterError::ServerFull);

        // Freeing a slot lets the next one in.
        reg.unregister(&a1.id);
        let _b2 = admit(&reg, "beta");
        assert_eq!(reg.site_connections("alpha"), 1);
        assert_eq!(reg.site_connections("beta"), 2);
    }

    #[test]
    fn channel_names_are_validated_and_capped_per_connection() {
        let reg = registry();
        let c = admit(&reg, "alpha");
        assert!(!reg.subscribe("alpha", &c.id, ""), "empty channel name");
        assert!(
            !reg.subscribe("alpha", &c.id, &"x".repeat(MAX_CHANNEL_NAME_LEN + 1)),
            "over-long channel name"
        );

        for i in 0..MAX_CHANNELS_PER_CONNECTION {
            assert!(reg.subscribe("alpha", &c.id, &format!("c{i}")), "channel {i}");
        }
        assert!(!reg.subscribe("alpha", &c.id, "one-too-many"));
        // Re-subscribing to an existing channel is still allowed at the cap.
        assert!(reg.subscribe("alpha", &c.id, "c0"));
    }

    #[test]
    fn zero_send_queue_is_normalized_rather_than_disabling_sends() {
        let reg = Registry::new(Limits {
            max_connections: 0,
            max_connections_per_site: 0,
            send_queue: 0,
            max_message_size: 0,
        });
        assert_eq!(reg.limits().send_queue, 1);
        let mut c = admit(&reg, "alpha");
        assert!(reg.send("alpha", &c.id, text("fits")));
        assert!(c.rx.try_recv().is_ok());
    }

    #[test]
    fn an_oversized_payload_is_refused_before_it_is_queued() {
        let reg = Registry::new(Limits {
            max_connections: 0,
            max_connections_per_site: 0,
            send_queue: 4,
            max_message_size: 8,
        });
        let mut c = admit(&reg, "alpha");
        assert!(reg.subscribe("alpha", &c.id, "room"));

        assert!(reg.send("alpha", &c.id, text("01234567")), "8 bytes is exactly the cap");
        assert!(c.rx.try_recv().is_ok());
        assert!(!reg.send("alpha", &c.id, text("012345678")), "9 bytes is over the cap");
        assert_eq!(reg.broadcast("alpha", "room", Bytes::from_static(b"0123456789"), false), 0);
        assert!(c.rx.try_recv().is_err(), "nothing oversized may reach the queue");

        // And the connection is NOT shed for it — an oversized payload is the
        // caller's bug, not back-pressure.
        assert_eq!(c.control.close_code.load(Ordering::SeqCst), 0);
        assert!(reg.send("alpha", &c.id, text("ok")));
        assert!(c.rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn closed_resolves_for_a_close_requested_before_it_is_awaited() {
        // The session task may be mid-PHP-dispatch when a close is scheduled;
        // the wakeup must not be lost.
        let reg = registry();
        let c = admit(&reg, "alpha");
        assert!(reg.close("alpha", &c.id, 4002));
        assert_eq!(c.control.closed().await, 4002);
    }
}
