+++
title = "ephpm php"
weight = 2
+++

Run the embedded PHP CLI. All arguments after `php` are passed straight through to the embedded interpreter — no shell wrapping, no separate PHP install required.

`ephpm php` is a drop-in for the stock `php` CLI: it reports `PHP_SAPI` as
`cli`, reads programs from stdin, applies `-d`/`-c`/`-n`, supports the
`-B`/`-R`/`-F`/`-E` line processor, and defines the `STDIN`/`STDOUT`/`STDERR`
constants. You can safely `alias php="ephpm php"` and run Composer, WP-CLI,
Laravel's artisan, and PHPUnit through it.

## Synopsis

```bash
ephpm php [PHP_ARGS...]
ephpm php [options] [-f] <file> [--] [args...]
ephpm php [options] -r <code> [--] [args...]
echo '<?php ...' | ephpm php            # program from stdin
```

## Examples

```bash
# Inline expression
ephpm php -r 'echo PHP_VERSION;'

# Run a script
ephpm php script.php

# Program from stdin (also `ephpm php -` and `ephpm php < file`)
echo '<?php echo 2 + 2;' | ephpm php

# Composer / WP-CLI / artisan — all work unmodified
ephpm php -d memory_limit=-1 composer.phar install
ephpm php wp-cli.phar --info
ephpm php artisan migrate

# Per-line stdin processor (awk-like): $argn = line, $argi = 0-based index
printf 'a\nbb\n' | ephpm php -R 'echo strlen($argn), "\n";'

# Print loaded modules
ephpm php -m
```

## Drop-in for the stock `php` CLI

`ephpm php` reports **`PHP_SAPI === "cli"`** (and `php_sapi_name()` returns
`"cli"`), so the near-universal `if (PHP_SAPI !== 'cli') { die(...); }` guard and
tools that gate on the SAPI name run normally:

- **Composer** (`composer.phar`) — runs; `-d memory_limit=-1` is honored.
- **WP-CLI** (`wp-cli.phar`) — runs; `STDIN`/`STDOUT`/`STDERR` are defined.
- **Laravel artisan** / any Symfony Console app — `$argv`/`$argc` are
  registered, so subcommands and options are parsed.

The server (`ephpm serve` / `ephpm dev`) is a separate process invocation and
keeps reporting `PHP_SAPI === "ephpm"` — only the `ephpm php` process is `cli`.

### Ini flags

- `-d key[=value]` — set an ini directive for this run (`-d key` means
  `key=1`). Applied after startup, so `PHP_INI_ALL`/`PHP_INI_SYSTEM` directives
  such as `memory_limit`, `error_reporting`, and `disable_functions` take
  effect. Directives that only apply at module startup (most `opcache.*`, and
  `-d extension=`) cannot be set this way — register extensions via `[php]
  extensions` in your config instead.
- `-c <path>` — load `php.ini` from this path.
- `-n` — load no `php.ini` at all.

### Not supported

- `-a` (interactive shell) — the PHP interactive shell is part of the standalone
  php-cli SAPI, which is not linked into ePHPm's embedded runtime. `ephpm php -a`
  prints a clear error rather than doing nothing. Use `-r`, a script, or stdin.
- `-S <addr>` (PHP built-in server) — the cli-server SAPI is not linked into the
  embed build, so `ephpm php -S` prints a clear error. It is deliberately **not**
  aliased to `ephpm serve`. Use a full php-cli for `php -S`, or `ephpm serve` /
  `ephpm dev` for ePHPm's own HTTP server.

## Phar support

The `phar` extension is compiled in, and `.phar` archives load and execute:
`ephpm php app.phar` runs the archive's stub, and `phar://` streams work.

## Why use `ephpm php`?

The embedded PHP interpreter is the **same** runtime that serves HTTP requests. Running CLI commands through `ephpm php` means:

- One PHP version to install — the one baked into the binary
- Same compiled-in extensions: `bcmath, calendar, ctype, curl, dom, exif, fileinfo, filter, gd, hash, iconv, mbstring, mysqli, mysqlnd, openssl, pcntl, pcre, pdo, pdo_mysql, phar, posix, session, simplexml, sodium, tokenizer, xml, xmlreader, xmlwriter, zip, zlib`
- Same `php.ini` overrides as the server (from `[php] ini_overrides` in your config)
- No drift between dev, CI, and production PHP versions

## How it works

`ephpm php` runs the embedded PHP runtime in CLI mode via FFI — the same runtime that serves HTTP requests, but with the SAPI reporting as `cli`. It's not a wrapper around an external `php` binary. The argument list is forwarded as-is, including `-r`, `-d`, file paths, and trailing application arguments — script arguments are registered as `$argv`/`$argc` (and mirrored into `$_SERVER`, with `PHP_SELF`/`SCRIPT_NAME`/`SCRIPT_FILENAME` set to the script path) exactly as the stock `php` CLI does, so Symfony Console and artisan see their arguments. A program read from stdin gets `$argv[0] === "-"`, matching php-cli. Shebang lines (`#!/usr/bin/env php`) are skipped.

## Windows note

On Windows, PHP is statically linked into `ephpm.exe` (`php8embed.lib`, static CRT) — no DLL is embedded, extracted, or written to disk at runtime. There's nothing to install; `ephpm php` works out of the box.

## See also

- [`ephpm serve`](../serve/) — the server command
- [`ephpm kv`](../kv/) — KV store debugging
