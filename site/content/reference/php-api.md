+++
title = "PHP API"
weight = 6
+++

Everything ePHPm adds to the PHP environment, in one place: the native
functions and classes it registers, the `$_SERVER` variables it injects,
and the ini settings it writes or leaves at PHP's defaults. The guides
linked from each section have the details and examples. The source of truth
is [`crates/ephpm-php/ephpm_wrapper.c`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-php/ephpm_wrapper.c)
(function tables and arginfo) and
[`crates/ephpm-php/src/request.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-php/src/request.rs)
(`$_SERVER`).

## The SAPI

`PHP_SAPI` / `php_sapi_name()` is `"ephpm"` when serving requests and
`"cli"` under [`ephpm php`](/reference/cli/php/).

Every function below is registered in **both** contexts, so
`function_exists()` tells you that you are running under ePHPm, not which
features are configured. Under `ephpm php`, the KV functions are present but
not connected to a store (see [KV from PHP](/guides/kv-from-php/#under-ephpm-php-the-cli)),
the worker functions are inert, and the database functions work only with
`--site` (see [`ephpm php`](/reference/cli/php/)).

## Native functions

### KV store — `ephpm_kv_*`

`ephpm_kv_get`, `ephpm_kv_set`, `ephpm_kv_setnx`, `ephpm_kv_del`,
`ephpm_kv_del_if_eq`, `ephpm_kv_exists`, `ephpm_kv_incr`, `ephpm_kv_decr`,
`ephpm_kv_incr_by`, `ephpm_kv_expire`, `ephpm_kv_ttl`, `ephpm_kv_pttl`,
`ephpm_kv_flush_all`, `ephpm_kv_wait`.

Signatures, return values and edge cases:
[KV from PHP — Function reference](/guides/kv-from-php/#function-reference).

### Embedded database — `ephpm_db_*`

`ephpm_db_query`, `ephpm_db_execute`, `ephpm_db_run`, `ephpm_db_columns`,
`ephpm_db_in_transaction`, `ephpm_db_available`, `ephpm_db_errno`,
`ephpm_db_error`.

See [Database from PHP](/guides/db-from-php/). The bridge's reserved error
codes (`2000`–`2004`) are exception codes, not PHP constants; compare
against the integers.

### WebSockets — `ephpm_ws_*` (experimental)

`ephpm_ws_send`, `ephpm_ws_connection_send`, `ephpm_ws_subscribe`,
`ephpm_ws_connection_subscribe`, `ephpm_ws_unsubscribe`,
`ephpm_ws_connection_unsubscribe`, `ephpm_ws_broadcast`, `ephpm_ws_close`,
`ephpm_ws_connection_close`.

See [WebSockets — Function reference](/guides/websockets/#function-reference).
Misuse throws a plain `\Exception` with code `0`.

### Worker mode — `Ephpm\Worker\*`

| Symbol | Signature |
|---|---|
| `Ephpm\Worker\take_request()` | `(): ?Ephpm\Worker\Envelope` |
| `Ephpm\Worker\send_response()` | `(int $status, array $headers, string $body): void` |
| `Ephpm\Worker\send_response_stream()` | `(int $status, array $headers, resource $body): void` |
| class `Ephpm\Worker\Envelope` | `serverVars()`, `headers()`, `cookies()`, `query()`, `parsedBody()`, `files()`, `bodyStream()`, `rawBody()` |

Registered by the internal module `ephpm_worker`. Outside worker mode
`take_request()` returns `null` immediately and the `send_response*()`
functions do nothing. See [Worker Mode](/guides/worker-mode/).

### PHP middleware — `ephpm_middleware_config()`

`ephpm_middleware_config(): ?string` returns the running PHP middleware
mount's `config` table as a JSON string, or `null` when the script is not
running as a PHP middleware (or the mount has no `config`). See
[Native Middleware — The PHP lane](/guides/native-middleware/#the-php-lane-experimental).

### CLI only — under `ephpm php`

`cli_set_process_title()` and `cli_get_process_title()`, plus the `STDIN`,
`STDOUT` and `STDERR` constants, exactly as php-cli provides them. They do
not exist while serving requests. See [`ephpm php`](/reference/cli/php/).

### IDE stubs

The [`ephpm/db`](/reference/php-packages/#database-adapters) package ships
IDE/static-analysis stubs for `ephpm_db_*`, and
[`ephpm/worker`](/reference/php-packages/#worker-mode-adapters) for the
worker API. Never autoload a stub at runtime: the functions are native, and
redefining them is a fatal error.

## Session save handler — `ephpm`

`session.save_handler = ephpm` stores `$_SESSION` in the KV store. It is
registered in every mode but **not enabled by default**. See
[Sessions](/guides/sessions/).

## `$_SERVER` variables

ePHPm builds `$_SERVER` from scratch for every request. Nothing it injects
is a process environment variable: **`getenv()` does not see any of the
variables below.** Read them from `$_SERVER`. (A process-wide environment
would be shared by every tenant's threads.)

| Variables | When |
|---|---|
| `REQUEST_METHOD`, `REQUEST_URI`, `SCRIPT_FILENAME`, `SCRIPT_NAME`, `PHP_SELF`, `DOCUMENT_ROOT`, `SERVER_NAME`, `SERVER_PORT`, `SERVER_PROTOCOL`, `QUERY_STRING`, `REMOTE_ADDR`, `REMOTE_PORT` | Every request. See [`$_SERVER` Variables](/architecture/http/#_server-variables). |
| `SERVER_SOFTWARE` | Every request. Currently the fixed string `ePHPm/0.1.0`, whatever the release; do not use it to detect the version. |
| `GATEWAY_INTERFACE` = `CGI/1.1`, `REDIRECT_STATUS` = `200` | Every request. |
| `HTTPS` = `on` | Requests that arrived over TLS. |
| `AUTH_TYPE`, `PHP_AUTH_USER`, `PHP_AUTH_PW`, `PHP_AUTH_DIGEST` | Derived from an `Authorization` header. See [HTTP Authentication Variables](/architecture/http/#http-authentication-variables). |
| `HTTP_*`, `CONTENT_TYPE`, `CONTENT_LENGTH` | One per request header, CGI-style. |
| `DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_PASSWORD`, `DATABASE_URL` | `[db.mysql]` or `[db.postgres]` with `inject_env = true` (the default). Proxy mode does **not** set `DB_DATABASE` or `DB_USERNAME`. |
| The above plus `DB_DATABASE` and `DB_USERNAME` | Per-site embedded databases (`[server] sites_dir` + `[db.sqlite]`): each site's own credentials. See [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/). |
| `EPHPM_REDIS_HOST`, `EPHPM_REDIS_PORT`, `EPHPM_REDIS_USERNAME`, `EPHPM_REDIS_PASSWORD` | `[kv.redis_compat] enabled = true` + `[kv] secret` + `[server] sites_dir`. See [KV from PHP — Multi-tenant note](/guides/kv-from-php/#multi-tenant-note). |
| `EPHPM_NODE_ID` | When this node has an identity: the running gossip node's id with `[cluster] enabled = true`, otherwise `[cluster] node_id` if it is set. Absent when neither applies. |
| `WS_EVENT`, `WS_CONNECTION_ID`, `WS_OPCODE` | WebSocket events only. See [WebSockets](/guides/websockets/). |

**Limit: 128 entries.** ePHPm registers at most 128 `$_SERVER` entries per
request and drops the rest without an error. The fixed variables and the
authentication variables are registered first, then one entry per request
header, and the ePHPm-injected variables (`DB_*`, `DATABASE_URL`,
`EPHPM_REDIS_*`, `EPHPM_NODE_ID`, `WS_*`) **last**. A request with roughly
95 or more headers can therefore lose the injected variables, including a
site's database credentials.

## ini settings

### Where PHP's configuration comes from

ePHPm generates a `php.ini` at startup and points PHP at it, so **a system
`php.ini` is never read.** Configure PHP through `[php] ini_overrides`,
`[php] ini_file` (its contents are copied into the generated file), or
`ini_set()` at runtime. See [`[php]`](/reference/config/#php).

### What ePHPm writes

In the order they appear in the generated file, so later lines win:

1. `log_errors=On` — worker mode only.
2. `extension=<entry>` — one per `[php] extensions` entry.
3. The contents of `[php] ini_file`, if set.
4. `opcache.validate_timestamps` (always), plus the autotuned OPcache,
   `memory_limit`, realpath-cache and assertion lines. See
   [Resource-aware autotuning](/reference/config/#resource-aware-autotuning).
   On Windows, also `opcache.cache_id=ephpm-<pid>`, which gives each process
   a private OPcache segment (#362).
5. `max_execution_time` — on builds with per-thread execution timers
   (not Windows).
6. Every `[php] ini_overrides` line except `disable_functions`.
7. `disable_functions` — the union of your list and ePHPm's multi-tenant
   baseline (vhost mode only).
8. `mysqli.allow_persistent=0`, `pgsql.allow_persistent=0`,
   `odbc.allow_persistent=0` and, unless cluster OPcache invalidation is on,
   `opcache.restrict_api` — with the multi-tenant hardening preset only. See
   [Multi-tenant hardening](/guides/multi-tenant-hardening/).

In multi-tenant mode, `open_basedir`, `sys_temp_dir`, `upload_tmp_dir`,
`session.save_path` and a site's `auto_prepend_file` are also set per
request. See [Virtual Hosts](/guides/virtual-hosts/).

### What ePHPm leaves at PHP's defaults — `display_errors` is on

ePHPm does **not** set `display_errors`, so it keeps PHP's compiled-in
default of `1`, and `error_reporting` defaults to `E_ALL`. Together with the
embed SAPI's built-in `html_errors=0`, warnings, notices and fatal errors
are written **into the HTTP response**, including file paths:

```
Warning: Undefined variable $x in /var/www/app/index.php on line 3
```

That is fine in development but leaks internals in production. Turn it off
and log instead:

```toml
[php]
ini_overrides = [
    ["display_errors", "Off"],
    ["log_errors", "On"],
]
```

The embed SAPI's other built-in values also apply unless you override them:
`output_buffering=0`, `implicit_flush=1`, `register_argc_argv=1` and
`max_input_time=-1`.
