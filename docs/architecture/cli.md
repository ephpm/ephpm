# ePHPm CLI Architecture

Single binary, all commands. Built with `clap` (Rust).

```
ephpm <command> [subcommand] [flags]
```

---

## Core Commands

### `ephpm serve`

Start the PHP application server. This is the primary command — what runs in production.

```bash
# Start with config file (default: ./ephpm.toml)
ephpm serve

# Explicit config path
ephpm serve --config /etc/ephpm/ephpm.toml

# Override listen address
ephpm serve --listen 0.0.0.0:443

# Embed admin UI on this node (dev convenience)
ephpm serve --admin

# Foreground with log level
ephpm serve --log-level debug

# Specific PHP worker count (overrides config)
ephpm serve --workers 16

# Daemonize (background, writes PID file)
ephpm serve --daemon --pid-file /var/run/ephpm.pid

# Test mode — embedded SQLite, no external DB needed (see Development & Testing)
ephpm serve --test
ephpm serve --test --db-memory      # in-memory SQLite (fastest, data lost on exit)
ephpm serve --test --db-temp        # temp file SQLite (cleaned up on exit)
```

**What it starts:**
- HTTP server (`:443` by default, `:80` for HTTP→HTTPS redirect)
- PHP worker pool
- DB proxy (if configured) — or embedded SQLite in `--test` mode
- KV store (if configured)
- OTLP receiver (`:4317` gRPC, `:4318` HTTP — if configured)
- Gossip listener (`:7946` — if clustering configured)
- Node API (`:9090` — always)
- Admin UI (`:8080` — only with `--admin`)

**Graceful shutdown:** `SIGTERM` or `SIGINT` → drains in-flight requests, closes DB connections, leaves cluster gracefully, writes KV snapshot (if persistence enabled). On Windows, `Ctrl+C` and `Ctrl+Break` trigger graceful shutdown via `SetConsoleCtrlHandler`.

**Graceful reload:** `SIGHUP` → reloads `ephpm.toml`, restarts PHP workers (rolling — no dropped requests), updates DB pool sizes, refreshes TLS config. Does NOT restart the Rust process. On Windows (which has no `SIGHUP`), use `ephpm reload` which connects to the running instance's Node API to trigger the reload.

---

### `ephpm admin`

Start the admin UI as a standalone instance. Connects to one or more serving nodes via their Node API.

```bash
# Connect to specific nodes
ephpm admin --nodes 10.0.1.1:9090,10.0.1.2:9090,10.0.1.3:9090

# With config file (nodes listed in [admin] section)
ephpm admin --config /etc/ephpm/admin.toml

# Custom listen address
ephpm admin --listen 0.0.0.0:8080

# With Node API auth
ephpm admin --nodes 10.0.1.1:9090 --secret your-shared-secret
```

**What it starts:**
- Admin web UI (`:8080` by default)
- Node connector (polls/streams Node API from each configured node)

**Does NOT start:** PHP workers, DB proxy, KV store, HTTP server, OTLP receiver. Zero PHP-related resource usage.

---

### `ephpm stop`

Signal a running ePHPm instance to shut down gracefully.

```bash
# Stop via PID file
ephpm stop --pid-file /var/run/ephpm.pid

# Stop via signal to process
ephpm stop --pid 12345
```

Sends `SIGTERM`. The running instance drains requests and exits cleanly.

---

### `ephpm reload`

Signal a running instance to reload configuration without downtime.

```bash
ephpm reload --pid-file /var/run/ephpm.pid
```

Sends `SIGHUP`. The running instance reloads `ephpm.toml` and performs a rolling restart of PHP workers.

---

## Configuration Commands

### `ephpm init`

Scaffold a new `ephpm.toml` with sensible defaults and commented documentation.

```bash
# Interactive — asks about DB, clustering, etc.
ephpm init

# Generate minimal config
ephpm init --minimal

# Generate full config with all options documented
ephpm init --full

# Specify output path
ephpm init --output /etc/ephpm/ephpm.toml
```

Generates something like:

```toml
# ePHPm Configuration
# Docs: https://ephpm.dev/docs/config

[server]
listen = "0.0.0.0:443"
http_redirect = true          # redirect :80 → :443
workers = 0                   # 0 = auto (num_cpus)
worker_max_requests = 0       # 0 = unlimited (restart after N requests for leak protection)

[php]
root = "./public"
entry = "index.php"           # for worker mode

[tls]
acme_email = ""               # required for auto TLS
# domains = ["example.com"]   # optional, auto-detected from requests

# [db.sqlite]
# path = "./data/app.db"         # file path, or ":memory:" for in-memory
# journal_mode = "wal"           # WAL mode for better read concurrency
# create = true                  # auto-create DB file if missing

# [db.mysql]
# url = "mysql://user:pass@db:3306/myapp"
# max_connections = 50

# [db.postgres]
# url = "postgres://user:pass@db:5432/myapp"
# max_connections = 30

# [cluster]
# enabled = false
# bind = "0.0.0.0:7946"
# join = ["10.0.1.2:7946"]

[node_api]
listen = "0.0.0.0:9090"
# secret = ""                 # set this in production
```

---

### `ephpm validate`

Check configuration for errors without starting the server.

```bash
ephpm validate
ephpm validate --config /etc/ephpm/ephpm.toml
```

Validates:
- TOML syntax
- Required fields present
- DB URLs parseable
- Port conflicts (HTTP, DB proxy, Node API, OTLP, gossip — all on different ports)
- PHP root directory exists
- TLS cert paths valid (if manual certs)
- Cluster seed nodes resolvable

```
$ ephpm validate
✓ Config loaded from ./ephpm.toml
✓ PHP root ./public exists
✓ DB MySQL URL valid
✓ No port conflicts
✓ Node API secret set
✗ TLS: acme_email is empty — auto TLS will not work
```

---

### `ephpm config`

Show the effective running configuration (with defaults applied, secrets redacted).

```bash
# Show effective config as TOML
ephpm config

# Show specific section
ephpm config server
ephpm config db

# Query from a running instance's Node API
ephpm config --node 10.0.1.1:9090
```

---

## Extension Management

### The Extension Problem

PHP is built around extensions — `gd` for images, `redis` for caching, `imagick` for thumbnails, `intl` for i18n. The PHP ecosystem assumes you can `pecl install` whatever you need.

ePHPm embeds PHP as a statically linked library (`libphp.a`). Extensions are compiled directly into the binary. Unlike a standard PHP installation where you drop a `.so` file into a directory, ePHPm's extensions are fixed at build time. This is what allows fully static binaries with zero runtime dependencies on every platform.

**How competitors handle this:**

- **RoadRunner / Swoole** — sidestep the problem entirely. They use a standard PHP installation on the system, so extensions work the normal way (`pecl install`, `apt install php-redis`, etc.). The tradeoff: they require a full PHP installation on the target machine.
- **FrankenPHP** — has the same problem. Their solution: ship a "mostly static" binary linked against glibc so that `.so` extensions can be loaded at runtime. This forces a glibc dependency and breaks `FROM scratch` / Alpine containers.

**ePHPm's approach:** All extensions are statically compiled into the binary. No runtime loading, no glibc dependency, no `.so` files to manage. Instead, we solve the "I need extension X" problem with:

1. **Pre-built suite binaries** — download a binary with extensions for your framework
2. **Custom builder** — rebuild the binary with exactly the extensions you need via a container

This keeps the single-binary, zero-dependency model intact on every platform.

### How It Works

ePHPm publishes multiple binary variants per platform, each with a different set of statically compiled extensions:

**Production suites:**

| Suite | Extensions | Binary size (approx) | Use case |
|---|---|---|---|
| **core** | ~15 exts (json, pcre, mbstring, openssl, curl, xml, zip, zlib, session, fileinfo, filter, dom, phar, tokenizer, sodium) | ~30 MB | Minimal base, add what you need via custom build |
| **wordpress** | core + mysqli, gd, exif, iconv, simplexml, xmlreader, pdo_sqlite, sqlite3 (~25 exts) | ~40 MB | WordPress and similar CMS apps |
| **laravel** | core + pdo_mysql, pdo_pgsql, pdo_sqlite, sqlite3, redis, gd, intl, bcmath, iconv (~30 exts) | ~70 MB | Laravel, Symfony, and modern PHP frameworks |
| **full** | Everything static-php-cli supports (~100+ exts) | ~150 MB | Kitchen sink — when you don't want to think about it |

**Development suites:**

Each production suite has a corresponding `-dev` variant that adds debugging and profiling tools:

| Suite | Adds on top of production suite | Use case |
|---|---|---|
| **wordpress-dev** | xdebug, pcov, spx | Local WordPress development with step debugging and coverage |
| **laravel-dev** | xdebug, pcov, spx | Local Laravel/Symfony development |
| **full-dev** | xdebug, pcov, spx, excimer | Development with all extensions |

Dev suites include Zend extensions (xdebug, pcov) that are normally impossible to statically compile. ePHPm's builder patches these into PHP's source tree before building, the same way PHP's own opcache (also a Zend extension) is built statically. See [Zend Extensions](#zend-extensions-xdebug-pcov-spx) for details.

**Dev suites should never be used in production** — xdebug adds significant overhead to every request, and pcov instruments code paths. The separation is intentional.

Users pick the suite that fits or use the custom builder.

### Release Naming

```
# Production suites
ephpm-0.1.0-php8.4-core-linux-x86_64
ephpm-0.1.0-php8.4-wordpress-linux-x86_64
ephpm-0.1.0-php8.4-laravel-linux-x86_64
ephpm-0.1.0-php8.4-full-linux-x86_64

# Development suites
ephpm-0.1.0-php8.4-wordpress-dev-linux-x86_64
ephpm-0.1.0-php8.4-laravel-dev-linux-x86_64
ephpm-0.1.0-php8.4-full-dev-linux-x86_64

# Other platforms
ephpm-0.1.0-php8.4-wordpress-macos-aarch64
ephpm-0.1.0-php8.4-wordpress-dev-macos-aarch64
ephpm-0.1.0-php8.4-laravel-windows-x86_64.exe
ephpm-0.1.0-php8.4-laravel-dev-windows-x86_64.exe
# ... etc for each PHP version × suite × platform
```

### Fully Static on Every Platform

Because all extensions are compiled in at build time, there is no need for `dlopen()` or `LoadLibrary()`. This means:

- **Linux:** Fully static musl binaries. Zero runtime dependencies. Works on Alpine, `FROM scratch`, any distro.
- **macOS:** Statically linked against libphp. Only system `libSystem.dylib` required (always present — Apple mandates it).
- **Windows:** Statically linked against `php8embed.lib` with static CRT (`/MT`). No DLL dependencies beyond Windows system libraries.

No glibc requirement. No extension directory. No `.so`/`.dll` file management. One binary = complete deployment.

---

### `ephpm ext build`

Build a **new ePHPm binary** with a custom extension set. Uses a container with static-php-cli and the Rust toolchain to rebuild both `libphp.a` (with your extensions) and the final `ephpm` binary.

```bash
# Add extensions to the default suite
ephpm ext build --add redis,imagick,intl

# Start from a specific suite and add more
ephpm ext build --suite laravel --add mongodb,grpc

# Build a dev variant (adds xdebug, pcov, spx automatically)
ephpm ext build --suite wordpress --dev

# Add xdebug to a custom build (builder detects it's a Zend extension)
ephpm ext build --suite wordpress --add redis,xdebug

# Build from an explicit extension list (no suite base)
ephpm ext build --extensions "json,pcre,mbstring,openssl,curl,redis,pdo_mysql"

# Pin PECL extension versions
ephpm ext build --suite wordpress --add "redis@6.0.2,apcu@5.1.24"

# Specify output path (default: ./ephpm or ./ephpm.exe)
ephpm ext build --suite laravel --output ./bin/ephpm

# Use Docker instead of Podman
CONTAINER_ENGINE=docker ephpm ext build --suite wordpress
```

**What happens:**

```
$ ephpm ext build --suite wordpress --add redis,intl

  ■ Reading current binary metadata...
    PHP 8.4.2, x86_64-linux, ePHPm v0.1.0

  ■ Pulling builder image...
    ghcr.io/ephpm/builder:0.1.0-php8.4 (cached)

  ■ Building libphp.a with extensions...
    Suite: wordpress (25 extensions)
    Adding: redis, intl
    Total: 27 extensions
    spc download --with-php=8.4 --for-extensions="bcmath,curl,...,redis,intl"
    spc build "bcmath,curl,...,redis,intl" --build-embed

  ■ Building ephpm binary...
    cargo build --release

  ■ Validating...
    Binary size: 72 MB
    PHP version: 8.4.2 ✓
    Extensions: 27 ✓ (redis ✓, intl ✓)

  ■ Output → ./ephpm

  Verify with: ./ephpm ext list
```

**Builder images** are published by the ePHPm project:

```
ghcr.io/ephpm/builder:0.1.0-php8.4
ghcr.io/ephpm/builder:0.1.0-php8.3
```

Each image contains: static-php-cli, Rust toolchain, ePHPm source (at the matching version tag), and all system library sources needed to build extensions from source (libpng, freetype, ICU, ImageMagick, etc.). The build runs entirely inside the container — no compiler toolchain needed on the host machine.

**Build time:** Expect 5-15 minutes depending on the extension set. ICU (for `intl`) and ImageMagick (for `imagick`) are the slowest to compile. Results can be cached — rebuilding with the same extension set reuses the static-php-cli build cache.

---

### `ephpm ext list`

Show all extensions compiled into the current binary.

```bash
ephpm ext list
```

```
$ ephpm ext list
Suite: wordpress + custom

EXTENSION       VERSION    STATUS
bcmath          8.4.2      built-in
curl            8.4.2      built-in
dom             20031129   built-in
exif            8.4.2      built-in
fileinfo        8.4.2      built-in
filter          8.4.2      built-in
gd              8.4.2      built-in
iconv           8.4.2      built-in
intl            8.4.2      custom
json            8.4.2      built-in
mbstring        8.4.2      built-in
mysqli          8.4.2      built-in
openssl         8.4.2      built-in
pcre            8.4.2      built-in
pdo_sqlite      8.4.2      built-in
redis           6.1.0      custom
session         8.4.2      built-in
simplexml       8.4.2      built-in
sodium          8.4.2      built-in
sqlite3         8.4.2      built-in
xml             8.4.2      built-in
xmlreader       8.4.2      built-in
zip             8.4.2      built-in
zlib            8.4.2      built-in

24 suite + 2 custom = 26 extensions
```

`built-in` = part of the suite. `custom` = added via `--add` during `ephpm ext build`.

---

### `ephpm ext search`

Search for extensions available in static-php-cli (the ~139 that can be statically compiled).

```bash
ephpm ext search redis
ephpm ext search image
```

```
$ ephpm ext search image
NAME          VERSION   DESCRIPTION                          IN SUITE
imagick       3.7.0     ImageMagick bindings                 full
gmagick       2.0.6     GraphicsMagick bindings              —
gd            8.4.2     GD image library                     wordpress, laravel, full
```

`IN SUITE` shows which pre-built suite binaries already include the extension, so users know if they need a custom build or can just download a different suite.

---

### `ephpm ext info`

Show details about a specific extension in the current binary.

```bash
ephpm ext info redis
```

```
$ ephpm ext info redis
Name:        redis
Version:     6.1.0
Type:        custom (added via ephpm ext build)
PHP API:     20240924
PECL:        https://pecl.php.net/package/redis
Deps:        igbinary (also compiled in)
```

```bash
# Extension not in the current binary
ephpm ext info intl
```

```
$ ephpm ext info intl
ext-intl is NOT in this binary.

Available in suites: laravel, full
Or add it: ephpm ext build --add intl
```

---

### Zend Extensions (xdebug, pcov, spx)

Zend extensions hook into PHP's engine at a deeper level than standard extensions — they intercept opcode execution, instrument function calls, and modify the compiler. PHP's external extension build system (`phpize`) only supports compiling them as shared `.so` files, which conflicts with ePHPm's fully-static model.

**However**, this is a build system limitation, not a technical one. PHP's own **opcache** is a Zend extension and it compiles statically — because it lives inside the PHP source tree. The key insight: if you patch a Zend extension's source into `ext/` before building PHP, `configure` treats it like opcache and compiles it statically.

**ePHPm's approach:** The builder patches Zend extension sources into the PHP source tree during the build. This lets xdebug, pcov, and other Zend extensions be compiled statically into the binary just like any other extension.

**Why dev suites exist:** Zend extensions like xdebug add overhead to every PHP opcode execution. pcov instruments every code path. These should never run in production. Rather than requiring users to remember which extensions are safe, ePHPm separates them into `-dev` suite variants:

```bash
# Development — xdebug, pcov, and spx compiled in
ephpm-0.1.0-php8.4-laravel-dev-linux-x86_64

# Production — same extensions minus dev tools
ephpm-0.1.0-php8.4-laravel-linux-x86_64
```

**Included dev Zend extensions:**

| Extension | Purpose | Controlled via |
|---|---|---|
| **xdebug** | Step debugging, stack traces, profiling | `xdebug.mode` INI setting (`off` by default — zero overhead until enabled) |
| **pcov** | Code coverage (faster than xdebug's coverage mode) | `pcov.enabled` INI setting |
| **spx** | Simple Profiling eXtension — web UI for profiling | `spx.http_enabled` INI setting |

Even in dev suites, these extensions are **disabled by default** via their INI settings. They only activate when explicitly configured:

```toml
# ephpm.toml — enable xdebug for step debugging
[php]
ini_overrides = [
    ["xdebug.mode", "debug"],
    ["xdebug.client_host", "host.docker.internal"],
    ["xdebug.start_with_request", "yes"],
]
```

```bash
# Or via environment variable
XDEBUG_MODE=debug ephpm serve --test
```

**Custom dev builds:** The builder also supports adding Zend extensions:

```bash
# Add xdebug to a custom build
ephpm ext build --suite wordpress --add redis,xdebug

# The builder detects xdebug is a Zend extension and patches it into ext/
```

**Zend extensions that cannot be statically compiled:**

Some commercial Zend extensions (ionCube Loader, Zend Guard) are distributed as pre-compiled `.so` binaries with no source code available. These cannot be patched into the source tree. This is a hard limitation — there is no workaround without access to the source.

---

## Development & Testing

### Embedded SQLite

ePHPm includes `pdo_sqlite` and `sqlite3` as **built-in extensions** (compiled into the binary). Combined with `--test` mode, this means a PHP application can run with zero external dependencies — no MySQL server, no Docker database container, no configuration.

**Why this matters:** The #1 barrier to "just try it" with any PHP application server is the database. Every other tool requires you to set up MySQL/PostgreSQL before you can see your app run. ePHPm removes that barrier.

**Performance characteristics:**

| | SQLite (embedded) | MySQL (network) |
|---|---|---|
| Read latency | ~1-5μs (in-process, no network) | ~200-500μs (TCP round-trip) |
| Write concurrency | Limited (WAL mode helps, but file-locked) | High |
| Setup time | 0ms (open a file) | Seconds (start server, create DB, configure) |
| Good for | Dev, testing, small/read-heavy apps | Production, write-heavy workloads |

SQLite is **not** a replacement for MySQL/PostgreSQL in production. It's a development and testing tool that eliminates infrastructure dependencies.

---

### `ephpm serve --test`

Test mode configures ePHPm for local development and testing with zero external dependencies.

```bash
# Default: SQLite file at ./data/ephpm-test.db (persists between runs)
ephpm serve --test

# In-memory SQLite — fastest, data lost when process exits
ephpm serve --test --db-memory

# Temp file SQLite — data cleaned up when process exits
ephpm serve --test --db-temp

# Combine with other flags
ephpm serve --test --listen :8080 --log-level debug
```

**What `--test` changes:**

| Setting | Normal mode | Test mode |
|---|---|---|
| Database | `[db.mysql]` or `[db.postgres]` from config | Embedded SQLite `:memory:` (automatic) |
| Listen address | `:443` | `:8080` (no TLS) |
| TLS | ACME / manual certs | Disabled |
| Workers | Auto (num_cpus) | 1 (NTS, simpler debugging) |
| Log level | `info` | `debug` |
| PHP `display_errors` | `Off` | `On` |
| PHP `error_reporting` | `E_ALL & ~E_NOTICE` | `E_ALL` |

**Test mode is explicit** — it never activates implicitly. You must pass `--test`. This prevents accidentally running a production server with SQLite.

---

### `ephpm serve --test` with Frameworks

**Laravel:**

```bash
# Laravel works out of the box — ePHPm sets DB_CONNECTION=sqlite automatically
cd my-laravel-app
ephpm serve --test --php-root ./public

# Run migrations against the embedded SQLite
ephpm php run artisan migrate
```

ePHPm injects `DB_CONNECTION=sqlite` and `DB_DATABASE` into the PHP environment in test mode. Laravel picks these up automatically via its `.env` fallback chain.

**Symfony:**

```bash
# Symfony uses DATABASE_URL — ePHPm sets it to the SQLite path
cd my-symfony-app
ephpm serve --test --php-root ./public

# Run migrations
ephpm php run bin/console doctrine:migrations:migrate
```

**WordPress:**

WordPress officially supports MySQL only, but the WordPress Performance Team maintains [`wp-sqlite-db`](https://github.com/WordPress/sqlite-database-integration) — a drop-in SQLite driver used by WordPress Playground. Setting this up with ePHPm is covered in the WordPress tutorial (see `docs/tutorials/`), not baked into the CLI.

---

### `ephpm test`

Run the application's test suite with an embedded SQLite database. This is a convenience wrapper that starts ePHPm in the background, runs your tests, and tears everything down.

```bash
# Run PHPUnit with an in-memory database (data fresh every run)
ephpm test -- vendor/bin/phpunit

# Run Pest
ephpm test -- vendor/bin/pest

# Run a specific test file
ephpm test -- vendor/bin/phpunit tests/Feature/CheckoutTest.php

# Pass flags to ePHPm
ephpm test --listen :9999 -- vendor/bin/phpunit

# Run with a persistent test database (useful for debugging failed tests)
ephpm test --db-persist ./test.db -- vendor/bin/phpunit
```

**What `ephpm test` does:**

```
1. Start ePHPm in background
   - --test --db-memory (default)
   - Random available port (or --listen)
   - Injects APP_URL=http://localhost:<port> into test env

2. Wait for server ready (health check)

3. Run your test command
   - Passes through all args after --
   - Inherits stdout/stderr

4. Capture exit code from test runner

5. Stop ePHPm, cleanup temp DB

6. Exit with the test runner's exit code
```

**CI pipeline — before vs after:**

Before (GitHub Actions with MySQL service):

```yaml
services:
  mysql:
    image: mysql:8
    env:
      MYSQL_ROOT_PASSWORD: test
      MYSQL_DATABASE: testdb
    ports:
      - 3306:3306
    options: >-
      --health-cmd="mysqladmin ping"
      --health-interval=10s
      --health-timeout=5s
      --health-retries=5

steps:
  - uses: actions/checkout@v4
  - run: cp .env.ci .env
  - run: php artisan migrate
  - run: php artisan serve &
  - run: sleep 3
  - run: vendor/bin/phpunit
```

After:

```yaml
steps:
  - uses: actions/checkout@v4
  - run: ephpm test -- vendor/bin/phpunit
```

No database service, no config copying, no sleep hacks, no background process management. One command.

---

### Database Configuration

#### SQLite (development & testing)

For development setups that persist the database between runs (not `--test` mode), configure SQLite explicitly in `ephpm.toml`:

```toml
[db.sqlite]
path = "./data/app.db"           # path to SQLite database file
journal_mode = "wal"             # "wal" (default, best concurrency) or "delete" (traditional)
create = true                    # auto-create DB file and parent dirs if missing
busy_timeout = 5000              # ms to wait on locked DB before returning SQLITE_BUSY
cache_size = -64000              # negative = KB (64MB), positive = pages
```

**In-memory mode:** Set `path = ":memory:"` for a fully in-process database with zero disk I/O. Fastest option for tests and throwaway environments. Data is lost when the process exits.

```toml
[db.sqlite]
path = ":memory:"                # no disk, no file — pure in-process
cache_size = -128000             # 128MB (generous for in-memory)
```

This is what `ephpm serve --test --db-memory` uses under the hood.

#### MySQL (production)

```toml
[db.mysql]
url = "mysql://user:pass@db-primary:3306/myapp"
min_connections = 5
max_connections = 50
idle_timeout = "300s"
inject_env = true                # auto-set DB_HOST, DB_PORT, etc. for PHP

[db.mysql.replicas]
urls = [
    "mysql://user:pass@db-replica-1:3306/myapp",
    "mysql://user:pass@db-replica-2:3306/myapp",
]
read_write_split = true          # SELECTs go to replicas
```

#### PostgreSQL (production)

```toml
[db.postgres]
url = "postgres://user:pass@pg-primary:5432/myapp"
min_connections = 5
max_connections = 30
inject_env = true                # auto-set DATABASE_URL, DB_HOST, etc. for PHP

[db.postgres.replicas]
urls = [
    "postgres://user:pass@pg-replica-1:5432/myapp",
]
read_write_split = true
```

PostgreSQL support uses the same DB proxy architecture as MySQL — connection pooling, query digest, slow query detection, read/write splitting. The proxy speaks the PostgreSQL wire protocol to PHP and to the real database.

#### Why no embedded PostgreSQL?

SQLite replaces MySQL/PostgreSQL for development because it runs in-process with zero setup. There is no equivalent embedded PostgreSQL — PostgreSQL is a client-server database by design and has no library/in-process mode.

For apps that depend on PostgreSQL-specific features (JSONB columns, arrays, `ON CONFLICT`, window functions with PostgreSQL syntax), SQLite won't be a drop-in replacement. In those cases, point `[db.postgres]` at a local or containerized PostgreSQL instance:

```bash
# Start a dev PostgreSQL (one-time setup)
podman run -d --name pg-dev -p 5432:5432 \
  -e POSTGRES_PASSWORD=dev postgres:17

# ephpm.toml
# [db.postgres]
# url = "postgres://postgres:dev@localhost:5432/myapp"
```

**When to use what:**

| Scenario | Database config | External infra needed? |
|---|---|---|
| Quick dev / `ephpm serve --test` | SQLite `:memory:` (automatic) | No |
| Dev with persistent data | `[db.sqlite]` with file path | No |
| Dev needing MySQL-specific features | `[db.mysql]` → local container | MySQL container |
| Dev needing PostgreSQL-specific features | `[db.postgres]` → local container | PostgreSQL container |
| Production (MySQL) | `[db.mysql]` with replicas | MySQL server(s) |
| Production (PostgreSQL) | `[db.postgres]` with replicas | PostgreSQL server(s) |

**Database configs are mutually exclusive.** Only one of `[db.sqlite]`, `[db.mysql]`, or `[db.postgres]` can be active. `ephpm validate` reports an error if multiple are present. Use environment variables to switch between environments:

```bash
# Development — zero setup
EPHPM_DB_SQLITE_PATH=":memory:" ephpm serve

# Staging — PostgreSQL
EPHPM_DB_POSTGRES_URL=postgres://user:pass@pg:5432/myapp ephpm serve

# Production — MySQL with replicas
ephpm serve --config /etc/ephpm/production.toml
```

---

## Inspection Commands

These connect to the Node API of a running instance. Useful for debugging, monitoring, and scripting.

### `ephpm status`

Quick overview of a running node.

```bash
ephpm status
ephpm status --node 10.0.1.1:9090
```

```
$ ephpm status
ePHPm v0.1.0 (PHP 8.4.2 ZTS)
Uptime:     3d 14h 22m
Workers:    12/16 busy, 4 idle, 0 queued
HTTP:       1,247 req/s (p99: 12ms)
DB Pool:    38/50 active connections
KV Store:   124MB used, 89,421 keys, 98.7% hit rate
Cluster:    3 nodes healthy
TLS:        4 certs managed, next renewal in 23d (this node is renewal leader)
```

---

### `ephpm workers`

PHP worker pool details.

```bash
# List workers
ephpm workers
ephpm workers --node 10.0.1.1:9090

# Restart all workers (rolling, no dropped requests)
ephpm workers restart

# Restart specific worker
ephpm workers restart --id 3
```

```
$ ephpm workers
ID  STATUS   REQUESTS  MEMORY   UPTIME     LAST REQUEST
 0  busy     14,231    32MB     3d 14h     12ms ago
 1  idle     13,887    28MB     3d 14h     340ms ago
 2  busy     14,102    35MB     3d 14h     2ms ago
 3  busy     14,450    31MB     3d 14h     8ms ago
...
16 workers | 12 busy | 4 idle | 0 queued | 0 crashed
```

---

### `ephpm db`

DB proxy inspection.

```bash
# Pool status
ephpm db status

# Top query digests (by total time)
ephpm db digests
ephpm db digests --sort count     # by execution count
ephpm db digests --sort max-time  # by worst single execution
ephpm db digests --limit 20

# Slow query log
ephpm db slow
ephpm db slow --since 1h
ephpm db slow --with-explain      # include EXPLAIN output

# Reset digest stats
ephpm db digests reset
```

```
$ ephpm db digests --limit 5
DIGEST       QUERY                                           COUNT    AVG      MAX      TOTAL
0xa3f2b1c4   SELECT * FROM users WHERE id = ?                45,231   2.1ms    89ms     95.0s
0xb1c4d9e7   INSERT INTO orders (user_id, ...) VALUES (?)    12,089   5.3ms    210ms    64.1s
0xd9e7f2a3   SELECT * FROM products WHERE category = ?        8,445   45.2ms   1.2s     381.8s
0xf2a3b1c4   UPDATE users SET last_login = ? WHERE id = ?     6,721   1.8ms    45ms     12.1s
0x1234abcd   SELECT COUNT(*) FROM orders WHERE status = ?     3,211   12.4ms   340ms    39.8s
```

---

### `ephpm kv`

KV store inspection and operations.

```bash
# Stats
ephpm kv stats

# Get/set/delete (for debugging — not a production data path)
ephpm kv get session:abc
ephpm kv set mykey myvalue --ttl 3600
ephpm kv del mykey

# Cluster membership
ephpm kv cluster

# Key scan (pattern match, like Redis SCAN)
ephpm kv keys "session:*" --limit 100
```

```
$ ephpm kv stats
Memory:     124MB / 512MB (24%)
Keys:       89,421
Hit rate:   98.7% (last 5m)
Evictions:  0 (last 5m)
Policy:     allkeys-lru

$ ephpm kv cluster
NODE            STATUS    KEYS      MEMORY    VNODES
10.0.1.1:7946   healthy   31,204    42MB      150
10.0.1.2:7946   healthy   28,891    39MB      150
10.0.1.3:7946   healthy   29,326    43MB      150
Replication: async, factor=2
```

---

### `ephpm cluster`

Cluster management.

```bash
# Cluster status
ephpm cluster status

# Force a node to leave
ephpm cluster leave --node 10.0.1.3:7946

# Show hash ring
ephpm cluster ring

# Show replication status
ephpm cluster replication
```

---

### `ephpm traces`

View recent traces from the ring buffer.

```bash
# List recent traces
ephpm traces
ephpm traces --limit 50

# Filter by slow requests
ephpm traces --min-duration 500ms

# Filter by status code
ephpm traces --status 500

# Show trace detail
ephpm traces show <trace-id>

# Live tail
ephpm traces tail
```

```
$ ephpm traces --min-duration 500ms --limit 5
TRACE ID          METHOD  PATH              STATUS  DURATION  DB QUERIES  KV OPS
a1b2c3d4e5f6     GET     /api/products     200     892ms     12          3
f6e5d4c3b2a1     POST    /checkout         200     1,204ms   28          7
...

$ ephpm traces show a1b2c3d4e5f6
[HTTP GET /api/products 892ms]
  ├─ [PHP: App\Http\Controllers\ProductController@index 845ms]
  │    ├─ [DB: SELECT * FROM products WHERE category = ? 312ms] ← SLOW
  │    ├─ [DB: SELECT * FROM categories WHERE id IN (?, ?, ?) 8ms]
  │    ├─ [KV: GET cache:products:featured 0.2ms] HIT
  │    ├─ [DB: SELECT COUNT(*) FROM reviews WHERE product_id IN (...) 445ms] ← SLOW
  │    └─ [KV: SET cache:products:listing 0.4ms]
  └─ [Response: 200 OK, 12.4KB]
```

---

## Diagnostic Commands

### `ephpm version`

```bash
$ ephpm version
ephpm 0.1.0 (rustc 1.83.0, PHP 8.4.2 ZTS)
Built:      2026-03-15T10:30:00Z
Commit:     a1b2c3d
Target:     x86_64-unknown-linux-musl
Suite:      wordpress + custom (redis, intl)
Extensions: 26 (use `ephpm ext list` for full list)
```

Shows the embedded PHP version, build target, suite, and extension count. Fully static on all platforms.

---

### `ephpm php`

Interact with the embedded PHP interpreter directly.

```bash
# PHP version info (like php -v)
ephpm php version

# PHP info (like php -i, but from the embedded SAPI)
ephpm php info

# List compiled extensions (same as ephpm ext list)
ephpm php extensions

# Run a PHP file with the embedded interpreter
ephpm php run script.php

# Evaluate PHP code
ephpm php eval "echo phpversion();"

# Interactive REPL (if feasible)
ephpm php repl
```

This is useful for verifying the embedded PHP works, checking which extensions are available, and debugging PHP issues without starting the full server.

---

### `ephpm doctor`

Run diagnostics to verify the system is ready. Includes **application extension scanning** — doctor analyzes the PHP application in the document root to detect which extensions it needs and warns about any that are missing.

```bash
$ ephpm doctor
Checking ePHPm environment...

✓ PHP 8.4.2 ZTS embedded and functional
✓ OPcache enabled
✓ Suite: wordpress (25 extensions compiled in)
✓ Config ./ephpm.toml valid
✓ PHP root ./public/index.php exists
✓ Port 443 available
✓ Port 9090 available
✓ Container engine: podman 5.3.1 (for ephpm ext build)
✓ DB connection: mysql://...@db:3306/myapp — connected (5ms)
✓ DB user has PROCESS privilege (required for auto-EXPLAIN)
✗ Cluster: seed node 10.0.1.2:7946 unreachable
✓ TLS: ACME account registered with Let's Encrypt
✓ TLS: cert renewal leader is node-a (healthy, heartbeat 12s ago)
✓ TLS: 4 certs replicated across 3 nodes
✓ DNS: example.com resolves to this server (93.184.216.34)
✓ Memory: 16GB available, recommended min 512MB per worker × 16 workers = 8GB

Scanning application for extension requirements...
✓ ext-json — required by composer.json ✓ (compiled in)
✓ ext-mbstring — required by composer.json ✓ (compiled in)
✓ ext-curl — required by composer.json ✓ (compiled in)
✓ ext-redis — required by composer.json ✓ (compiled in)
✗ ext-intl — required by composer.json ✗ MISSING
✓ ext-gd — detected: imagecreatefromjpeg() in src/ImageService.php:42 ✓ (compiled in)
⚠ ext-memcached — detected: new Memcached() in src/Cache/MemcachedStore.php:18 — not in binary (optional?)

2 issues found:
  ✗ ext-intl required by composer.json — rebuild: ephpm ext build --add intl
  ✗ Cluster seed node 10.0.1.2:7946 unreachable — check firewall or node status

1 warning:
  ⚠ ext-memcached detected in source but not in composer.json — may be optional or dead code
```

**How application scanning works:**

Doctor uses a layered detection strategy, from most to least authoritative:

1. **`composer.json` / `composer.lock`** (highest confidence) — Parses the `require` and `require-dev` sections for `ext-*` entries. This is the authoritative source because the developer explicitly declared these dependencies. Also recursively checks `composer.lock` for transitive `ext-*` requirements from packages.

2. **PHP source scanning** (medium confidence) — Scans `.php` files in the document root for function calls, class instantiations, and constants that map to specific extensions. PHP's function-to-extension mapping is well-defined — every function in the PHP docs belongs to exactly one extension. Examples:
   - `new \Redis()`, `$redis->connect()` → `ext-redis`
   - `new \Imagick()` → `ext-imagick`
   - `curl_init()`, `curl_exec()` → `ext-curl`
   - `mb_strlen()`, `mb_detect_encoding()` → `ext-mbstring`
   - `\IntlDateFormatter`, `\NumberFormatter` → `ext-intl`
   - `sodium_crypto_secretbox()` → `ext-sodium`
   - `yaml_parse()` → `ext-yaml`

3. **WordPress detection** (framework-specific) — If the document root contains `wp-config.php` or `wp-includes/`, doctor knows the WordPress core requirements (mysqli, json, mbstring, xml, curl, openssl, gd, zip, etc.) and checks for them directly. Also scans active plugin/theme directories for their own declared requirements.

**Confidence levels in output:**

- `required by composer.json` — definite requirement, will fail at runtime without it
- `detected: function() in file:line` — found in source code, very likely needed
- `⚠ detected in source but not in composer.json` — may be optional, dead code, or behind a feature flag

**Skipping the scan:**

```bash
# Skip application scanning (faster, just checks config + infra)
ephpm doctor --no-scan

# Only run the application scan
ephpm doctor --scan-only
```

---

## Command Summary

```
ephpm serve          Start the PHP application server
ephpm serve --test   Start in test mode (embedded SQLite, no external DB)
ephpm admin          Start the admin UI (standalone)
ephpm stop           Graceful shutdown of a running instance
ephpm reload         Reload config + rolling worker restart

ephpm init           Scaffold ephpm.toml
ephpm validate       Check config for errors
ephpm config         Show effective configuration

ephpm ext build      Rebuild binary with custom extensions via container
ephpm ext list       List extensions compiled into the current binary
ephpm ext search     Search available extensions (static-php-cli supported)
ephpm ext info       Show details about a specific extension

ephpm test           Run tests with embedded SQLite (start, test, teardown)

ephpm status         Quick overview of a running node
ephpm workers        PHP worker pool details + restart
ephpm db             DB proxy: pool stats, query digests, slow queries
ephpm kv             KV store: stats, get/set/del, cluster membership
ephpm cluster        Cluster management: status, ring, replication
ephpm traces         View/filter/tail distributed traces

ephpm version        Version, build info, embedded PHP version
ephpm php            Interact with embedded PHP (version, info, eval, run)
ephpm doctor         Run system diagnostics
```

---

## Design Principles

1. **Zero to running in one command.** A developer should go from `git clone` to a working app with `ephpm serve --test`. No database server, no config files, no infrastructure. The embedded SQLite and sensible defaults make this possible. The config file is for production tuning, not getting started.

2. **Development-first, production-ready.** ePHPm ships binaries for Linux, macOS, and Windows. Developers run it natively on their machine — no Docker required for local dev. The same binary that runs on a developer's laptop runs in production (different config, same tool).

3. **Inspection commands connect to the Node API.** They don't read internal state directly — they're HTTP clients to `:9090`. This means they work locally (`ephpm status`) or remotely (`ephpm status --node 10.0.1.1:9090`).

4. **Machine-readable output.** All inspection commands support `--json` for scripting and automation:
   ```bash
   ephpm workers --json | jq '.[] | select(.status == "busy")'
   ephpm db digests --json --sort total-time --limit 10
   ```

5. **No interactive prompts in production commands.** `ephpm serve`, `ephpm admin`, `ephpm stop`, `ephpm reload` never prompt. Only `ephpm init` is interactive (and has `--minimal`/`--full` for non-interactive use).

6. **Consistent `--node` flag.** Any inspection command can target a remote node:
   ```bash
   ephpm status --node 10.0.1.1:9090
   ephpm workers --node 10.0.1.1:9090
   ephpm db digests --node 10.0.1.1:9090
   ```
   Without `--node`, commands connect to `localhost:9090` (assumes local instance).

7. **Exit codes matter.** `0` = success, `1` = error, `2` = validation failure. `ephpm validate`, `ephpm doctor`, and `ephpm test` use this for CI/CD gating:
   ```bash
   ephpm validate && ephpm serve
   ephpm test -- vendor/bin/phpunit   # exits with test runner's exit code
   ```
