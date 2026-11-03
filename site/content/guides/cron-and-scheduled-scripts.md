+++
title = "Cron and Scheduled Scripts"
weight = 14
+++

Most PHP apps have a script that has to run on a timer next to the web
process: Sendy's `scheduled.php` (every five minutes, with
`import-csv.php`, `update-segments.php` and `autoresponders.php` beside it),
Laravel's `artisan schedule:run`, WordPress's `wp cron event run --due-now`.
Under a classic stack that is a crontab line calling `php`. Under ePHPm the
supported shape is the same line calling `ephpm php --site`:

```bash
ephpm php --site sendy --config /etc/ephpm/ephpm.toml /srv/sites/sendy/scheduled.php
```

This guide covers what that command gives the script, what it needs from the
environment, how to run it on a timer (systemd, Docker, Kubernetes), and what
happens when a run outlives its interval. It applies to **multi-tenant
per-site mode** (`[server] sites_dir` with `[db.sqlite]`), which is where
`--site` is meaningful. Added in
[#689](https://github.com/ephpm/ephpm/issues/689).

## What the script gets

`ephpm php --site <key> --config <path>` does two things before it runs the
script, both derived from the same site key a request's `Host` header would
resolve to:

1. **Binds the in-process `ephpm_db_*` bridge** to that site's database
   (issue [#471](https://github.com/ephpm/ephpm/issues/471)) — apps built on
   the [`db-*` packages](/reference/php-packages/) (WordPress with
   `db-wordpress`, Laravel with `db-laravel`) need nothing else.
2. **Exports the request-path `DB_*` credentials into `$_SERVER`** when the
   running server's MySQL listener answers. These are the *same* variables the
   HTTP router injects into every request of that site, built by the same
   function, so a script that opens its own stock `mysqli` or `pdo_mysql`
   connection from them connects to the running server's listener and lands in
   its own database exactly as a web request would:

   | Variable | Value |
   |----------|-------|
   | `DB_HOST`, `DB_PORT` | the configured `[db.sqlite.proxy] mysql_listen`, split at the last `:` (default `127.0.0.1` / `3306`); the host is passed through verbatim |
   | `DB_USER`, `DB_USERNAME` | the site key |
   | `DB_PASSWORD` | `HMAC-SHA256([kv] secret, site key)`, the per-site wire password the listener verifies |
   | `DB_NAME`, `DB_DATABASE` | the site key (cosmetic — the connection's database is fixed by the credential) |
   | `DB_CONNECTION` | `mysql` |
   | `DATABASE_URL` | `mysql://<site>:<password>@<host>:<port>/<site>` |

   Sendy's `includes/config.php` reads exactly these
   (`$_SERVER['DB_HOST']` and friends) and calls `mysqli(...)`; it runs under
   `ephpm php --site sendy …` unchanged.

One difference from the server is worth knowing. In `ephpm serve` the `DB_*`
variables exist only in that request's `$_SERVER` — `getenv()` sees nothing,
because the process is multi-tenant. `ephpm php --site` is a single-tenant
process, so it puts them in its real environment: `$_SERVER`, `$_ENV` and
`getenv()` all see them, and any process the script spawns inherits them.

## What it needs

- **A running server whose MySQL listener is reachable** at the
  `[db.sqlite.proxy] mysql_listen` address in the config you pass, with
  `mysql_wire_enabled = true` (the default). The CLI probes that address at
  startup; see [Offline](#offline-no-server) for what happens when nothing
  answers. A raw socket needs a listener, and nothing listens on `:3306`
  outside the ePHPm server process — a plain `php scheduled.php` in a pod of
  its own has nowhere to connect.
- **`[kv] secret` set, and the same value the server uses.** The per-site
  password is derived from it; without it the CLI cannot authenticate and
  fails closed with a message naming the key. The trade-off is documented in
  the [multi-tenant pdo_mysql guide](/guides/multi-tenant-pdo-mysql/#the-wire-path-needs-kv-secret):
  whoever can read the config can derive any tenant's password.
- **The site's directory under `[server] sites_dir`** — `--site` is validated
  against it, fail-closed, before anything is bound.

## Offline (no server)

When nothing answers on `mysql_listen`, `ephpm php --site` opens the site's
own database file directly (single-node / per-site-single only; per-site
*clustered* is refused offline) and **exports no `DB_*` at all**. The
`ephpm_db_*` bridge works; a raw `mysqli`/`pdo_mysql` script finds
`$_SERVER['DB_HOST']` unset and fails on its own terms. This is deliberate:
injecting `DB_HOST=127.0.0.1` with nothing listening would turn a clear
"not configured" into a connection refused after a timeout, and the two
failure modes read very differently in a cron log. The CLI logs which path it
took on stderr (`… opened local database (offline) — … $_SERVER['DB_*'] is NOT
set …`).

So a raw-socket cron job is a job that runs **next to a reachable server**:
the same host, the same container network, or the same pod.

## Running it on a timer

### systemd

```ini
# /etc/systemd/system/sendy-scheduled.service
[Service]
Type=oneshot
ExecStart=/usr/local/bin/ephpm php --site sendy --config /etc/ephpm/ephpm.toml /srv/sites/sendy/scheduled.php

# /etc/systemd/system/sendy-scheduled.timer
[Timer]
OnCalendar=*:0/5
Unit=sendy-scheduled.service
[Install]
WantedBy=timers.target
```

A systemd timer never starts a second instance of a `oneshot` service that is
still running, so overlapping runs are impossible here by construction.

### Docker Compose

A second service on the same image and network, looping on the interval:

```yaml
services:
  web:
    image: ghcr.io/ephpm/ephpm:latest
    command: ["ephpm", "serve", "--config", "/etc/ephpm/ephpm.toml"]
    volumes: [ "./ephpm.toml:/etc/ephpm/ephpm.toml:ro", "./sites:/srv/sites", "./db:/var/lib/ephpm/db" ]
  sendy-cron:
    image: ghcr.io/ephpm/ephpm:latest
    network_mode: "service:web"          # share web's loopback: 127.0.0.1:3306 is the server
    entrypoint: ["sh", "-c"]
    command:
      - |
        while true; do
          ephpm php --site sendy --config /etc/ephpm/ephpm.toml /srv/sites/sendy/scheduled.php
          sleep 300
        done
    volumes: [ "./ephpm.toml:/etc/ephpm/ephpm.toml:ro", "./sites:/srv/sites:ro" ]
```

`network_mode: "service:web"` is what makes the default loopback
`mysql_listen` reachable from the cron container. The loop is serial, so runs
never overlap; the next one starts `sleep` seconds after the previous one
*ends*.

### Kubernetes

**Sidecar in the server pod (recommended).** Containers in one pod share a
network namespace, so the default `mysql_listen = "127.0.0.1:3306"` is
reachable with the *same* config file the server uses — no second config, no
exposed listener:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      containers:
        - name: ephpm
          image: ghcr.io/ephpm/ephpm:latest
          args: ["serve", "--config", "/etc/ephpm/ephpm.toml"]
          volumeMounts:
            - { name: config, mountPath: /etc/ephpm, readOnly: true }
            - { name: sites,  mountPath: /srv/sites }
            - { name: db,     mountPath: /var/lib/ephpm/db }
        - name: sendy-cron
          image: ghcr.io/ephpm/ephpm:latest
          command: ["sh", "-c"]
          args:
            - |
              while true; do
                ephpm php --site sendy --config /etc/ephpm/ephpm.toml /srv/sites/sendy/scheduled.php
                sleep 300
              done
          volumeMounts:
            - { name: config, mountPath: /etc/ephpm, readOnly: true }
            - { name: sites,  mountPath: /srv/sites, readOnly: true }
      volumes:
        - name: config
          secret: { secretName: ephpm-config }   # ephpm.toml, which holds [kv] secret
        # sites / db as in your deployment
```

The sidecar needs the config (for `sites_dir`, `[db.sqlite]` and
`[kv] secret`) and the site directory (`--site` validates it, and the script
lives there). It does **not** need the database volume: with the server up,
the CLI forwards over the listener and never opens the file. Keep it off the
sidecar so an outage of the server container cannot make the CLI fall back to
opening the file offline while the server still holds it.

**A real `CronJob`.** A `CronJob` pod is a separate pod, so the server's
loopback listener is out of reach. For `ephpm php --site` to take the wire
path from there, the server's listener has to be reachable on the pod network
— bind it beyond loopback (`[db.sqlite.proxy] mysql_listen = "0.0.0.0:3306"`)
behind a `Service`, and give the job a config whose `mysql_listen` names that
Service (`"web-db.default.svc:3306"`): the CLI probes it, authenticates as the
site, and hands the script `DB_HOST=web-db.default.svc`, `DB_PORT=3306`. The
job also needs `[server] sites_dir` with the site directory mounted and the
same `[kv] secret`. Before choosing this shape, weigh what it exposes: the
listener is authenticated (every connection must present a tenant's derived
password), but it is now reachable by every pod the network policy lets in,
and a `NetworkPolicy` restricting `:3306` to the job's pods is part of the
pattern, not an afterthought. Set `concurrencyPolicy: Forbid` (see below). The
loopback sidecar is the shape ePHPm's end-to-end suite exercises; the
Service shape uses the same code path (the listen host is passed through
verbatim), but is not covered by a live test.

## Overlap and locking

ePHPm has **no scheduler and no overlap control of its own**: `ephpm php
--site` is one process running one script, and nothing in the server knows
one cron run from another. Whether two runs can overlap is decided by
whatever starts them, and a long run — a big Sendy send, a slow import — can
outrun a five-minute interval.

- **systemd timers** and the **shell-loop sidecar** above are serial by
  construction.
- **Kubernetes `CronJob`**: set `concurrencyPolicy: Forbid` (skip a tick while
  the previous job is running) or `Replace`; the default `Allow` starts a
  second pod on schedule regardless.
- **The app's own lock.** Sendy 7.1+ takes a per-campaign named lock
  (`SELECT GET_LOCK("sendy_campaign_send_<id>", 0)`) before sending, so an
  overlapping run skips campaigns another run holds rather than double-sending.
  ePHPm serves `GET_LOCK` and friends over the wire with MySQL semantics,
  **scoped per site** and **released when the holding connection ends,
  however it ends** — a killed cron process drops its locks. The details, and
  the clustered-mode caveats (locks live on the site's owner node), are in
  [MySQL compatibility: named locks](/reference/mysql-compatibility/).

A job that holds no lock of its own and runs under something that permits
overlap can run twice at once. That is the platform's expectation to state
plainly: ePHPm guarantees that each run gets the right database and
credentials; it does not serialize runs.

## What `ephpm php --site` does not give a cron script

- **No KV store.** `ephpm php` starts no KV, so `ephpm_kv_*` returns failure
  values and the `ephpm` session handler persists nothing; see
  [`ephpm php` → No KV store](/reference/cli/php/#no-kv-store-under-ephpm-php).
  A script that needs the server's KV talks to its RESP listener with a Redis
  client.
- **No sandbox.** The script runs as the user invoking `ephpm php`, with
  that user's filesystem view. [`ephpm exec`](/reference/cli/exec/) provides
  the sandbox and injects no credentials of its own; nesting
  `ephpm php --site` inside it does not currently work with the config in
  `/etc/ephpm` (the sandbox hides that file on purpose).

## See also

- [`ephpm php`](/reference/cli/php/) — the CLI reference, including the
  strategy the `--site` binding picks and the `$_SERVER` contract
- [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/) — how the
  per-site credential works on the request path
- [MySQL compatibility: named locks](/reference/mysql-compatibility/)
