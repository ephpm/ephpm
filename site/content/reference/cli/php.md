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

# Bind a virtual host's database first, then run PHP (see "Binding a
# virtual host's database" below). --site/--config are ePHPm's own options
# and must come BEFORE the PHP program/args:
ephpm php --site <key> --config <path> [PHP_ARGS...]
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

## Binding a virtual host's database (`--site`)

In multi-tenant mode a request picks its tenant from the `Host` header; a CLI
invocation has none. `--site <key>` (with `--config <path>`) binds the
in-process [`ephpm_db_*` bridge](/guides/db-from-php/) to that vhost's database
before PHP runs, so `wp`, `artisan`, migrations, and seeders built on the
[`db-*` packages](/reference/php-packages/) reach the right tenant. Added in
[#471](https://github.com/ephpm/ephpm/issues/471).

| Option | Meaning |
|--------|---------|
| `--site <key>` | The vhost directory name under `[server] sites_dir`. Binds `ephpm_db_*` to that site's database. Validated fail-closed (an unknown or malformed key is rejected). |
| `--config <path>` | The ePHPm config file (`[server] sites_dir`, `[db.sqlite]`, `[kv] secret`). **Required** with `--site`. On its own it does nothing: `ephpm php --config <path> …` without `--site` prints a warning on stderr that `--config` has no effect without `--site`, ignores it, and runs PHP normally. |

Both are ePHPm's own options and must appear **before** the PHP program and its
arguments; they are `--long`-only (never `-c`, which stays php-cli's ini flag),
and everything after them — including a `--` separator and any `--site` a script
itself wants — is passed to PHP untouched. Without `--site`, `ephpm php` runs
with no database bound, exactly as before.

```bash
# Offline (no server) — opens the site's own .db file directly:
ephpm php --site shop --config /etc/ephpm/ephpm.toml -- wp --path=/srv/sites/shop db check

# Against a running server — forwards over its MySQL listener:
ephpm php --site shop --config /etc/ephpm/ephpm.toml -- vendor/bin/wp option get siteurl
```

At startup a strategy is chosen automatically:

- **A server is running** (its `[db.sqlite.proxy] mysql_listen` answers) → the
  CLI connects to it authenticating **as the site** and forwards raw SQL. The
  server does all translation, screening, query-stats, and (clustered)
  owner-forwarding. This is the correct path while the server holds the database
  file open. It requires `[kv] secret` to be set (so the CLI can derive the
  site's password — see the [multi-tenant pdo_mysql
  guide](/guides/multi-tenant-pdo-mysql/#the-wire-path-needs-kv-secret)); if it
  is unset the wire path fails closed with a clear message.
- **No server, single-node / per-site-single** → the CLI opens the site's own
  `<key>.db` directly (offline seeding).
- **No server, per-site clustered** → **refused**: a standalone CLI cannot reach
  the site's owner, and opening the local file could write to a replica whose
  writes never replicate.

`--site` requires multi-tenant per-site mode (`[server] sites_dir` + `[db.sqlite]`);
a single-shared-database config is rejected rather than bound.

Wrapping this in [`ephpm exec --site …`](../exec/) to run it inside the tenant's
Linux filesystem/uid sandbox **does not currently work** when the config lives
in `/etc/ephpm`. The sandbox denies that directory on purpose, so the inner
`ephpm php` cannot read its config. See
[`ephpm exec` → Database access](../exec/#database-access). Run
`ephpm php --site` as the operator instead.

## No KV store under `ephpm php`

`ephpm php` runs PHP without starting the server, so there is no KV store behind
the [`ephpm_kv_*` functions](/guides/kv-from-php/) (`--site` binds the database
bridge only). The functions are defined, but every call returns its failure
value (`null` / `false` / `0` / `-2`) and the first call in the process emits an
`E_WARNING` saying no KV store is attached. `session.save_handler = ephpm` is
backed by the same store, so under `ephpm php` it starts every session empty and
persists nothing (it does not emit that warning). To
read or write a running server's KV from a script, connect to its RESP listener
(`[kv.redis_compat]`) with a Redis client, or use [`ephpm kv`](../kv/).

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

`PHP_BINARY` is the path of the running `ephpm` executable, and running that
path the way php-cli is run (`PHP_BINARY script.php`, `PHP_BINARY -r …`, or
bare with a program on stdin) runs PHP — see [Re-invoking
`PHP_BINARY`](#re-invoking-php_binary) below.

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

### Re-invoking `PHP_BINARY`

Many libraries start a second PHP process by running `PHP_BINARY` with php-cli
arguments: spatie/async and anything built on Symfony Process
(`PhpExecutableFinder` returns `PHP_BINARY` under the `cli` SAPI), Composer's
`@php` script lines, PHPUnit's process isolation, parallel test runners. Under
ePHPm `PHP_BINARY` is the `ephpm` executable — a single-binary install has no
separate `php` file to name — so `ephpm` itself answers like php-cli when it is
run that way. Every argument then goes to the embedded PHP CLI verbatim,
exactly as `ephpm php <args…>`.

The binary takes the php-cli role when **any** of these holds:

1. **It is invoked under a php name.** If `argv[0]`'s file name is `php` or
   `php` plus a version (`php8`, `php8.4`, `php84`; `.exe` ignored on
   Windows), it *is* php-cli for every argument, including ones that look like
   ePHPm subcommands. This makes a symlink a complete php install:

   ```bash
   ln -s "$(command -v ephpm)" /usr/local/bin/php
   php -v                       # PHP 8.x.y (cli) ... — the embedded PHP
   php vendor/bin/phpunit       # no `ephpm php` prefix needed
   ```

   (On Windows, use a hard link or a copy named `php.exe`.)

2. **It is a child of `ephpm php` or `ephpm composer`** and is not asked for a
   real ePHPm subcommand. Both subcommands export `EPHPM_PHP_CLI=1` into their
   own environment before PHP (or vivacity) starts, so every process they spawn
   inherits it. A child `ephpm` that sees the marker treats its arguments as
   php's — including no arguments at all (program on stdin) and `-v`, `-h`,
   `--version`, which then mean what they mean to php. An argument that names
   an `ephpm` subcommand (`ephpm kv …`, `ephpm cache purge`, `ephpm help`)
   still runs that subcommand, so scripts that drive ePHPm keep working.

3. **Its first argument is php-shaped**, even without the marker (a library
   that passes `proc_open()` a replacement environment drops it): an option
   ePHPm does not have (anything starting with `-` except `-h`, `--help`,
   `-V`, `--version`), or an existing file that is not a subcommand name. These
   argv shapes were usage errors before, so nothing that used to work changes
   meaning; a typo such as `ephpm sevre` still gets ePHPm's "did you mean
   `serve`" error.

What the rules cannot recover: **bare `ephpm` without the marker** is still
the dev server (`ephpm dev`), by contract. A tool that runs `PHP_BINARY` with
no arguments *and* a scrubbed environment would start a server; set
`EPHPM_PHP_CLI=1` in the environment you pass, or install the `php` symlink
and point the tool at it. And a PHP script file whose name is exactly an ePHPm
subcommand (`serve`, `dev`, `install`, …) must be passed with a path
(`./serve`), as it would be under the marker rules above.

The re-invoked process is a fresh `ephpm php`: it does not inherit
`--site`/`--config`, so a child of `ephpm php --site shop …` has no tenant
database bound.

> **Added after v0.11.2.** Before, re-invoking `PHP_BINARY` reached `ephpm`'s
> own argument parser: `PHP_BINARY script.php` was a usage error, and a bare
> `PHP_BINARY` fed a program on stdin started the dev server.

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

### Reflection flags

The five php-cli reflection flags are implemented against the always-compiled
Reflection extension and follow php-cli's own dispatch:

| Flag | Subject | Prints |
|------|---------|--------|
| `--rf` / `--rfunction` | `function`, or `Class::method` | `ReflectionFunction` — or `ReflectionMethod` when the name contains `::` |
| `--rc` / `--rclass` | class name | `ReflectionClass` |
| `--re` / `--rextension` | extension name | `ReflectionExtension` |
| `--rz` / `--rzendextension` | Zend extension name | `ReflectionZendExtension` |
| `--ri` / `--rextinfo` | extension name, or `main` | the extension's `phpinfo` block — or, for `main`, the core ini table |

```bash
ephpm php --rf strlen                 # Function [ <internal:Core> function strlen ] { … }
ephpm php --rf ArrayObject::count     # Method [ <internal:SPL, …> public method count ] { … }
ephpm php --ri json                   # json support => enabled
ephpm php --ri main                   # Directive => Local Value => Master Value …
```

`main` is not an extension: it is php-cli's magic name for
`display_ini_entries(NULL)`, the table of core ini directives with their local
and master values. It is only consulted after the module registry misses, so a
real extension named `main` would still win.

A subject that does not resolve prints `Exception: <message>` on stdout — not
an uncaught-exception fatal — and exits `1`. `--ri` on an extension that is not
loaded prints `Extension '<name>' not present.` and also exits `1`, so
`ephpm php --ri <ext> >/dev/null` is a usable presence probe. A flag that
succeeds exits `0`.

On PHP 8.5, `--rf Class::method` emits a `Deprecated: Calling
ReflectionMethod::__construct() with 1 argument is deprecated …` notice on
**stdout** before the dump. That is php-cli's output too, verbatim — including
the `in Unknown on line 0` context — because php-cli constructs the object the
same way.

> **Fixed after v0.9.1.** Up to and including v0.9.1, `--rf Class::method`
> always built a `ReflectionFunction`, so every method name came back as
> `Exception: Function Class::method() does not exist`; `--ri` on an absent
> extension printed the right diagnostic but exited `0`, so a script probing
> `php --ri <ext>` saw every extension as present; and `--ri main` was treated
> as an extension name and reported absent
> ([#358](https://github.com/ephpm/ephpm/issues/358)).

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
`Exception: <message>` on stdout and yields `1`, and `--ri` on an extension
that is not loaded yields `1` as well (see [Reflection
flags](#reflection-flags)).

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
- [`ephpm exec`](../exec/) — run a command in a vhost's tenant sandbox (Linux)
