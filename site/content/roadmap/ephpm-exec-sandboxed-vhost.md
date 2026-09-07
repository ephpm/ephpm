# `ephpm exec` — Run a Command Inside a Virtual Host's Tenant Sandbox

> **Status: design spike + working proof-of-concept.** The containment core —
> uid drop + Landlock filesystem scope + the uid-keyed egress firewall — is
> **implemented and confirmed on a live preview node** (see
> [PoC status](#poc-status--implemented-and-confirmed-on-a-preview-node-linux)).
> A `Commands::Exec` subcommand now exists in `crates/ephpm/src/main.rs`
> (`crates/ephpm/src/exec_sandbox.rs`). Parts still described in the future tense
> — the per-site DB bind (#471), per-site-clustered owner-refusal, `setrlimit`
> caps, and the switchboard integration — are **planned, not present**, and are
> called out where they appear. Treat the sandboxed form as **Linux-only** for
> the reasons in [Windows](#windows-the-honest-story).

## Why this exists — three things it unifies

1. **A build/deploy sandbox for `ephpm/switchboard`.** Switchboard runs manifest
   `build:` / `seed:` steps as **root, unsandboxed** today — a confirmed root
   RCE surface on the live cluster. If those steps ran as
   `ephpm exec --site <key> -- bash build.sh`, they would inherit ePHPm's
   existing per-vhost isolation instead of switchboard reimplementing (or
   omitting) it.
2. **Issue #471.** `ephpm php` — and therefore wp-cli, `artisan migrate`,
   Doctrine console, any framework CLI — cannot reach a per-site database,
   because a per-site DB handle only exists inside an HTTP request where
   `Router::resolve_site` bound the site. `ephpm exec --site` is the general
   form of the #471 fix: bind the site, open its DB session, then run the
   command. See [The #471 relationship](#the-471-relationship).
3. **Operator cron / queue workers / migrations.** Anyone running an app on
   ePHPm wants to run periodic tasks in the tenant's sandbox without standing up
   a second, differently-configured PHP.

The through-line: ePHPm *already computes* a tenant's full isolation profile
once per request. `ephpm exec` is the question "can that profile be
reconstructed for a one-shot command outside a request?" — and, mostly, the
answer is yes, by reusing code that already exists.

## What the per-request sandbox is made of (verified)

Everything below is what a request already gets. The design reuses each piece;
the file references are the current implementations.

| Layer | Where it lives today | Keyed on |
|-------|----------------------|----------|
| Per-vhost `open_basedir` | `router.rs::vhost_open_basedir_value` (docroot + private state root); applied as a per-request INI directive | filesystem paths |
| Private temp/session root | `router.rs::vhost_state_root` / `ensure_vhost_private_dirs` (`<tmpdir>/ephpm-vhosts/<label>-<digest>`, `0700`) | derived from docroot |
| `disable_functions` hardening | `main.rs::compose_disable_functions` + `HARDENING_*` constants | process-global php.ini |
| Per-site DB session | `db_bridge.rs::{set_resolver, set_current_site}` + `SiteBackendResolver` | site key (per-thread) |
| Per-site KV keyspace | `Router::site_identities` → `ephpm_kv` site store | site key |
| Single non-root uid | `privdrop.rs::drop_privileges` (`setgroups`+`setgid`+`setuid`, irreversible) | process-wide |
| Egress firewall | host nftables `inet ephpm_egress`, **not ePHPm code** | `skuid` (uid 997) |
| Per-vhost eBPF net policy | `tenant_ebpf.rs` + `bpf/vhostnet.bpf.c` | **TID** (per-thread) |
| Resource limits | `[php] max_execution_time` (Linux per-thread timer); `[server.timeouts] request` | per-request |

The canonical tenant identity is derived **once**, by `Router::resolve_site`
returning `ResolvedSite { key, document_root, .. }`, and everything per-tenant
is derived from that `key` via `Router::site_identities` — never re-derived from
the `Host` header (issues #290/#291, pinned by `router::tests::site_key_agreement`).
`ephpm exec` must derive the same `key` the same way and feed the same
`site_identities`, or it becomes a *second*, divergent derivation of tenant
identity — exactly the class of bug that invariant exists to prevent.

## The key finding: uid-drop makes the existing firewall do the containment

The live preview nodes (verified read-only on `198.58.124.223`, 2026-09-07)
carry an nftables table whose `output` chain is uid-keyed:

```
table inet ephpm_egress {
    chain output {
        type filter hook output priority filter; policy accept;
        meta skuid != 997 accept          # everything NOT ephpm-web is unfiltered
        ct state established,related accept
        ip daddr { 1.1.1.1, 8.8.8.8 } udp/tcp dport 53 accept   # public DNS only
        ip daddr 192.168.128.0/17 accept  # cluster range
        ip daddr 127.0.0.0/8 drop         # loopback denied
        ip6 daddr ::1 drop
        ip daddr { 10/8, 169.254/16, 172.16/12, 192.168/16 } drop   # LAN denied
        ...
        meta l4proto tcp accept           # public internet TCP allowed
        counter ... drop
    }
}
```

`getent passwd 997` → `ephpm-web:x:997:989:…:/usr/sbin/nologin`. So the rule
filters **exactly uid 997 = `ephpm-web`** and nothing else. The consequence is
the whole design's leverage:

- A command that runs **as uid 997** is automatically egress-locked — no
  loopback, no LAN, public internet + cluster range + public DNS only — **with
  zero extra ePHPm code**. The firewall already exists and already keys on
  `skuid`.
- Switchboard's current build step runs as **root**, which hits
  `meta skuid != 997 accept` on the first rule and bypasses the entire table.
  That is the RCE blast radius in one line.

So `ephpm exec` should **drop to the tenant uid (997)** and let the host
firewall contain it. The drop primitive already exists —
`privdrop.rs::drop_privileges` does `setgroups` → `setgid` → `setuid`, verifies
the drop took, and proves root can't be regained (`seteuid(0)` must fail). The
one semantic difference: the server drops the *whole long-running process* once
at startup; `ephpm exec` drops a *one-shot* process (or its forked child)
before `execve`, which is if anything simpler.

### The loopback-drop tension (important, and not hand-waved)

The same rule that contains a hostile build script — `127.0.0.0/8 drop` for uid
997 — **also blocks a legitimate one from reaching the local MySQL wire
listener on `127.0.0.1:3306`.** This is the sharp edge of the design:

- **PHP commands (wp-cli, `artisan migrate`) sidestep it entirely.** The #471
  DB path is the *in-process* `ephpm_db_*` bridge — a Rust function call
  (`db_bridge.rs`), not a socket. It never touches loopback, so the firewall is
  irrelevant to it. This is the case that matters most, and it works.
- **Arbitrary child commands that expect a TCP database do not.** A `bash
  build.sh` that runs `mysql -h 127.0.0.1` as uid 997 will have its connection
  dropped by the firewall. That is *correct* containment, but it means "run any
  build script unchanged" is not a promise `ephpm exec` can make for scripts
  that reach a DB over loopback TCP. Such scripts must route DB work through a
  child `ephpm php` / wp-cli invocation (in-process bridge) rather than a raw
  socket.

Stating this plainly is the point: `ephpm exec` gives PHP tenant CLIs a working
database (#471) and gives arbitrary commands a correctly-jailed egress — but the
intersection ("an arbitrary non-PHP command that also needs the tenant DB over
TCP") is deliberately closed, because opening loopback for uid 997 would reopen
the containment the firewall is there to provide.

## Sandbox layers `ephpm exec` would apply

Applied to the command (or its forked child), in this order:

1. **Resolve the site → `ResolvedSite`/`SiteIdentities`.** Reuse
   `Router::resolve_site` semantics against the loaded config so the key,
   document root, and identities are byte-identical to the HTTP path. Refuse if
   the key resolves to `None` (no such site) — same fail-closed as a request
   that matched no vhost.
2. **Filesystem scope.** Two options, both Linux-only:
   - **Landlock ruleset** (kernel ≥ 5.13; preview nodes are 6.12 with
     `landlock` present in `/sys/kernel/security/lsm`). Grant read/write/exec
     only under the site's `document_root` and its `vhost_state_root`, plus the
     minimal read set a program needs (the interpreter, shared libs). Crucially
     **do not** grant `/etc/ephpm` — where `ephpm.toml` holds the cluster master
     secret (mode 640, `ephpm-web`-*readable*). Landlock is the tighter fit
     because it needs no mounts and no `CAP_SYS_ADMIN`, and it composes with the
     uid drop.
   - **Mount namespace** (`unshare(CLONE_NEWNS)` + bind mounts) as the fallback
     on kernels without Landlock ABI 3+, at the cost of needing more privilege
     to set up. Landlock is the recommended primary.

   Note the relationship to `open_basedir`: for **PHP** commands, `open_basedir`
   is still set (via the generated php.ini, exactly as a request does) and is
   the in-interpreter boundary; Landlock is the *kernel* backstop that also
   covers non-PHP children and anything PHP shells out to. For **non-PHP**
   commands, Landlock is the only filesystem boundary — there is no
   `open_basedir` for `bash`.
3. **uid/gid drop to the tenant uid (997 / `ephpm-web`).** Reuse
   `privdrop`-style `setgroups`+`setgid`+`setuid` with the same fail-closed
   verification. This is what arms the `ephpm_egress` firewall
   (§ [key finding](#the-key-finding-uid-drop-makes-the-existing-firewall-do-the-containment)).
4. **Resource limits.** `setrlimit` before `execve`: `RLIMIT_AS` /
   `RLIMIT_DATA` from a memory ceiling (mirror `[php] memory_limit`),
   `RLIMIT_CPU` and/or a wall-clock `--timeout` enforced by the parent (a
   `SIGKILL` after N seconds — the same "enforce timeout above the interpreter"
   posture ePHPm already takes on Windows where per-thread timers are absent).
   Optionally place the child in a transient cgroup for a memory/pids cap; a
   cgroup is *not required* for the core design.
5. **Per-site DB + KV bind (PHP path only).** Register the same
   `SiteBackendResolver` the server builds, `set_current_site(key)`, and set the
   KV site store — see [The #471 relationship](#the-471-relationship).

Layers 2–4 are pure Rust + libc/Landlock syscalls with **no PHP linkage**, so
they are testable independently of the embedded runtime.

### eBPF per-vhost policy: out of scope for `exec` (verified why)

The shipped `[server.tenant_network] ebpf_policy` attaches `cgroup/bind4+6` and
`cgroup/connect4+6` programs to ePHPm's **own cgroup**, and they act only on
threads whose **TID** is present in the `tag` BPF hash map
(`vhostnet.bpf.c`: `tag SEC(".maps")` is `BPF_MAP_TYPE_HASH` keyed on the
namespace-resolved TID; `lookup_tag` returns null for untagged threads, which
pass through). ePHPm writes a thread's tag on the per-request dispatch path
(`TagGuard`), for threads it owns.

An `ephpm exec` child is a **separate process** that `execve`s arbitrary code
and spawns threads ePHPm never sees, so ePHPm cannot tag its TIDs — and even if
the child inherits ePHPm's cgroup, untagged threads are un-governed by these
programs. Making the eBPF policy cover an exec child would require a different
mechanism (a cgroup-scoped default-deny keyed on cgroup membership, not TID),
which is a redesign of `tenant_ebpf`, not a reuse of it. **The uid-keyed
`ephpm_egress` nftables table already provides equivalent egress containment
for the child, keyed on `skuid` rather than TID** — which is exactly why it,
not eBPF, is the right tool here. So: eBPF per-vhost policy is **out of scope**
for the exec child; the nftables uid rule is the containment.

## The authorization model

`ephpm exec --site X` is a **privileged operation by construction**: being able
to name any site and receive its database, its filesystem, and its KV keyspace
is precisely the boundary multi-tenancy protects. The design must not make it an
*easier* cross-tenant path than the wire-auth + site-key invariant already
guard. The proposal:

- **Authorization is filesystem access to the ePHPm config, not a new auth
  surface.** `ephpm exec` requires `--config <path>` (or the default
  `/etc/ephpm/ephpm.toml`) and reads it. Deriving a site's DB path, its
  `pdo_mysql`/KV credential (`HMAC-SHA256(master_secret, site_key)`), and its
  document root all require the config and the per-process master secret in it.
  Whoever can read that file can already impersonate any tenant to the running
  server; `ephpm exec` grants nothing they didn't already have. So the gate is
  the config file's own permissions (`640 root:ephpm-web` on the preview
  nodes), and in practice the caller must be **root / an operator with sudo**.
- **The launching process needs privilege; the command must not keep it.**
  `ephpm exec` starts privileged (to bind the site, open the DB dir, set up
  Landlock, and `setuid`), then drops to 997 and Landlock-scopes **away from
  `/etc/ephpm`** before `execve`. So even though `ephpm-web` can *read* the
  config in the normal filesystem, the exec'd command cannot — Landlock removes
  the path from its view. This is what lets switchboard hand an untrusted
  `build.sh` to `ephpm exec` without handing it the cluster secret.
- **No network-reachable exec.** This is deliberately *not* an admin-API verb in
  this design (see the server-running question). It is a local CLI gated by
  local privilege. An admin API that exposes exec remotely would need its own
  authn/authz story and is out of scope.

The honest risk to name: `ephpm exec` is a legitimate cross-tenant tool for
whoever holds config access. That is the same trust level as "can edit
`ephpm.toml` and restart the server," so it does not lower the bar — but it does
make cross-tenant access a *single command* rather than a config edit + restart,
so it should log every invocation (site key, command, uid) at INFO, the way
`privdrop` logs the drop.

## Server running or not?

A running `ephpm serve` is a **separate process**; a CLI `ephpm exec` cannot
reach the daemon's in-memory backend registry, KV store, or cluster membership
by sharing memory. So there are only two real shapes, and they differ by mode:

- **Single-node per-site (`is_per_site_sqlite`) → open the DB file directly, in
  the exec process.** `ephpm exec` builds its own `SiteBackends`-style resolver
  against `[db.sqlite].dir/<key>.db` and binds the bridge. Honest caveat: if the
  server is *also* running and serving that site, two processes now hold the
  same Turso file. Turso/SQLite use file locking, and WAL permits concurrent
  readers, but two concurrent **writers** across processes is the danger zone —
  so the recommendation is: run schema-changing `exec` commands (migrations,
  `wp core install`) against an idle site, or accept single-writer discipline.
  This is the same concurrency story `ephpm php` already has; `exec` does not
  make it worse, it just finally makes the DB reachable (#471).
- **Per-site clustered (`is_per_site_clustered`) → refuse on a non-owner, or
  require routing through the cluster.** Writes are owner-served over
  `sql/<site>` (`sql_forward.rs`), and a write committed to a non-owner replica
  is never captured into CDC — it diverges silently (the exact bug the wire
  route was fixed for). An `ephpm exec` process that opened the site DB locally
  on a non-owner would reintroduce that bug. The safe options are: (a) **refuse
  unless this node is the site's HRW owner** (`sqlite_election.rs::hrw_owner`
  over live membership), with a clear message telling the operator which node
  owns it; or (b) forward statements to the owner over the cluster channel —
  which requires a live cluster membership the CLI would have to *join*, a heavy
  and racy thing for a one-shot process. **Recommendation for a first cut:
  option (a), refuse on non-owner.** It is correct, cheap, and never writes
  where writes don't replicate. Note `/_ephpm/primary` returns 200 on every
  node in this mode, so "which node" must be answered by HRW ownership, not the
  primary endpoint.

So the chosen answer: **`ephpm exec` is a standalone process that opens
resources directly (like `ephpm php` does), not an IPC call into the daemon.**
It does not *require* the server to be running. In single-node mode it accepts
the operator owns write concurrency; in per-site-clustered mode it refuses
unless invoked on the site's current HRW owner.

## Windows: the honest story

Landlock, seccomp, nftables, `setuid`, and cgroups are all Linux mechanisms.
ePHPm already documents Windows as lacking `crash_guard.c` and per-thread
execution timers, and `privdrop::drop_privileges` is already a warn-only stub on
non-Unix. Consistent with that: **the sandboxed form of `ephpm exec` is
Linux-only.** On Windows, `ephpm exec` should either:

- **refuse** with a clear "sandboxed exec is Linux-only" error (the safe
  default — it never pretends to isolate when it can't), or
- offer an explicit, loudly-labelled `--no-sandbox` mode that only performs the
  site DB bind (the #471 mechanism, which *is* cross-platform — `db_bridge` and
  the Turso engine both work on Windows) and runs the command with **no** uid
  drop, no Landlock, no egress lock.

The `--no-sandbox` DB-bind-only path is genuinely useful on Windows for local
development (run `artisan migrate` against a per-site Turso file) and is honest
as long as it never claims isolation it doesn't provide. The default on Windows
should be refuse; `--no-sandbox` is opt-in.

## The #471 relationship

`ephpm exec --site` is the general form of the #471 fix. #471 is specifically
"`ephpm php` can't reach a per-site database"; `exec` solves it by supplying the
missing **site session** the CLI lacks:

- The bridge is already compiled in and callable from `ephpm php` (issue #471
  confirms `function_exists("ephpm_db_query")` is `true`), but a query returns
  *"no embedded database is active"* because no request ever ran
  `set_resolver` + `set_current_site`.
- `ephpm exec` performs that bind before running the command: build the same
  per-site resolver the server builds (`db_bridge::set_resolver` with a
  `SiteBackendResolver` over `[db.sqlite].dir`), then `set_current_site(key)`.
- **The `block_on` invariant holds for a CLI.** `db_bridge` drives async
  `Session::query` via a pinned `tokio::runtime::Handle` and `block_on`, which
  is legal *only* because the caller is a PHP execution OS thread, never an
  async task (module docs, `Async boundary`). An `ephpm exec` process running
  the embedded PHP on a normal (non-async-task) thread with a multi-thread
  runtime handle satisfies exactly that invariant — the same reason `ephpm
  serve`'s per-request pool threads may block. So the bind works outside the
  HTTP request path with no new unsafe assumptions.
- **Tenant screening is preserved.** `ATTACH`/`DETACH`/`VACUUM`/path-`PRAGMA`
  are rejected on the tenant path in all modes (`db_bridge::screen_sql` and
  litewire's session screen). `exec` must route through the *same* screened
  session, not a raw backend, so a CLI can't become the way around the rejection
  #471 explicitly warns about.

A narrower fix for #471 alone would be `ephpm php --config <path> --site <key>`
(no sandbox layers) — and that is a reasonable staged first deliverable. `exec`
is the superset that adds the uid drop + filesystem scope + arbitrary (non-PHP)
commands.

## What still lives in switchboard

If `ephpm exec --site` existed, switchboard would **stop** owning:

- the ad-hoc, root, unsandboxed execution of `build:` / `seed:` steps — replaced
  by `ephpm exec --site <key> -- bash build.sh`, which inherits the uid drop,
  Landlock scope, egress lock, and (for PHP steps) the site DB bind.

Switchboard would **still** own:

- **Fetching the repo / manifest** and materialising it into the site's document
  root before any build runs. `ephpm exec` runs a command in an *existing*
  site's sandbox; it does not clone code.
- **Writing the per-site override file** (`auto_prepend_file`, etc.) — a
  privileged config action outside any single tenant's sandbox.
- **Reaching `loopback:8080`** (the running server's admin/health surface) — the
  switchboard daemon must run as a **non-997** uid precisely because the
  `ephpm_egress` rule drops loopback for 997. Switchboard's own control-plane
  traffic is exactly the "not ephpm-web, so unfiltered" case the firewall's
  first rule allows.
- **Orchestration**: deciding *when* to build, sequencing steps, reporting
  status, retries. `ephpm exec` is one primitive it calls, not the orchestrator.

The line: switchboard stays the privileged control plane (fetch, configure,
sequence, talk to the daemon); `ephpm exec` becomes the sandboxed execution
primitive it delegates the actual tenant-scoped work to.

## PoC status — implemented and confirmed on a preview node (Linux)

**A working proof-of-concept ships alongside this doc** and was confirmed on a
live preview node (Debian 13, kernel 6.12, glibc 2.41). It proves the security
property this design exists for — *uid drop + Landlock filesystem scope + the
egress firewall*, no DB bind — and closes the exact root-RCE hole the switchboard
build step exposes.

What shipped, all of it stub-mode-clean (no PHP SDK needed) and
`clippy`-pedantic + `-D warnings` clean:

- `Commands::Exec { config, site, timeout, no_sandbox, command }` in
  `crates/ephpm/src/main.rs` — `--` captures the trailing command (clap
  `trailing_var_arg` + `allow_hyphen_values`), mirroring `Php { args }`.
- `crates/ephpm/src/exec_sandbox.rs` — a `#[cfg(target_os = "linux")]` impl and a
  `#[cfg(not)]` refuse-stub, matching the `privdrop` shape. Layer order:
  resolve site → `PR_SET_NO_NEW_PRIVS` → Landlock (ABI v1, best-effort, refuse if
  `NotEnforced`) → irreversible uid drop → `chdir` → `execvp`. `--timeout` arms
  `alarm(2)` (survives `execve`; default `SIGALRM` terminates).
- `ephpm_server::router::resolve_sandbox_site(config, key)` — the site derivation
  reused verbatim from the request path (`resolve_site` → container +
  `vhost_state_root`), so the exec sandbox is not a *second*, divergent
  derivation of tenant identity (issues #290/#291). Pinned by
  `router::tests::resolve_sandbox_site_matches_request_derivation_and_fails_closed`.
- `privdrop::drop_to_user(uid, gid)` — the `setgroups`/`setgid`/`setuid` +
  fail-closed verification, **factored out** of `drop_privileges` so the server
  drop and the exec drop share one audited implementation; `resolve_user` /
  `resolve_group` are now `pub` for the same reuse.
- `landlock = "0.4"` added Linux-only (`libc` was already a dep).

### The confirmed before/after (identical benign probe, run twice)

The site config used was a throwaway (`sites_dir` under `/tmp`), but the probe
read the node's **real** `/etc/ephpm/ephpm.toml` — the file that holds the
cluster gossip secret, owned `640 ephpm-web:ephpm-web` (uid 997).

| Probe check | Baseline: run as **root** | Via `ephpm exec --site` | Closed by |
|---|---|---|---|
| `id` | `uid=0(root)` | `uid=997(ephpm-web) gid=989` | **uid drop** (`setuid`, irreversible) |
| `whoami` | `root` | `ephpm-web` | uid drop |
| write `/root/.sb-poc` | YES | **NO** | Landlock (path not granted) + DAC |
| read `/etc/shadow` (`open`) | YES | **NO** | DAC (997 ∉ `shadow`) + Landlock |
| read `/etc/ephpm/ephpm.toml` (`open`) | YES | **NO** | **Landlock only** — see below |
| `test -r /etc/ephpm/ephpm.toml` (`access(2)`) | YES | **YES** | *nothing* — the tell |

**The sharpest evidence is the last two rows.** `/etc/ephpm/ephpm.toml` is owned
by uid 997, so after the drop DAC *still permits* the read — `test -r`
(`access(2)`, which Landlock does not intercept) returns **YES** exactly as it
did for root. But the actual `open()` returns **NO**. The only layer that can
account for that gap is **Landlock**: the uid drop alone does not close the
secret read, Landlock does. That is the whole design's leverage made visible in
one probe. Landlock reported `FullyEnforced` at ABI v1 on the 6.12 kernel — no
partial negotiation.

### Firewall layer confirmed too

The uid-997 drop arms the host's `inet ephpm_egress` nftables table with zero
extra ePHPm code (it keys on `skuid`, `meta skuid != 997 accept`). Demonstrated
on the node: **root** connects to `127.0.0.1:22` (bypasses the table); the
**tenant (997)** hangs on the same loopback connect and is `alarm`-killed by
`--timeout` (`127.0.0.0/8 drop` for 997) — while the tenant's connect to the
*allowed* public DNS `1.1.1.1:53` **succeeds**. So it is specifically the
uid-keyed egress policy, not a blanket network cut, and it arms automatically.

### Honest limits / caveats

- **Landlock ABI floor.** The impl pins ABI **v1** best-effort and refuses if the
  kernel reports `NotEnforced` (fail-closed). It has not been exercised below
  ABI v1 (a mount-namespace fallback is a follow-up, not built).
- **`access(2)` is Landlock-blind.** As the table shows, a tenant can still learn
  a path is DAC-readable via `test -r`; it just cannot `open` it. Code that keys
  a decision on `access(2)` rather than a real open would be misled — an
  in-kernel property of Landlock, worth stating.
- **`--no-sandbox` on Linux removes all containment** (loudly warned) and exists
  only for the future Windows DB-bind path; on non-Linux the subcommand refuses.
- **`setrlimit` / cgroup caps not built** — only the `alarm(2)` wall-clock
  timeout is. Memory/CPU/pids ceilings are follow-up.
- The DB bind (`TODO(#471)`) and per-site-clustered owner-refusal are **not**
  wired — out of scope for proving containment, marked `TODO` in the module.

### Intended switchboard call site (follow-up, not in this PR)

Switchboard runs manifest `build:` / `seed:` steps as **root** today — the RCE
surface. The one-line change is to run each step as, instead of `bash -c "$step"`:

```text
ephpm exec --config /etc/ephpm/ephpm.toml --site <key> -- bash -c "$step"
```

which inherits the uid drop, Landlock scope, and egress lock. Rewriting
switchboard's deployer is a separate change in that repo, deliberately not made
here.

## Open questions for review

- ~~**Factor `privdrop` credential-drop into a shared module?**~~ **Done in the
  PoC** — `privdrop::drop_to_user(uid, gid)` is the one audited
  `setgroups`/`setgid`/`setuid` + fail-closed copy, called by both the server
  drop and `ephpm exec`.
- **Landlock ABI floor.** Preview nodes are 6.12; the PoC pins ABI **v1**
  best-effort and refuses on `NotEnforced` (fail-closed, confirmed
  `FullyEnforced` on the node). Open: raise the pinned ABI for finer rights, or
  add a mount-namespace fallback below v1? Refusing is simplest.
- **cgroup placement.** Is a transient per-exec cgroup (memory/pids cap) worth
  it for a first cut, or are `setrlimit` + wall-clock timeout sufficient? This
  spike says rlimits are sufficient to start.
- **Staged delivery.** Ship `ephpm php --site`/DB-bind (solves #471, cross-
  platform) first, then the Linux sandbox layers as `ephpm exec`? The #471 pain
  is immediate and the DB bind is the cross-platform half.
