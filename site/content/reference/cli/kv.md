+++
title = "ephpm kv"
weight = 3
+++

Inspect or manipulate the KV store on a running ePHPm server. Speaks the RESP2 protocol — the same one that PHP clients (Predis, phpredis) use, so you're seeing the live store as PHP sees it.

## Synopsis

```bash
ephpm kv [--host HOST] [--port PORT] [--password PW] [--user NAME] <subcommand> [args]
```

| Flag | Default | Purpose |
|------|---------|---------|
| `--host` | `127.0.0.1` | KV server host: a hostname (`localhost`, `kv.internal` — resolved, every returned address tried in turn), an IPv4 literal, or an IPv6 literal with or without brackets (`::1` / `[::1]`) |
| `--port` | `6379` | KV server port |
| `--password` | `$EPHPM_KV_PASSWORD` | Password sent as RESP `AUTH` before the first command |
| `--user` | (none) | First argument of the two-argument `AUTH <user> <password>` form. Requires `--password`. |

Like `--host` and `--port`, these are flags of `ephpm kv` itself, so they go **before** the subcommand.

The server must be configured with `[kv.redis_compat] enabled = true` for these commands to connect. An `ephpm dev` server reads no `ephpm.toml`, but it does honour `EPHPM_*` overrides, so `EPHPM_KV__REDIS_COMPAT__ENABLED=true ephpm dev` turns the listener on there.

## Authentication

The RESP listener requires `AUTH` as soon as either `[kv.redis_compat] password` or `[kv] secret` is set — until a connection authenticates, every command except `AUTH` and `QUIT` is answered with `-NOAUTH Authentication required`. A **single-site** server with `[kv] secret` set and no `password` refuses to start, since the secret cannot authenticate anyone without `sites_dir`. The full matrix is in [KV RESP Protocol → Authentication](/reference/kv-resp/#authentication). There are two server-side modes:

**Single password** (`[kv.redis_compat] password`) — pass `--password`, or set `EPHPM_KV_PASSWORD` and leave the flag off. Do not add `--user` here: the server compares the first `AUTH` argument, so `AUTH <user> <password>` fails.

```bash
ephpm kv --password "$KV_PASSWORD" keys '*'

export EPHPM_KV_PASSWORD=...
ephpm kv keys '*'
```

**Per-site HMAC** (`[kv] secret` together with `[server] sites_dir`) — the server expects `AUTH <site-key> <HMAC-SHA256(secret, site-key)>` and scopes the connection to that vhost's store. The site key is the vhost directory name (the value PHP sees as `EPHPM_REDIS_USERNAME`, with any `sites_domain_suffix` already stripped). The derived password is the same value ePHPm injects into PHP as `EPHPM_REDIS_PASSWORD`, so pass it with `--user`:

```bash
ephpm kv --user blog.example.com --password "$DERIVED" keys '*'
```

`[kv.redis_compat] password` is ignored in this mode.

The CLI does not derive that value itself — it never reads `[kv] secret`.

**Limitation:** `ephpm deploy` and `ephpm cache reset` accept `--password` but not `--user`. They write `opcache:version:*`, which the server's OPcache watcher reads from the *default* store, and a site-scoped HMAC connection can only reach one vhost's store. Under per-site HMAC auth there is currently no CLI route to those keys.

## Subcommands

### `ping`

Checks the connection.

```bash
ephpm kv ping
# PONG
```

### `keys [PATTERN]`

Lists keys matching a glob pattern. Default pattern is `*`.

```bash
ephpm kv keys              # all keys
ephpm kv keys 'session:*'  # all session keys
ephpm kv keys 'cache:user:*'
```

### `get <KEY>`

Reads a value.

```bash
ephpm kv get mykey
ephpm kv get session:abc123
```

### `set <KEY> <VALUE> [--ttl SECS]`

Writes a value. Optionally sets a TTL in seconds.

```bash
ephpm kv set greeting "hello"
ephpm kv set session:abc123 '{"user":1}' --ttl 3600
```

### `del <KEY>...`

Deletes one or more keys.

```bash
ephpm kv del mykey
ephpm kv del key1 key2 key3
```

### `incr <KEY> [--by N]`

Atomic increment. Default delta is 1.

```bash
ephpm kv incr page:views
ephpm kv incr counter --by 10
```

### `ttl <KEY>`

Reports TTL info as one of three lines. It sends `TTL` and `PTTL`, and prints a sentence rather than the raw integer:

```bash
$ ephpm kv ttl session:abc123
expires in 3542s (3541873ms)

$ ephpm kv ttl greeting
no expiry (persistent key)

$ ephpm kv ttl nosuchkey
key does not exist
```

All three exit `0`.

### `snapshot`

Warm-restart snapshot operations ([`[kv.snapshot]`](/reference/config/#kvsnapshot)). These do **not** use the RESP listener, so `--host`/`--port`/`--password`/`--user` do not apply: the file subcommands read a snapshot directly, and the server subcommands call the running server's `/_ephpm/kv/snapshot` [control endpoint](/reference/http-endpoints/#server-owned-paths), which is gated like `/_ephpm/requests` (`[server.diagnostics] allow`/`token`; a token is mandatory on a multi-site server).

```bash
ephpm kv snapshot inspect <FILE|s3://bucket/key> [--config PATH] [--json]
ephpm kv snapshot verify  <FILE|s3://bucket/key>  --config PATH
ephpm kv snapshot list    --config PATH [--json]
ephpm kv snapshot rm      <IDENTITY> --config PATH
ephpm kv snapshot save    [--url URL] [--token TOKEN] [--config PATH]
ephpm kv snapshot load    [--url URL] [--token TOKEN] [--config PATH]
ephpm kv snapshot status  [--url URL] [--token TOKEN] [--config PATH]
```

**`inspect <SOURCE>`** prints the plaintext header — format version, when and by which node it was written, whether it is multi-tenant, the cipher/KDF/compression ids and the file size. It never decrypts a value. With `--config` (read for `[kv] secret`) it also opens the sealed index and lists, per site, the number of blocks, entries and plaintext bytes. `--json` prints the same as one JSON document. The source is a file path, or `s3://bucket/key` — then `--config` is required (for `[kv.snapshot] endpoint`/`region`/`path_style`; the bucket comes from the URL) and the credentials from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`; the object is downloaded to a temporary directory that is removed afterwards.

```bash
$ ephpm kv snapshot inspect /var/lib/ephpm/kv/web-1.kvsnap --config /etc/ephpm/ephpm.toml
file:              /var/lib/ephpm/kv/web-1.kvsnap
format:            v1
created:           2026-10-22T14:03:11Z (unix ms 1792764191000)
written by:        web-1
multi-tenant:      yes
sealed with:       ChaCha20-Poly1305 / HKDF-SHA256 / block compression lz4
size:              48213776 bytes
blocks:            41
entries:           212044
plaintext bytes:   97611230

site                                       blocks      entries  plaintext bytes
(default)                                       2         1203          3115201
blog.example.com                               39       210841         94496029
```

**`verify <SOURCE> --config PATH`** authenticates and decodes every block (what a load would do, without restoring anything) and reports `ok: … N blocks, M entries verified` or lists the block indexes that failed. Same sources as `inspect`.

**`list --config PATH`** lists the snapshots at the configured location — the `*.kvsnap` objects under `[kv.snapshot] url` in object mode, the files under `[kv.snapshot] path` in local mode — as identity, size and last-modified. `--json` for a document.

```bash
$ ephpm kv snapshot list --config /etc/ephpm/ephpm.toml
location: s3://kv-snapshots/prod/kv/
identity                                          bytes modified
web-0                                          48213776 2026-10-22T14:03:11.000Z
web-1                                          47990112 2026-10-22T14:02:58.000Z
```

**`rm <IDENTITY> --config PATH`** deletes one snapshot (object or file) by identity; exits `1` when there is none.

**`save`**, **`load`**, **`status`** talk to a running server. `--url` is the server's base URL (`http://127.0.0.1:8080`); with `--config` instead, the URL is derived from `[server] listen` (an unspecified bind such as `0.0.0.0` becomes `127.0.0.1`) and the token from `[server.diagnostics] token`. `save` asks the server to write its snapshot now and prints the report; `load` asks it to load the configured snapshot into the live store; `status` prints the status document (file, last save, last load).

```bash
ephpm kv snapshot save --config /etc/ephpm/ephpm.toml
ephpm kv snapshot status --url http://10.0.1.5:8080 --token "$DIAG_TOKEN"
```

Exit status for `snapshot`: `0` success; `1` usage, I/O, connection or server error (`4xx`/`5xx`, including `409` when a save is already running); `2` the file is not a usable snapshot (`inspect` could not read it, or `verify` found bad blocks).

## Exit status

`0` when the server answers normally, including `(nil)` from `get`, `(empty)` from `keys`, and `key does not exist` from `ttl`. `1` when the server answers with an error (`-ERR`, `-NOAUTH`, …), printed as `error: ...` on stderr. A connection failure, an authentication failure, or an invalid `--host` also prints `error: ...` and exits `1`. See [`snapshot`](#snapshot) for that subcommand's codes.

## Common patterns

```bash
# Debug rate limiting
ephpm kv get "ratelimit:$user_id"
ephpm kv ttl "ratelimit:$user_id"

# Count active sessions
ephpm kv keys 'session:*' | wc -l

# Clear all sessions
ephpm kv keys 'session:*' | xargs -r ephpm kv del

# Connect to a remote instance (by IP or by name)
ephpm kv --host 10.0.1.5 --port 6379 keys '*'
ephpm kv --host kv.internal keys '*'

# Connect to a password-protected listener
ephpm kv --host 10.0.1.5 --password "$KV_PASSWORD" keys '*'
```

## See also

- [KV from PHP](/guides/kv-from-php/) — the `ephpm_kv_*` SAPI functions
- [KV store architecture](/architecture/kv-store/)
