+++
title = "ephpm exec"
weight = 7
+++

Run a command inside a virtual host's tenant sandbox. `ephpm exec` resolves
`--site <key>` to that vhost's directory, confines the command's filesystem view
to it with [Landlock](https://docs.kernel.org/userspace-api/landlock.html),
drops to the unprivileged tenant user, and then `exec`s the command. Use it to
run build, seed, or maintenance steps for one tenant (`composer install`, a
shell script, `wp`) without giving that step root or a view of other tenants'
files.

> **Linux only, and an early implementation.** On every other platform the
> subcommand refuses with an error, with or without `--no-sandbox`. There are
> no memory, CPU, or process-count limits yet. Only the wall-clock `--timeout`
> exists. The design and its open items are in the
> [roadmap page](/roadmap/ephpm-exec-sandboxed-vhost/).

## Synopsis

```bash
ephpm exec [-c FILE] -s <KEY> [-t SECS] [--no-sandbox] -- <COMMAND> [ARGS...]
```

```bash
# Run a tenant's build step as the tenant, confined to its directory
sudo ephpm exec --site shop -- composer install --no-dev

# Arbitrary shell, with a 10-minute wall-clock limit
sudo ephpm exec --config /etc/ephpm/ephpm.toml --site shop --timeout 600 -- bash -c './build.sh'
```

Everything after `--` is the command and its arguments. The command is looked up
on `PATH` (`execvp`).

## Flags

| Flag | Long | Default | Purpose |
|------|------|---------|---------|
| `-c` | `--config <FILE>` | `/etc/ephpm/ephpm.toml` | The ePHPm config. It supplies `[server] sites_dir`, any site overrides, and `[server] run_as_user` / `run_as_group`. `EPHPM_*` environment overrides apply, as for `serve`. |
| `-s` | `--site <KEY>` | (required) | The site key: the vhost directory name under `[server] sites_dir`. |
| `-t` | `--timeout <SECS>` | `0` (no limit) | Wall-clock limit in seconds. See [Timeout](#timeout). |
| | `--no-sandbox` | off | Skip the sandbox entirely. See [`--no-sandbox`](#--no-sandbox). |

**`ephpm exec --help` does not print help.** The subcommand disables clap's help
flag so it can capture the command verbatim. Without `--site`, `--help` is an
error about the missing `--site`. After `--site`, `--help` is taken as the
command to run. Use `ephpm help exec` instead.

## Requirements

- **Linux.** Other platforms always get an error.
- **Start as root.** Without `--no-sandbox`, a non-root caller is refused
  (`ephpm exec must start as root ...`): the command has to create the
  tenant's state directory and `setuid` to the tenant.
- **Multi-tenant mode.** `[server] sites_dir` must be set, and
  `<sites_dir>/<key>` must be an existing directory. A malformed key, a
  single-site config, or an unknown site is an error. Nothing falls back to a
  wider directory.
- **The tenant user must exist.** It is `[server] run_as_user` (and
  `run_as_group`) from the config, or the user `ephpm-web` when that is unset.
  ePHPm does not create the user.
- **A kernel that enforces Landlock.** If the kernel does not enforce Landlock
  at all, `ephpm exec` refuses to run.

## What it does

In order:

1. Loads the config and resolves the site with the same code a request uses,
   so the site key, the site directory, the web root (a `public/`-style
   override if the site has one), and the private state root all match what
   the server derives for that vhost.
2. Creates the tenant's private state root (`$TMPDIR/ephpm-vhosts/<label>-<hash>/`
   with `tmp/` and `sessions/`, mode `0700`, owned by the tenant) if it does
   not exist. It refuses if any component of that path is a symlink.
3. Sets `PR_SET_NO_NEW_PRIVS`, so setuid binaries and file capabilities in
   the command cannot regain privilege.
4. Applies a Landlock ruleset (ABI v1). Everything not listed here is denied:

   | Access | Paths |
   |--------|-------|
   | read/write | the site directory, the site's state root, `/dev/null`, `/dev/zero`, `/dev/full` |
   | read + execute | `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/opt` |
   | read-only | `/etc/passwd`, `/etc/group`, `/etc/nsswitch.conf`, `/etc/ld.so.cache`, `/etc/ld.so.preload`, `/etc/localtime`, `/etc/resolv.conf`, `/etc/hosts`, `/dev/urandom`, `/dev/random`, `/proc/self`, and the public CA certificate directories `/etc/ssl/certs`, `/etc/pki/tls/certs`, `/etc/pki/ca-trust` |

   So `/etc/ephpm` (the config and its secrets), `/etc/ssl/private`, `/root`,
   `/etc/shadow`, other tenants' directories, and other processes' `/proc`
   entries are not reachable. Paths that don't exist on the host are skipped.
   If the kernel enforces only part of the ruleset, a warning is logged and the
   command still runs.
5. Arms the `--timeout` alarm, if one is set.
6. Drops to the tenant uid/gid (`setgroups`, `setgid`, `setuid`, then checks
   that the drop took). The drop cannot be undone.
7. Changes into the site's web root. If that fails, it logs a warning and
   stays in the current directory.
8. Replaces itself with the command.

Each run logs the site, uid/gid, timeout, and command at INFO before the
sandbox is entered.

### What it does not do

- **It does not filter the network.** Landlock ABI v1 covers the filesystem
  only. ePHPm itself adds no egress rules. A host can filter by the tenant's uid
  (the ePHPm preview fleet does this with an operator-managed nftables table),
  but that is your host's configuration, not part of this command.
- **It does not clean the environment.** The command inherits `ephpm exec`'s
  environment unchanged. Don't run it with secrets in the environment
  (`EPHPM_KV__SECRET`, cloud credentials, tokens) unless the command is meant to
  see them.
- **It does not hide paths readable through the read+execute grants.** Anything
  under `/usr`, `/opt`, `/lib`, `/bin` or `/sbin` is readable inside the
  sandbox. Don't keep the ePHPm config or other secrets there.
- **It does not bind the embedded database.** See
  [Database access](#database-access).
- **No resource limits** beyond `--timeout`.

`access(2)` (for example `test -r`) is not checked by Landlock. A sandboxed
command can still learn that a file is readable by its uid, even though opening
it fails.

## Timeout

`--timeout N` arms `alarm(2)` before the `exec`. The alarm survives `exec`, so
after `N` seconds the command receives `SIGALRM` and, unless it handles that
signal, is terminated (a shell reports exit status `142`). Only the command's
own process gets the signal. Processes it started are not signalled. For example,
the children of `bash -c '...'` keep running. Values above `u32::MAX` are
clamped.

## `--no-sandbox`

`--no-sandbox` skips steps 2, 3, 4 and 6 above. The command runs with **the
caller's full privileges** (root, under `sudo`): no Landlock scope, no uid
drop, no `PR_SET_NO_NEW_PRIVS`. A warning is logged. It does not need root.
The config is still loaded, the site is still resolved, `--timeout` still
applies, and the command still starts in the site's web root.

It removes all containment. Never use it for a command you don't trust. It does
not enable `ephpm exec` on non-Linux platforms.

## Exit status

When the command starts, `ephpm exec` has been replaced by it, so the exit
status is the command's own. If anything fails before that (config, site
resolution, sandbox setup, privilege drop, or the command not being found),
`ephpm exec` prints `error: ...` on stderr and exits `1`.

## Logging

Diagnostics go to **stderr**. The level comes from `RUST_LOG` (default `info`).
The command's own stdout and stderr are untouched.

## Who may run it

The config file is the authorization boundary. Anyone who can read it can
derive any tenant's per-site credentials once `[kv] secret` is set, so
`ephpm exec` gives an operator with config access nothing new. In practice the
caller is root or an operator with `sudo`. Keep the config `0600` or `0640`, or
in a secret mount.

## Database access

`ephpm exec` does not bind the `ephpm_db_*` bridge. The intended way to give a
sandboxed PHP tool a database is to nest [`ephpm php --site`](../php/#binding-a-virtual-hosts-database---site):

```bash
ephpm exec --site shop -- ephpm php --site shop --config /etc/ephpm/ephpm.toml -- wp core version
```

> **Known limitation: this doesn't work with the config in `/etc/ephpm`.**
> The inner `ephpm php --site` has to read the config file (for `sites_dir`,
> `[db.sqlite]` and `[kv] secret`), and the sandbox denies `/etc/ephpm` on
> purpose. Hiding that file from the tenant command is the point of the
> sandbox. As shipped, the inner command cannot load its config and fails.
> Don't work around it by moving the config somewhere the sandbox can read, or
> by passing `EPHPM_KV__SECRET` in the environment: either one hands the master
> secret to the tenant's command. Until this is resolved, run
> `ephpm php --site` as the operator, outside `ephpm exec`.

## See also

- [`ephpm php`](../php/): the embedded PHP CLI and `--site` database binding
- [Multi-tenant hardening](/guides/multi-tenant-hardening/): the request-path isolation layers
- [`ephpm exec` design](/roadmap/ephpm-exec-sandboxed-vhost/): background, the confirmed probe results, and open items
