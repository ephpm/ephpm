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

# WordPress (WP-CLI)
ephpm php wp-cli.phar plugin list
ephpm php wp-cli.phar user list

# Composer (also a phar)
ephpm php composer.phar install

# Print loaded modules
ephpm php -m
```

## Why use `ephpm php`?

The embedded PHP interpreter is the **same** runtime that serves HTTP requests. Running CLI commands through `ephpm php` means:

- One PHP version to install — the one baked into the binary
- Same compiled-in extensions: `bcmath, calendar, ctype, curl, dom, exif, fileinfo, filter, gd, hash, iconv, mbstring, mysqli, mysqlnd, openssl, pcntl, pcre, pdo, pdo_mysql, phar, posix, session, simplexml, sodium, tokenizer, xml, xmlreader, xmlwriter, zip, zlib`
- Same `php.ini` overrides as the server (from `[php] ini_overrides` in your config)
- No drift between dev, CI, and production PHP versions

## How it works

`ephpm php` invokes PHP's CLI SAPI directly via FFI. It's not a wrapper around an external `php` binary. The argument list is forwarded as-is, including `-r`, `-d`, file paths, and trailing application arguments.

## Windows note

The first call extracts `php8embed.dll` from the binary into a temp directory before invoking PHP. The DLL is removed when the command exits. This is invisible — there's nothing to install.

## See also

- [`ephpm serve`](../serve/) — the server command
- [`ephpm kv`](../kv/) — KV store debugging
