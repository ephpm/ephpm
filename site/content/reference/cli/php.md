+++
title = "ephpm php"
weight = 2
+++

Run the embedded PHP CLI. All arguments after `php` are passed straight through to the embedded interpreter — no shell wrapping, no separate PHP install required.

## Synopsis

```bash
ephpm php [PHP_ARGS...]
```

## Examples

```bash
# Inline expression
ephpm php -r 'echo PHP_VERSION;'

# Run a script
ephpm php script.php

# Laravel
ephpm php artisan migrate
ephpm php artisan tinker

# Print loaded modules
ephpm php -m
```

## Phar support and the SAPI caveat

The `phar` extension is compiled in, and `.phar` archives load and execute:
`ephpm php app.phar` runs the archive's stub, and `phar://` streams work.

However, `ephpm php` reports `PHP_SAPI` as `ephpm`, not `cli` — and some
popular phar tools refuse to run on any SAPI other than `cli`:

- **WP-CLI** (`wp-cli.phar`) exits immediately: *"WP-CLI only works correctly
  from the command line, using the 'cli' PHP SAPI."*
- **Composer** (`composer.phar`) aborts: *"Composer cannot be run safely on
  non-CLI SAPIs with register_argc_argv=On."*

For those tools, use a stock `php` CLI binary. Phars that don't gate on
`PHP_SAPI` run fine under `ephpm php`.

## Why use `ephpm php`?

The embedded PHP interpreter is the **same** runtime that serves HTTP requests. Running CLI commands through `ephpm php` means:

- One PHP version to install — the one baked into the binary
- Same compiled-in extensions: `bcmath, calendar, ctype, curl, dom, exif, fileinfo, filter, gd, hash, iconv, mbstring, mysqli, mysqlnd, openssl, pcntl, pcre, pdo, pdo_mysql, phar, posix, session, simplexml, sodium, tokenizer, xml, xmlreader, xmlwriter, zip, zlib`
- Same `php.ini` overrides as the server (from `[php] ini_overrides` in your config)
- No drift between dev, CI, and production PHP versions

## How it works

`ephpm php` runs the embedded PHP runtime (the `ephpm` SAPI) in CLI mode via FFI. It's not a wrapper around an external `php` binary. The argument list is forwarded as-is, including `-r`, `-d`, file paths, and trailing application arguments — script arguments are registered as `$argv`/`$argc` (and mirrored into `$_SERVER`, with `PHP_SELF`/`SCRIPT_NAME`/`SCRIPT_FILENAME` set to the script path) exactly as the stock `php` CLI does, so Symfony Console and artisan see their arguments. Shebang lines (`#!/usr/bin/env php`) are skipped.

## Windows note

On Windows, PHP is statically linked into `ephpm.exe` (`php8embed.lib`, static CRT) — no DLL is embedded, extracted, or written to disk at runtime. There's nothing to install; `ephpm php` works out of the box.

## See also

- [`ephpm serve`](../serve/) — the server command
- [`ephpm kv`](../kv/) — KV store debugging
