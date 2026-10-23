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

> **Pinned to vivacity 0.23.0.** ePHPm pins vivacity to an exact version
> (`=0.23.0` in the workspace `Cargo.toml`, from crates.io). Flags and behavior
> can change when that pin moves, so this page describes 0.23.0 only. Released
> binaries differ: ePHPm v0.11.0 embeds vivacity 0.11.1, v0.11.1 embeds 0.15.0,
> and v0.11.2 through v0.12.0 embed 0.17.0; 0.23.0 ships in v0.13.0.
> `ephpm composer --version` prints the version your binary embeds.

## What needs PHP or Composer

| You run | Needs a `php` executable? | Needs a real `composer`? |
|---|---|---|
| `install` (with a `composer.lock` in scope), `dump-autoload` | No. Without one, `install` skips the platform check (with a warning if the lock declares platform requirements) | No |
| `update`, `require`, `remove`, and `install` without a usable `composer.lock` (it [runs an update](#install-without-a-lock)) | **Yes**, for the platform probe ([details](#platform-requirements)). A PHP-linked `ephpm` is its own `php`, so only a **stub** build needs one installed | No |
| Any command that is [out of scope](#fallback-to-composer) | — | Yes, or it exits `3` |
| `--run-scripts` (on `install` or `dump-autoload`) | Whatever your scripts need | Yes, or it exits `3` |
| `-vv`/`-vvv`, or `-v` while a plugin is active ([details](#verbosity)) | — | Yes, or it exits `3` |

vivacity itself links no PHP, so the subcommand exists in every build,
including stub mode (no linked PHP) and Windows, and no `composer.phar` is
needed. For the platform probe it runs a `php` executable: `VIVACITY_PHP` if
set. When you have not set it, a **PHP-linked** `ephpm composer` sets
`VIVACITY_PHP` to its own executable, which answers as php-cli when vivacity
runs it ([how](../php/#re-invoking-php_binary)), so `update`, `require` and
`remove` work on a machine with **no PHP installed** and probe the PHP that
ePHPm actually runs. A **stub** build falls back to `php` on `PATH`, and there
`update`, `require` and `remove` (and an `install` with no lock, which becomes
an update) fail on a machine that has none.

## Synopsis

```bash
ephpm composer <COMMAND> [OPTIONS] [ARGS...]
ephpm composer --version          # → vivacity 0.23.0
ephpm composer --help             # or: ephpm composer help <COMMAND>
ephpm composer <COMMAND> --help
```

## Commands

| Command | Aliases | What it does |
|---|---|---|
| `install` | — | Installs exactly what `composer.lock` says, then generates the autoloader. Without a usable `composer.lock` it resolves and installs, as Composer does ([details](#install-without-a-lock)) |
| `update [PACKAGES...]` | `upgrade` | Resolves dependencies, writes `composer.lock`, then installs. Package names may use `vendor/*` patterns, and `vendor/pkg:<constraint>` adds a temporary constraint for this run (as `--with` does) |
| `require [PACKAGES...]` | `r` | Adds packages to `composer.json` (creating it if it is missing), then runs a partial update. Accepts `vendor/name`, `vendor/name:^1.0` and `vendor/name ^1.0` |
| `remove [PACKAGES...]` | `rm` | Removes packages from `composer.json`, then runs a partial update. Restores `composer.json` if the update fails |
| `dump-autoload` | `dumpautoload` | Regenerates `vendor/autoload.php` and `vendor/composer/*` from the lock and the installed state (from `vendor/composer/installed.json` alone when there is no lock to read) |

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
--optimize-autoloader` runs unchanged under `ephpm composer`. `-n` and
`--no-ansi` are no-ops. `-q` and `-v` change the output, and **`-v` can hand
the whole command to Composer** (see [Verbosity](#verbosity)). Still a usage
error: `--ansi`, Composer's `-d` short form of `--working-dir`,
`--no-progress` on `dump-autoload`, and the `--prefer-*` flags on `update` and
`remove`.

### Options by command

`✓` means the command accepts the option. A blank cell means passing it exits `2`.

| Option | install | update | require | remove | dump-autoload |
|---|:-:|:-:|:-:|:-:|:-:|
| `--working-dir <DIR>`: project directory (default: current directory) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `-q`, `--quiet`, `-v`/`-vv`/`-vvv`, `--verbose`: output level ([details](#verbosity)) | ✓ | ✓ | ✓ | ✓ | ✓ |
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
| `--no-fallback`: never delegate to Composer, exit `3` instead | ✓ | ✓ | ✓ | ✓ | ✓ ⁵ |
| `--check-scope`: report native vs. delegated, then stop ([details](#fallback-to-composer)) | ✓ | | | | |
| `--no-blocking`, `--no-security-blocking`: disable [blocking policies](#security-policies) | ✓ | ✓ | ✓ | ✓ | |
| `--no-install`: write the lock without installing | ² | ✓ | ✓ | ✓ | |
| `--no-update`: edit `composer.json` only (implies `--no-install`) | | | ✓ | ✓ | |
| `-w`, `--with-dependencies`: also update dependencies of the listed packages, except root requirements | | ✓ | ✓ ³ | ⁴ | |
| `-W`, `--with-all-dependencies`: also update dependencies, root requirements included | | ✓ | ✓ ³ | ✓ ³ | |
| `--no-update-with-dependencies`: update only the listed packages | | | | ✓ | |
| `--prefer-stable`, `--prefer-lowest` | | ✓ | ✓ | | |
| `--with <REQ>` (repeatable): a temporary constraint for this run, such as `--with=vendor/pkg:^1.2` | | ✓ | | | |
| `--prefer-dist`, `--prefer-source`, `--prefer-install <dist\|source\|auto>`: this run's `preferred-install` ⁶ | ✓ | | ✓ | | |
| `--fixed`: write the exact version found instead of `^x.y` (`project`-type packages or `--dev` only) | | | ✓ | | |
| `--sort-packages`: keep the section sorted (also `config.sort-packages`) | | | ✓ | | |
| `--unused`: remove every locked package nothing requires | | | | ✓ | |

¹ On `update`, `--no-dev` skips installing dev packages; they are still resolved and locked.
² `install --no-install` is accepted by the parser, then refused with Composer's message, exit `1`.
³ Spelled `--update-with-dependencies` / `--update-with-all-dependencies` (short `-w` / `-W`). `--with-dependencies` / `--with-all-dependencies` work as aliases, except that `remove` has no `--with-dependencies`.
⁴ `remove -w` / `--update-with-dependencies` only prints Composer's deprecation notice. Including dependencies is already the default.
⁵ `dump-autoload` never delegates for plugins (it exits `3` on what it cannot dump). Its `--no-fallback` only matters for a [verbosity hand-over](#verbosity), which it turns into exit `3`.
⁶ On `install` the flag sets the effective `preferred-install`, exactly as `config.preferred-install` would: `--prefer-dist` is what vivacity does anyway, while `--prefer-source` (or `--prefer-install source`/`auto` where it selects source) puts the affected packages [out of scope](#fallback-to-composer), so the install is handed to Composer. An invalid combination or an unknown `--prefer-install` value exits `1` with Composer's message. On `require` they apply the same way to the install phase that follows the update: a package that would install from source hands that phase to Composer (a `composer install` carrying the flag), and an invalid combination is refused before resolving, with `composer.json` restored.

A temporary constraint that is disjoint from what `composer.json` requires is
refused with Composer's message, exit `1`.

### Accepted but ignored

These are parsed so Composer command lines keep working, and they do nothing:

| Option | Commands | Note |
|---|---|---|
| `--no-scripts` | all five | Scripts only run with `--run-scripts`. On `install` it cancels `--run-scripts` (no scripts run, and a [fallback](#fallback-to-composer) passes `--no-scripts` to Composer). On `dump-autoload` it cannot be combined with `--run-scripts` (exit `2`) |
| `--no-audit`, `--audit-format <FORMAT>` | `--no-audit`: update, require, remove. `--audit-format`: require, remove | vivacity has no audit |
| `-n`, `--no-interaction` | all five | vivacity never prompts |
| `--no-ansi` | all five | vivacity never writes ANSI escapes |
| `--no-progress` | install, update, require, remove | vivacity has no progress bar |
| `--no-suggest` | require | Prints Composer's deprecation notice |

### Refused

These exit `1` with an error rather than doing something different from Composer:

- `update lock`, `update nothing`, `update mirrors`: refreshing lock metadata is not supported.
- `-m`, `--minimal-changes`, `COMPOSER_MINIMAL_CHANGES=1` and `config.update-with-minimal-changes: true` on `require` and `remove`. `update` has no `-m` (it exits `2` there), and it does not read the variable or the config key, so an `update` with either set resolves without minimal changes.
- An alternate manifest via `COMPOSER=other.json`, on every command.
- A root `composer.json` that Composer itself refuses (an invalid root package name, a package that requires itself, an invalid link name), on every command and before anything is written, with Composer's message.

## Differences from Composer

### `install` without a lock

As in Composer, an `install` with no usable `composer.lock` (missing, without
a `packages` key, or ignored because `config.lock` is `false`) prints `No
composer.lock file present.` (or `Lockfile creation is disabled in config.`)
and runs an update instead. That update needs a `php` for the
[platform probe](#platform-requirements), like any `update`. With
`--check-scope` or `--run-scripts` it does not resolve natively: it exits `3`
or hands the command to Composer ([Fallback](#fallback-to-composer)). Commit
your lock if you want `install` to stay PHP-free.

`dump-autoload` without a lock to read lays the autoloader out from
`vendor/composer/installed.json`, as Composer does.

With `config.lock: false`, every command warns that `./composer.lock is
present but ignored` (when the file exists), resolves without the lock's packages and writes no lock,
as Composer does.

Without a `composer.json`, `install`, `update` and `dump-autoload` exit `1`
with Composer's own message (`Composer could not find a composer.json file in
<dir>`). `require` creates the file instead.

### Scripts are not run

**vivacity never executes the `scripts` section of `composer.json`, and it
does not print a warning when it skips them.** `--no-scripts` is accepted; it
only matters next to `install --run-scripts`, which it cancels.
`post-install-cmd`, `post-update-cmd`, `post-autoload-dump` and the rest are
skipped silently.

The opt-in is `--run-scripts`, on `install` and `dump-autoload` only:

- vivacity runs `composer run-script --no-interaction -d <project> <event>`
  (adding `--no-dev` in no-dev mode, and `-q` or `-v` to match the run's
  [verbosity](#verbosity)) for each event your manifest declares, at the point
  where Composer would dispatch it. On `install` those events are
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

vivacity never loads plugin code. At install time a `composer-plugin`
package in the lock falls into one of four groups, decided by its name:

- **Emulated:** vivacity reproduces the plugin's effect natively.
  `composer/installers`, `roots/wordpress-core-installer`,
  `mnsami/composer-custom-directory-installer`, `symfony/runtime`,
  `symfony/flex` (its install-time `.env.dist` → `.env` copy),
  `composer/package-versions-deprecated`, `pestphp/pest-plugin`,
  `dealerdirect/phpcodesniffer-composer-installer`,
  `phpstan/extension-installer`, `rector/extension-installer`,
  `wikimedia/composer-merge-plugin`, `yiisoft/yii2-composer`, `codeception/c3`
  and `bamarni/composer-bin-plugin`. The lines these plugins print are
  printed too, in the order Composer activates the plugins (for example
  `phpstan/extension-installer: Extensions installed`). For
  `phpstan/extension-installer` (1.4.0 to 1.4.3), `rector/extension-installer`
  (0.11.0 to 0.11.2, and 1.0.0), `dealerdirect/phpcodesniffer-composer-installer` (1.0.0 to
  1.2.1, except 1.1.0) and `composer/package-versions-deprecated`
  (1.11.99.4 and 1.11.99.5), emulation is tied to those versions: any other
  version is out of scope. Emulation follows your
  `config.allow-plugins` as Composer would: for example, a
  `composer/installers` that `allow-plugins` denies installs everything into
  `vendor/`, and one it does not mention at all is out of scope (Composer
  would refuse to run). Some of these have configurations vivacity does not
  emulate, which push the lock out of scope: `yiisoft/yii2-composer` together
  with `yiisoft/yii2-dev`, `bamarni/composer-bin-plugin` with
  `forward-command: true`, a merge-plugin configuration vivacity cannot merge,
  and install paths that `composer/installers` cannot reproduce.
- **Installed as plain libraries, with a note:** these plugins are known to
  write nothing at install time. `php-http/discovery`,
  `drupal/core-project-message`, `drupal/core-recipe-unpack`,
  `ibexa/post-install`, `symfony/thanks`, `ergebnis/composer-normalize` and
  `metasyntactical/composer-plugin-license-check` (out of scope instead when
  it is allowed and `extra` gives it an allow-list or deny-list).
- **Always out of scope:** these change the install layout or package
  contents. `cweagans/composer-patches` and
  `oomphinc/composer-installers-extender`.
- **Any other plugin** is out of scope.

The last two groups do not consult `allow-plugins`: an unknown or
layout-changing plugin puts the lock out of scope even when `allow-plugins`
denies it. Only `--no-plugins` (below) lets such a lock install natively.

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

- `symfony/flex`: on `require`, and on `remove` with an install. On `update`
  (and `remove --no-install`) only when Flex would do something vivacity does
  not reproduce. `update` applies Flex **recipes** natively for packages
  `symfony.lock` does not record yet: `config/bundles.php` (only in the exact
  form Flex writes it), files copied from the recipe, `.env*` and PHPUnit env
  lines, `.gitignore`, `add-lines` patches, `scripts.auto-scripts` in
  `composer.json`, and the new `symfony.lock` entries, printed as Flex prints
  them. It hands over instead for the `container`, `makefile`,
  `composer-commands` and `copy-from-package` configurators, Docker
  configuration (unless `extra.symfony.docker` is `false`), a contrib recipe
  without `allow-contrib`, a `%generate(secret)%` value, a matching recipe
  conflict, a non-default `root-dir`, a GitHub API endpoint, `--offline` with
  a recipe to download, removing a package `symfony.lock` records, Flex being
  installed by the run itself, an `.env.dist` copy, a `symfony-pack` to
  unpack, and a `package.json`/`importmap.php` synchronisation that would
  write. That is decided before anything is written
- `symfony/thanks`: on `update` with an install
- `yiisoft/yii2-composer`: with an install
- `drupal/core-recipe-unpack`: on `require`
- `wikimedia/composer-merge-plugin`: when a merged file declares its own
  `repositories`
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
- the effective `preferred-install` (`config.preferred-install` in the project
  or global config, or the `--prefer-*` flags) installs a package from
  `source`
- `vendor-dir` / `bin-dir` settings, or install paths, that vivacity refuses
  (for example two packages mapped to the same path, or a package installed
  at the project root)
- the project sits in a Mercurial, Fossil or Subversion working copy whose
  tool answers (Composer would read the root version from it; vivacity reads
  git only). The whole command is then handed over before anything is read.
  With no such working copy this costs a few `stat` calls; if the tool is
  missing or fails, vivacity stays native, as Composer falls back too

What happens next depends on the flags:

| Situation | Result |
|---|---|
| Default, and `composer` (`composer.bat`/`.cmd`/`.exe` on Windows) is on `PATH` | Prints the reasons on stderr (not under `-q`) and runs `composer` as a child process (never `exec`, so `ephpm` keeps its process) with the command line you gave, minus vivacity's own options (`--no-fallback`, `--check-scope`, `--run-scripts`; `--offline` becomes `COMPOSER_DISABLE_NETWORK=1`), plus `--no-scripts` unless you passed `--run-scripts`. The exit code is Composer's |
| Default, and no `composer` on `PATH` | Prints the reasons and `composer not found for the fallback`, exit `3`. Nothing written |
| `--no-fallback` | Prints the reasons, exit `3`. Nothing written |
| `--check-scope` | Exit `0` (`this lock is installed natively`) or exit `3` with the reasons. Nothing is downloaded or written, but the [malware-list check](#security-policies) and the platform check still run first, so a lock can also exit `2` or `4` here |

`update`, `require` and `remove` delegate the same way for the plugin cases
above. They re-run the original command line under `composer`, dropping
`--no-fallback`, `--offline` (which becomes `COMPOSER_DISABLE_NETWORK=1`) and
(on `require`/`remove`) `--no-autoloader`, and adding `--no-scripts`. A
`composer.json` that `require`/`remove` already edited is restored first, and
nothing runs after Composer. When only their install phase is out of scope,
vivacity writes the lock and hands Composer a `composer install` built from
the same options. With `--no-fallback`, or without `composer` on `PATH`, they
exit `3`.

`dump-autoload` does not delegate for plugins. If the lock contains an unknown
or layout-changing plugin (and `--no-plugins` is not set), it exits `3` and
tells you to run `composer dump-autoload`.

Every Composer process vivacity starts gets the run's verbosity through
`SHELL_VERBOSITY`. The [verbosity hand-over](#verbosity) is a separate,
earlier path.

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
projects with a real Composer. `install` reads the lock and does not resolve
against `repositories` (it only fetches the `composer` repositories' malware
lists, [below](#security-policies)).

### No audit

vivacity has no `audit` command and does not audit after `update`/`require`.
`--no-audit` and `--audit-format` are accepted and ignored. Advisory
*blocking* is a separate mechanism, and that one is implemented
([below](#security-policies)).

### Output

All of vivacity's narrative goes to **stderr**. stdout carries only
`--help`/`--version`, `dump-autoload`'s two lines (`Generating [optimized]
autoload files`, then `Generated … containing N classes`, as Composer prints
them), the emulated plugins' lines (`codeception/c3`,
`phpstan/extension-installer`, `rector/extension-installer`,
`dealerdirect/phpcodesniffer-composer-installer`,
`composer/package-versions-deprecated`), the hint Composer also prints there
when an `update --with` constraint conflicts with `composer.json`, and, under
GitHub Actions (`GITHUB_ACTIONS` set), the `::error ::` annotations Composer
writes for an unsolvable set and similar failures. A successful `install` ends with a summary line such as
`vivacity: 1 installed, 0 unchanged, 0 removed (…) in 0.25s`.

### Verbosity

vivacity computes the output level as Composer does: `SHELL_VERBOSITY` from
the environment first, then `-q`/`--quiet` (which wins over any `-v`), then
`-vvv`, `-vv` and `-v`/`--verbose`.

- **`-q`:** only the lines Composer writes at its quiet level remain (errors,
  the stale-lock warning, the headline of an unsolvable set, and vivacity's
  own reasons for stopping with exit `3`). Everything else, stdout included,
  is silent, so the `install` summary line disappears too. The exit code is
  unchanged.
- **`-v`:** adds Composer's verbose lines, such as `Dependency resolution
  completed in N seconds` and the `Installs:` / `Updates:` / `Removals:`
  summaries.
- **`-vv` and `-vvv` hand the whole command to Composer**, because vivacity
  does not reproduce Composer's debug output.
- **So does `-v` while a plugin is active:** any `composer-plugin` package
  that is installed (project or `COMPOSER_HOME`) or in the lock, and that
  `allow-plugins` allows, emulated or not. Composer then prints a line per
  plugin listener, which vivacity does not reproduce. Laravel's skeleton
  allows `pestphp/pest-plugin` and `php-http/discovery`, so on a typical
  Laravel project `ephpm composer install -v` runs Composer, not vivacity.
  Drop `-v` in CI if you want vivacity to do the work.

The hand-over happens after the command line is parsed and before anything is
read or written. `--check-scope` is answered natively whatever the verbosity.
With `--no-fallback`, or without `composer` on `PATH`, the command exits `3`
instead. Otherwise `composer` runs as a child process (never `exec`) with the
same arguments minus `--no-fallback`, `--check-scope`, `--offline` (which
becomes `COMPOSER_DISABLE_NETWORK=1`) and `--run-scripts`, plus `--no-scripts`
unless you passed `--run-scripts` or `--no-scripts`, and the exit code is
Composer's. An inherited `SHELL_VERBOSITY` of `1` or more has the same effect
as the flags.

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

- **`install`:** if the probe runs, the lock is verified against it the way
  Composer's installer does, in one solve that also covers the root's
  `require`, `conflict`, `replace` and `provide`. Unsatisfied platform
  requirements print `Your lock file does not contain a compatible set of
  packages. Please run composer update.` and the numbered problems, exit `2`.
  If the probe
  cannot run (no `php`, or a `VIVACITY_PHP` that does not work), the check is
  skipped and the install goes ahead. vivacity then prints
  `Warning: php not found, platform requirements were not checked` only when
  the lock declares platform requirements (`platform`, or `platform-dev`
  outside `--no-dev`). With `--ignore-platform-reqs` the probe is not run at
  all.
- **`update`, `require`, `remove`:** the probe is **mandatory**. When it
  cannot run they exit `1`, even with `--ignore-platform-reqs` or
  `config.platform` set. A missing executable reads
  `Error: cannot run <php>:` followed by the operating system's error
  ("No such file or directory" on Linux and macOS, "program not found" on
  Windows). With the PHP-linked default this only happens when a
  `VIVACITY_PHP` you set does not work; a stub build also needs a `php` on
  `PATH`.
- `config.platform` overrides and `--ignore-platform-req(s)` apply on top of
  the probed platform, as in Composer. `COMPOSER_IGNORE_PLATFORM_REQS` and
  `COMPOSER_IGNORE_PLATFORM_REQ` are honored, with Composer's warning, by
  every command that has the matching options (all five, `dump-autoload`'s
  `platform_check.php` included).

To probe a different PHP — say, a system `php` whose extensions match a
production php-fpm — set `VIVACITY_PHP` explicitly; `ephpm composer` never
overrides a value you set.

vivacity caches the probe result (`platform-probe.json` in its cache
directory), but only for a `php` whose loaded files it can see: the binary the
probe reports must be the one vivacity located, and vivacity must be able to
list what that process loaded. The cache key then covers the binary, every ini
file PHP loaded or scanned plus the scan directory, every library the process
loaded, the probe script itself, the `PHPRC`, `PHP_INI_SCAN_DIR`,
`XDEBUG_MODE`, `XDEBUG_CONFIG` and `COMPOSER_ALLOW_XDEBUG` variables and the
dynamic linker's `LD_*`/`DYLD_*` variables, and system markers such as the
dynamic linker's cache and the package databases. Anything vivacity cannot
see this way is probed on every run instead of cached. With the PHP-linked
default the probed binary is `ephpm` itself, so when the result is cached the
key covers the `ephpm` binary and replacing it (upgrading ePHPm) invalidates
the entry. A wrapper script set as `VIVACITY_PHP` is never cached, because the
PHP it runs does not report the wrapper as its binary.
`VIVACITY_NO_PLATFORM_CACHE=1` skips the cache for that run only: it neither
reads nor rewrites the file.

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
| `1` | Runtime error: missing `composer.json`, a lock without `packages-dev` (where Composer's locker fails), a platform probe that cannot run on `update`/`require`/`remove` (or an `install` with no lock), an unsupported repository type, a refused option or manifest, local I/O failures |
| `2` | Usage error (unknown command or option, or no command at all). Also Composer's code `2`: an unsolvable dependency set, a lock that does not contain a compatible set of packages on `install` (unmet platform requirements included), a lock blocked by policy, or `remove` leaving the package installed |
| `3` | Out of scope with no fallback possible (`--no-fallback`, `--check-scope`, or no `composer` on `PATH`), `--run-scripts` without `composer`, or a [verbosity hand-over](#verbosity) that cannot run. Also Composer's code `3` for a partial `update <packages>` with no `composer.lock` |
| `4` | On `install`: `composer.json` requires something the lock does not contain, or the lock cannot be installed without changes (`Your lock file cannot be installed on this system without changes`) |
| `100` | A fetch failed (a dist or repository that answers an HTTP error, is unreachable, or a local dist path that is missing), after Composer's retry policy (at most three retries, and only for the errors Composer retries, so a 404 fails at once). Composer exits `100` for the same failures |
| other | When vivacity delegates to Composer or runs scripts, the exit code of that process |

## Environment and caches

- **Composer variables honored:** `COMPOSER_HOME`, `COMPOSER_CACHE_DIR`,
  `COMPOSER_AUTH`, `COMPOSER_NO_DEV` (any value but empty or `0`, on every
  command including `dump-autoload`), `COMPOSER_PREFER_STABLE`,
  `COMPOSER_PREFER_LOWEST`, `COMPOSER_WITH_DEPENDENCIES`,
  `COMPOSER_WITH_ALL_DEPENDENCIES`, `COMPOSER_IGNORE_PLATFORM_REQ(S)`,
  `COMPOSER_FUND=0`, and the policy variables above. `SHELL_VERBOSITY` sets
  the [verbosity](#verbosity), as it does for Composer.
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
  are handled natively, but because they are active, `-v` hands the command to
  Composer ([Verbosity](#verbosity)).
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

vivacity's fallback, the verbosity hand-over and `--run-scripts` look for a
`composer` executable on `PATH`. They do not use `ephpm php composer.phar`, so
for those you need a `composer` (or `composer.bat` on Windows) wrapper on
`PATH`, for example:

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
