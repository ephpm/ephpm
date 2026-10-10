+++
title = "ephpm composer"
weight = 3
+++

Install PHP dependencies with the [vivacity](https://github.com/Adelagric/vivacity)
installer that is compiled into the `ephpm` binary. vivacity is a Rust
reimplementation of five Composer commands: `install`, `update`, `require`,
`remove` and `dump-autoload`. Every argument after `composer` is forwarded
verbatim to vivacity, and `ephpm` exits with vivacity's exit code.

`ephpm composer` is **not** Composer. Within those five commands it covers the
common case, and its upstream says its target is output that matches Composer
2.10.3 byte for byte (vivacity's claim, not measured by ePHPm). Outside that,
it hands the work to a real `composer` on `PATH` or stops with an error. The
differences are listed [below](#differences-from-composer), and you need to
read them before you put it in CI.

> **Pinned to vivacity 0.20.0, from the ePHPm fork.** ePHPm pins vivacity to
> an exact version (`vivacity = "=0.20.0"` in the workspace `Cargo.toml`) and
> builds it from [ephpm/vivace](https://github.com/ephpm/vivace) (a
> `[patch.crates-io]` entry): upstream v0.20.0 plus one change that accepts
> Composer's global `-n`/`-q`/`-v`/`--no-ansi` on every command. Flags and
> behavior can change when that pin moves, so this page describes that build
> only. `ephpm composer --version` prints the version your binary embeds.

## What needs PHP or Composer

| You run | Needs a `php` executable? | Needs a real `composer`? |
|---|---|---|
| `install`, `dump-autoload` (lock in scope) | No. Without one, platform requirements are skipped with a warning | No |
| `update`, `require`, `remove` | **Yes**, for the platform probe ([details](#platform-requirements)). A PHP-linked `ephpm` is its own `php`, so only a **stub** build needs one installed | No |
| Any command that is [out of scope](#fallback-to-composer) | — | Yes, or it exits `3` |
| `--run-scripts` (on `install` or `dump-autoload`) | Whatever your scripts need | Yes, or it exits `3` |

vivacity itself links no PHP, so the subcommand exists in every build,
including stub mode (no linked PHP) and Windows, and no `composer.phar` is
needed. For the platform probe it runs a `php` executable: `VIVACITY_PHP` if
set. When you have not set it, a **PHP-linked** `ephpm composer` sets
`VIVACITY_PHP` to its own executable, which answers as php-cli when vivacity
runs it ([how](../php/#re-invoking-php_binary)), so `update`, `require` and
`remove` work on a machine with **no PHP installed** and probe the PHP that
ePHPm actually runs. A **stub** build falls back to `php` on `PATH`, and there
`update`, `require` and `remove` fail on a machine that has none.

## Synopsis

```bash
ephpm composer <COMMAND> [OPTIONS] [ARGS...]
ephpm composer --version          # → vivacity 0.20.0
ephpm composer --help             # or: ephpm composer help <COMMAND>
ephpm composer <COMMAND> --help
```

## Commands

| Command | Aliases | What it does |
|---|---|---|
| `install` | — | Installs exactly what `composer.lock` says, then generates the autoloader. **Requires an existing `composer.lock`** |
| `update [PACKAGES...]` | `upgrade` | Resolves dependencies, writes `composer.lock`, then installs. Package names may use `vendor/*` patterns |
| `require [PACKAGES...]` | `r` | Adds packages to `composer.json` (creating it if it is missing), then runs a partial update. Accepts `vendor/name`, `vendor/name:^1.0` and `vendor/name ^1.0` |
| `remove [PACKAGES...]` | `rm` | Removes packages from `composer.json`, then runs a partial update. Restores `composer.json` if the update fails |
| `dump-autoload` | `dumpautoload` | Regenerates `vendor/autoload.php` and `vendor/composer/*` from the lock and the installed state |

Any other Composer command is an error. `validate`, `show`, `outdated`,
`run-script`, `create-project`, `config`, `audit`, `why`, `fund`, `suggest`,
`init`, `exec` and the rest all exit `2` with `error: unrecognized subcommand`.
Use a real Composer for those (see
[Relationship to `ephpm php`](#relationship-to-ephpm-php)).

## Options

vivacity parses each command's options separately, and they are not the same
across commands. **An option a command does not declare is a usage error that
exits `2`**, even if Composer accepts it.

Composer's global options `-n`/`--no-interaction`, `-q`/`--quiet`,
`-v`/`--verbose` and `--no-ansi` are accepted by **all five** commands, and
`install` accepts `--prefer-dist` and `--no-progress`, so a typical deploy line
such as `composer install --no-dev --no-interaction --no-ansi --prefer-dist
--optimize-autoloader` runs unchanged under `ephpm composer`. They are no-ops
(see [Accepted but ignored](#accepted-but-ignored)): in particular `-q` does
**not** silence vivacity's output. Still a usage error: `--ansi`, Composer's
`-d` short form of `--working-dir`, `--no-progress` on `dump-autoload`, and
the `--prefer-*` flags on `update`.

### Options by command

`✓` means the command accepts the option. A blank cell means passing it exits `2`.

| Option | install | update | require | remove | dump-autoload |
|---|:-:|:-:|:-:|:-:|:-:|
| `--working-dir <DIR>`: project directory (default: current directory) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `--dry-run`: print the operations and write nothing | ✓ | ✓ | ✓ | ✓ | |
| `--offline`: use local caches only, never the network | ✓ | ✓ | ✓ | ✓ | |
| `--no-dev`: skip `require-dev` | ✓ | ✓ ¹ | | | ✓ |
| `--update-no-dev`: run the update with `--no-dev` | | | ✓ | ✓ | |
| `--dev`: edit `require-dev` instead of `require` | | | ✓ | ✓ | |
| `--no-autoloader`: do not generate the autoloader | ✓ | ✓ | ✓ | ✓ | |
| `-o`, `--optimize-autoloader`: optimized classmap | ✓ | ✓ | ✓ | ✓ | |
| `-o`, `--optimize`: same, dump-autoload's spelling | | | | | ✓ |
| `-a`, `--classmap-authoritative`: authoritative classmap (implies `-o`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `--apcu-autoloader`, `--apcu-autoloader-prefix <PREFIX>` | ✓ | ✓ | ✓ | ✓ | |
| `--apcu`, `--apcu-prefix <PREFIX>`: dump-autoload's spelling | | | | | ✓ |
| `--ignore-platform-reqs`: ignore all platform requirements | ✓ | ✓ | ✓ | ✓ | ✓ |
| `--ignore-platform-req <REQ>`: ignore one (repeatable, `ext-*` patterns) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `--no-plugins`: no plugins, **including emulated ones** | ✓ | ✓ | ✓ | ✓ | ✓ |
| `--run-scripts`: run scripts through `composer run-script` ([details](#scripts-are-not-run)) | ✓ | | | | ✓ |
| `--no-fallback`: never delegate to Composer, exit `3` instead | ✓ | ✓ | ✓ | ✓ | |
| `--check-scope`: report native vs. delegated, then stop ([details](#fallback-to-composer)) | ✓ | | | | |
| `--no-blocking`, `--no-security-blocking`: disable [blocking policies](#security-policies) | ✓ | ✓ | ✓ | ✓ | |
| `--no-install`: write the lock without installing | ² | ✓ | ✓ | ✓ | |
| `--no-update`: edit `composer.json` only (implies `--no-install`) | | | ✓ | ✓ | |
| `-w`, `--with-dependencies`: also update dependencies of the listed packages, except root requirements | | ✓ | ✓ ³ | ⁴ | |
| `-W`, `--with-all-dependencies`: also update dependencies, root requirements included | | ✓ | ✓ ³ | ✓ ³ | |
| `--no-update-with-dependencies`: update only the listed packages | | | | ✓ | |
| `--prefer-stable`, `--prefer-lowest` | | ✓ | ✓ | | |
| `--with <REQ>`, and the `vendor/pkg:<constraint>` argument form: a temporary constraint for this run | | ✓ | | | |
| `--prefer-dist`, `--prefer-source`, `--prefer-install <dist\|source\|auto>`: this run's `preferred-install` ⁵ | ✓ | | ✓ | | |
| `--fixed`: write the exact version found instead of `^x.y` (`project`-type packages or `--dev` only) | | | ✓ | | |
| `--sort-packages`: keep the section sorted (also `config.sort-packages`) | | | ✓ | | |
| `--unused`: remove every locked package nothing requires | | | | ✓ | |

¹ On `update`, `--no-dev` skips installing dev packages; they are still resolved and locked.
² `install --no-install` is accepted by the parser, then refused with Composer's message, exit `1`.
³ Spelled `--update-with-dependencies` / `--update-with-all-dependencies` (short `-w` / `-W`). `--with-dependencies` / `--with-all-dependencies` work as aliases, except that `remove` has no `--with-dependencies`.
⁴ `remove -w` / `--update-with-dependencies` only prints Composer's deprecation notice. Including dependencies is already the default.
⁵ On `install` the flag sets the effective `preferred-install`, exactly as `config.preferred-install` would: `--prefer-dist` is what vivacity does anyway, while `--prefer-source` (or `--prefer-install source`/`auto` where it selects source) puts the affected packages [out of scope](#fallback-to-composer), so the install is handed to Composer. On `require` the three are parsed and ignored.

### Accepted but ignored

These are parsed so Composer command lines keep working, and they do nothing:

| Option | Commands | Note |
|---|---|---|
| `--no-scripts` | all five | Scripts never run anyway. On `dump-autoload` it cannot be combined with `--run-scripts` (exit `2`) |
| `--no-audit`, `--audit-format <FORMAT>` | `--no-audit`: update, require, remove. `--audit-format`: require, remove | vivacity has no audit |
| `--prefer-source`, `--prefer-dist`, `--prefer-install <X>` | require | vivacity always installs dist archives (on `install` they are honored, see ⁵) |
| `-n`, `--no-interaction` | all five | vivacity never prompts |
| `-q`, `--quiet`, `-v`/`-vv`/`-vvv`, `--verbose` | all five | They do not change the output: `-q` does not silence it |
| `--no-ansi` | all five | vivacity never writes ANSI escapes |
| `--no-progress` | install, update, require, remove | vivacity has no progress bar |
| `--no-suggest` | require | Prints Composer's deprecation notice |

### Refused

These exit `1` with an error rather than doing something different from Composer:

- `update lock`, `update nothing`, `update mirrors`: refreshing lock metadata is not supported.
- `-m`, `--minimal-changes`, `COMPOSER_MINIMAL_CHANGES=1` and `config.update-with-minimal-changes: true` on `require` and `remove` (`update` has no `-m`, so it exits `2` there).
- An alternate manifest via `COMPOSER=other.json`, on every command.
- A root `composer.json` that Composer itself refuses (an invalid root package name, a package that requires itself, an invalid link name), on every command and before anything is written, with Composer's message and exit code.
- `require` with `config.lock: false`, unless you also pass `--no-install`.

## Differences from Composer

### `install` and `dump-autoload` need a lock

Composer's `install` without a `composer.lock` falls back to an update.
vivacity's does not. It exits `1` with `no composer.lock in <dir> — `install`
needs one; run `vivacity update` to resolve and write it`. Commit your lock, or
run `ephpm composer update` first.
`dump-autoload` also needs a `composer.lock` and exits `1` without one, while
Composer's works from `installed.json` alone.

### Scripts are not run

**vivacity never executes the `scripts` section of `composer.json`, and it
does not print a warning when it skips them.** `--no-scripts` is accepted but
changes nothing. `post-install-cmd`, `post-update-cmd`, `post-autoload-dump`
and the rest are skipped silently.

The opt-in is `--run-scripts`, on `install` and `dump-autoload` only:

- vivacity runs `composer run-script --no-interaction -d <project> <event>`
  (adding `--no-dev` in no-dev mode) for each event your manifest declares, at
  the point where Composer would dispatch it. On `install` those events are
  `pre-install-cmd`, `pre-autoload-dump`, `post-autoload-dump` and
  `post-install-cmd`. On `dump-autoload` they are `pre-autoload-dump` and
  `post-autoload-dump`.
- It **requires a real `composer` on `PATH`**. If the manifest declares one of
  those events and no `composer` is found, it exits `3` before doing anything.
- Composer then runs your scripts with *its* PHP (`@php` is the `php` that
  Composer finds), not ePHPm's embedded PHP.
- Per-package events (`pre-package-install` and the like) never run, and
  `run-script` passes no `--optimize` flag to the autoload events.
- `update`, `require` and `remove` have no `--run-scripts`, so
  `post-update-cmd` never runs.

A non-zero exit from a script stops the command with that exit code.

### Plugins are emulated, never executed

vivacity never loads plugin code. A `composer-plugin` package in the lock
falls into one of four groups, all subject to your `config.allow-plugins`:

- **Emulated:** vivacity reproduces the plugin's effect natively.
  `composer/installers`, `roots/wordpress-core-installer`,
  `mnsami/composer-custom-directory-installer`, `symfony/runtime`,
  `pestphp/pest-plugin`, `dealerdirect/phpcodesniffer-composer-installer`,
  `phpstan/extension-installer`, `rector/extension-installer`,
  `wikimedia/composer-merge-plugin`, `yiisoft/yii2-composer`, `codeception/c3`
  and `bamarni/composer-bin-plugin`. Some of these have configurations
  vivacity does not emulate, which push the lock out of scope:
  `yiisoft/yii2-composer` together with `yiisoft/yii2-dev`,
  `bamarni/composer-bin-plugin` with `forward-command: true`, a merge-plugin
  include that declares its own `repositories`, and install paths that
  `composer/installers` cannot reproduce.
- **Installed as plain libraries, with a note:** these plugins are known to
  write nothing at install time. `symfony/flex`,
  `composer/package-versions-deprecated`, `php-http/discovery`,
  `drupal/core-project-message`, `drupal/core-recipe-unpack`, `symfony/thanks`
  and `ergebnis/composer-normalize`.
- **Always out of scope:** these change the install layout or package
  contents. `cweagans/composer-patches` and
  `oomphinc/composer-installers-extender`.
- **Any other plugin** is out of scope.

`--no-plugins` works as it does in Composer: no plugin runs, so **emulated
plugins are switched off too**. Every package goes into `vendor/`, custom
`composer/installers` paths are ignored, and the emulated output files
(`vendor/autoload_runtime.php`, `vendor/pest-plugins.json` and so on) are not
written. It also makes otherwise out-of-scope plugins acceptable, because they
are then installed as plain libraries.

For `update`, `require` and `remove`, vivacity checks the plugins that are
installed (project and global `installed.json`) and allowed. It delegates the
whole command to Composer when one of them would change what gets resolved or
written:

- `symfony/flex`: on `require`, or with an install (`--no-install` resolves natively)
- `symfony/thanks`: on `update` with an install
- `yiisoft/yii2-composer`: with an install
- `drupal/core-recipe-unpack`: on `require`
- any plugin that is not on vivacity's lists

### Fallback to Composer

Before `install` writes anything, vivacity checks whether it can reproduce
Composer's result for this lock. A lock is **out of scope** when:

- a package has no usable dist: source-only (`git`) packages, or dist types
  other than zip/tar
- a `path` repository package is involved **on Windows**, where Composer would
  use junctions. On Linux and macOS, path packages install natively when their
  directory exists
- an unknown or layout-changing plugin is present, or an emulated plugin is
  used in a configuration vivacity does not emulate (see above)
- `config.preferred-install` asks for `source` (project or global config)
- `vendor-dir` / `bin-dir` settings, or install paths, that vivacity refuses
  (for example two packages mapped to the same path, or a package installed
  at the project root)

What happens next depends on the flags:

| Situation | Result |
|---|---|
| Default, and `composer` (`composer.bat`/`.cmd`/`.exe` on Windows) is on `PATH` | Prints the reasons on stderr and runs `composer install` itself, with `--no-scripts` (unless `--run-scripts`) and your `--no-plugins`, `--no-dev`, `--no-autoloader`, `-o`, `-a` and `--ignore-platform-req(s)` passed along. The exit code is Composer's. On Unix, `composer` **replaces** the process (`exec`) |
| Default, and no `composer` on `PATH` | Prints the reasons and `composer not found for the fallback`, exit `3`. Nothing written |
| `--no-fallback` | Prints the reasons, exit `3`. Nothing written |
| `--check-scope` | Exit `0` (`this lock is installed natively`) or exit `3` with the reasons. Nothing is downloaded or written, but the [malware-list check](#security-policies) and the platform check still run first, so a lock can also exit `2` or `4` here |

`update`, `require` and `remove` delegate the same way for the plugin cases
above. They re-run the original command line under `composer`, dropping
`--no-fallback` and `--offline` and adding `--no-scripts`. With
`--no-fallback`, or without `composer` on `PATH`, they exit `3`.

`dump-autoload` never delegates. If the lock contains an unknown or
layout-changing plugin (and `--no-plugins` is not set), it exits `3` and tells
you to run `composer dump-autoload`.

> `install --dry-run` does **not** run the scope check, whatever its help text
> says. It exits `0` and lists the operations even for a lock that a real
> install would delegate. Use `--check-scope` to answer that question.

### Repository types (`update`, `require`, `remove`)

When resolving, vivacity reads only `composer` repositories (Packagist,
WPackagist, Private Packagist, Satis and the like) and `path` repositories.
**`vcs`, `git`, `github`, `package`, `artifact` and other repository types are
an error (exit `1`), not a fallback.** That remains true when `composer` is on
`PATH`. Repository filters (`only` / `exclude` / `canonical`) and the Composer
v1 `providers` protocol are also rejected.

This affects ePHPm's own PHP packages (`ephpm/db-*`, `ephpm/cache-*`, the
worker adapters and so on). They are distributed through GitHub `vcs`
repositories and not Packagist ([PHP packages](/reference/php-packages/)), so
`ephpm composer update` and `require` cannot resolve them. Resolve those
projects with a real Composer. `install` reads the lock and does not consult
`repositories`.

### No audit

vivacity has no `audit` command and does not audit after `update`/`require`.
`--no-audit` and `--audit-format` are accepted and ignored. Advisory
*blocking* is a separate mechanism, and that one is implemented
([below](#security-policies)).

### Output

All of vivacity's narrative goes to **stderr**. stdout is empty, apart from
the `codeception/c3` emulation's messages. A successful `install` ends with a
summary line such as `vivacity: 1 installed, 0 unchanged, 0 removed (…) in
0.25s`.

## Platform requirements

vivacity gets its platform (the `php` version, `ext-*`, `lib-*`) by running a
probe script through a `php` executable, as `$VIVACITY_PHP <probe.php>`
(otherwise `php` on `PATH`). Which `php` that is depends on the build:

- **PHP-linked `ephpm` (release builds):** unless you set `VIVACITY_PHP`
  yourself (an empty value counts as unset), `ephpm composer` sets it to the path of the running `ephpm`
  executable before vivacity starts. vivacity's `ephpm <probe.php>` then runs
  the script on the embedded PHP CLI ([`ephpm php`](../php/#re-invoking-php_binary)),
  so the probe reports exactly the PHP version and extensions `ephpm php -m`
  shows, and no system PHP is needed.
- **Stub build (no linked PHP):** `php` on `PATH`, or `VIVACITY_PHP`.

Then:

- **`install`:** if a `php` is found, the lock is checked against it.
  Unsatisfied requirements exit `4`
  (`Your lock file cannot be installed on this platform`). If no `php` is
  found (stub build only), the check is skipped with
  `Warning: php not found, platform requirements were not checked`, and the
  install goes ahead.
- **`update`, `require`, `remove`:** the probe is **mandatory**. Without a
  `php` (stub build only), they exit `1` with `Error: cannot run php: program
  not found`, even with `--ignore-platform-reqs` or `config.platform` set.
- `config.platform` overrides and `--ignore-platform-req(s)` apply on top of
  the probed platform, as in Composer. `COMPOSER_IGNORE_PLATFORM_REQS` and
  `COMPOSER_IGNORE_PLATFORM_REQ` are honored on `update`, `require` and
  `remove`, but not on `install`.

To probe a different PHP — say, a system `php` whose extensions match a
production php-fpm — set `VIVACITY_PHP` explicitly; `ephpm composer` never
overrides a value you set.

vivacity caches the probe result. The cache is keyed on the probed
executable's path, size and mtime (plus the ini files it loaded), so replacing
the `ephpm` binary invalidates it. With a wrapper script as `VIVACITY_PHP`
that executable is the wrapper, not what it runs: after changing what it runs,
pass `VIVACITY_NO_PLATFORM_CACHE=1` once so the probe runs again.

What actually guards production is the runtime check. Like Composer, the
generated autoloader includes `vendor/composer/platform_check.php`, which
checks the PHP version (`config.platform-check` defaults to PHP-only) when
ePHPm's embedded PHP loads `vendor/autoload.php`.

## Security policies

Blocking policies (Composer's `config.policy`, and the legacy `config.audit`)
are on by default:

- **`install`** checks the locked packages against the repositories' malware
  filter lists. This makes one metadata request to each `composer`
  repository. With `--offline`, vivacity uses the cached list and warns that
  it may be out of date. A flagged package fails the install, exit `2`.
- **`update`, `require`, `remove`** filter packages with security advisories
  and malware-listed versions out of resolution. Abandoned packages produce a
  warning and are not blocked.

`--no-blocking` or `--no-security-blocking` (they are equivalent here), or
`COMPOSER_NO_BLOCKING=1` / `COMPOSER_NO_SECURITY_BLOCKING=1`, disables advisory,
malware and abandoned blocking. `COMPOSER_POLICY=0` disables policies
entirely. `COMPOSER_POLICY_ADVISORIES_BLOCK`, `COMPOSER_POLICY_MALWARE_BLOCK`
and `COMPOSER_POLICY_ABANDONED_BLOCK` toggle each one. vivacity does not
support custom lists (`policy.<name>`).

## Exit codes

`ephpm composer` exits with vivacity's status unchanged. It calls
`std::process::exit` directly, so the code reaches CI as-is.

| Code | Meaning |
|---|---|
| `0` | Success. Also `--help`, `--version`, `--dry-run`, and `--check-scope` on an in-scope lock |
| `1` | Runtime error: missing `composer.lock` on `install`, no `php` for `update`/`require`/`remove` (stub build), an unsupported repository type, a refused option, local I/O failures |
| `100` | A fetch failed (a dist or repository that answers an HTTP error, is unreachable, or a local dist path that is missing), after Composer's retry policy (up to three attempts). Composer exits `100` for the same failures |
| `2` | Usage error (unknown command or option, or no command at all). Also Composer's code `2`: an unsolvable dependency set, a lock blocked by policy, or `remove` leaving the package installed |
| `3` | Out of scope with no fallback possible (`--no-fallback`, `--check-scope`, or no `composer` on `PATH`), or `--run-scripts` without `composer` |
| `4` | Platform requirements not met on `install`, or `composer.json` requires something the lock does not contain |
| other | When vivacity delegates to Composer or runs scripts, the exit code of that process |

## Environment and caches

- **Composer variables honored:** `COMPOSER_HOME`, `COMPOSER_CACHE_DIR`,
  `COMPOSER_AUTH`, `COMPOSER_NO_DEV`, `COMPOSER_PREFER_STABLE`,
  `COMPOSER_PREFER_LOWEST`, `COMPOSER_WITH_DEPENDENCIES`,
  `COMPOSER_WITH_ALL_DEPENDENCIES`, `COMPOSER_IGNORE_PLATFORM_REQ(S)`,
  `COMPOSER_FUND=0`, and the policy variables above.
- **Authentication:** `github-oauth`, `http-basic` and `bearer` entries, read
  from `$COMPOSER_HOME/auth.json`, `COMPOSER_AUTH`, and the project's
  `auth.json`. Later sources take priority.
- **Caches:** downloads use Composer's cache directory (`COMPOSER_CACHE_DIR`,
  otherwise Composer's default location), so a cache warmed by either tool
  serves the other. vivacity also keeps a package store, a classmap cache and
  the platform-probe cache in its own directory: `VIVACITY_CACHE_DIR`,
  otherwise `%LOCALAPPDATA%\vivacity` on Windows, `~/Library/Caches/vivacity`
  on macOS, or `$XDG_CACHE_HOME/vivacity` / `~/.cache/vivacity`.
- **vivacity variables:** `VIVACITY_PHP` (the probe executable),
  `VIVACITY_NO_PLATFORM_CACHE`, `VIVACITY_NO_CLASSMAP_CACHE`, and
  `VIVACITY_TRACE=1` (per-phase timings on stderr).
- **TLS:** vivacity's HTTPS client uses ePHPm's `aws-lc-rs` rustls provider,
  which `ephpm composer` installs before dispatch, so a cold-cache fetch works
  without a running server.

## Framework notes

### Laravel

- **`package:discover` does not run.** Laravel's skeleton runs
  `Illuminate\Foundation\ComposerScripts::postAutoloadDump` and
  `@php artisan package:discover` from `post-autoload-dump`, and vivacity skips
  both. After an `install`, `update`, `require` or `remove` that changes
  packages, regenerate the package manifest yourself:

  ```bash
  ephpm composer install --no-dev --optimize-autoloader
  ephpm php artisan package:discover --ansi
  ```

  The alternative is `ephpm composer install --run-scripts`, which needs a
  real `composer` and a `php` on `PATH`. `post-update-cmd`
  (`vendor:publish --tag=laravel-assets`) never runs under vivacity, so run it
  by hand if you rely on it.
- The skeleton's allowed plugins (`pestphp/pest-plugin`, `php-http/discovery`)
  are handled natively.
- `create-project` is not supported. Use `composer create-project`, or clone
  and run `update` (a stub build needs a `php` for that,
  [above](#platform-requirements)).

### WordPress

- `composer/installers` (`wordpress-plugin`, `wordpress-theme`,
  `wordpress-muplugin` paths) and `roots/wordpress-core-installer` (Bedrock)
  are emulated, provided `allow-plugins` permits them.
  `johnpbloch/wordpress-core-installer` is not on vivacity's list, so a lock
  containing it is delegated to Composer.
- WPackagist is a `composer` repository and resolves natively. ePHPm's
  `ephpm/*-wordpress` packages come from `vcs` repositories, which
  `update`/`require` [cannot resolve](#repository-types-update-require-remove).

## Relationship to `ephpm php`

`ephpm composer` is a separate installer compiled into the binary. It is not
the Composer PHAR running under [`ephpm php`](../php/). To run real Composer,
whether for a command vivacity lacks, for scripts, or for an out-of-scope
project, run the PHAR through the embedded PHP CLI (this needs a PHP-linked
build):

```bash
ephpm php -d memory_limit=-1 composer.phar install
ephpm php composer.phar validate
```

vivacity's fallback and `--run-scripts` look for a `composer` executable on
`PATH`. They do not use `ephpm php composer.phar`, so for those you need a
`composer` (or `composer.bat` on Windows) wrapper on `PATH`, for example:

```bash
#!/bin/sh
exec ephpm php -d memory_limit=-1 /opt/composer/composer.phar "$@"
```

Under that wrapper Composer's `@php` script lines resolve to `PHP_BINARY`,
which is the `ephpm` executable, and it [answers as
php-cli](../php/#re-invoking-php_binary), so scripts such as
`@php artisan package:discover` run on the embedded PHP with no system PHP.

## Argument passthrough

The subcommand captures **all** trailing arguments verbatim (clap
`trailing_var_arg` + `allow_hyphen_values`) and turns off `ephpm`'s own help
flag, so options that start with a hyphen are never intercepted. Before
dispatch, it prepends a synthetic `composer` program-name element
(vivacity's parser expects `argv[0]`) and installs the TLS provider, then
calls `vivacity::run`. Nothing else is rewritten.

## See also

- [`ephpm php`](../php/): the embedded PHP CLI (runs `composer.phar` too)
- [PHP packages](/reference/php-packages/): ePHPm's Composer packages and their `vcs` repositories
- [vivacity](https://github.com/Adelagric/vivacity): the embedded installer
