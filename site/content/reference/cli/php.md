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
ephpm php [options] -- [args...]        # program from stdin, with args
echo '<?php ...' | ephpm php            # program from stdin
```

## Examples

```bash
# Inline expression
ephpm php -r 'echo PHP_VERSION;'

# Run a script
ephpm php script.php

# Program from stdin (also `ephpm php < file`)
echo '<?php echo 2 + 2;' | ephpm php

# ...with arguments: everything after `--` belongs to the script
echo '<?php var_dump($argv);' | ephpm php -- one two

# Composer / WP-CLI / artisan — all work unmodified
ephpm php -d memory_limit=-1 composer.phar install
ephpm php wp-cli.phar --info
ephpm php artisan migrate

# Per-line stdin processor (awk-like): $argn = line, $argi = line number (1-based)
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
  They are opened through the `php://stdin` / `php://stdout` / `php://stderr`
  wrappers exactly as php-cli opens them, so `stream_get_meta_data()` reports
  `wrapper_type = "PHP"` and the matching `php://…` uri — what code that
  sniffs a handle's provenance expects
  ([#340](https://github.com/ephpm/ephpm/issues/340), fixed in v0.7.4).
- **Laravel artisan** / any Symfony Console app — `$argv`/`$argc` are
  registered, so subcommands and options are parsed.
- **cli-SAPI-only functions** — `cli_set_process_title()` /
  `cli_get_process_title()` exist (PsySH calls them, so `artisan tinker`
  works). The title genuinely changes on Linux (`/proc/self/cmdline`, `ps`)
  and sets the console title on Windows, mirroring php-src's `ps_title.c`; on
  other platforms the functions exist but honestly return `false`/`null` with
  php's own "Not available on this OS" warning.

The server (`ephpm serve` / `ephpm dev`) is a separate process invocation and
keeps reporting `PHP_SAPI === "ephpm"` — only the `ephpm php` process is `cli`.

### `$_SERVER`, the environment, and `PHP_BINARY`

Like php-cli, the CLI `$_SERVER` **contains the whole process environment**
(php-cli's default `variables_order` is `EGPCS`, and a CLI process's "S" is its
environment), plus an empty-string `DOCUMENT_ROOT` — there is no document root
in CLI mode, but code that reads the key unconditionally must not warn. This
matters in practice: Composer probes `$_SERVER['HOME']` / `$_SERVER['APPDATA']`
before falling back to `getenv()`.

`PHP_BINARY` is the path of the running `ephpm` executable. Note the
consequence for tools that *re-invoke* PHP through it (PHPUnit's process
isolation, Symfony's `PhpExecutableFinder`): the path is `ephpm`, and running
it without the `php` subcommand is not a PHP CLI. Point such tools at a
wrapper script that execs `ephpm php "$@"` if you need re-invocation.

This applies to `ephpm php` only. In the server the process environment is
deliberately **not** merged into a request's `$_SERVER`: `ephpm serve` is
multi-tenant, and the process environment holds cross-tenant material.

> **Fixed in v0.7.4.** Up to and including v0.7.3, CLI `$_SERVER` carried
> neither the environment nor `DOCUMENT_ROOT`
> ([#338](https://github.com/ephpm/ephpm/issues/338)), and `PHP_BINARY` was an
> empty string on Linux and macOS
> ([#339](https://github.com/ephpm/ephpm/issues/339) — the embed SAPI's
> `executable_location` was the bare name `ephpm`, which php-src then hunted
> for on `PATH` and did not find; Windows was unaffected because php-src asks
> the OS there instead).

### Ini flags

- `-d key[=value]` — set an ini directive for this run (`-d key` means
  `key=1`). Applied at module startup exactly as php-cli applies it, so it
  overrides `php.ini` and works for startup-only directives too:
  `-d opcache.enable_cli=1` activates OPcache, the `opcache.jit*` knobs enable
  the JIT, and `-d extension=` / `-d zend_extension=` load extensions — in
  addition to the runtime-changeable directives (`memory_limit`,
  `error_reporting`, `disable_functions`, …).
- `-c <path>` — load `php.ini` from this path.
- `-n` — load no `php.ini` at all (`-d` still applies, as in php-cli).

### Programs on stdin

With no `-r`, no `-B`/`-R`/`-F`/`-E` line mode, and no script named on the
command line, the program is read from **stdin** — the same rule stock php-cli
uses, so `ephpm php < script.php` and `… | ephpm php` both work. Everything
after `--` is treated as script arguments rather than a filename, so
`… | ephpm php -- a b` still reads the program from stdin.

(Any one of `-B`/`-R`/`-F`/`-E` selects the line processor instead, where stdin
is consumed as *input lines* rather than compiled as a program — `$argn` is the
current line and `$argi` its 1-based line number.)

A stdin program is a real script: it needs `<?php` tags (unlike `-r`, which is
raw code), and it is identified as php-cli identifies it — `$argv[0]`,
`$_SERVER['PHP_SELF']` and `$_SERVER['SCRIPT_NAME']` are the literal string
`Standard input code`, while `SCRIPT_FILENAME`/`PATH_TRANSLATED` stay empty
because no file was executed. The same identity applies to `-r`.

As in stock php-cli there is **no `-` stdin sentinel**: `ephpm php -` reports
`Could not open input file: -` and exits 1, exactly as `php -` does.

`-l` (lint), `-w` (strip comments/whitespace) and `-s` (syntax highlight) also
read stdin when no file is named:

```bash
ephpm php -l < src/Controller.php      # No syntax errors detected in Standard input code
cat src/Controller.php | ephpm php -w  # stripped source on stdout
```

### Errors and exit codes

Diagnostics go where php-cli sends them. `display_errors` defaults to on and
routes to **stdout** (php-cli's CLI default), so a fatal error, an uncaught
exception or a parse error prints the same `Fatal error:` / `Parse error:`
block php-cli prints — including the file/line and the stack trace — whether
the code came from `-r`, a script file, or stdin. Set `-d display_errors=stderr`
to route them to stderr instead.

Exit codes match php-cli: `exit(N)` yields `N`; an uncaught exception or fatal
error yields `255`; a syntax error under `-l` yields `255` (with
`Errors parsing <name>` on stdout); a script file that cannot be opened prints
`Could not open input file: <path>` and yields `1`; a reflection flag whose
subject does not resolve (`--rf nosuchfunc`, `--rc NoSuchClass`) prints
`Exception: <message>` on stdout and yields `1`.

An **unrecognized option** — or one missing its required argument — is a hard
error, again as in php-cli: the diagnostic
(`Error in argument 2, char 2: option not found Z`) goes to stderr, the usage
screen to stdout, and the process exits `1`. Nothing is executed.

```bash
ephpm php -Z; echo $?     # usage on stdout, "option not found Z" on stderr, 1
```

> **Fixed in v0.7.4.** Up to and including v0.7.3, an unknown option was
> silently ignored: no mode was selected, the CLI fell through to reading
> (empty) stdin as a program, and a typo'd flag ran nothing while reporting
> success ([#336](https://github.com/ephpm/ephpm/issues/336)). In the same
> release, `--rf`/`--rc` on a missing symbol stopped exiting `0`
> ([#335](https://github.com/ephpm/ephpm/issues/335)).

The usage screen itself is not byte-identical to php-cli's: it names the
program `ephpm php`, and it lists only the options this build implements — the
built-in server (`-S`/`-t`), `--repeat` and `--ini=diff` are absent because
they are [not supported](#not-supported).

> **Fixed in v0.7.1.** Up to and including v0.7.0, a fatal error or uncaught
> exception under `ephpm php -r` printed **nothing** and exited `0`
> ([#321](https://github.com/ephpm/ephpm/issues/321)) — the snippet was
> evaluated without exception handling, so the thrown `Error`/`Exception` was
> never reported and never set the exit status. Both halves are now pinned by
> the `cli` E2E suite.

### End of script

Script teardown follows php-cli's end-of-request order: callbacks registered
with `register_shutdown_function()` run first (in registration order,
including callbacks registered *during* shutdown), then destructors of
objects still alive at script end, then output is flushed. Shutdown functions
run even after an uncaught exception or fatal error, exactly as in php-cli.
`exit(N)` inside a shutdown function sets the process exit status and
overrides the script's own `exit()` — including `exit(0)` clearing a nonzero
status.

> **Fixed in v0.7.2.** Up to and including v0.7.1, `ephpm php` skipped its
> end-of-request teardown while the CLI output path was live
> ([#334](https://github.com/ephpm/ephpm/issues/334)): shutdown functions and
> end-of-script destructors ran invisibly at process exit, and `exit()` inside
> a shutdown function could not set the exit status. Pinned by the `cli` E2E
> suite and the CLI conformance corpus (cases 021/036).

### Not supported

- `-a` (interactive shell) — the PHP interactive shell is part of the standalone
  php-cli SAPI, which is not linked into ePHPm's embedded runtime. `ephpm php -a`
  prints a clear error rather than doing nothing. Use `-r`, a script, or stdin.
- `-S <addr>` (PHP built-in server) — the cli-server SAPI is not linked into the
  embed build, so `ephpm php -S` prints a clear error. It is deliberately **not**
  aliased to `ephpm serve`. Use a full php-cli for `php -S`, or `ephpm serve` /
  `ephpm dev` for ePHPm's own HTTP server.
- `-z <file>` (load a Zend extension) — not offered, matching current php-cli,
  which also rejects it. Zend extensions must be present at module startup;
  register them through `[php] extensions` in your config.

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

`ephpm php` runs the embedded PHP runtime in CLI mode via FFI — the same runtime that serves HTTP requests, but with the SAPI reporting as `cli`. It's not a wrapper around an external `php` binary. The argument list is forwarded as-is, including `-r`, `-d`, file paths, and trailing application arguments — script arguments are registered as `$argv`/`$argc` (and mirrored into `$_SERVER`, with `PHP_SELF`/`SCRIPT_NAME`/`SCRIPT_FILENAME` set to the script path) exactly as the stock `php` CLI does, so Symfony Console and artisan see their arguments. When no script file is named — a stdin program, or `-r` — the identity is `Standard input code` instead, again matching php-cli (see [Programs on stdin](#programs-on-stdin)). Scripts are read byte-exactly from a binary-mode file handle, and shebang lines (`#!/usr/bin/env php`) are skipped.

## Windows note

On Windows, PHP is statically linked into `ephpm.exe` (`php8embed.lib`, static CRT) — no DLL is embedded, extracted, or written to disk at runtime. There's nothing to install; `ephpm php` works out of the box.

## See also

- [`ephpm serve`](../serve/) — the server command
- [`ephpm kv`](../kv/) — KV store debugging
