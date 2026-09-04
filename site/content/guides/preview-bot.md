+++
title = "PR Preview Bot"
weight = 6
+++

Every pull request gets a live preview URL — its own database, its own KV
keyspace, deployed in seconds and torn down when the PR closes — and a sticky
comment with the link. This is the same machinery that runs the previews on
`ephpm/wordpress-sample`.

The runtime half is ePHPm itself: multi-tenant [virtual hosts](/guides/virtual-hosts/),
per-site Turso databases, per-site KV, and the `[server] preview` limits preset.
The bot half is two separate open-source projects that sit on top of ePHPm:

| Component | Repo | Language | Role |
|-----------|------|----------|------|
| **switchboard-api** | [`ephpm/switchboard-api`](https://github.com/ephpm/switchboard-api) | PHP (PSR-15) | The webhook receiver and GitHub App surface. Runs **on ePHPm** as a confined vhost. HMAC-verifies each webhook, dedups it, and publishes the desired preview state. |
| **switchboard** | [`ephpm/switchboard`](https://github.com/ephpm/switchboard) | Rust | A daemon on every node. Claims deploy jobs, fetches the PR, runs the build, atomically swaps the checkout into `sites_dir`, health-gates, and posts/updates the sticky comment. |

Both consume ePHPm; neither is part of the ePHPm binary. This guide covers how
the pieces fit together and how to stand the system up on your own repo. Pull
exact flag names and config values from each repo's README before you deploy —
they are the authority, and the projects are young.

## How it works

```
GitHub  ──pull_request webhook──►  switchboard-api (PHP, on ePHPm)
                                        │  1. verify X-Hub-Signature-256 (HMAC-SHA256)
                                        │  2. dedup by X-GitHub-Delivery GUID
                                        │  3. publish desired state
                                        ▼
                        ┌─── single node ───┐   ┌──────── cluster ────────┐
                        │ enqueue job file  │   │ write to gossip KV:      │
                        │ into ./queue/     │   │  switchboard:preview:<l> │
                        └─────────┬─────────┘   │  switchboard:index       │
                                  │             │  switchboard:gen         │
                                  │             └────────────┬────────────┘
                                  │                          │  each node's daemon kicks
                                  │                          │  GET /drain on its local
                                  │                          │  switchboard-api, which
                                  │                          │  materializes KV → ./queue/
                                  ▼                          ▼
                            switchboard daemon (Rust) consumes ./queue/
                                  ├─ git fetch refs/pull/<N>/head
                                  ├─ load ephpm.yaml (or synthesize from framework)
                                  ├─ run build:  (sh -c)
                                  ├─ atomic swap ──► <sites_dir>/<label>/
                                  ├─ run seed:   (sh -c, gets $PREVIEW_URL/$PREVIEW_HOST/$PR)
                                  ├─ poll health: until 200
                                  ├─ POST a GitHub Deployment (state: success)
                                  └─ post/update the sticky comment
                                        │
Browser ──► ephpm (:443, *.preview wildcard cert) ──► <label>.preview.<domain>
```

**The two boundaries never overlap.** switchboard-api talks to the daemon only
through **files** (`./queue/`) — never a socket, never a shared library. The
gossip KV carries *desired state between nodes*; each node's daemon then kicks
its own local switchboard-api, which reads the KV and writes fresh queue files
for the Rust daemon on that node. The daemon itself has **no KV awareness at
all** — it only ever consumes queue files.

### Single-node vs cluster

- **Single node** — switchboard-api enqueues jobs directly into the daemon's
  `./queue/` directory. No KV, no `/drain`. Run the daemon with
  `--drain-interval-secs 0` to disable the drain kick.
- **Cluster** — switchboard-api auto-detects that it is running on ePHPm (the
  `ephpm_kv_*` SAPI functions exist) and publishes desired state into the
  gossip-replicated KV instead of enqueueing locally. Every node runs both a
  switchboard-api vhost and a switchboard daemon; each daemon periodically kicks
  the localhost-only `GET /drain` on its node's switchboard-api, which
  reconciles the KV against what has already been applied and writes any new
  jobs into that node's local queue. **This cluster path is covered by
  in-process tests only — it has not been validated against a live multi-node
  cluster.**

### The `/drain` endpoint

`/drain` is a plain-HTTP `GET` the daemon kicks on `--drain-addr` (default
`127.0.0.1:8080`), carrying a `Host:` header (`--drain-host`, which selects the
switchboard-api vhost) and an `X-Drain-Token:` header (the trimmed contents of
`--drain-token-file`, re-read on every kick so it rotates without a restart).
switchboard-api gates it three ways, each failing closed with an identical
`404`:

1. `REMOTE_ADDR` must be `127.0.0.1` — **localhost only**.
2. `X-Drain-Token` must `hash_equals` a line in the vhost's
   `.switchboard/drain_secret`.
3. A missing secret file, missing header, or wrong token → `404`.

### Exactly-once: the hidden marker

Every preview has exactly **one** PR comment, and it is deduplicated by a hidden
HTML-comment marker — not by any shared-state coordination. Before posting, the
daemon lists the PR's comments, scans for the marker string

```
<!-- switchboard-preview -->
```

and updates the existing comment if it finds one, else creates a new one. N
nodes that all reconcile the same preview converge on the same single comment.
There is **no RESP/redis coordination path** — the operator can leave ePHPm's
RESP listener (`[kv.redis_compat]`) off entirely.

The one honest limitation: the find-then-create is not atomic, so two nodes that
both list *before* either has created can momentarily post two comments. It is a
sub-second window and self-heals — the next push finds one by its marker and
updates it in place.

### Preview URL and label

The host is `<label>.<preview-domain>`, e.g.
`ephpm-wordpress-sample-pr-7.preview.ephpm.dev`, served by ePHPm on `:443`
behind the `*.preview.<domain>` wildcard certificate. The **label** is
`<owner>-<repo>-pr-<N>`, sanitized to a single DNS label: lowercased, every
character outside `[a-z0-9-]` replaced with `-`, runs collapsed. If sanitizing
changed nothing and the result is ≤ 63 characters it is used verbatim;
otherwise a 6-hex-character hash of `owner/repo#N` is appended to keep it unique.

Keeping the whole PR identity in a **single** DNS label is deliberate — a
wildcard certificate for `*.preview.<domain>` covers exactly one label, so a
multi-label host like `pr-42.my-blog.preview.<domain>` would need its own
per-hostname certificate. The label is also the vhost directory name and
therefore the [canonical site key](/guides/virtual-hosts/) that selects the
site's database file, KV keyspace, and private temp/session root. The label is
computed once by switchboard-api and travels in the job file; the daemon uses it
verbatim.

### Comment lifecycle

- **On deploy** the comment shows **ready** once the health check passes, or
  **deployed (health check pending)** if it has not, plus a table with the URL,
  detected framework, PHP version, and deploy time, and the footer "Preview
  updates automatically on each push to this PR."
- **On each push** (`synchronize`) the deploy re-runs and the *same* comment is
  updated in place.
- **On PR close** the preview directory under `<sites_dir>/<label>/` is removed
  and the sticky comment is rewritten to "Preview deployment has been torn
  down." Note the per-site **database file is left in place** — teardown does
  not delete it.

### GitHub Deployment status

Alongside the comment the daemon creates a GitHub Deployment
(`environment: preview-pr-<N>`) and sets one status of **`success`** with the
preview URL as its `environment_url`. This is a single terminal status — the
daemon does **not** drive a `queued → in_progress → success/inactive`
lifecycle, and it does not mark the deployment `inactive` on teardown. Unlike
the comment, deployment records are **not** deduplicated across nodes, so a
multi-node cluster may create more than one Deployment for the same PR. Failing
to create the Deployment is non-fatal — the comment is what matters.

## Install it on your repo

### 1. Create and install the GitHub App

Create a GitHub App and subscribe it to the **Pull requests** webhook event
(`pull_request`). switchboard acts on `opened`, `synchronize`, and `reopened`
(deploy) and `closed` (teardown). The App needs the permissions the operations
below require:

| Permission | Access | Why |
|------------|--------|-----|
| Pull requests | Read & write | Post and update the sticky PR comment. |
| Deployments | Read & write | Create the Deployment and its status. |
| Contents | Read | `git fetch refs/pull/<N>/head` from the repo. |
| Metadata | Read | Mandatory for every GitHub App. |

Set the App's **webhook URL** to your switchboard-api endpoint
(`https://switchboard.<your-domain>/webhook`) and set a **webhook secret** — the
same secret switchboard-api verifies against (below). Install the App on the
repos you want previews for. (The reference deployment on `ephpm.dev` runs as
the "ePHPm" App; you create your own for your own domain.)

### 2. Wildcard DNS and TLS

Point wildcard DNS at the node(s):

```
*.preview.<your-domain>.   A   <node IP>
```

and have ePHPm issue **one wildcard certificate** for `*.preview.<your-domain>`
via a DNS-01 ACME challenge — this is exactly the case the
[DNS-01 lane](/guides/tls-acme/#dns-01-challenge-wildcards) exists for. One cert
covers every ephemeral preview subdomain and stays under Let's Encrypt's rate
limit:

```toml
[server.tls]
domains = ["*.preview.example.com", "preview.example.com"]
email = "ops@example.com"
challenge = "dns-01"
dns_provider = "cloudflare"          # or linode / digitalocean / route53 / google
cloudflare_api_token_file = "/run/secrets/cf-token"
```

### 3. Deploy switchboard-api as an ePHPm vhost

switchboard-api is a flat PHP app (front controller at `index.php`, no
`public/`). Run it as a confined vhost under `sites_dir`. Its
`examples/ephpm.toml` is the authoritative starting point; the load-bearing
parts:

```toml
[server]
sites_dir = "/var/www/sites"
sites_domain_suffix = ".example.com"   # dir "switchboard/" serves Host switchboard.example.com

[server.security]
open_basedir = true
# Only these pre-rewrite URIs may execute PHP. allowed_php_paths matches the URI
# BEFORE routing, so every route must be listed — omitting one returns 403.
allowed_php_paths = ["/index.php", "/webhook", "/healthz", "/drain"]
blocked_paths = ["/vendor/*", "/src/*", "/tests/*", "/composer.*", "/worker.php"]

# hidden_files lives under [server.static], NOT [server.security] — putting it
# in the wrong section is silently ignored (SecurityConfig does not reject
# unknown fields). "deny" is already the default; it is spelled out here so the
# intent is explicit in the deployed config.
[server.static]
hidden_files = "deny"
# Also the default. ePHPm refuses to serve `ephpm.yaml` / `.yml` / `.json`
# from any vhost, so an app whose manifest declares `docroot: "."` cannot
# publish its own build commands and seed sequence. This is a second layer:
# switchboard already moves the manifest out of the served tree at deploy
# time. Neither replaces the other — see the note under "Add ephpm.yaml".
deploy_manifests = "deny"
```

Set the webhook secret in the vhost's `.switchboard/webhook_secret` (one secret
per line; multiple lines are accepted so you can rotate). A file is preferred
over the `SWITCHBOARD_WEBHOOK_SECRET` env var, because env is shared across every
vhost in the process. If no secret is configured the webhook fails closed with a
`500`. In cluster mode also set the drain secret in `.switchboard/drain_secret`
(matching the daemon's `--drain-token-file`).

### 4. Deploy the switchboard daemon

Run the Rust daemon on each node. It is configured entirely by flags, each with
a `SWITCHBOARD_*` env fallback (designed for a systemd `EnvironmentFile=`). The
repo ships **no** `.service` file — the unit below is illustrative:

```ini
# /etc/systemd/system/switchboard.service  (illustrative — no unit ships in-repo)
[Unit]
Description=ePHPm switchboard preview daemon
After=network-online.target

[Service]
EnvironmentFile=/etc/switchboard/env
ExecStart=/usr/local/bin/switchboard \
  --state-dir /var/lib/switchboard \
  --sites-dir /var/www/sites \
  --preview-domain preview.example.com \
  --app-id <APP_ID> \
  --app-key /etc/switchboard/app-private-key.pem \
  --drain-host switchboard.example.com \
  --drain-token-file /var/www/sites/switchboard/.switchboard/drain_secret
Restart=always

[Install]
WantedBy=multi-user.target
```

Flags worth calling out:

- `--state-dir` is **required** (holds the queue and applied-markers).
- `--app-id` and `--app-key` (the PEM path — not `--app-key-file`) must be given
  **together**; the daemon mints installation tokens from them.
- `--drain-host` and `--drain-token-file` are **required whenever the drain kick
  is enabled** (`--drain-interval-secs` non-zero, the default). For a
  single-node install, set `--drain-interval-secs 0` and omit both — the API
  enqueues directly.
- **There are no `--kv-*` flags.** The daemon needs no KV configuration; cluster
  fan-out reaches it as queue files written by `/drain`.

### 5. Add `ephpm.yaml` to your app repo

An app repo describes its preview build in an `ephpm.yaml` at its root. The
schema is owned by switchboard (`src/manifest.rs`); **ePHPm never reads it** —
see [Virtual Hosts](/guides/virtual-hosts/) for why the runtime refuses to route
on a file inside a tenant's checkout. `build:` and `seed:` steps run as `sh -c`
children of the daemon; `seed:` additionally gets `$PREVIEW_URL`, `$PREVIEW_HOST`,
and `$PR` in its environment. A representative manifest (the shape used by
`ephpm/wordpress-sample`):

```yaml
version: 1                 # required; only version 1 is understood
php: "8.5"
docroot: "."
build:                     # run in the checkout BEFORE the site goes live
  - "./assemble.sh ."
  - "composer install --no-dev --optimize-autoloader --no-interaction"
services:
  database: "turso"        # "turso" | false
  kv: true
  websocket: true
seed:                      # run AFTER the site serves and its DB exists
  - "wp core install --url=$PREVIEW_URL --title=\"Preview\" --admin_user=admin --admin_email=preview@example.com --skip-email"
env:
  WP_ENVIRONMENT_TYPE: "staging"
health: "/"                # polled for a 200 before the deploy reports ready
ini:
  memory_limit: "256M"     # advisory — surfaced, not enforced in v1
```

A repo with no manifest is not rejected: switchboard synthesizes one from the
detected framework (WordPress, Laravel, Symfony, Drupal, or generic).

**The manifest is not web content.** With `docroot: "."` it sits directly under
the served document root, and it names build commands, enabled services and the
seed sequence. Two independent layers keep it off the wire, and neither
subsumes the other:

- switchboard moves the manifest into a dot-prefixed directory before the
  atomic swap, so it is *not in a served directory* — a guarantee that holds
  even if an operator sets `hidden_files = "allow"`.
- ePHPm refuses any request naming `ephpm.yaml`, `ephpm.yml` or `ephpm.json`
  (`[server.static] deploy_manifests`, default `"deny"`), so it is *not
  servable* — a guarantee that holds however the site was laid down, including
  a hand-provisioned vhost or a checkout copied into `sites_dir` by something
  other than switchboard.

## Limitations to plan around

- **Cluster mode is not live-validated.** The KV fan-out and `/drain`
  reconciliation are covered by in-process tests only.
- **Rare duplicate comment.** The sub-second find-then-create race can post two
  comments momentarily; the next push self-heals to one.
- **Deployment records are not deduped across nodes.** One PR can produce
  multiple GitHub Deployments in a cluster (the *comment* is always single).
- **The per-site database is not torn down** on PR close — only the checkout
  directory is removed. Abandoned previews are not reaped automatically.
- **`build:`/`seed:` run with the daemon's privileges** as plain `sh -c`
  children (no container/namespace) — treat the whole system as trusted-tenant,
  and see the isolation notes in [Virtual Hosts](/guides/virtual-hosts/).

## See also

- [Virtual Hosts](/guides/virtual-hosts/) — the multi-tenant runtime the bot writes into
- [TLS / ACME → DNS-01 wildcards](/guides/tls-acme/#dns-01-challenge-wildcards)
- [Preview Deployments roadmap](/roadmap/preview/) — what is still design, not shipped
