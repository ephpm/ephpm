+++
title = "ephpm composer"
weight = 3
+++

Run [Composer](https://getcomposer.org/) through the embedded
[vivacity](https://github.com/Adelagric/vivacity) installer — a pure-Rust,
Composer-compatible dependency installer built into the `ephpm` binary. Every
argument after `composer` is forwarded verbatim to vivacity, and the process
exits with the installer's own exit code.

vivacity is a Rust reimplementation of Composer with byte-parity dependency
resolution. It needs **no PHP runtime** to resolve and install dependencies —
it only shells out to a real `composer` for the source-package fallback — so
`ephpm composer` works in **every** build, including stub mode (no linked PHP),
and on **Windows**. There is no separate `composer.phar` to download.

## Synopsis

```bash
ephpm composer <COMMAND> [ARGS...]
ephpm composer --version
ephpm composer --help
```

## Examples

```bash
# Install locked dependencies — arguments pass through untouched
ephpm composer install --no-dev --optimize-autoloader

# Resolve dependencies and write composer.lock
ephpm composer update

# Add / remove packages
ephpm composer require monolog/monolog
ephpm composer remove monolog/monolog

# Regenerate the autoloader
ephpm composer dump-autoload -o

# Version of the embedded installer
ephpm composer --version    # → vivacity <x.y.z>
```

## Argument passthrough

The subcommand captures **all** trailing arguments verbatim
(`trailing_var_arg` + `allow_hyphen_values` in clap) and disables clap's own
help flag, so hyphen-led Composer options are never intercepted by `ephpm`.
Before dispatch, a synthetic `composer` program-name element is prepended (the
installer's argument parser expects `argv[0]` to be the program name, as in a
normal CLI), then control is handed to `vivacity::run`. Nothing else is
rewritten — `ephpm composer install --no-dev --optimize-autoloader` reaches
vivacity exactly as typed.

## Exit codes

`ephpm composer` exits with the exact status vivacity returns, so it drops
straight into CI and shell pipelines. A usage error (an unknown subcommand, or
no subcommand at all) exits `2`, matching the underlying clap parser;
`--version` and `--help` exit `0`.

## Relationship to `ephpm php`

`ephpm composer` is **not** the stock Composer PHAR running under
[`ephpm php`](../php/). It is a separate, PHP-free installer compiled into the
binary. If you specifically need the canonical PHP `composer.phar` — for a
plugin or script that only the reference Composer implementation supports — you
can still run it through the embedded PHP CLI:

```bash
ephpm php -d memory_limit=-1 composer.phar install
```

## See also

- [`ephpm php`](../php/) — the embedded PHP CLI (runs `composer.phar` too)
- [vivacity](https://github.com/Adelagric/vivacity) — the embedded installer
