# Cluster Channel — the shared cluster data plane

> **Status: EXPERIMENTAL-adjacent, v1 shipped alongside Turso CDC.**
> The transport is implemented and used by `cdc_experimental`; a
> config without any opt-in channel feature is byte-identical to a
> config from before — no socket, no task, no log noise.

## The rule ePHPm's cluster stack follows

ePHPm's cluster stack splits cleanly along a **state vs log** line, and
the design encodes that split as two separate protocols:

- **Gossip (chitchat) = control plane ONLY.** Membership, phi-accrual
  failure detection, primary election, ACME leader lock, opcache
  version broadcasts, small session state (KV values under
  `[cluster.kv] small_key_threshold`). Gossip is a UDP-native chatter
  protocol; it must stay small and bounded regardless of write volume.
- **Cluster channel = data plane for LOGS.** CDC transaction batches
  today, snapshot bootstrap and watermark sync in future phases, any
  future bulk stream feature after that. The channel is a
  yamux-multiplexed TCP protocol; it never carries elections,
  membership, or KV state.

Concretely: gossip announces *"node A is primary for sqlite:default"*;
the channel is what ships the actual transaction batches from A to its
replicas. Gossip carries names; the channel carries payloads.

Every existing feature already implicitly obeys this — the sqld
integration ships WAL frames over gRPC (not gossip), the KV data plane
uses a dedicated TCP protocol (not gossip) for large values. The
cluster channel v1 makes the rule explicit and gives future features a
single shared transport to reuse instead of each inventing one.

## Lazy-bind: "if nothing uses it, don't turn it on"

The channel listener is **only bound when at least one feature asks
for it**. A v0.5.0 config that opts in to no channel feature ships
the same startup as v0.4.x: no new socket, no background task, no log
line above `debug!`. Adding `[cluster.channel]` to a config is not
itself an opt-in — a feature elsewhere (today just
`[db.sqlite.replication] cdc_experimental = true`) has to ask.

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

Version 1 handshake — before any yamux frame flows:

```text
initiator → responder:
  [version: u8 = 0x01]
  [sealed_len: u16 BE]
  [sealed_challenge]         # seal(random 32-byte nonce)

responder → initiator:
  [version: u8 = 0x01]
  [sealed_len: u16 BE]
  [sealed_reply]             # seal(SAME challenge nonce)
```

Both sides derive `ClusterCipher::for_cluster_channel(secret)` — a
distinct HKDF-SHA256 domain (`ephpm-cluster-channel-v1`) from
`for_gossip` and `for_kv_data_plane`, so a stray gossip datagram or KV
data-plane frame can never authenticate here. The responder opens the
challenge (proves it holds the secret), seals the *same* nonce back
with a fresh AEAD nonce, and the initiator verifies the round trip.
Either side dropping on any failure is the "wrong secret" signal —
deliberately no typed error reply, so a wrong-secret peer is
indistinguishable from a stray TCP port scan.

**Fail-closed:** the channel refuses to bind when a channel feature is
enabled but no secret is configured (neither `[cluster.channel] secret`
nor `[cluster] secret`). Authentication is not optional; a channel
feature is authenticated or absent.

**TLS is Phase 2.1** — the channel today is authenticated with
ChaCha20-Poly1305 (same primitive as gossip / KV data plane) but not
TLS-wrapped. Eavesdroppers see ciphertext, but there is no PKI-based
peer identity beyond "holds the shared cluster secret". TLS wrapping
adds no additional security on a trusted network segment but is needed
for the mixed-trust operator posture; see the deferred items below.

## Multiplexing

After the handshake, both sides speak **yamux 0.14** over the raw TCP
stream. Each yamux stream is opened by the initiator and begins with a
length-prefixed UTF-8 stream-type string:

```text
[stream_type_len: u16 BE][stream_type: utf-8 bytes]
```

### Stream registry

| Prefix | Status | Purpose |
|---|---|---|
| `cdc/<vhost>` | Implemented | CDC replication (Turso engine) — see [`turso-engine`](/roadmap/turso-engine/#phase-2--cdc-native-replication-experimental-implementation-available-gated-on-ga-for-default) |
| `snapshot/<vhost>` | **RESERVED** — refused with a logged warning today | Fresh-replica base snapshot before CDC catch-up (Phase 2.1) |

The stream-type string is what drives dispatch on the accepting side.
Unknown types are logged (WARN) and the stream is closed; the yamux
connection stays alive so other streams keep flowing. This lets a
future feature ship without a version bump — old nodes just refuse the
new stream type until they upgrade.

### Backpressure

Yamux gives per-stream flow control (256 KiB window by default). A
stalled reader on one stream pauses writes to that stream without
blocking other streams on the same connection. In CDC terms: a slow
replica pauses the primary's tail broadcast on **that** subscriber's
stream only. The `broadcast::Sender` upstream of the write loop
absorbs transient stall via its 1024-entry queue; sustained stall
produces a `RecvError::Lagged` on the receiver side, which the
subscriber loop treats as "close the stream" — the replica's
reconnect loop opens a fresh stream and litewire's
`__litewire_cdc_watermark` guarantees idempotent replay.

## What rides the channel today

Just CDC replication (`cdc/default`). Opt in with:

```toml
[cluster]
enabled = true
secret = "..."                # required — channel is fail-closed
bind = "0.0.0.0:7946"

[db.sqlite]
engine = "turso"

[db.sqlite.replication]
cdc_experimental = true       # this is what turns the channel on
```

That's the complete opt-in. `[cluster.channel]` needs no entries at
all in the common case — the channel listens on `bind_port + 1` by
default and reuses `[cluster] secret`. Explicit `listen` /
`secret` overrides are available if you need them.

## What's reserved for the channel next

Roughly in priority order:

1. **Snapshot bootstrap (`snapshot/<vhost>`).** A joining replica
   opens a snapshot stream, receives the primary's current DB image,
   then subscribes to `cdc/<vhost>` from the corresponding watermark.
   Removes the "operator seeds the DB file" step from the Turso CDC
   deployment story.
2. **Watermark sync stream.** Persist subscriber watermarks
   cluster-wide so a subscriber that reconnects to a *different*
   primary (post-failover) resumes from its last-applied cursor
   instead of restarting from zero.
3. **TLS wrap (Phase 2.1).** Optional TLS layer between the TCP
   handshake and yamux, using ACME-issued certs from the existing
   `rustls-acme` integration. Reuses the per-cluster secret as the
   handshake fallback so mixed-version rollouts don't need coordinated
   flips.
4. **Bulk log stream (unfixed schema).** A generic
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
