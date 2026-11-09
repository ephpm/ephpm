# Transparent CLI KV wire — `ephpm php --site` reaches the server's KV

> **Status: SHIPPED, EXPERIMENTAL (0.16.0).** Implemented and covered by
> unit, integration, and e2e tests. Single-node is exercised in CI; the
> clustered-replication observation is the documented manual lab step below.
> Issue [#700](https://github.com/ephpm/ephpm/issues/700).

An out-of-band `ephpm php --site <key> --config <path>` — a cron job, a cache
warmer, a nightly rebuild — now reads and writes a **running server's live KV**
through the `ephpm_kv_*` SAPI functions, symmetric to the database CLI wire
([#471](https://github.com/ephpm/ephpm/issues/471)). Its writes replicate across
the cluster through the server's own `Store`, with no new replication code.

## The gap

`ephpm php` runs PHP without the server, so the `ephpm_kv_*` functions were
compiled in but **unbound**: every call hit a "no KV store is attached" warning
and returned its failure value. A cron script that "warmed a cache" or "took a
lock" did nothing and could not tell. The DB wire had already solved the
analogous problem for the database; this closes the same gap for the KV.

## Design: symmetric with the DB wire

**Bar.** The KV wire works **whenever the DB wire works** — a running server
plus `[kv] secret` — *without* additionally requiring the public RESP listener
(`[kv.redis_compat]`, off by default).

**Transport.** When a server runs multi-tenant with `[kv] secret`, it binds a
second, **always-available loopback-only RESP listener** on an ephemeral port
(`ephpm_server::internal_kv`). It reuses the existing RESP command dispatch and
per-site `AUTH` (`ephpm_kv::server::serve_on`) and the *same* `MultiTenantStore`
the router and the public listener use, so a CLI `AUTH <site> <derived>` lands
in the identical `Arc<Store>` a web request reaches for that vhost. Loopback TCP
(not a Unix socket) is used on every platform: all tenants share one process and
one uid, so neither a port nor a socket's permissions separate them — the
per-site HMAC `AUTH` is the real gate, and loopback TCP is the simplest thing
that is correct and cross-platform.

**Discovery.** The port is ephemeral, so the server writes a small runtime file
(`<temp-dir>/ephpm-kv-endpoint-<hash>.json`, `<hash>` from the canonical
`sites_dir`, `0600` on Unix) at startup and removes it on graceful shutdown. The
CLI, which has the same `--config`, derives the same path and **probes the port
before using it**, so a stale file from a crashed server is simply ignored.

**FFI binding.** The CLI installs a *remote* `EphpmKvOps` vtable
(`ephpm_php::kv_bridge::REMOTE_KV_OPS`) through the same `ephpm_set_kv_ops` entry
point the local store uses. Each store-touching op forwards to the endpoint over
a blocking RESP client (`ephpm_php::kv_remote::RemoteKvClient`) — blocking, not
async, because the ops are synchronous FFI callbacks and RESP is one round trip
per call; every connect/read/write is bounded by a timeout so a wedged server
can never hang the CLI thread. The three buffer-reading callbacks
(`get_result`, `list_item`, `site_context`) are **shared** with the local table
unchanged, since they only read the thread-local staging buffers the remote ops
fill exactly as the local ops do.

**No offline mode.** The KV is a running server's in-memory state; there is
nothing to open when no server answers (unlike the DB wire, which can direct-open
a site file). No server reachable ⇒ the functions stay unattached, and
`ephpm_kv_attached()` reports `false`.

**`ephpm_kv_attached(): bool`.** A new SAPI function, true iff a KV ops table is
installed (local in the server, remote in the CLI). Libraries branch on it
instead of `function_exists('ephpm_kv_get')` (always true). The one op without a
RESP equivalent, `ephpm_kv_wait()` (a versioned in-process wait for
request-scoped coordination, not a cron primitive), reports a timeout over the
wire.

## Trust model

Loopback only; per-site `AUTH` is `HMAC-SHA256([kv] secret, site key)` —
identical to the public listener and to the DB wire password. No trust boundary
weaker than what `ephpm exec` and the DB wire already document: **with
`[kv] secret` set, whoever can read the config can derive any tenant's password
and impersonate that tenant.** Without `[kv] secret`, or in single-site mode, the
CLI KV wire is simply unavailable (fail closed).

## Test matrix

| What | Where | Runs in CI |
|------|-------|-----------|
| RESP client: frame encode/decode, AUTH flow, timeout → failure, redial | `ephpm-php` `kv_remote` unit tests | yes (stub) |
| Discovery-file path/round-trip, start gating | `ephpm-server` `internal_kv` unit tests | yes (stub) |
| Strategy picker (validation, secret, endpoint discovery) | `ephpm` `site_kv` unit tests | yes (stub) |
| **Endpoint + client together, redis_compat OFF**, per-site isolation, bad-password rejection, fail-closed | `ephpm` `tests/internal_kv_wire.rs` integration | yes (stub) |
| Headline (CLI write visible to the serving process), isolation, operator-tooling interop | `ephpm-e2e` `tests/cli_kv_wire.rs` | e2e (needs a built binary) |
| `ephpm_kv_attached()` false with no server | `ephpm-e2e` `tests/cli.rs` | e2e (needs a built binary) |

**Clustered replication — manual lab step.** A CLI write lands in the server's
own per-site `Store`; once `[cluster]` is enabled that store replicates through
the existing gossip path, so a CLI write on one node is observed on another with
no new code. To observe it directly: stand up a two-node cluster (per-site
clustered, `[db.sqlite.replication] per_site = true`, same `[kv] secret`), run
`ephpm php --site shop -r 'ephpm_kv_set("x","1",0)'` against node A, then
`ephpm php --site shop -r 'var_dump(ephpm_kv_get("x"))'` against node B (or read
`x` over node B's public listener) and confirm it is visible after gossip
convergence (~seconds). This path is unit/integration-proven but not yet a
live-cluster automated e2e.

## Not done

- **Single-site** CLI KV wire (no `sites_dir`): out of scope for now — like the
  DB wire, this targets multi-tenant deployments.
- **Automated multi-node e2e** for the clustered observation (lab-gated above).
- **`ephpm_kv_wait()` over the wire**: reports a timeout (no RESP equivalent).
