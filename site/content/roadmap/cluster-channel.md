# Cluster Channel — the shared cluster data plane

> **Status: EXPERIMENTAL-adjacent, v1 shipped alongside Turso CDC.**
> The transport is implemented and used by clustered SQLite (Turso CDC
> replication); a single-node config uses no channel feature and is
> byte-identical to a config from before — no socket, no task, no log
> noise.

## The rule ePHPm's cluster stack follows

ePHPm's cluster stack splits cleanly along a **state vs log** line, and
the design encodes that split as two separate protocols:

- **Gossip (chitchat) = control plane ONLY.** Membership, phi-accrual
  failure detection, primary election, ACME leader lock, opcache
  version broadcasts, small session state (KV values under
  `[cluster.kv] small_key_threshold`). Gossip is a UDP-native chatter
  protocol; it must stay small and bounded regardless of write volume.
- **Cluster channel = data plane for LOGS.** CDC transaction batches
  and snapshot bootstrap today, any
  future bulk stream feature after that. The channel is a
  yamux-multiplexed TCP protocol; it never carries elections,
  membership, or KV state.

Concretely: gossip announces *"node A is primary for sqlite:default"*;
the channel is what ships the actual transaction batches from A to its
replicas. Gossip carries names; the channel carries payloads.

Every existing feature already implicitly obeys this — Turso CDC ships
transaction batches over the cluster channel (not gossip), the KV data
plane uses a dedicated TCP protocol (not gossip) for large values. The
cluster channel v1 makes the rule explicit and gives future features a
single shared transport to reuse instead of each inventing one.

## Lazy-bind: "if nothing uses it, don't turn it on"

The channel listener is **only bound when at least one feature asks
for it**. A v0.5.0 config that opts in to no channel feature ships
the same startup as v0.4.x: no new socket, no background task, no log
line above `debug!`. Adding `[cluster.channel]` to a config is not
itself an opt-in — a feature elsewhere (today just clustered SQLite:
`[db.sqlite]` with clustering enabled, which replicates via Turso CDC)
has to ask.

The single source of truth is `ChannelFeatureFlags` in
`crates/ephpm-cluster/src/cluster_channel.rs`. Adding a new
channel-using feature means adding a field to `ChannelFeatureFlags`
and updating `any_enabled()` — that makes the contract mechanically
enforceable: a feature that forgets to set its flag gets no channel,
not a silently-half-wired one.

The lazy-bind is unit-tested
(`channel_stays_off_when_no_features_enabled`): with `FeatureFlags`
all-off, `maybe_start` returns `Ok(None)` and the derived port stays
free (the test binds it directly to prove the channel didn't).

## Handshake

Version 2 handshake — mutual challenge/response, before any yamux frame
flows:

```text
initiator → responder:
  [version: u8 = 0x02][sealed_len: u16 BE][seal(C_i)]

responder → initiator:
  [version: u8 = 0x02][sealed_len: u16 BE][seal(C_i || C_r)]

initiator → responder:
                      [sealed_len: u16 BE][seal(C_r)]
```

`C_i` and `C_r` are fresh 32-byte `OsRng` challenges chosen
independently by each side. Both derive
`ClusterCipher::for_cluster_channel(secret)` for these three messages —
a distinct HKDF-SHA256 domain (`ephpm-cluster-channel-v1`) from
`for_gossip` and `for_kv_data_plane`, so a stray gossip datagram or KV
data-plane frame can never authenticate here. Message 2 proves the
responder holds the secret; message 3 proves the initiator does. Both
echo comparisons are constant-time.

Version 1 was a single round trip in which the responder echoed back
the challenge it was handed. That was **replayable**: a passive
observer who recorded one legitimate setup could re-send the captured
bytes verbatim and clear the accept path without holding the secret.
The responder's own challenge is what closes that — a replayer receives
a message 2 built around a value it cannot open, so it cannot produce
message 3. v1 peers are rejected; both ends of a channel ship in the
same binary, so there is no mixed-version window inside a release.

**Fail-closed:** the channel refuses to bind when a channel feature is
enabled but no secret is configured (neither `[cluster.channel] secret`
nor `[cluster] secret`). Authentication is not optional; a channel
feature is authenticated or absent.

**Peer admission.** After the handshake — actually, before it — the
accepting side checks the connecting IP against the live gossip member
set. This is coarse: per-host rather than per-process, and it trusts
the TCP source address. It is defence in depth behind the secret, so
that a leaked secret alone does not let an arbitrary host pull a
database snapshot.

**Post-handshake confidentiality.** Both sides derive a
*per-connection* key from the secret salted with the transcript
`C_i || C_r` (HKDF domain `ephpm-cluster-channel-session-v1`) and wrap
the socket in a sealing adapter: every byte yamux writes goes out as a
length-prefixed ChaCha20-Poly1305 frame and every frame is
authenticated on read. Because the key is per-connection, frames cannot
be spliced between connections. A frame that fails authentication
drops the connection.

**TLS is still deferred.** There is no PKI-based peer identity beyond
"holds the shared cluster secret". TLS wrapping adds no additional
security on a trusted network segment but is needed for the mixed-trust
operator posture; see the deferred items below.

## Multiplexing

After the handshake, both sides speak **yamux 0.14** over the sealed
stream described above (yamux never sees the raw socket). Each yamux
stream is opened by the initiator and begins with a length-prefixed
UTF-8 stream-type string:

```text
[stream_type_len: u16 BE][stream_type: utf-8 bytes]
```

### Stream registry

| Prefix | Status | Purpose |
|---|---|---|
| `cdc/<vhost>` | Implemented | CDC replication (Turso engine) — see [`turso-engine`](/roadmap/turso-engine/#phase-2--cdc-native-replication-experimental-implementation-available-gated-on-ga-for-default) |
| `snapshot/<vhost>` | Implemented | Cold-replica base snapshot before CDC catch-up. Served only while the local node is the elected primary. |

The stream-type string is what drives dispatch on the accepting side.
Unknown types are logged (WARN) and the stream is closed; the yamux
connection stays alive so other streams keep flowing. This lets a
future feature ship without a version bump — old nodes just refuse the
new stream type until they upgrade.

### Backpressure

Yamux gives per-stream flow control (256 KiB window by default). A
stalled reader on one stream pauses writes to that stream without
blocking other streams on the same connection. In CDC terms: a slow
replica pauses only its **own** subscriber stream, because each
subscriber owns a private `CdcTailer` rather than sharing a fan-out.
The tailer simply stops advancing while its write is blocked, so a
stalled replica cannot cause the primary to skip past batches it has
not yet received — the failure mode is lag, not loss. (An earlier
design broadcast one shared cursor to every subscriber; lag there meant
`RecvError::Lagged` and permanently dropped batches.)

### Resource bounds

Pre-authentication work is capped: at most 64 concurrent inbound
handshakes (further connections are closed immediately rather than
queued) and a 10-second deadline on each. Post-handshake, yamux is
configured for at most 64 concurrent streams per connection.

## What rides the channel today

CDC replication (`cdc/default`) and its cold-replica snapshot bootstrap
(`snapshot/default`). Turn it on with:

```toml
[cluster]
enabled = true
secret = "..."                # required — channel is fail-closed
bind = "0.0.0.0:7946"

[db.sqlite]
path = "app.db"               # embedded Turso; clustering turns on CDC
```

That's the complete opt-in — enabling `[cluster]` with `[db.sqlite]`
selects Turso CDC replication, which is what binds the channel (there is
no `cdc_experimental` flag anymore). `[cluster.channel]` needs no entries at
all in the common case — the channel listens on `bind_port + 2` by
(skipping gossip + 1 = 7947, the KV data-plane default)
default and reuses `[cluster] secret`. Explicit `listen` /
`secret` overrides are available if you need them.

## What's reserved for the channel next

Roughly in priority order:

1. **TLS wrap.** Optional TLS layer between the TCP
   handshake and yamux, using ACME-issued certs from the existing
   `rustls-acme` integration. Reuses the per-cluster secret as the
   handshake fallback so mixed-version rollouts don't need coordinated
   flips.
2. **Bulk log stream (unfixed schema).** A generic
   `log/<feature>/<vhost>` stream for future features that need
   ordered, backpressured, authenticated cluster-wide log distribution
   without inventing a fresh transport.

## Non-goals

- **Not a message queue.** No persistence, no consumer groups, no
  offsets across restart. Producers are expected to have their own
  source of truth (CDC's tail cursor, snapshot's file bytes) and
  restart cleanly.
- **Not a service mesh.** The channel is peer-to-peer between cluster
  nodes only; PHP application traffic doesn't ride it.
- **Not a control plane.** No membership, no leader election, no
  configuration distribution — those all remain on gossip (chitchat).

## Design docs and code

- Module: `crates/ephpm-cluster/src/cluster_channel.rs`
- Config: `crates/ephpm-config/src/lib.rs` — `ClusterChannelConfig`
- Startup: `crates/ephpm-server/src/lib.rs` —
  `resolve_channel_features` and `maybe_start_cluster_channel` call
- First consumer: `crates/ephpm-server/src/turso_cdc.rs`
