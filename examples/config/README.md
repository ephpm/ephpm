# Example `ephpm.toml` configurations

Heavily-commented, copy-pasteable configs for recent ePHPm features. Every key
and default in these files is verified against
[`crates/ephpm-config/src/lib.rs`](../../crates/ephpm-config/src/lib.rs) — if a
key here ever stops matching the code, that is a bug (the "Docs Must Match
Code" rule). Replace every `change-me-*` placeholder before using one.

| File | Shows | Key sections |
|------|-------|--------------|
| [`multitenant-hardened.toml`](./multitenant-hardened.toml) | A hardened multi-tenant / virtual-hosting node: per-site DB isolation, `open_basedir` + function denylist, root privilege drop, host aliases, and the full secure-by-default F1–F5 breakout-audit hardening — environment scrub (F1), Landlock self-sandbox + fatal privilege-drop (F3), the Linux eBPF per-vhost AF_INET policy + AF_UNIX connect fence (F2), authenticated SQL wire listeners (F4), and cron CPU/memory ceilings (F5) | `[php] scrub_environment`/`env_allowlist`, `[server] run_as_user`/`self_sandbox`/`require_privilege_drop`, `[server.host_map]`, `[server.security]`, `[server.tenant_network]`, `[db.sqlite] dir`, `[db.wire_auth]`, `[cron]`, `[kv] secret` |
| [`per-site-clustered.toml`](./per-site-clustered.toml) | A clustered node where each tenant's embedded Turso database replicates across the cluster (HRW-owned writes, CDC) — **experimental** | `[db.sqlite.replication] per_site`, `[cluster]`, `[cluster.kv]`, `[kv.snapshot] peer_handoff` |
| [`kv-snapshots.toml`](./kv-snapshots.toml) | KV warm-restart snapshots — encrypted, local-file and S3/object-storage modes, plus the control endpoint and CLI | `[kv] secret`, `[kv.snapshot]` |

## Already covered elsewhere in `examples/`

These features ship with their own worked examples, so they are not duplicated
here:

- **Distributed cron / scheduled jobs** — [`examples/cron/`](../cron/)
  (`[cron]` + `[[cron.jobs]]`: per-site PHP, siteless binaries, timezones,
  catch-up windows, the directory tier, clustering caveats).
- **Scheduled DB backups to S3/MinIO** — [`examples/db-backup-minio/`](../db-backup-minio/)
  (`[db.sqlite.backup]`, with AWS / R2 / MinIO variants).
- **Native middleware** — [`examples/php-middleware/`](../php-middleware/) (PHP
  middleware) and [`examples/rust-middleware/`](../rust-middleware/) (the
  native C-ABI authoring kit).
- **KV from PHP** — [`examples/README-KV.md`](../README-KV.md),
  [`kv-sapi-basic.php`](../kv-sapi-basic.php),
  [`kv-redis-predis.php`](../kv-redis-predis.php).
- **Worker mode** — [`examples/worker/`](../worker/).
- **WordPress (compose)** — [`examples/wordpress-compose/`](../wordpress-compose/).

## Related guides

Each example links its full guide inline. The reference for every config key is
[`site/content/reference/config.md`](../../site/content/reference/config.md).
