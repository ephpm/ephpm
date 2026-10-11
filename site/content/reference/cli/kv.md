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

## Exit status

`0` when the server answers normally, including `(nil)` from `get`, `(empty)` from `keys`, and `key does not exist` from `ttl`. `1` when the server answers with an error (`-ERR`, `-NOAUTH`, …), printed as `error: ...` on stderr. A connection failure, an authentication failure, or an invalid `--host` also prints `error: ...` and exits `1`.

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
