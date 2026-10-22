+++
title = "KV Snapshots"
weight = 7
+++

A restarted ePHPm used to start with an empty KV store: every session, every
cached query, every rate-limit window gone until the application rebuilt
them. `[kv.snapshot]` writes an encrypted copy of the store at graceful
shutdown and loads it at the next start, so a restart serves warm. This page
is the design: what a snapshot is and is not, the file format, how it is
written and loaded, and the decisions behind each choice. Setup is in the
[warm-restart guide](/guides/kv-warm-restart/); knobs in the
[config reference](/reference/config/#kvsnapshot).

## What it is, and is not

A snapshot is a **cache warm-up, not durability.** Stated exactly:

- Nothing is written between snapshots. A key set after the last snapshot is
  not in it. With `interval` unset, a crash or `SIGKILL` loses everything
  since the previous graceful stop (or manual save).
- A snapshot is not atomic across keys. The writer copies the store one shard
  at a time, so two keys written "together" can straddle the copy.
- A node that loads a snapshot may briefly serve a value a peer overwrote or
  deleted while it was down. In a cluster, the next replicated write to that
  key corrects it; a key deleted meanwhile lingers on that node until its TTL.
- Entries past their expiry are never restored; a surviving TTL resumes from
  the wall clock, not from the restart.

What *is* guaranteed: a file that loads is byte-for-byte what this node's
stores held when it was written; every entry lands in the store of the site
it came from and no other; and a file that was truncated, altered, written by
a newer format, or sealed under a different secret loads **nothing** rather
than something wrong.

## Format (version 1)

```
┌───────────────────────────────────────────────────────────┐
│ HEADER   plaintext, CRC-32, 85–340 bytes                   │
│          magic · version · flags · created-at · node id    │
│          cipher/KDF/compression ids · 32-byte salt         │
│          key check (AEAD tag of "" under the file key)     │
├───────────────────────────────────────────────────────────┤
│ BLOCK 0  kind=DATA  len  ciphertext  ← one site, ~1 MiB   │
│ BLOCK 1  …                                                 │
│ BLOCK n-1                                                  │
├───────────────────────────────────────────────────────────┤
│ BLOCK    kind=INDEX  len  ciphertext  ← offset/len/site/   │
│                                          entries per block │
├───────────────────────────────────────────────────────────┤
│ TRAILER  plaintext, CRC-32, 24 bytes: index offset + len   │
└───────────────────────────────────────────────────────────┘
```

Each data block opens to `compression · raw length · payload`, and the
payload is `site · value-compression id · count · entry…`. An entry is
`type · flags · ext-len · ext · expires-at-ms · key · payload-len · payload`.
Type 1 is a string (payload = the bytes **as stored**; a flag says they are
compressed with the store's own algorithm, so writing a snapshot never
decodes a value and loading one into a store with the same `[kv]
compression` never re-encodes it). Type 2 is a hash (`count` fields, each
`[expires-at-ms] · name · value`; a flag says fields carry their own
expiry, so a per-field TTL survives a restart). `ext` is reserved for
per-entry metadata a later version may add (an LRU hint, a cluster stamp),
and `payload-len` lets a reader skip an entry type it does not know — both
so version 1 readers can tolerate version 1.x writers, while a **different
major version is refused** outright (`cold`, reason `version`).

Why this shape:

- **Streaming, bounded memory.** The writer walks the `DashMap` shard by
  shard, copying each shard's entries (an `Arc` clone of the value bytes, no
  memcpy) under its read lock and releasing the lock before serialising. A
  three-stage pipeline — producer → *N* sealers (LZ4 + AEAD, CPU-bound) →
  one in-order writer — is joined by bounded channels, so memory in flight is
  a handful of blocks whatever the store's size.
- **Per-site blocks.** A block holds one site's entries. The site key is
  inside the sealed payload and repeated in the sealed index; the loader
  resolves it to that site's store and refuses a block whose two copies
  disagree. A tenant's keys cannot be made to load into another tenant's
  store without breaking the AEAD.
- **Index + trailer.** The trailer is the last thing written; a file without
  a valid one was truncated and never loads. The index lets the loader
  verify and apply blocks on every core (8 workers on the benchmark box) and
  gives `inspect` per-site totals without touching a data block.
- **Pure-Rust block compression (LZ4).** The binary links the PHP SDK's own
  zlib and zstd; a C compressor exporting the same symbols would shadow them
  (the reason `deny.toml` bans `libz-sys`). `lz4_flex` with its safe
  encoder/decoder has no unsafe code and no dependencies, and at ~500 MiB/s
  it is never the bottleneck. A block is compressed only when that makes it
  smaller; values already compressed by the store simply stay as they are.
- **CRC-32 on the plaintext parts only.** The sealed blocks are
  integrity-checked by their Poly1305 tag; a second checksum there would be
  redundant. `crc32fast` is already in the dependency graph via `flate2`.

## Encryption

Every snapshot is sealed. There is no plaintext mode: a snapshot holds every
tenant's keys and sessions, and `mode != "off"` without `[kv] secret` is a
startup error, not a warning.

- **Key.** `file_key = HKDF-SHA256(salt = the header's 32 random bytes, ikm
  = [kv] secret, info = "ephpm-kv-snapshot/v1/file-key")`. A fresh salt per
  file means every snapshot has its own key even though the secret never
  changes; the `info` label is distinct from the cluster transport's, so a
  snapshot key is never a gossip or data-plane key. The secret itself is
  never written, never logged, and never sent to PHP.
- **Cipher.** ChaCha20-Poly1305 per block, nonce = the block's sequence
  number (unique under the per-file key), associated data = the complete
  header bytes plus the sequence number. A block cannot be moved, reordered,
  truncated, or paired with an altered header. The index block is sealed
  under a sequence number derived from its own file offset, which binds it
  to the position the trailer names.
- **Key check.** The header carries the Poly1305 tag of an empty message
  under the file key. A reader therefore tells "wrong secret" (`cold`,
  reason `wrong_key`) from "corrupt file" before touching any data, and
  rotating `[kv] secret` produces one clear WARN and a cold start rather
  than a pile of block failures.
- **Why ChaCha20-Poly1305 + HKDF, not AES-GCM via aws-lc-rs.** They are the
  pair `ephpm-cluster` already uses to seal gossip and data-plane frames
  from the same secret — no new primitive, no new crate, pure Rust in the
  KV crate. AES-256-GCM through aws-lc-rs would be faster per byte, but a
  snapshot is bounded by disk and (for object storage) network, not by the
  cipher; the benchmark below is already past 500 MiB/s of plaintext.

## Writing

`write_snapshot` streams into `.<identity>.kvsnap.tmp-<pid>-<n>` beside the
destination, `fsync`s it, renames it over `<identity>.kvsnap`, and `fsync`s
the directory (Unix). A reader therefore sees either the previous complete
snapshot or the new complete one, never a partial file; a crash mid-write
leaves a temporary file the next start removes. A write that would exceed
`max_size` is abandoned with the previous snapshot intact.

The server writes at graceful shutdown (after the HTTP drain, so the file
reflects the last request served), every `interval` if set, and on `POST
/_ephpm/kv/snapshot`. Saves are serialised; the shutdown save is not bounded
by `[server.timeouts] shutdown` — that budget is for draining — but by its
own last-resort five minutes. All of it runs on blocking threads, never on a
tokio worker, for the same reason the engine work in `db_shutdown` does.

## Loading

`load_snapshot` runs before any listener binds, under `load_timeout`:

1. Header: magic, version, CRC. A different major version → `cold`
   (`version`); damage → `cold` (`corrupt`).
2. Key check → `cold` (`wrong_key`).
3. Trailer and index: a missing trailer means a truncated file → `cold`
   (`corrupt`). Nothing has been applied to any store yet.
4. Data blocks, in parallel. Each worker reads, opens, decompresses and
   restores one block at a time into the store its site resolves to. A block
   that fails to authenticate or decode is **skipped and counted**
   (`partial`, reason `corrupt_blocks`) — every block is independently
   sealed, so a bad one cannot affect its neighbours. Past the deadline the
   workers stop; what was restored stays (`partial`, reason `timeout`). A
   store that refuses entries under `noeviction` is `partial`
   (`memory_limit`); a block for a site this node has no store for is
   skipped (`unknown_site`).

"Skip bad blocks" rather than "discard all" is deliberate: a snapshot is a
cache, every restored entry is independently verified, and warmth is the
whole point. The exception is the header/trailer/index: those are what tell
the loader which bytes to trust, so any doubt there means nothing is
restored. The loader never blocks readiness indefinitely and never fails
startup; the outcome is one log line and
`ephpm_kv_snapshot_load_total{outcome,reason}`.

## Numbers

`cargo test -p ephpm-kv --release --test snapshot_bench -- --ignored
--nocapture`, 1,000,000 keys × 200-byte JSON-like values, a quarter with
TTLs, AMD Ryzen 9 5950X, NVMe:

| | Time | Rate |
|---|---|---|
| Save (8 sealer threads) | 0.40 s | 2.5 M keys/s, 513 MiB/s plaintext, 47 MiB written (LZ4 4.3×) |
| Load, 8 workers | 0.35 s | 2.8 M keys/s |
| Load, 1 worker | 1.04 s | 0.96 M keys/s, 197 MiB/s |

The `30s` default `load_timeout` is therefore generous for stores of tens
of millions of keys on similar hardware; the knob exists for slow disks and
cgroup-throttled pods.

## Object storage

A pod whose disk does not survive it (a Deployment, a spot instance) has
nowhere to keep a local file, so `mode = "object"` keeps the snapshot in an
S3-compatible bucket instead: `s3://<bucket>/<prefix>/<identity>.kvsnap`.
The file format is identical — the object *is* the file — so `inspect` and
`verify` accept an `s3://` URL, and nothing about encryption changes: the
bucket holds ciphertext sealed under `[kv] secret`, and the bucket's own
server-side encryption, if any, is a second layer, not the first.

A save writes the file to a staging directory, uploads it as one streamed
`PutObject` (never held in memory; the single-PUT limit is 5 GiB, far above
any `max_size` worth loading at startup) and deletes the staging copy. A
load downloads the object to the staging file, loads it and deletes it; the
download counts against `load_timeout`. The object is the only source — a
failed download is a cold start (reason `io`), never a fallback to a stale
local file, because "warm from last week" is worse than cold.

**Identity in a Deployment.** Pod names are random, so `identity` must be
set. The simple, correct choice is one shared identity per Deployment: every
replica overwrites the same object at shutdown, and a replacement pod loads
whichever replica stopped last. For a cache warm-up that is exactly right —
any recent replica's store is a good approximation of the others'. A
per-replica object (with the pod ordinal, say) only makes sense for a
StatefulSet, where a PVC is the simpler answer anyway.

**The client** ([`crates/ephpm-server/src/s3.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/src/s3.rs))
is ~300 lines over reqwest and the shared SigV4 signer (`sigv4.rs`, also
used by the Route 53 DNS-01 provider): PUT, GET, HEAD, DELETE and
ListObjectsV2. Not the AWS SDK and not `object_store`: both enable
reqwest's `rustls-tls-native-roots`, which links `ring` — a second rustls
crypto provider next to aws-lc-rs, the exact configuration that broke
static TLS in every release up to v0.6.1 (#241). Uploads and downloads are
signed with `UNSIGNED-PAYLOAD` (S3, MinIO, R2 and GCS all accept it), so a
gigabyte is streamed once and hashed never; the snapshot's AEAD verifies
every byte on load. Credentials are the standard `AWS_*` environment
variables only. Instance-profile and web-identity (IRSA) providers are not
implemented; mount a static key from a Secret with a least-privilege policy
(the guide has one). Tested against an in-process mock that re-derives
every request's signature, and against MinIO in the lab.

## Cluster

In a cluster the snapshot covers everything this node held: gossip-tier
small values (which the live cluster re-sends anyway) and the large values
this node owned or replicated. A snapshot-restored entry carries no write
stamp, so any replicated write that arrives for the key — gossip catch-up or
a data-plane push — replaces it.

### Peer handoff (`peer_handoff`, on by default)

Small values (at or below `[cluster.kv] small_key_threshold`) live in the
gossip tier and reach every node on their own; a restarted node gets them
back from chitchat's state sync. Large values do not: they are copied to
`replication_factor` replicas **at write time only**, and nothing
rebalances them afterwards. A restart used to expose two gaps:

1. When node A leaves, every key whose replica set contained A has one copy
   fewer until it is next written (with `replication_factor = 1`, none).
2. When A comes back, it is again in the replica set of ~1/N of the large
   keys — but holds none of them, and the read-through fetch does not fetch
   for a node that is itself a replica. Those keys miss on A until
   rewritten.

**Push on drain** closes (1): after the HTTP drain, A walks its large
values and sends each one to the nodes that form the key's replica set
*without A*, as stamped SETs over the data plane (`OP_SET_V2`, the same
path a replica write takes). **Pull on start** closes (2): as soon as
gossip lists a peer (up to a 10 s join wait), the returning node asks every
alive peer for its share (`OP_SHARE`), **while already serving** — the
pull runs beside the accept loop, not before it. Holding the first request
back for the ~2 s the transfer takes was measured to make things worse:
it doubled the node's downtime per restart, and the gossip tier's own
catch-up window (see below) widened with it to 1.3–1.7× the stale reads of
a plain restart. Serving during the pull is safe because every pulled
entry lands through the stamped-write path: a request racing the pull sees
a miss, never an older value, which is exactly what a node without handoff
serves. The peer walks its own large values and streams back the ones
whose replica set — computed over the peer's alive view **with the
requester forced in** — includes the requester, and only keys the peer
holds as a *current replica* (a leftover copy from an earlier membership
is never forwarded; it could be stale). Forcing the requester in makes the
answer independent of gossip timing. A share streams batches of entries
(~256 KiB per message, one sealed frame each when the cluster secret is
set), fed from a bounded queue by a blocking-thread walk that holds no
shard lock while the socket is slow, so a share of any size is bounded by
a few batches in flight on both ends; the puller asks all peers at once. The
walk itself copies each shard's entries under its read lock and visits them
after, like the snapshot writer. Both directions are bounded by
`load_timeout`; whatever was sent or received before the deadline stays.

**Ordering.** Every handed-off value lands through the receiver's
applied-stamp map, with the stamp the sender recorded for the key when it
still has one (the key was written within the 600 s applied horizon), else
a stamp that **loses to every real write**. So a write that reaches the
receiver before, during or after the handoff — from a client, gossip or a
replica push — is never overwritten by the older handed-off copy; the
handoff only fills slots the receiver has no newer record for. The one way
a handoff can install a stale value: a key the receiver wrote more than the
horizon ago that nobody touched since, whose copy on the sender was older
than the receiver's own — which requires the sender to have missed that
write's replication in the first place. A snapshot-restored entry carries
no stamp (fresh process), so a pulled copy from a peer that stayed online
replaces it — the peer's is the one the cluster kept writing to.

**With peers present, the snapshot is not restored at all.** On a
clustered node with handoff the startup load is deferred until the pull
has run; if the pull found at least one alive peer, nothing is restored
from the file (logged as `restored_small_tier=false reason=peers_present`).
Large values come from the pull — exactly this node's current share,
stamped. Small values arrive through gossip over the following minutes: a
restarted node is a fresh chitchat generation and receives the gossip tier
at roughly one 64 KB datagram per gossip round (1 s) per peer, so catching
up on ~100k small keys takes minutes (pre-existing; in the lab a node
restarted empty climbed from 70 % to 84 % cache hits over three minutes).
Restoring the small tier from the snapshot was measured and rejected: it
made the same node serve 93–95 % hits at once, but every small key that
another node rewrote during the downtime-plus-catch-up window read as the
*pre-restart* value on it until its gossip entry arrived — median 60 s,
p90 230 s after the newer write, 0.6 % of that node's reads under the
lab's 1,500 rewrites/s. A cache that serves a value rewritten elsewhere
while it was down is worse than a miss: sessions, lock tokens
(`SETNX`/`DELIFEQ`) and invalidations can all go wrong on it. So with
peers present the restarted node misses until gossip has caught up, and
is never stale. Restoring large values from the snapshot was rejected for
the same reason in a different shape: the file holds every large value
the node *had*, including keys it is no longer a replica for, which later
writes never reach.

**Alone, the snapshot is restored whole.** When the pull finds no alive
peer within the join wait (10 s) — the first node of a cold cluster, or a
single surviving node — the file is the only copy there is, and the whole
snapshot is loaded (`reason=no_peers`). Two consequences, plainly: a
whole-cluster restart where every node comes up at the same time is
**cold** (each sees the others within a gossip round) — stop all nodes and
start one first if warmth matters; and a value restored by that lone node
lives in its own store only — it is not re-published into gossip, so a
node that joins afterwards does not receive it (it does receive the lone
node's large values, through the pull).

**Follow-ups that would change this.** Bulk gossip-state transfer for a
fresh chitchat generation (so a rejoining node catches up in seconds, not
minutes), and anti-entropy for the large tier (so a copy a non-replica
holds is either refreshed or never served). With both, restoring a
snapshot next to live peers would be safe again.

**What it does not do.** It does not rebalance on every membership change
(only the leaving and the joining node move data), it does not hand off
hashes — it does not need to: since v0.13 every hash field rides the
gossip tier, which every node holds, so a restarted node gets hashes back
from gossip state sync just as it gets small strings back — and a node killed without a
graceful stop pushes nothing — its keys survive only where
`replication_factor` already put them. The pull is a walk of every peer's
large values per joining node: O(total large values) of CPU on each peer,
once per join.

## See also

- [Warm restarts](/guides/kv-warm-restart/) — setup for systemd, Docker and Kubernetes
- [`[kv.snapshot]`](/reference/config/#kvsnapshot) — the knobs
- [`ephpm kv snapshot`](/reference/cli/kv/#snapshot) — inspect, verify, save, load, status
- [KV store](/architecture/kv-store/) — the store being snapshotted
