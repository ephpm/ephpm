# Scheduled embedded-database backups → S3 (MinIO demo)

A runnable stack that exercises ePHPm's **`[db.sqlite.backup]`** (v0.15.0):
every `interval`, each embedded SQLite database is snapshotted with
`VACUUM INTO`, gzipped, and uploaded to S3-compatible object storage. This
example uploads to a local **MinIO** so you can try it with no cloud account;
the same config block works against AWS S3 or Cloudflare R2 — see
[`ephpm.toml`](./ephpm.toml).

> For a purely local setup with no object store at all, set `dir = "/some/path"`
> instead of `s3_bucket` (and drop the `endpoint`/`region`/credentials): backups
> are then written to that local directory, and `db restore`/`db backup list`
> read it back with `--from /some/path`.

## Run it

```bash
docker compose up -d
```

- App: <http://localhost:8080>
- MinIO console: <http://localhost:9001> — login `minioadmin` / `minioadmin`

Within ~a minute, objects appear in the `ephpm-backups` bucket under
`ephpm/db/default/<YYYYMMDDTHHMMSSZ>.db.gz` (the site key for a single-node
config is `default`; `ephpm/db` is the default `s3_path`).

## Inspect and restore with the CLI

`ephpm db backup list` and `ephpm db restore` read the same `[db.sqlite.backup]`
block for the endpoint/region, and the `AWS_*` credentials from the environment.

```bash
# List the backup objects for the `default` site (oldest first):
docker compose exec \
  -e AWS_ACCESS_KEY_ID=minioadmin -e AWS_SECRET_ACCESS_KEY=minioadmin \
  ephpm ephpm db backup list --site default -c /app/ephpm.toml

# Restore is OFFLINE-ONLY (it replaces the live file), so stop the server first:
docker compose stop ephpm
docker compose run --rm \
  -e AWS_ACCESS_KEY_ID=minioadmin -e AWS_SECRET_ACCESS_KEY=minioadmin \
  ephpm ephpm db restore --site default -c /app/ephpm.toml \
  --from s3://ephpm-backups --yes
docker compose start ephpm
```

`ephpm db restore` downloads the newest object (or the newest at or before
`--at <YYYY-MM-DD[THH:MM:SSZ]>`), gunzips it, verifies it with
`PRAGMA integrity_check`, and only then renames it over the live `.db` (stale
`-wal`/`-shm` removed). `--from` can also be a local `.db` / `.db.gz` file.

To watch new data flow into later backups, add a row offline and wait a minute:

```bash
docker compose stop ephpm
docker compose run --rm ephpm \
  ephpm db query --site default -c /app/ephpm.toml -e "INSERT INTO visits DEFAULT VALUES;"
docker compose start ephpm
```

## Notes & limits

- **Credentials never live in the config** — only `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY` (+ optional `AWS_SESSION_TOKEN`) in the environment.
  Startup fails if `enabled = true` and none are set.
- The uploaded object is a **plain gzip of a standard SQLite file — not
  encrypted by ePHPm** (so anything can open it even when ePHPm is gone). Put
  server-side encryption and a tight policy on the bucket.
- **Retention**: objects older than `retention` are deleted after each
  successful upload, but the **newest object is always kept** regardless of age.
- **Memory headroom**: budget roughly ~100 MB extra resident per database being
  snapshotted; a multi-tenant server snapshots up to 4 databases at once.
- **Clustered mode** (v0.16.1+, experimental): backups run from each database's
  write node by default (`source = "primary"`) or a non-write holder
  (`source = "replica"`, whole-database only), gated so each database is shipped
  by exactly one node. Restoring **into** a cluster is experimental and manual —
  `ephpm db restore` installs the file on one node and replicas do **not**
  auto-converge, so you must stop every node, restore on the lowest-ordinal
  node, wipe every other node's `.db`/`-wal`/`-shm`, then start the restored
  node first (the command prints the exact procedure). This single-node example
  restores simply offline on the one node.

Reference: the `[db.sqlite.backup]` section of
`site/content/reference/config.md`.
