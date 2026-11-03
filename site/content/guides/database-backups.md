+++
title = "Database backups to object storage"
weight = 14
+++

Every embedded database ePHPm serves — the one `[db.sqlite] path` of a
single-node server, or every site's `<dir>/<site-key>.db` of a multi-tenant
one — can be snapshotted on a schedule, gzipped and uploaded to any
S3-compatible bucket, and brought back with `ephpm db restore`. This page is
the from-zero walkthrough; the knob-by-knob reference is
[`[db.sqlite.backup]`](/reference/config/#dbsqlitebackup), the commands are
[`ephpm db restore` / `db backup list`](/reference/cli/db/#ephpm-db-restore),
and the design (why `VACUUM INTO` inside the process, and why nothing else
may touch the file) is in
[Engines → Backups](/architecture/database/engines/#backups-and-why-external-sqlite-tools-lock-the-file).

**What you get, stated plainly.** A restorable copy of each database every
`interval` (the recovery point objective: a restore loses what was written
after the chosen snapshot), kept for `retention`. Restore is **offline**
(stop the server, install, start). It is not transaction-granularity
point-in-time recovery — that is
[planned, not yet implemented](/roadmap/turso-engine/#tier-1-point-in-time-restore-planned)
— and it does not run on clustered nodes (startup refuses it there; see
[limits](#what-it-does-not-do)).

## 1. Enable it

Add the section to the config of a single-node or per-site server. Nothing
else changes: the server keeps serving while a snapshot runs (the source is
read under a `BEGIN` read view, no write pause).

```toml
[db.sqlite]
dir = "/var/lib/ephpm/db"            # per-site: one <site-key>.db per vhost
# path = "/var/lib/ephpm/ephpm.db"   # or single-node: one database, site key `default`

[db.sqlite.backup]
enabled   = true
s3_bucket = "ephpm-backups"
s3_path   = "ephpm/db"               # default; objects land at <s3_path>/<site-key>/<ts>.db.gz
interval  = "15m"                    # default; the RPO
retention = "14d"                    # default; older objects are deleted, the newest is always kept
```

That is the whole AWS S3 case: `region` defaults to `us-east-1` (set it to
the bucket's region), `endpoint` defaults to `https://s3.<region>.amazonaws.com`
and addressing is virtual-hosted.

### Credentials

Credentials come **only** from the environment of the `ephpm` process —
never from `ephpm.toml`, never logged:

```sh
AWS_ACCESS_KEY_ID=…
AWS_SECRET_ACCESS_KEY=…
AWS_SESSION_TOKEN=…        # only for temporary credentials
```

With `enabled = true` and no credentials, **startup fails** (`failed to set
up embedded database backups: … needs credentials in the environment`).
Instance profiles, IRSA and other credential providers are not implemented:
mount static credentials. Under systemd that is an `EnvironmentFile=` the
service user can read and nobody else; on Kubernetes an `envFrom:
secretRef:` on the container. The same two variables serve the CLI commands
below, run on the same host.

### MinIO, Cloudflare R2, or any other S3-compatible endpoint

Set `endpoint` (with its scheme) and `region`; `path_style` then defaults to
`true`, which is what MinIO-style servers expect:

```toml
[db.sqlite.backup]
enabled   = true
s3_bucket = "ephpm-backups"
endpoint  = "http://minio.internal:9000"   # MinIO / versitygw / Garage / …
region    = "us-east-1"                    # any value MinIO accepts
# path_style = true                        # already the default once `endpoint` is set
```

```toml
[db.sqlite.backup]
enabled   = true
s3_bucket = "ephpm-backups"
endpoint  = "https://<account-id>.r2.cloudflarestorage.com"
region    = "auto"                         # R2 signs with `auto`
```

The bucket must exist (ePHPm never creates one) and, because the objects are
**plaintext** gzips of a standard SQLite file, it must carry server-side
encryption and a policy that limits who can read it. The object is readable
by anyone who can read the bucket; that is the price of an artifact that
opens with `sqlite3` when ePHPm is gone.

## 2. Check that it runs

At startup the server logs what it will do:

```text
INFO embedded database backup enabled (VACUUM INTO snapshot → gzip → object storage; the object is not encrypted by ePHPm — use bucket-side encryption) bucket=ephpm-backups endpoint=https://s3.us-east-1.amazonaws.com prefix=ephpm/db interval_secs=900 retention_secs=1209600 databases=3 staging=/tmp/ephpm-db-backup-…
```

`databases` counts the files that exist right now; a site whose database has
not been created yet is simply not listed until it is. The first tick fires
**one `interval` after startup**, not immediately, and each success is one
`INFO` line per database:

```text
INFO database backup uploaded site=blog key=ephpm/db/blog/20260301T084500Z.db.gz bytes=1048231 elapsed_ms=312
INFO database backup tick finished databases=3 ok=3 failed=0 elapsed_ms=940
```

A failure is a `WARN` naming the site and the stage (`database backup
snapshot FAILED` / `upload FAILED` / `retention sweep FAILED`). The object
keys are `<s3_path>/<site-key>/<YYYYMMDDTHHMMSSZ>.db.gz` — UTC, compact, so a
listing sorted by key is sorted by time. Single-node servers use the site key
`default`.

Three ways to see the state without reading logs:

- **The bucket itself**, with your own tooling:
  `aws s3 ls s3://ephpm-backups/ephpm/db/blog/` (add `--endpoint-url` for a
  non-AWS endpoint).
- **The CLI**, through the same client the server uses:

  ```text
  $ ephpm db backup list --site blog --config /etc/ephpm/ephpm.toml
  site:     blog
  location: s3://ephpm-backups/ephpm/db/blog/
  taken (UTC)                     bytes key
  2026-03-01T08:45:00Z          1048231 ephpm/db/blog/20260301T084500Z.db.gz
  2026-03-01T09:00:00Z          1049920 ephpm/db/blog/20260301T090000Z.db.gz
  ```

  `--json` prints the same as a document; `--from s3://bucket/prefix` lists a
  bucket other than the configured one.
- **The status endpoint**, `GET /_ephpm/db/backup`: the last successful
  upload and the last error per site, and the last tick. It is gated like
  `/_ephpm/requests` (on a multi-site server, `[server.diagnostics] token`
  or mTLS; it moves to the metrics listener with `[server.metrics] listen`),
  and never carries credentials. The
  [metrics](/reference/metrics/#database-backups) carry the same facts for
  Prometheus — `time() - ephpm_db_backup_last_success_timestamp_seconds` is
  the backup lag to alert on.

## 3. Retention

After every **successful** upload for a site, the objects of that site older
than `retention` are deleted — **except the newest object, which is always
kept** whatever its age, so a site that stopped changing keeps a restorable
backup. A sweep that fails (a listing or a delete refused by the bucket) is
logged, counted in `ephpm_db_backup_retention_errors_total`, and retried on
the next tick; it never fails the backup that just succeeded. There is no
`ephpm db backup rm`: delete objects with your bucket tooling, and the sweep
will not resurrect them.

With `interval = "15m"` and `retention = "14d"`, expect about 1,350 objects
per site at steady state. Size the bucket (and its lifecycle rules, if you
add any — they must not delete the newest object) accordingly.

## 4. Restore — a worked example

A bad deploy at 09:30 UTC corrupted the `blog` site's data; the last good
state is whatever was committed before that.

**Stop the server.** Restore is offline-only: a database file cannot be
replaced under an open engine handle, and `ephpm db restore` refuses while
either `[server] listen` or `[db.sqlite.proxy] mysql_listen` answers.

```sh
systemctl stop ephpm
```

**Find the snapshot.** The credentials must be in this shell's environment
too.

```text
$ export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=…
$ ephpm db backup list --site blog --config /etc/ephpm/ephpm.toml
site:     blog
location: s3://ephpm-backups/ephpm/db/blog/
taken (UTC)                     bytes key
2026-03-01T09:00:00Z          1049920 ephpm/db/blog/20260301T090000Z.db.gz
2026-03-01T09:15:00Z          1050112 ephpm/db/blog/20260301T091500Z.db.gz
2026-03-01T09:30:00Z          1061337 ephpm/db/blog/20260301T093000Z.db.gz
2026-03-01T09:45:00Z          1061402 ephpm/db/blog/20260301T094500Z.db.gz
```

**Restore the newest snapshot taken at or before the bad moment.** `--from`
is the bucket plus the configured `s3_path` (the site directory is appended
from `--site`); `--at` takes the compact form the objects are named with,
`YYYY-MM-DDTHH:MM:SSZ`, or a bare `YYYY-MM-DD` (midnight UTC). Without
`--at` the newest object is used.

```text
$ ephpm db restore --site blog --config /etc/ephpm/ephpm.toml \
    --from s3://ephpm-backups/ephpm/db --at 2026-03-01T09:29:00Z
site:    blog
source:  s3://ephpm-backups/ephpm/db/blog/20260301T091500Z.db.gz (taken 2026-03-01T09:15:00Z, 1050112 bytes compressed)
target:  /var/lib/ephpm/db/blog.db (will be REPLACED)
This replaces the live database with the backup. Type `yes` to continue: yes
restored /var/lib/ephpm/db/blog.db from s3://ephpm-backups/ephpm/db/blog/20260301T091500Z.db.gz (taken 2026-03-01T09:15:00Z, 1050112 bytes compressed)
```

The command picked 09:15 — the newest at or before `--at` — downloaded it
next to the target, gunzipped it, opened it with the engine and ran
`PRAGMA integrity_check`, then renamed it over `blog.db` and deleted the
stale `blog.db-wal` / `-shm`. A file that does not verify is deleted and the
command exits `2` **without touching the live database**. Pass `--yes` from
a script (without a terminal the prompt is refused rather than assumed).
Everything written between 09:15 and the stop is gone — that is the RPO.

**Start the server.**

```sh
systemctl start ephpm
```

The site serves the restored data; the next tick backs it up again. Two
variants:

```sh
# Single-node config: the one database is addressed as `default`
ephpm db restore --site default --config ephpm.toml --from s3://ephpm-backups/ephpm/db --yes

# A snapshot you downloaded yourself (or a plain .db copied off another host)
ephpm db restore --site blog --config ephpm.toml --from /backups/20260301T091500Z.db.gz --yes
```

A restore into a site whose `.db` does not exist yet (a fresh host) works the
same way — the target line says `(does not exist yet)` — which is how you
move a site between servers.

## What it does not do

- **Clustered nodes.** With `[cluster] enabled = true` (and `role = "auto"`,
  or an explicit `primary`/`replica`), `enabled = true` is a startup error:
  backups are not scheduled per owner/primary yet, and the artifact would
  carry replication bookkeeping a restore cannot reconcile. `db restore`
  refuses a clustered config for the same reason. On a cluster, use the
  stop-and-copy or volume-snapshot paths in
  [Engines → Backups](/architecture/database/engines/#supported-backups).
- **A live restore**, a "run a backup now" endpoint, and boot-time
  auto-hydrate from the newest object are all
  [planned — not yet implemented](/roadmap/turso-engine/).
- **Encryption of the object.** By design (see above); the bucket does it.
- **Litestream/LiteFS-style continuous replication.** Those tools lock a
  Turso-backed file and take the site down — the reason this feature exists
  inside the process in the first place.

## Testing it yourself

The end-to-end suite `crates/ephpm-e2e/tests/db_backup.rs` runs this whole
page against a real S3-compatible server in a container (versitygw — the
MinIO community images are no longer anonymously pullable), through the
shipped binary: schedule, key layout, `db backup list`, the status endpoint,
`db restore` newest and `--at`, the retention sweep, and the
server-is-running refusal. `cargo xtask e2e --tests db_backup` on a host
with `podman` or `docker`; it skips, loudly, where there is neither.
