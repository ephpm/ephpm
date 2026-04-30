+++
title = "ephpm kv"
weight = 3
+++

Inspect or manipulate the KV store on a running ePHPm server. Speaks the RESP2 protocol — the same one that PHP clients (Predis, phpredis) use, so you're seeing the live store as PHP sees it.

## Synopsis

```bash
ephpm kv [--host HOST] [--port PORT] <subcommand> [args]
```

| Flag | Default | Purpose |
|------|---------|---------|
| `--host` | `127.0.0.1` | KV server host |
| `--port` | `6379` | KV server port |

The server must be configured with `[kv.redis_compat] enabled = true` for these commands to connect.

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

Reports TTL info: a positive number (seconds), `-1` (no expiry), or `-2` (key missing).

```bash
ephpm kv ttl session:abc123
```

## Common patterns

```bash
# Debug rate limiting
ephpm kv get "ratelimit:$user_id"
ephpm kv ttl "ratelimit:$user_id"

# Count active sessions
ephpm kv keys 'session:*' | wc -l

# Clear all sessions
ephpm kv keys 'session:*' | xargs -r ephpm kv del

# Connect to a remote instance
ephpm kv --host 10.0.1.5 --port 6379 keys '*'
```

## See also

- [KV from PHP](/guides/kv-from-php/) — the `ephpm_kv_*` SAPI functions
- [KV store architecture](/architecture/kv-store/)
