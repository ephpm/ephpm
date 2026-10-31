use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use bytes::BytesMut;
use clap::{Parser, Subcommand};
use ephpm_kv::resp::{Frame, parse_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Rust's global allocator for the ephpm binary (Unix only).
///
/// mimalloc gives lower allocator contention and better locality than the
/// default system allocator under the ePHPm workload profile (many small,
/// short-lived allocs from HTTP frames, RESP payloads, and query buffers).
/// Applied only in the binary (and its own unit tests); library crates' tests
/// and other tooling stay on the standard allocator.
///
/// This attribute covers **Rust** allocations only. On glibc Linux the
/// `malloc-override` cargo feature (default on) also makes mimalloc the C
/// `malloc`/`free` for the whole process — libphp, the SDK's static libs,
/// glibc's internal allocations and `dlopen`ed extensions/middleware — so both
/// sides share one allocator and recycled PHP worker threads do not leave
/// glibc malloc arenas behind (`malloc_override_tests` below proves it is
/// live). On macOS C code keeps the system malloc; see `crates/ephpm/Cargo.toml`
/// for why each platform is gated the way it is.
///
/// Tuning: mimalloc reads `MIMALLOC_*` environment variables (e.g.
/// `MIMALLOC_PURGE_DELAY`, the delay before freed memory is returned to the
/// OS) — with the override they apply to PHP's C allocations as well.
///
/// Windows uses the system allocator — see the Cargo.toml comment on the
/// mimalloc dep for the MSVC `/MD` vs PHP-SDK `/MT` link mismatch.
#[cfg(unix)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod db_console;
mod db_import;
mod db_query;
mod db_shell;
mod dev_xdebug;
mod exec_sandbox;
mod ext_ini;
mod fatal_signal;
mod kv_snapshot_cli;
mod php_personality;
mod service;
mod site_db;

/// ePHPm — All-in-one PHP application server
#[derive(Parser, Debug)]
#[command(name = "ephpm", version = env!("EPHPM_VERSION"), about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the PHP application server in production mode (binds 0.0.0.0)
    Serve {
        /// Path to the configuration file
        #[arg(short, long, default_value = "ephpm.toml")]
        config: PathBuf,

        /// Address to listen on (overrides config)
        #[arg(short, long)]
        listen: Option<String>,

        /// Document root directory (overrides config)
        #[arg(short, long)]
        document_root: Option<PathBuf>,

        /// Increase log verbosity (-v = debug, -vv = trace)
        #[arg(short, long, action = clap::ArgAction::Count)]
        verbose: u8,
    },

    /// Local development server — binds 127.0.0.1, serves CWD, auto-picks port
    ///
    /// This is also what plain `ephpm` (no subcommand) runs. Use `ephpm serve`
    /// for production (binds 0.0.0.0, expects an ephpm.toml) or `ephpm install`
    /// to register the system service.
    Dev {
        /// Address to listen on (overrides default 127.0.0.1:<port>)
        #[arg(short, long)]
        listen: Option<String>,

        /// Document root directory (defaults to current working directory)
        #[arg(short, long)]
        document_root: Option<PathBuf>,

        /// Preferred port — if busy, the next free port is picked
        #[arg(short, long, default_value_t = 8080u16)]
        port: u16,

        /// Sites directory for `*.localhost` vhosting. Each subdirectory
        /// becomes a vhost reachable at `http://<name>.localhost:<port>` —
        /// no /etc/hosts edit required (RFC 6761 covers `*.localhost`).
        #[arg(short, long)]
        sites: Option<PathBuf>,

        /// Increase log verbosity (-v = debug, -vv = trace)
        #[arg(short, long, action = clap::ArgAction::Count)]
        verbose: u8,

        #[command(flatten)]
        xdebug: dev_xdebug::XdebugArgs,
    },

    /// Statically analyze a PHP application and gate on the verdict
    ///
    /// Runs the configured analyzers (external tools wrapped as
    /// subprocesses, plus native passes) over PATH, merges their findings,
    /// and applies the fail-closed policy: allow / quarantine / deny. The
    /// exit code reflects the verdict per `fail_on`, so this works directly
    /// as a CI or deploy-gate step. Configured by `.ephpm-analyze.yml` in
    /// the analyzed directory (or `--config`).
    Analyze {
        /// Directory to analyze (defaults to the current directory)
        path: Option<PathBuf>,

        /// Path to the analysis configuration file. Defaults to
        /// `<PATH>/.ephpm-analyze.yml` when present, else built-in defaults.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Output format: text | sarif (overrides the config file)
        #[arg(long)]
        format: Option<String>,

        /// Profile preset: security | none (overrides the config file)
        #[arg(long)]
        profile: Option<String>,

        /// Least severe verdict that fails the exit code:
        /// allow (report-only) | quarantine | deny (overrides the config
        /// file)
        #[arg(long)]
        fail_on: Option<String>,

        /// Strictness level 0..=3 (overrides the config file): 0 gates only
        /// on hard denies, 1 is the weighted-score default, 2 quarantines
        /// any high finding, 3 quarantines any medium finding
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=3))]
        level: Option<u8>,

        /// Write a baseline of the current findings to FILE and exit 0.
        /// Later runs with `baseline: FILE` in the config suppress those
        /// findings and gate only on new ones
        #[arg(long, value_name = "FILE")]
        baseline: Option<PathBuf>,

        /// Diff-aware scan: analyze only files changed versus this git ref
        /// (plus untracked files). Fails closed if git errors (overrides the
        /// config file)
        #[arg(long, value_name = "GIT_REF")]
        since: Option<String>,

        /// Directory for the incremental result cache (overrides the config
        /// file; default: ephpm-analyze-cache under the system temp dir)
        #[arg(long, value_name = "DIR")]
        cache_dir: Option<PathBuf>,

        /// Disable the incremental result cache for this run
        #[arg(long)]
        no_cache: bool,
    },

    /// Run PHP CLI commands using the embedded PHP runtime
    ///
    /// With `--site` (and `--config`) the in-process `ephpm_db_*` bridge is
    /// bound to a virtual host's database, so `wp`, `artisan`, migrations, and
    /// seeders built on the `db-*` Composer packages reach that tenant's Turso
    /// database — over the running server's wire listener when one is up, or by
    /// opening the site's file directly for offline work (issue #471).
    #[command(disable_help_flag = true)]
    Php {
        /// Path to the ephpm configuration file. Only read together with
        /// `--site`; supplies `[server] sites_dir`, `[db.sqlite]`, and
        /// `[kv] secret` so the CLI can locate and authenticate to the site's
        /// database. `--long` only (never `-c`), to avoid colliding with
        /// php-cli's own `-c <path>` ini flag in the passthrough args.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Bind `ephpm_db_*` to virtual host `<key>`'s database (requires
        /// multi-tenant `[server] sites_dir` + `[db.sqlite]` and `--config`).
        /// Without it, `ephpm php` runs with no embedded database bound, exactly
        /// as before.
        #[arg(long)]
        site: Option<String>,

        /// Arguments to pass to the PHP interpreter
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Composer install/update/require/remove/dump-autoload via the embedded
    /// vivacity installer
    ///
    /// `ephpm composer <args...>` forwards every trailing argument verbatim to
    /// the in-process [vivacity](https://github.com/Adelagric/vivacity)
    /// installer, a Rust reimplementation of five Composer commands (`install`,
    /// `update`, `require`, `remove`, `dump-autoload`). It is not full
    /// Composer: no other command exists, scripts are not run unless you pass
    /// `--run-scripts`, and locks it cannot install natively are handed to a
    /// real `composer` on `PATH` (exit 3 without one). `install` from a
    /// `composer.lock` and `dump-autoload` need no PHP, so they work in every
    /// build, including stub mode, and on Windows. `update`, `require` and
    /// `remove` (and `install` without a lock, which runs an update) run a
    /// `php` to probe platform requirements: `VIVACITY_PHP` if set, otherwise
    /// in a PHP-linked build this `ephpm` binary itself (so no system PHP is
    /// needed), in a stub build `php` on `PATH`. `-vv`/`-vvv`, and `-v` on a
    /// project with an active plugin, hand the whole command to a real
    /// `composer` (exit 3 without one). The process exits with vivacity's exit
    /// code.
    ///
    /// Examples: `ephpm composer install --no-dev --optimize-autoloader`,
    /// `ephpm composer dump-autoload -o`, `ephpm composer --version`.
    #[command(disable_help_flag = true)]
    Composer {
        /// Arguments passed through to Composer, untouched (a leading
        /// `composer` program-name element is prepended before dispatch).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },

    /// Run a command inside a virtual host's tenant sandbox (Linux only).
    ///
    /// Resolves `--site <key>` to the vhost's filesystem boundary, scopes the
    /// command to that boundary with Landlock (denying `/etc/ephpm`, `/root`,
    /// `/etc/shadow`, and everything else), then irreversibly drops to the
    /// tenant uid/gid before exec — so the process runs as the unprivileged
    /// `ephpm-web` user the host `ephpm_egress` firewall already jails.
    ///
    /// This is the sandboxed execution primitive a control plane (switchboard)
    /// should call for build/seed steps instead of running them as root. See
    /// `site/content/roadmap/ephpm-exec-sandboxed-vhost.md`.
    #[command(disable_help_flag = true)]
    Exec {
        /// Path to the configuration file (holds `[server] sites_dir` and the
        /// per-site derivation). Whoever can read this can already impersonate
        /// any tenant, so config-file permissions are the authorization gate.
        #[arg(short, long, default_value = "/etc/ephpm/ephpm.toml")]
        config: PathBuf,

        /// The virtual host key (the vhost directory name under `sites_dir`).
        #[arg(short, long)]
        site: String,

        /// Wall-clock timeout in seconds. `0` (default) means no timeout. The
        /// deadline is armed with `alarm(2)` and survives `execve`, so it fires
        /// even inside the exec'd command (default `SIGALRM` disposition
        /// terminates the process).
        #[arg(short, long, default_value_t = 0u64)]
        timeout: u64,

        /// Skip the Landlock scope and uid drop, running the command with the
        /// caller's privileges. Loudly labelled and never the default — only
        /// for platforms/paths where the sandbox is unavailable. On Linux this
        /// removes all containment; prefer omitting it.
        #[arg(long)]
        no_sandbox: bool,

        /// The command and its arguments, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },

    /// Database tooling: import a SQL dump, run a one-shot query, or open an
    /// interactive SQL shell against a virtual host's embedded database.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },

    /// Inspect or manipulate the KV store on a running server
    Kv {
        /// KV server host
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// KV server port
        #[arg(long, default_value_t = 6379u16)]
        port: u16,

        /// Password sent as RESP AUTH before the first command. Needed
        /// whenever the server sets [kv.redis_compat] password or [kv] secret
        /// — without it every command is refused with NOAUTH. Falls back to
        /// the EPHPM_KV_PASSWORD environment variable.
        #[arg(long)]
        password: Option<String>,

        /// First argument of the two-argument AUTH <user> <password> form.
        /// Under per-site HMAC auth ([kv] secret with [server] sites_dir) this
        /// is the vhost hostname, and the connection is scoped to that vhost's
        /// store. Requires --password.
        #[arg(long)]
        user: Option<String>,

        #[command(subcommand)]
        subcommand: KvSubcommand,
    },

    /// Install ephpm as a system service and start it
    Install {
        /// Run the service as this dedicated user instead of the elevated
        /// installing account (root/Administrator). Implemented on Linux
        /// (systemd `User=` plus a conservative hardening profile —
        /// `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
        /// `CAP_NET_BIND_SERVICE` so a non-root service can still bind
        /// port 80/443, and `ReadWritePaths` for the data dir, log dir, and
        /// document root) and macOS (launchd `UserName`). Not yet supported
        /// on Windows — the SCM needs a stored account password ephpm has no
        /// secure way to collect, so passing this flag there is a hard error
        /// rather than a silent no-op. Omit for the historical default
        /// (root, no sandboxing) — that behavior is unchanged.
        #[arg(long)]
        user: Option<String>,

        /// Group to run the service as. Defaults to the same name as
        /// `--user` when omitted. Ignored (with a parse error) unless
        /// `--user` is also given.
        #[arg(long, requires = "user")]
        group: Option<String>,
    },

    /// Uninstall the system service
    Uninstall {
        /// Keep the configuration file and data directory in place
        #[arg(long)]
        keep_data: bool,
    },

    /// Start the installed service
    Start,

    /// Stop the installed service
    Stop,

    /// Restart the installed service
    Restart,

    /// Show service status (PID, uptime, listen address)
    Status,

    /// Tail the service log file
    Logs {
        /// Follow the log (like `tail -f`)
        #[arg(short, long)]
        follow: bool,
    },

    /// Deploy: invalidate the cluster-wide OPcache for one vhost, or every
    /// vhost via the broadcast key. Writes `opcache:version:<vhost>` (or
    /// `opcache:version:_ALL`) via the running server's RESP listener; gossip
    /// replicates the write to every peer within seconds.
    ///
    /// Requires the running server to have `[kv.redis_compat] enabled = true`
    /// so the CLI (a separate process) can reach the in-process KV store.
    Deploy {
        /// Invalidate a specific vhost. Mutually exclusive with `--all`.
        #[arg(long, group = "target")]
        site: Option<String>,

        /// Invalidate every vhost via the broadcast key (`opcache:version:_ALL`).
        #[arg(long, group = "target")]
        all: bool,

        /// Optional revision tag (e.g. a git SHA). Recorded at
        /// `opcache:revision:<vhost>` for observability; does not itself
        /// trigger invalidation.
        #[arg(long)]
        rev: Option<String>,

        /// RESP server host (default: 127.0.0.1)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// RESP server port (default: 6379)
        #[arg(long, default_value_t = 6379u16)]
        port: u16,

        /// Password sent as RESP AUTH before the first command. Needed
        /// whenever the server sets [kv.redis_compat] password. Falls back to
        /// the EPHPM_KV_PASSWORD environment variable.
        #[arg(long)]
        password: Option<String>,
    },

    /// Cache management subcommands (currently: OPcache reset).
    Cache {
        /// RESP server host (default: 127.0.0.1)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// RESP server port (default: 6379)
        #[arg(long, default_value_t = 6379u16)]
        port: u16,

        /// Password sent as RESP AUTH before the first command. Needed
        /// whenever the server sets [kv.redis_compat] password. Falls back to
        /// the EPHPM_KV_PASSWORD environment variable.
        #[arg(long)]
        password: Option<String>,

        #[command(subcommand)]
        subcommand: CacheSubcommand,
    },

    /// Internal: run as a Windows service (invoked by SCM, not by users)
    #[cfg(windows)]
    #[command(hide = true)]
    ServiceRun {
        /// Path to the configuration file
        #[arg(long, default_value = "C:\\ProgramData\\ephpm\\ephpm.toml")]
        config: PathBuf,
    },
}

/// Dialect selector for `ephpm db import`.
#[derive(clap::ValueEnum, Clone, Debug)]
enum DialectOpt {
    /// Detect from the dump header (fail if ambiguous).
    Auto,
    /// Force MySQL (`mysqldump`).
    Mysql,
    /// Force PostgreSQL (`pg_dump`).
    Pg,
}

/// Error policy for `ephpm db import`.
#[derive(clap::ValueEnum, Clone, Debug)]
enum OnErrorOpt {
    /// Stop at the first unsupported construct or execution error.
    Stop,
    /// Record the failure and keep going.
    Skip,
}

/// Dialect selector for `ephpm db query` / `ephpm db shell`. Defaults to
/// `sqlite` (the engine's native tongue) — distinct from `db import`, which
/// sniffs the dump header.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum QueryDialectOpt {
    /// Run straight against the Turso engine, no litewire translation.
    Sqlite,
    /// Translate MySQL SQL through litewire.
    Mysql,
    /// Translate PostgreSQL SQL through litewire.
    Pg,
}

impl QueryDialectOpt {
    /// Map onto the shared [`ephpm_dbimport::Dialect`].
    fn to_dialect(self) -> ephpm_dbimport::Dialect {
        match self {
            Self::Sqlite => ephpm_dbimport::Dialect::Sqlite,
            Self::Mysql => ephpm_dbimport::Dialect::MySql,
            Self::Pg => ephpm_dbimport::Dialect::Pg,
        }
    }
}

/// Output format for `ephpm db query`.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum FormatOpt {
    /// Aligned, human-readable columns (the default).
    Table,
    /// Tab-separated values with a header row.
    Tsv,
    /// RFC 4180 comma-separated values.
    Csv,
    /// A JSON array of row objects.
    Json,
}

impl FormatOpt {
    /// Map onto the console's [`db_console::Format`].
    fn to_format(self) -> db_console::Format {
        match self {
            Self::Table => db_console::Format::Table,
            Self::Tsv => db_console::Format::Tsv,
            Self::Csv => db_console::Format::Csv,
            Self::Json => db_console::Format::Json,
        }
    }
}

#[derive(Subcommand, Debug)]
enum DbCommand {
    /// Import a `mysqldump` / `pg_dump` SQL file into a virtual host's
    /// embedded (Turso) database.
    ///
    /// Opens the dump (path, `-` for stdin, or a `.gz`), resolves the target
    /// the same way `ephpm php --site` does (offline direct-open preferred;
    /// forwarded over a running server's MySQL listener when one answers;
    /// per-site clustered with no server refused), sanitizes each statement
    /// for SQLite/Turso, and prints a compatibility report of everything that
    /// was downgraded, dropped, or is unsupported.
    Import {
        /// The dump file, or `-` to read from stdin.
        dump: String,

        /// The virtual host key (vhost directory name under `[server]
        /// sites_dir`) whose database to load. Validated fail-closed.
        #[arg(long)]
        site: String,

        /// The ephpm config file (supplies `[server] sites_dir`,
        /// `[db.sqlite]`, and `[kv] secret`).
        #[arg(long, short = 'c')]
        config: PathBuf,

        /// Dump dialect (default: auto-detect from the header).
        #[arg(long, value_enum, default_value = "auto")]
        dialect: DialectOpt,

        /// Force gzip decompression regardless of the file extension.
        #[arg(long)]
        gzip: bool,

        /// What to do on an unsupported construct or execution error.
        #[arg(long, value_enum, default_value = "stop")]
        on_error: OnErrorOpt,

        /// Parse, sanitize, and report only — execute nothing and bind no
        /// backend.
        #[arg(long)]
        dry_run: bool,

        /// Statements per transaction.
        #[arg(long, default_value_t = 1000usize)]
        batch_size: usize,

        /// Write the compatibility report as JSON to this path.
        #[arg(long)]
        report: Option<PathBuf>,

        /// Hold `CREATE INDEX` statements and replay them after the data has
        /// loaded (often faster for large tables).
        #[arg(long)]
        defer_indexes: bool,

        /// Also list dropped safe-no-op findings (otherwise only counted).
        #[arg(long)]
        verbose: bool,
    },

    /// Run one SQL statement (or a `;`-separated batch) against a virtual
    /// host's embedded database and print the result set.
    ///
    /// The transport mirrors `ephpm php --site`: with a server running, the SQL
    /// is forwarded over its MySQL listener (MySQL dialect only); with no
    /// server, the site's database file is opened directly (any dialect,
    /// including `sqlite`-native). A `sqlite`/`pg` request while a server is up
    /// is a clear error, never a silent fallback (single-writer rule). Pass the
    /// SQL positionally, with `-e`, or on stdin.
    Query {
        /// The vhost key (directory name under `[server] sites_dir`).
        #[arg(long)]
        site: String,

        /// The ephpm config file.
        #[arg(long, short = 'c')]
        config: PathBuf,

        /// SQL dialect. `sqlite` runs native against the engine; `mysql`/`pg`
        /// translate via litewire. Live mode is MySQL-only.
        #[arg(long, value_enum, default_value = "sqlite")]
        dialect: QueryDialectOpt,

        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: FormatOpt,

        /// Refuse any statement that is not a pure read (SELECT / WITH / VALUES
        /// / EXPLAIN / SHOW / DESCRIBE).
        #[arg(long)]
        read_only: bool,

        /// Offline-only: drop statement screening so engine-maintenance SQL
        /// (`PRAGMA integrity_check`, `wal_checkpoint`, path-PRAGMAs) can run.
        /// Refused while a server is up or in per-site clustered mode.
        #[arg(long)]
        maintenance: bool,

        /// The SQL to run. Alternatively pass it positionally or on stdin.
        #[arg(long, short = 'e', value_name = "SQL")]
        execute: Option<String>,

        /// The SQL to run (positional). Omit to read it from `-e` or stdin.
        #[arg(value_name = "SQL")]
        sql: Option<String>,
    },

    /// Open an interactive SQL shell (REPL) against a virtual host's embedded
    /// database.
    ///
    /// Same transport and single-writer rules as `db query`. The prompt shows
    /// the mode and dialect (`blog ‹offline · sqlite›`). Slash meta-commands:
    /// `/dialect`, `/format`, `/tables`, `/schema`, `/kv`, `/kv-put`, `/help`,
    /// `/quit`. See `ephpm help db` and the REPL guide.
    Shell {
        /// The vhost key (directory name under `[server] sites_dir`).
        #[arg(long)]
        site: String,

        /// The ephpm config file.
        #[arg(long, short = 'c')]
        config: PathBuf,

        /// Initial SQL dialect (`sqlite` native by default; live mode forces
        /// `mysql`). Switch at runtime with `/dialect`.
        #[arg(long, value_enum, default_value = "sqlite")]
        dialect: QueryDialectOpt,

        /// Refuse any statement that is not a pure read.
        #[arg(long)]
        read_only: bool,

        /// Offline-only: drop statement screening (see `db query --maintenance`).
        #[arg(long)]
        maintenance: bool,
    },
}

#[derive(Subcommand, Debug)]
enum CacheSubcommand {
    /// Invalidate the OPcache for one vhost, or every vhost via `--all`.
    ///
    /// Wire-identical to `deploy`: the reset is written as
    /// `opcache:version:<vhost>` through the running server's RESP listener,
    /// so on a cluster gossip replicates it to every peer within seconds.
    /// `deploy` is the same operation plus an optional `--rev` stamp — use
    /// whichever names the intent better in shell history and audit logs.
    ///
    /// Requires the running server to have `[kv.redis_compat] enabled = true`
    /// so the CLI (a separate process) can reach the in-process KV store.
    Reset {
        /// Reset the OPcache under one vhost's docroot. Mutually exclusive
        /// with `--all`.
        #[arg(long, group = "reset-target")]
        site: Option<String>,

        /// Reset every vhost via the broadcast key.
        #[arg(long, group = "reset-target")]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
enum KvSubcommand {
    /// List keys matching a pattern (default: *)
    Keys {
        #[arg(default_value = "*")]
        pattern: String,
    },
    /// Get the value of a key
    Get { key: String },
    /// Set the value of a key
    Set {
        key: String,
        value: String,
        /// Time-to-live in seconds
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Delete one or more keys
    Del {
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Increment a counter key
    Incr {
        key: String,
        /// Increment by this amount (default: 1)
        #[arg(long, default_value_t = 1i64)]
        by: i64,
    },
    /// Show TTL information for a key
    Ttl { key: String },
    /// Check the connection
    Ping,
    /// Warm-restart snapshots ([kv.snapshot]): inspect/verify a file, or ask
    /// a running server to save/load/report. Uses the server's HTTP control
    /// endpoint, not the RESP listener, so --host/--port/--password do not
    /// apply.
    Snapshot {
        #[command(subcommand)]
        command: kv_snapshot_cli::KvSnapshotCommand,
    },
}

fn main() -> ExitCode {
    // First thing, before argument parsing and long before any PHP extension
    // or native middleware is dlopen'd: a fault inside one of those is the
    // whole reason this exists, and a fault during their *initialisation* is
    // the one an operator has least other evidence about.
    //
    // Always on, deliberately. It costs one `sigaction` per signal plus a
    // single warm-up `backtrace()` at startup and nothing at all thereafter,
    // and a knob would have to be set before the crash nobody predicted. The
    // `EPHPM_FATAL_HANDLER=0` escape hatch documented on `install` is there
    // for the case where the handler itself misbehaves.
    fatal_signal::install();

    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    // Re-invoked as `php` (argv[0] `php`/`php8.x`, or `PHP_BINARY …` from a
    // script under `ephpm php`/`ephpm composer`)? Then argv belongs to php-cli,
    // not to clap. See `php_personality` for the exact rules.
    if let Some(php_args) = php_personality::php_args(
        &std::env::args_os().collect::<Vec<_>>(),
        php_personality::marker_set(),
        is_ephpm_subcommand,
        |arg| std::path::Path::new(arg).is_file(),
    )? {
        return run_php(None, None, &php_args);
    }

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Analyze {
            path,
            config,
            format,
            profile,
            fail_on,
            level,
            baseline,
            since,
            cache_dir,
            no_cache,
        }) => {
            let overrides = AnalyzeOverrides {
                format,
                profile,
                fail_on,
                level,
                baseline,
                since,
                cache_dir,
                no_cache,
            };
            run_analyze(path, config, overrides)
        }
        Some(Commands::Php { config, site, args }) => run_php(config, site, &php_cli_args(&args)),
        Some(Commands::Composer { args }) => run_composer(args),
        Some(Commands::Exec { config, site, timeout, no_sandbox, command }) => {
            ensure_cli_tracing();
            exec_sandbox::run(&config, &site, &command, timeout, no_sandbox)
        }
        Some(Commands::Db { command }) => run_db(command),
        // Snapshot operations are blocking (file I/O, a blocking HTTP
        // client) and must run outside the tokio runtime the RESP commands
        // use.
        Some(Commands::Kv { subcommand: KvSubcommand::Snapshot { command }, .. }) => {
            kv_snapshot_cli::run(command)
        }
        Some(Commands::Kv { host, port, password, user, subcommand }) => {
            let auth = KvAuth::resolve(user, password)?;
            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(run_kv(&host, port, &auth, subcommand))
        }
        Some(Commands::Deploy { site, all, rev, host, port, password }) => {
            let auth = KvAuth::resolve(None, password)?;
            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(run_deploy(&host, port, &auth, site.as_deref(), all, rev.as_deref()))
        }
        Some(Commands::Cache { host, port, password, subcommand }) => {
            let auth = KvAuth::resolve(None, password)?;
            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(run_cache(&host, port, &auth, subcommand))
        }
        Some(Commands::Install { user, group }) => {
            let owner = user.map(|user| {
                let group = group.unwrap_or_else(|| user.clone());
                service::ServiceOwner { user, group }
            });
            run_service_cmd(move || service::install(owner.as_ref()))
        }
        Some(Commands::Uninstall { keep_data }) => {
            run_service_cmd(|| service::uninstall(keep_data))
        }
        Some(Commands::Start) => run_service_cmd(service::start),
        Some(Commands::Stop) => run_service_cmd(service::stop),
        Some(Commands::Restart) => run_service_cmd(service::restart),
        Some(Commands::Status) => run_service_cmd(service::status),
        Some(Commands::Logs { follow }) => run_service_cmd(|| service::logs(follow)),
        #[cfg(windows)]
        Some(Commands::ServiceRun { .. }) => {
            // Hand control over to the Windows service dispatcher, which calls
            // back into our service-main once SCM is ready. The config path is
            // re-read inside `service_main` from the SCM-passed arguments so
            // the value parsed here is ignored.
            service::windows::run_as_service()
                .map(|()| ExitCode::SUCCESS)
                .map_err(|e| anyhow::anyhow!("service dispatcher failed: {e}"))
        }
        Some(Commands::Dev { listen, document_root, port, sites, verbose, xdebug }) => {
            run_dev(listen, document_root, port, sites, verbose, &xdebug)
        }
        // Bare `ephpm` (no subcommand) is the dev-mode entry point. Service
        // backends always invoke the binary with explicit `serve --config`
        // arguments, so this default never executes under SCM/systemd/launchd.
        None => run_dev(None, None, 8080, None, 0, &dev_xdebug::XdebugArgs::default()),
        other @ Some(Commands::Serve { .. }) => run_serve_sync(other),
    }
}

/// Dispatch an `ephpm db` subcommand (`import` / `query` / `shell`).
fn run_db(command: DbCommand) -> anyhow::Result<ExitCode> {
    ensure_cli_tracing();
    match command {
        DbCommand::Import {
            dump,
            site,
            config,
            dialect,
            gzip,
            on_error,
            dry_run,
            batch_size,
            report,
            defer_indexes,
            verbose,
        } => {
            let req = db_import::ImportRequest {
                dump,
                site,
                config,
                dialect: match dialect {
                    DialectOpt::Auto => db_import::DialectArg::Auto,
                    DialectOpt::Mysql => db_import::DialectArg::MySql,
                    DialectOpt::Pg => db_import::DialectArg::Pg,
                },
                gzip,
                on_error: match on_error {
                    OnErrorOpt::Stop => db_import::OnErrorArg::Stop,
                    OnErrorOpt::Skip => db_import::OnErrorArg::Skip,
                },
                dry_run,
                batch_size,
                report,
                defer_indexes,
                verbose,
            };
            db_import::run(&req)
        }
        DbCommand::Query {
            site,
            config,
            dialect,
            format,
            read_only,
            maintenance,
            execute,
            sql,
        } => db_query::run(db_query::QueryRequest {
            site,
            config,
            dialect: dialect.to_dialect(),
            format: format.to_format(),
            read_only,
            maintenance,
            // An explicit `-e` wins over a positional argument.
            sql: execute.or(sql),
        }),
        DbCommand::Shell { site, config, dialect, read_only, maintenance } => {
            db_shell::run(db_shell::ShellRequest {
                site,
                config,
                dialect: dialect.to_dialect(),
                read_only,
                maintenance,
            })
        }
    }
}

/// CLI overrides for `ephpm analyze` — each `Some` wins over the config
/// file (env-var-style precedence).
#[derive(Default)]
struct AnalyzeOverrides {
    format: Option<String>,
    profile: Option<String>,
    fail_on: Option<String>,
    level: Option<u8>,
    /// Baseline **write** mode: run, write the findings to this file, exit 0.
    baseline: Option<PathBuf>,
    since: Option<String>,
    cache_dir: Option<PathBuf>,
    no_cache: bool,
}

/// `ephpm analyze` — load the config, run the aggregator, print the report,
/// and map the verdict to the exit code.
///
/// Exit codes: `0` when the verdict passes the `fail_on` gate, `2` when a
/// `quarantine` verdict fails it, `3` when a `deny` verdict fails it, `1`
/// for internal errors (bad config, unreadable target, git failure on a
/// `--since` run, missing configured baseline).
///
/// With `--baseline FILE` the run instead *writes* a baseline of the
/// current findings to FILE and exits 0 (the generation half of the PHPStan
/// model; `baseline:` in the config is the consumption half). Operator
/// suppressions still apply first — permanently waived findings don't
/// belong in a baseline — while a configured `baseline:` is ignored for the
/// generating run so the written file is complete.
fn run_analyze(
    path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    overrides: AnalyzeOverrides,
) -> anyhow::Result<ExitCode> {
    use ephpm_analyze::{AnalyzeConfig, Baseline, FailOn, OutputFormat, Verdict, output};

    ensure_cli_tracing();
    let root = match path {
        Some(path) => path,
        None => std::env::current_dir().context("failed to read current directory")?,
    };

    let mut config = match config_path {
        // An explicit --config that doesn't exist is an error (fail-closed),
        // never silently "defaults".
        Some(path) => AnalyzeConfig::load(&path)?,
        None => {
            let found = [".ephpm-analyze.yml", ".ephpm-analyze.yaml"]
                .iter()
                .map(|name| root.join(name))
                .find(|candidate| candidate.is_file());
            match found {
                Some(path) => AnalyzeConfig::load(&path)?,
                None => AnalyzeConfig::default(),
            }
        }
    };

    // CLI flags override the config file.
    if let Some(profile) = overrides.profile {
        config.profile = profile.parse()?;
    }
    if let Some(fail_on) = overrides.fail_on {
        config.fail_on = fail_on.parse()?;
    }
    if let Some(format) = overrides.format {
        config.output = format.parse()?;
    }
    if let Some(level) = overrides.level {
        config.level = level;
    }
    if let Some(since) = overrides.since {
        config.since = Some(since);
    }
    if let Some(cache_dir) = overrides.cache_dir {
        config.cache.dir = Some(cache_dir);
    }
    if overrides.no_cache {
        config.cache.enabled = false;
    }
    if overrides.baseline.is_some() {
        // Generating a baseline must see every finding, including the ones a
        // previously configured baseline would suppress.
        config.baseline = None;
    }
    let output_format = config.output;
    let gate = config.fail_on;

    let report = ephpm_analyze::analyze(&root, config)?;

    if let Some(baseline_path) = overrides.baseline {
        Baseline::from_findings(&report.findings).save(&baseline_path)?;
        println!(
            "baseline written: {} ({} finding(s) recorded)",
            baseline_path.display(),
            report.findings.len()
        );
        return Ok(ExitCode::SUCCESS);
    }

    match output_format {
        OutputFormat::Sarif => println!("{}", output::to_sarif(&report)?),
        OutputFormat::Text => print!("{}", output::to_text(&report)),
    }

    let gated = match gate {
        FailOn::Allow => false,
        FailOn::Quarantine => report.verdict >= Verdict::Quarantine,
        FailOn::Deny => report.verdict >= Verdict::Deny,
    };
    Ok(if gated {
        if report.verdict == Verdict::Deny { ExitCode::from(3) } else { ExitCode::from(2) }
    } else {
        ExitCode::SUCCESS
    })
}

/// Initialise a small tracing subscriber for CLI commands so `tracing::info!`
/// calls (service management, and the `analyze` pipeline) show up on the
/// console.
///
/// Logs are written to **stderr**, never stdout: `ephpm analyze --format sarif`
/// (and `--format text`) emit a machine-read report on stdout, and any log line
/// prepended there corrupts it — a consumer parsing the SARIF (e.g. the
/// switchboard preview gate) would choke on the leading ANSI/log bytes and see
/// zero findings. stdout is for the report; stderr is for diagnostics.
fn ensure_cli_tracing() {
    use std::sync::Once;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(tracing_subscriber::fmt::layer().with_target(false).with_writer(std::io::stderr))
            .try_init();
    });
}

/// Dispatch a service-management command and convert the result into an
/// `ExitCode`. All service errors are propagated through `anyhow` with context.
fn run_service_cmd<F>(f: F) -> anyhow::Result<ExitCode>
where
    F: FnOnce() -> service::Result<()>,
{
    ensure_cli_tracing();
    f().context("service command failed")?;
    Ok(ExitCode::SUCCESS)
}

/// Entry point used by the Windows service worker thread. Reads the config at
/// `config` and runs the HTTP server until shutdown.
#[cfg(windows)]
pub(crate) fn run_serve_with_config(config: PathBuf) -> anyhow::Result<()> {
    let cmd = Commands::Serve { config, listen: None, document_root: None, verbose: 0 };
    let code = run_serve_sync(Some(cmd))?;
    if matches!(code, ExitCode::SUCCESS) {
        Ok(())
    } else {
        anyhow::bail!("server exited with non-zero status")
    }
}

/// Run the `ephpm dev` subcommand — local development server with sensible
/// defaults (loopback bind, CWD doc root, auto-port-pick). This is the path
/// the bare `ephpm` invocation routes through.
///
/// Differences from `ephpm serve`:
/// - Binds `127.0.0.1` (loopback) instead of `0.0.0.0`
/// - Auto-picks the next free port if `port` is busy
/// - Defaults document_root to the current working directory
/// - Prints a banner with the URL and PHP runtime status
/// - Ignores any `ephpm.toml` in CWD — dev mode is intentionally
///   configuration-free so that `cd && ephpm` "just works"
/// - `--xdebug` / `--xdebug-path` load Xdebug for step debugging (see
///   [`dev_xdebug`]); the library is resolved here, before PHP starts, and
///   verified right after PHP starts
fn run_dev(
    listen: Option<String>,
    document_root: Option<PathBuf>,
    port: u16,
    sites: Option<PathBuf>,
    verbose: u8,
    xdebug: &dev_xdebug::XdebugArgs,
) -> anyhow::Result<ExitCode> {
    let mut config = ephpm_config::Config::default_config()
        .context("failed to build default dev-mode configuration")?;

    // Resolve listen address. Explicit --listen wins; otherwise we auto-pick
    // a free port starting from `port` on 127.0.0.1.
    config.server.listen = match listen {
        Some(addr) => addr,
        None => {
            let picked = find_free_port("127.0.0.1", port)
                .context("could not find a free TCP port to listen on")?;
            format!("127.0.0.1:{picked}")
        }
    };

    // Resolve document root — CLI override (made absolute: PHP would resolve a
    // relative script path against its own working directory), else CWD.
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    config.server.document_root = document_root.unwrap_or_else(|| cwd.clone());

    // When --sites is provided, point the vhost machinery at it and enable
    // the `.localhost` suffix-stripping so on-disk dirs are short names
    // (`blog/`) while browsers use `blog.localhost:<port>`.
    if let Some(sites_path) = sites {
        let canonical = sites_path.canonicalize().unwrap_or_else(|_| sites_path.clone());
        config.server.sites_dir = Some(canonical);
        config.server.sites_domain_suffix = Some(".localhost".into());
    }
    config.resolve_relative_roots(&cwd);

    // Resolve + wire Xdebug (no-op without --xdebug). Needs the final listen
    // address for its banner, and must finish — network client included —
    // before PHP initializes.
    dev_xdebug::prepare(xdebug, &mut config)?;

    print_dev_banner(&config);
    run_with_config(config, verbose, true)
}

/// Pretty banner printed once at dev-server startup. Stdout, not tracing,
/// so it's stable across log-format changes and visible regardless of
/// `RUST_LOG`.
fn print_dev_banner(config: &ephpm_config::Config) {
    let version = env!("EPHPM_VERSION");
    let url = format!("http://{}", config.server.listen);
    let php = ephpm_php::PhpRuntime::php_version();

    // Pull the port out of the listen address for vhost URLs.
    let port = config.server.listen.rsplit(':').next().unwrap_or("8080");

    println!();
    println!("  ePHPm {version} — dev server");

    if let Some(sites_dir) = &config.server.sites_dir {
        println!("    sites:    {}", sites_dir.display());
        match list_site_dirs(sites_dir) {
            Ok(entries) if !entries.is_empty() => {
                let suffix = config.server.sites_domain_suffix.as_deref().unwrap_or("");
                println!("    routing:");
                for name in entries {
                    println!("              http://{name}{suffix}:{port}  →  {name}/");
                }
                println!(
                    "              http://localhost:{port}              →  document_root fallback"
                );
            }
            Ok(_) => println!(
                "    routing:  (sites directory is empty — create subdirectories to add vhosts)"
            ),
            Err(e) => println!("    routing:  (could not enumerate sites: {e})"),
        }
        println!("    fallback: {}", config.server.document_root.display());
    } else {
        println!("    serving:  {}", config.server.document_root.display());
        println!("    url:      {url}");
    }

    println!("    php:      {php}");
    println!("    press ctrl+c to stop");
    println!();
}

/// List the immediate subdirectory names under `sites_dir`, sorted, lowercased,
/// excluding dotfiles. Used by the banner — not authoritative for routing
/// (the router does lazy discovery for dirs created after startup).
fn list_site_dirs(sites_dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(sites_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            Some(name.to_ascii_lowercase())
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Probe ports starting at `start_port` on `host`, returning the first one
/// that accepts a `TcpListener::bind`. Gives up after 50 attempts. There's a
/// small TOCTOU window between dropping the probe listener and the real bind,
/// which is acceptable for a dev server — worst case the real bind fails and
/// we surface the OS error.
fn find_free_port(host: &str, start_port: u16) -> anyhow::Result<u16> {
    use std::net::TcpListener;

    for offset in 0..50u16 {
        let candidate = start_port.saturating_add(offset);
        if let Ok(listener) = TcpListener::bind((host, candidate)) {
            drop(listener);
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free port in range {start_port}..={}", start_port.saturating_add(49))
}

/// Whether `name` is one of `ephpm`'s own subcommands (or an alias, or clap's
/// implicit `help`) — the tokens that keep their ePHPm meaning even when the
/// binary is re-invoked through `PHP_BINARY` (see `php_personality`).
fn is_ephpm_subcommand(name: &str) -> bool {
    use clap::CommandFactory;

    name == "help"
        || Cli::command()
            .get_subcommands()
            .any(|sub| sub.get_name() == name || sub.get_all_aliases().any(|a| a == name))
}

/// The verbatim argument list for `ephpm php`, including any `--` separator.
///
/// `--` is *meaningful* to php-cli: it ends option parsing, so
/// `… | php -- a b` reads the program from stdin and passes `a b` to it
/// rather than running a script named `a`. clap consumes the first `--` it
/// sees, so the parsed `Vec<String>` cannot express the difference. `ephpm`
/// declares no global options before its subcommand, so argv[1] is the `php`
/// token; argv[2..] is what the user typed after it, minus the ePHPm-owned
/// `--config`/`--site` options clap consumes as a leading prefix (they are
/// `--long`-only, chosen so they never collide with php-cli's own short flags
/// in the passthrough). Those are stripped by [`strip_leading_site_opts`]
/// before the `--` reconstruction so the comparison against clap's parsed args
/// lines up.
///
/// Falls back to clap's view for any argv shape that doesn't match that
/// expectation (including non-UTF-8 arguments), so this can only ever restore
/// a separator — never invent arguments.
fn php_cli_args(parsed: &[String]) -> Vec<String> {
    let raw =
        std::env::args_os().skip(2).map(|a| a.into_string().ok()).collect::<Option<Vec<String>>>();
    restore_php_separator(raw.map(strip_leading_site_opts), parsed)
}

/// Drop the leading run of ePHPm-owned `--config`/`--site` option tokens (and
/// their values) from raw `php` argv, leaving exactly the PHP passthrough. Only
/// a *leading* run is stripped — matching where clap consumes them (before the
/// first positional / `--`), so a `--site` appearing *after* a PHP script name
/// is left untouched for the script, exactly as clap routes it into `args`.
fn strip_leading_site_opts(raw: Vec<String>) -> Vec<String> {
    let mut i = 0;
    while i < raw.len() {
        let tok = raw[i].as_str();
        if tok == "--config" || tok == "--site" {
            // Option plus its value token (if present).
            i += if i + 1 < raw.len() { 2 } else { 1 };
        } else if tok.starts_with("--config=") || tok.starts_with("--site=") {
            // `--opt=value` is a single token.
            i += 1;
        } else {
            break;
        }
    }
    raw[i..].to_vec()
}

/// Pure half of [`php_cli_args`]: `raw` is argv[2..] when it was all valid
/// UTF-8. Returns `raw` only when it differs from `parsed` by exactly the one
/// `--` clap swallowed.
fn restore_php_separator(raw: Option<Vec<String>>, parsed: &[String]) -> Vec<String> {
    let Some(raw) = raw else { return parsed.to_vec() };

    let mut without_separator = raw.clone();
    if let Some(i) = without_separator.iter().position(|a| a == "--") {
        without_separator.remove(i);
    }
    if without_separator == parsed { raw } else { parsed.to_vec() }
}

/// Run the `ephpm php` subcommand — pass args through to the embedded PHP CLI.
///
/// `-d` directives (including `-d extension=`) are applied at module startup
/// by the CLI pre-scan in `ephpm_php::PhpRuntime::cli_main`, matching php-cli
/// — the former "runtime `-d extension=` is ignored" warning is gone because
/// the limitation is gone (issue #331).
///
/// With `--site`, the `ephpm_db_*` bridge is bound to that virtual host's
/// database before PHP runs (issue #471, [`site_db::bind_site_db`]): a
/// background tokio runtime drives the async backend work while PHP executes on
/// this (main) thread, so the bridge's `Handle::block_on` is legal — this thread
/// is never a runtime worker, the same invariant the server relies on.
fn run_php(
    config: Option<PathBuf>,
    site: Option<String>,
    args: &[String],
) -> anyhow::Result<ExitCode> {
    // Before any thread exists: let `PHP_BINARY …` re-invocations from this
    // script come back as php-cli rather than as `ephpm` (php_personality).
    php_personality::export_for_children(false);

    // Hold the runtime for the whole PHP execution when a --site backend is
    // bound: the bridge captured its `Handle` and `block_on`s onto it from PHP.
    let db_runtime = match site {
        Some(site_key) => {
            ensure_cli_tracing();
            let config_path = config.ok_or_else(|| {
                anyhow::anyhow!(
                    "`ephpm php --site {site_key}` requires `--config <path>` (it supplies \
                     [server] sites_dir, [db.sqlite], and [kv] secret)"
                )
            })?;
            if !config_path.exists() {
                anyhow::bail!("configuration file not found: {}", config_path.display());
            }
            let cfg = ephpm_config::Config::load(&config_path)
                .with_context(|| format!("loading configuration from {}", config_path.display()))?;
            // A dedicated multi-thread runtime, NOT entered on this thread —
            // PHP runs here, the runtime's own worker threads drive the backend.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("ephpm-php-db")
                .build()
                .context("failed to build the database runtime for `ephpm php --site`")?;
            let offline = site_db::bind_site_db(&cfg, &site_key, runtime.handle())?;
            Some((runtime, offline))
        }
        None => {
            // `--config` alone does nothing — it only locates a site's database.
            // Say so rather than ignoring it silently.
            if config.is_some() {
                eprintln!(
                    "warning: `ephpm php --config` has no effect without `--site`; ignoring it"
                );
            }
            None
        }
    };

    let exit_code = ephpm_php::PhpRuntime::cli_main(args);
    if exit_code.is_ok() {
        let _ = ephpm_php::PhpRuntime::shutdown();
    }
    // A directly opened database: truncate its WAL now — the SDK never
    // checkpoints on drop, so otherwise every write the script made would be
    // left in the log for the next opener to recover. Also on a failed run:
    // whatever the script committed before the failure is in the log too.
    if let Some((runtime, Some(offline))) = &db_runtime {
        offline.checkpoint_before_exit(runtime.handle());
    }
    Ok(exit_code_from(exit_code.context("PHP CLI failed")?))
}

/// Run the `ephpm composer` subcommand — forward argv verbatim to the embedded
/// vivacity Composer installer and propagate its exit code.
///
/// `vivacity::run` is built on clap and expects `argv[0]` to be the program
/// name (as in `std::env::args()`), so a synthetic `"composer"` element is
/// prepended before the user's trailing arguments. Nothing else is touched —
/// the caller's `install`/`require`/`--version`/etc. pass through unchanged.
///
/// This exits the process directly with vivacity's `i32` status rather than
/// returning through `main`, so the exact Composer exit code reaches CI and
/// shells unmodified.
fn run_composer(args: Vec<std::ffi::OsString>) -> anyhow::Result<ExitCode> {
    // Point vivacity's platform probe (`$VIVACITY_PHP <probe.php>`) at this
    // binary when it embeds PHP, so update/require/remove need no system php,
    // and mark the environment so that re-invocation runs as php-cli. Must
    // precede vivacity::run, which spawns threads.
    php_personality::export_for_children(true);
    let forwarded =
        std::iter::once(std::ffi::OsString::from("composer")).chain(args).collect::<Vec<_>>();
    // vivacity reqwest uses rustls-tls-no-provider, so it consults the process
    // default CryptoProvider. The serve path installs it; the CLI does not, so
    // install aws-lc-rs here before the first cold-cache HTTPS fetch (#523).
    ephpm_server::tls::install_default_crypto_provider();
    let code = vivacity::run(forwarded);
    std::process::exit(code);
}

/// Startup diagnostics for `[php] max_execution_time`.
///
/// Two-layer timeout model:
///   * `max_execution_time` — the PHP-level limit. On a Linux ZTS build whose
///     libphp has per-thread execution timers (`php_max_exec_timers`), it is
///     written into the generated php.ini and PHP arms its own per-thread POSIX
///     timer. It is wall-clock (CLOCK_BOOTTIME), catchable, and overridable at
///     runtime via `set_time_limit()`. Exceeding it raises a PHP fatal
///     ("Maximum execution time exceeded"), runs shutdown functions, and flushes
///     buffered output — a normal 500, not a hard kill.
///   * `[server.timeouts] request` — the OUTER hard ceiling enforced at the HTTP
///     layer. It still fires (504) for a request wedged in a C extension or
///     syscall that never returns to the VM to observe the timer.
///
/// The inner limit can only fire if it is strictly below the outer one, so warn
/// when `max_execution_time >= request` — see [`php_timeout_preempted`] for the
/// two `0` (disabled) cases that are exempt.
#[cfg(php_max_exec_timers)]
fn warn_max_execution_time(config: &ephpm_config::Config) {
    let inner = config.php.max_execution_time;
    let outer = config.server.timeouts.request;
    if php_timeout_preempted(inner, outer) {
        tracing::warn!(
            max_execution_time = inner,
            request_timeout = outer,
            "[php] max_execution_time ({inner}s) is >= [server.timeouts] request \
             ({outer}s); the outer hard request deadline will always preempt PHP's \
             own timeout, so the catchable \"Maximum execution time exceeded\" fatal \
             can never fire. Lower max_execution_time below the request timeout to \
             let PHP handle it gracefully."
        );
    }
}

/// Whether PHP's own `max_execution_time` (`inner`, seconds) can never fire
/// because the outer `[server.timeouts] request` deadline (`outer`, seconds)
/// always cuts the request off first.
///
/// `0` disables either limit, and a disabled limit never preempts or is
/// preempted:
/// - `inner == 0` — no PHP limit; the request deadline is the only ceiling,
///   by design.
/// - `outer == 0` — no request deadline at all (the router arms no timer), so
///   PHP's own timeout is the ONLY ceiling and fires normally. Comparing
///   `inner >= 0` here used to print "max_execution_time (30s) is >=
///   [server.timeouts] request (0s)" — the exact opposite of the truth.
#[cfg_attr(not(php_max_exec_timers), allow(dead_code))]
fn php_timeout_preempted(inner: u32, outer: u64) -> bool {
    inner != 0 && outer != 0 && u64::from(inner) >= outer
}

/// Fallback diagnostics when the linked libphp has no per-thread execution
/// timers (macOS, Windows — its ZTS SDK lacks `ZEND_MAX_EXECUTION_TIMERS` —
/// or a Linux SDK built without `--enable-zend-max-execution-timers`). PHP's only native mechanism there is
/// the process-wide setitimer/SIGPROF timer, which is unsafe on tokio worker
/// threads and is deliberately neutered — so `max_execution_time` is not
/// natively enforced and the request-layer deadline is the only ceiling.
#[cfg(not(php_max_exec_timers))]
fn warn_max_execution_time(config: &ephpm_config::Config) {
    let php_defaults = ephpm_config::PhpConfig::default();
    if config.php.max_execution_time != php_defaults.max_execution_time {
        let in_force = if config.server.timeouts.request == 0 {
            "[server.timeouts] request is 0 (disabled), so no per-request deadline is in \
             force at all."
        } else {
            "The per-request deadline actually in force is [server.timeouts] request."
        };
        tracing::warn!(
            max_execution_time = config.php.max_execution_time,
            request_timeout = config.server.timeouts.request,
            "[php] max_execution_time is not natively enforced on this build \
             (the linked PHP has no per-thread execution timers; its process-wide \
             SIGPROF timer is unsafe under tokio and stays disabled). {in_force}"
        );
    }
}

/// Warn once at startup for every config knob that is parsed but not acted
/// upon *in this configuration*, so a silently-ignored setting can never look
/// like it took effect.
///
/// Two categories:
/// - **Never implemented** — the knob has no code behind it at all. Each
///   field is compared against its own section's `Default`, so an untouched
///   config stays quiet and only a deliberate override produces a line.
/// - **Implemented, but not on this path** — `[server.security]`
///   `open_basedir` / `disable_shell_exec` exist only for multi-tenant
///   deployments and do nothing when `[server] sites_dir` is unset.
///
/// Whenever one of these knobs gains a real implementation (or gains one for
/// the missing mode), delete its branch here and update the matching doc
/// comment in `ephpm-config`.
fn warn_unimplemented_knobs(config: &ephpm_config::Config) {
    warn_max_execution_time(config);

    let rw_defaults = ephpm_config::ReadWriteSplitConfig::default();
    if config.db.read_write_split.strategy != rw_defaults.strategy {
        tracing::warn!(
            strategy = %config.db.read_write_split.strategy,
            "[db.read_write_split] strategy is parsed but not acted upon — the \
             proxy always behaves as \"{}\" (reads stick to the primary for \
             sticky_duration after a write)",
            rw_defaults.strategy
        );
    }
    if config.db.read_write_split.max_replica_lag != rw_defaults.max_replica_lag {
        tracing::warn!(
            max_replica_lag = %config.db.read_write_split.max_replica_lag,
            "[db.read_write_split] max_replica_lag is parsed but not acted \
             upon — replica lag is never measured, so no replica is skipped"
        );
    }

    let analysis_defaults = ephpm_config::DbAnalysisConfig::default();
    if config.db.analysis.auto_explain != analysis_defaults.auto_explain {
        tracing::warn!(
            auto_explain = config.db.analysis.auto_explain,
            "[db.analysis] auto_explain is parsed but not acted upon — slow \
             queries are still logged via [db.analysis] slow_query_threshold, \
             but EXPLAIN is never run"
        );
    }
    if config.db.analysis.auto_explain_target != analysis_defaults.auto_explain_target {
        tracing::warn!(
            auto_explain_target = %config.db.analysis.auto_explain_target,
            "[db.analysis] auto_explain_target is parsed but not acted upon — \
             EXPLAIN analysis is not implemented, so nothing is written to any \
             target"
        );
    }

    // [server.security] open_basedir / disable_shell_exec are implemented
    // only on the multi-tenant path: the per-request `open_basedir` is set
    // from the resolved vhost's directory, and `disable_functions` is only
    // emitted into the generated php.ini when `sites_dir` is set. An
    // operator who turns either on in single-site mode was getting no
    // sandbox and no warning — the worst possible combination, because the
    // config reads as if the process were hardened.
    for flag in config.server.inert_security_flags() {
        let remedy = match flag {
            "disable_shell_exec" | "multi_tenant_hardening" => {
                "[[\"disable_functions\", \
                 \"exec,passthru,shell_exec,system,proc_open,popen,pcntl_exec,...\"]]"
            }
            _ => "[[\"open_basedir\", \"/app:/tmp\"]]",
        };
        tracing::warn!(
            flag,
            "[server.security] {flag} is enabled but has no effect — it is \
             implemented only for multi-tenant deployments ([server] \
             sites_dir), and this process is single-site. Nothing is \
             sandboxing PHP here. Set PHP's own directive through [php] \
             ini_overrides instead (ini_overrides = {remedy}) — those lines \
             are written into the generated php.ini and applied at MINIT."
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Multi-tenant PHP function denylist composition
// ─────────────────────────────────────────────────────────────────────────────

/// The shell-execution family disabled by `[server.security] disable_shell_exec`
/// and always included in the multi-tenant hardening preset. Blocks shell
/// escape out of `open_basedir`.
const SHELL_EXEC_FUNCTIONS: &[&str] =
    &["exec", "passthru", "shell_exec", "system", "proc_open", "popen", "pcntl_exec"];

/// Functions the multi-tenant hardening preset disables on top of the
/// shell-exec family. Each group closes a cross-tenant channel a
/// hostile-PHP-userland pentest proved reachable in the shared-process ZTS
/// model — see `site/content/guides/virtual-hosts.md`.
const HARDENING_FUNCTIONS: &[&str] = &[
    // pcntl process control: fork bomb + fd/secret inheritance in the child.
    "pcntl_fork",
    "pcntl_signal",
    "pcntl_alarm",
    "pcntl_wait",
    "pcntl_waitpid",
    "pcntl_async_signals",
    "pcntl_signal_dispatch",
    "pcntl_sigprocmask",
    "pcntl_sigwaitinfo",
    "pcntl_sigtimedwait",
    // posix: signal/kill the shared process, or change its credentials.
    "posix_kill",
    "posix_setuid",
    "posix_setgid",
    "posix_seteuid",
    "posix_setegid",
    // Persistent raw socket: `EG(persistent_list)` is keyed `host:port` with
    // no tenant component and survives request end on the shared ZTS worker, so
    // a tenant reusing another's host:port inherits its live, authenticated
    // socket. This is a *persistence* leak, not a reachability one, so it stays
    // disabled regardless of any external egress control (unlike `fsockopen`,
    // see `HARDENING_REACHABILITY`).
    "pfsockopen",
    // SysV IPC: a global kernel namespace keyed by integer; one shared uid ⇒
    // full cross-tenant read/write.
    "shm_attach",
    "shm_get_var",
    "shm_put_var",
    "shm_remove",
    "shm_detach",
    "shm_has_var",
    "sem_get",
    "sem_acquire",
    "sem_release",
    "sem_remove",
    "msg_get_queue",
    "msg_send",
    "msg_receive",
    "msg_remove_queue",
    "msg_set_queue",
    "msg_stat_queue",
    // Runtime extension loading + mail relay from the shared identity.
    "dl",
    "mail",
];

/// Reachability-only functions the hardening preset disables by default but
/// LIFTS when `[server.security] network_egress_externally_managed = true`.
///
/// `fsockopen` opens a *non-persistent* raw TCP/UDP socket — it dies at request
/// end, so it carries none of `pfsockopen`'s cross-tenant persistence risk. The
/// only thing blocking it ever bought was reachability (stop a tenant dialing
/// arbitrary hosts), and that is redundant — and inconsistent — once egress is
/// enforced below PHP at the network/kernel layer, because `stream_socket_client`
/// and `curl` stay open and reach the same destinations anyway. When the
/// operator asserts an external egress control, these come back out of the
/// denylist; otherwise they stay blocked (safe default).
const HARDENING_REACHABILITY: &[&str] = &["fsockopen"];

/// OPcache functions the hardening preset disables regardless of cluster
/// invalidation: a whole-cache flush (`opcache_reset`) and arbitrary-file
/// compile (`opcache_compile_file`) are pure attack surface that ePHPm never
/// calls from userland.
const HARDENING_OPCACHE_ALWAYS: &[&str] = &["opcache_reset", "opcache_compile_file"];

/// OPcache introspection/invalidation the preset additionally disables **only
/// when `[opcache] cluster_invalidation` is off**. ePHPm's own cluster
/// invalidator (`ephpm-server/src/opcache.rs`) looks these up in the function
/// table, so they must stay callable when that feature is on.
const HARDENING_OPCACHE_WHEN_NO_CLUSTER_INVALIDATION: &[&str] = &[
    "opcache_invalidate",
    "opcache_get_status",
    "opcache_get_configuration",
    "opcache_is_script_cached",
];

/// Sentinel `opcache.restrict_api` prefix — a path no tenant script can live
/// under, so every remaining OPcache userland call is refused by PHP. Only
/// emitted when ePHPm's own cluster invalidator does not need the API
/// (`cluster_invalidation` off), because the directive's check keys on the
/// executing script's path and would block ePHPm's invalidator too.
const OPCACHE_RESTRICT_API_SENTINEL: &str = "/nonexistent/ephpm-opcache-api-disabled";

/// How long stopping the async runtime may take after `serve` has finished
/// its own drain. Covers the background tasks being dropped and the joins of
/// tokio's worker and blocking-pool threads; a blocking task still running
/// after this is left behind (the process exits around it) rather than
/// keeping a SIGTERMed server alive until SIGKILL.
///
/// Database durability does not depend on this bound: `serve` runs a final
/// `wal_checkpoint(TRUNCATE)` of every open embedded database
/// (`ephpm_server::db_shutdown`) to completion *before* it returns, so the
/// tasks dropped here hold no un-checkpointed state worth waiting for. It is
/// still not tiny, so in-flight commits (observed: runtime threads parked in
/// `jbd2_log_wait_commit`) normally finish; it only has to catch a genuinely
/// stuck task.
const RUNTIME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Mode-dependent PHP error display/logging defaults for the generated ini.
///
/// PHP's built-in default is `display_errors=1`, which renders every warning
/// — including the script's absolute filesystem path — into the HTTP
/// response. `ephpm serve` is the production entry point, so it turns display
/// off (php.ini-production's stance) and turns logging on; with no
/// `error_log` file configured, PHP's log lines reach ePHPm's own log through
/// the SAPI `log_message` hook. `ephpm dev` keeps diagnostics on screen.
///
/// These are written *first* into the generated ini, so `[php] ini_file` and
/// `[php] ini_overrides` both win over them.
fn error_display_ini_defaults(dev_mode: bool) -> [(&'static str, &'static str); 3] {
    if dev_mode {
        [("display_errors", "On"), ("display_startup_errors", "On"), ("log_errors", "On")]
    } else {
        [("display_errors", "Off"), ("display_startup_errors", "Off"), ("log_errors", "On")]
    }
}

/// Split every operator `disable_functions` entry in `ini_overrides` into
/// individual, trimmed function names. Multiple entries (or a comma list)
/// are all collected so the composed union below preserves every one.
fn collect_operator_disable_functions(ini_overrides: &[[String; 2]]) -> Vec<String> {
    let mut out = Vec::new();
    for [k, v] in ini_overrides {
        if k.trim().eq_ignore_ascii_case("disable_functions") {
            for name in v.split(',') {
                let name = name.trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// Case-insensitively append `name` to `ordered` if not already present.
fn push_unique(
    ordered: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
    name: &str,
) {
    let name = name.trim();
    if !name.is_empty() && seen.insert(name.to_ascii_lowercase()) {
        ordered.push(name.to_string());
    }
}

/// Compose the effective `disable_functions` value as the UNION of any
/// operator-supplied list and ePHPm's baseline, so an operator's additions
/// (e.g. disabling `unserialize`) are never clobbered by ePHPm's own line.
///
/// - `include_shell` — add the shell-exec family (`disable_shell_exec`).
/// - `include_hardening` — add the full multi-tenant hardening set.
/// - `include_reachability_block` — when `true`, also add the reachability-only
///   functions (`fsockopen`). Set `false` when
///   `network_egress_externally_managed` asserts the network/kernel layer owns
///   egress, so blocking them at the PHP layer is redundant. Only consulted
///   when `include_hardening` is `true`.
/// - `cluster_invalidation` — when `false`, also disable the OPcache
///   introspection/invalidation functions that ePHPm otherwise needs.
/// - `gated` — ePHPm baseline functions that a per-site capability grant moves
///   from global-disable to per-request gating (canonical lower-case names).
///   These are **omitted** from ePHPm's baseline here so they stay registered;
///   the per-site gate decides each call. An **operator**-supplied name is never
///   omitted — an operator who explicitly disabled a function keeps it disabled,
///   so a capability grant for it is refused upstream (`resolve_gated`). An empty
///   set reproduces the previous output byte-for-byte.
///
/// Returns `None` when the union is empty (nothing to emit).
// Each bool selects an independent, orthogonal baseline group (shell / full
// hardening / reachability / cluster-invalidation carve-out). Folding them into
// one flags struct would obscure the call site more than it clarifies.
#[allow(clippy::fn_params_excessive_bools)]
fn compose_disable_functions(
    operator: &[String],
    include_shell: bool,
    include_hardening: bool,
    include_reachability_block: bool,
    cluster_invalidation: bool,
    gated: &std::collections::HashSet<String>,
) -> Option<String> {
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // An ePHPm baseline name that a per-site capability gate owns is left
    // registered (not disabled). Operator names below never consult this.
    let push_baseline =
        |ordered: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, name: &str| {
            if !gated.contains(name) {
                push_unique(ordered, seen, name);
            }
        };

    // Operator list first: preserve their explicit intent and ordering. Never
    // skipped for `gated` — an operator's explicit disable always wins.
    for f in operator {
        push_unique(&mut ordered, &mut seen, f);
    }
    if include_shell || include_hardening {
        for f in SHELL_EXEC_FUNCTIONS {
            push_baseline(&mut ordered, &mut seen, f);
        }
    }
    if include_hardening {
        for f in HARDENING_FUNCTIONS {
            push_baseline(&mut ordered, &mut seen, f);
        }
        if include_reachability_block {
            for f in HARDENING_REACHABILITY {
                push_baseline(&mut ordered, &mut seen, f);
            }
        }
        for f in HARDENING_OPCACHE_ALWAYS {
            push_baseline(&mut ordered, &mut seen, f);
        }
        if !cluster_invalidation {
            for f in HARDENING_OPCACHE_WHEN_NO_CLUSTER_INVALIDATION {
                push_baseline(&mut ordered, &mut seen, f);
            }
        }
    }

    if ordered.is_empty() { None } else { Some(ordered.join(",")) }
}

/// What a set of per-site capability grants resolves to against the effective
/// hardening baseline.
///
/// Given the union of every site's granted capabilities (`granted`, canonical
/// names) and the baseline flags, partition it into:
///
/// * `gated` — names ePHPm's baseline *would* disable and that a grant moves to
///   per-request gating instead. Exactly the functions to leave out of
///   `disable_functions` ([`compose_disable_functions`]) and to install a gate
///   for.
/// * `redundant` — granted names ePHPm's baseline would not disable anyway
///   (e.g. `fsockopen` when egress is externally managed, or any name when
///   hardening is off). Callable by every tenant already, so the grant does
///   nothing — surfaced so the operator is not misled.
/// * `operator_blocked` — granted names the operator *explicitly* disabled via
///   `[php] ini_overrides`. The operator's intent wins; the grant is refused.
///
/// The three are disjoint. Only `gated` changes behaviour.
#[allow(clippy::fn_params_excessive_bools)]
fn resolve_gated(
    granted: &std::collections::BTreeSet<String>,
    operator: &[String],
    include_shell: bool,
    include_hardening: bool,
    include_reachability_block: bool,
    cluster_invalidation: bool,
) -> GatedResolution {
    // The exact set of names ePHPm's baseline would add, given the flags — the
    // only functions a capability grant can meaningfully un-disable.
    let mut baseline: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    if include_shell || include_hardening {
        baseline.extend(SHELL_EXEC_FUNCTIONS.iter().copied());
    }
    if include_hardening {
        baseline.extend(HARDENING_FUNCTIONS.iter().copied());
        if include_reachability_block {
            baseline.extend(HARDENING_REACHABILITY.iter().copied());
        }
        baseline.extend(HARDENING_OPCACHE_ALWAYS.iter().copied());
        if !cluster_invalidation {
            baseline.extend(HARDENING_OPCACHE_WHEN_NO_CLUSTER_INVALIDATION.iter().copied());
        }
    }

    let operator_lower: std::collections::HashSet<String> =
        operator.iter().map(|f| f.trim().to_ascii_lowercase()).collect();

    let mut resolution = GatedResolution::default();
    for name in granted {
        if operator_lower.contains(name) {
            resolution.operator_blocked.push(name.clone());
        } else if baseline.contains(name.as_str()) {
            resolution.gated.insert(name.clone());
        } else {
            resolution.redundant.push(name.clone());
        }
    }
    resolution
}

/// The partition [`resolve_gated`] produces. `gated` drives behaviour; the other
/// two are for operator-facing diagnostics only.
#[derive(Default)]
struct GatedResolution {
    gated: std::collections::HashSet<String>,
    redundant: Vec<String>,
    operator_blocked: Vec<String>,
}

/// Apply the resolved per-site capability grants: install the request-time gate
/// (C handler swap list + the site→capabilities map) and log, loudly and
/// completely, what the feature did.
///
/// A no-op beyond logging when nothing is gated — the common case — so a
/// deployment that uses no capability keeps exactly today's behaviour. Every
/// grant that does *not* take effect (redundant, or overridden by an operator
/// `disable_functions`) is warned about, so the feature is never silently inert.
///
/// Must be called before PHP initialization (the handler swap runs during module
/// startup). A no-op in stub mode via the underlying FFI setters.
fn apply_per_site_capabilities(
    resolution: &GatedResolution,
    grants: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    worker_mode: bool,
) -> anyhow::Result<()> {
    // Grants that cannot take effect: say so rather than let the operator
    // believe a function was scoped when it is callable by everyone, or enabled
    // when the operator's own denylist keeps it off.
    if !resolution.redundant.is_empty() {
        let mut names = resolution.redundant.clone();
        names.sort_unstable();
        tracing::warn!(
            capabilities = %names.join(", "),
            "per-site [security] capabilities grant has no effect: these functions are not \
             disabled by this configuration (multi-tenant hardening off, or egress externally \
             managed for fsockopen), so every tenant can already call them. The grant is ignored."
        );
    }
    if !resolution.operator_blocked.is_empty() {
        let mut names = resolution.operator_blocked.clone();
        names.sort_unstable();
        tracing::warn!(
            capabilities = %names.join(", "),
            "per-site [security] capabilities grant is overridden by an explicit [php] \
             ini_overrides disable_functions entry: the operator disabled these process-wide, \
             which wins. The grant is ignored; remove the disable_functions entry to gate them \
             per site instead."
        );
    }

    if resolution.gated.is_empty() {
        // Nothing moved from global-disable to gating: do not install any gate
        // or map. `disable_functions` and the function table are unchanged.
        return Ok(());
    }

    // Worker mode has no per-request site context for the gate (the router's
    // per-request `set_current_site` runs on the per_request dispatch path
    // only), so every gated call fails closed — the grant would silently never
    // work. Say so rather than let an operator believe it took effect. The
    // functions still move out of the global denylist; they are simply
    // uncallable everywhere in worker mode.
    if worker_mode {
        let mut names: Vec<String> = resolution.gated.iter().cloned().collect();
        names.sort_unstable();
        tracing::warn!(
            capabilities = %names.join(", "),
            "per-site [security] capabilities are set but `[php] mode = \"worker\"` cannot honour \
             them: the gate has no per-request site in worker mode, so every gated call is denied \
             (fail-closed). Use per-request mode to grant capabilities, or remove the grants."
        );
    }

    let mut gated_names: Vec<String> = resolution.gated.iter().cloned().collect();
    gated_names.sort_unstable();

    // The map the request-time gate consults: each site's granted capabilities
    // restricted to the ones actually gated. A site whose grants were all
    // redundant/operator-blocked contributes nothing.
    let mut map: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (site, caps) in grants {
        let effective: std::collections::HashSet<String> =
            caps.iter().filter(|c| resolution.gated.contains(*c)).cloned().collect();
        if !effective.is_empty() {
            map.insert(site.clone(), effective);
        }
    }

    // Hand the gate list to the C wrapper (handler swap at module startup) and
    // the map to the Rust gate, before init.
    ephpm_php::PhpRuntime::set_capability_gates(&gated_names)
        .context("failed to register per-site capability gates")?;
    ephpm_php::capabilities::install(map.clone());

    tracing::info!(
        functions = %gated_names.join(", "),
        "per-site function capabilities ENABLED (experimental): these functions were moved from \
         the process-global disable_functions denylist to PER-REQUEST, PER-SITE gating. Only a \
         virtual host explicitly granted a function may call it; every other tenant gets the same \
         'disabled for security reasons' error as before. This is cooperative-tenant isolation at \
         the ePHPm layer, NOT a kernel boundary."
    );
    let mut sites: Vec<&String> = map.keys().collect();
    sites.sort();
    for site in sites {
        let mut caps: Vec<&String> = map[site].iter().collect();
        caps.sort();
        tracing::info!(
            site = %site,
            capabilities = %caps.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
            "per-site capability grant"
        );
    }
    if map.values().filter(|caps| caps.contains("pfsockopen")).count() > 1 {
        tracing::warn!(
            "more than one site was granted `pfsockopen`: a persistent socket lives in the \
             shared per-thread EG(persistent_list) keyed host:port, so two granted sites can \
             inherit each other's live sockets on a reused worker thread. Grant pfsockopen to at \
             most one site, or use fsockopen (non-persistent) instead."
        );
    }

    Ok(())
}

/// After PHP module startup: refuse to run when the `memory_limit` PHP started
/// with is above an explicit `[php] memory_limit_max` (set through
/// `ini_overrides` or `ini_file`, which `Config::validate` cannot see), and log
/// the ceiling scripts are held to — or that there is none.
fn check_memory_limit_ceiling(php: &ephpm_config::PhpConfig) -> anyhow::Result<()> {
    let fmt = |bytes: u64| {
        if bytes == u64::MAX {
            "-1".to_string()
        } else {
            ephpm_config::format_php_memory_quantity(bytes)
        }
    };
    match ephpm_php::PhpRuntime::memory_limit_bounds() {
        Some(bounds) => {
            if php.memory_limit_max.is_some() && bounds.startup_bytes > bounds.ceiling_bytes {
                anyhow::bail!(
                    "PHP started with memory_limit = {} (from [php] ini_overrides or ini_file), \
                     above [php] memory_limit_max = {}. Lower that memory_limit or raise \
                     memory_limit_max (\"-1\" = no ceiling)",
                    fmt(bounds.startup_bytes),
                    fmt(bounds.ceiling_bytes),
                );
            }
            tracing::info!(
                memory_limit = %fmt(bounds.startup_bytes),
                memory_limit_max = %fmt(bounds.ceiling_bytes),
                "PHP memory_limit ceiling: scripts may lower memory_limit and raise it up to \
                 memory_limit_max; ini_set() above it (including -1) is refused"
            );
        }
        // Linked engine with no ceiling: either the operator chose `-1`, or
        // PHP started unlimited and no explicit ceiling was set.
        None if ephpm_php::PhpRuntime::abi().is_some() => tracing::warn!(
            "PHP memory_limit has no ceiling: any script can ini_set('memory_limit', '-1') and \
             allocate without bound, past PHP's hard memory limit. Set [php] memory_limit_max \
             (and a finite memory_limit) to bound it"
        ),
        None => {}
    }
    Ok(())
}

/// Log, at startup, exactly what the multi-tenant hardening preset did — the
/// denylist it added, the performance it cost, and any residual it could not
/// close in this configuration. Never silent: an operator who relies on the
/// preset must be able to see precisely what it bought them.
fn log_multi_tenant_hardening(
    vhost_hardening: bool,
    egress_externally_managed: bool,
    cluster_invalidation: bool,
) {
    if !vhost_hardening {
        // The knob is a loosening switch, so leaving it unheeded is safe — but
        // an operator who set it must not be left believing it did something.
        if egress_externally_managed {
            tracing::warn!(
                "[server.security] network_egress_externally_managed is set but \
                 has no effect: it only lifts the reachability-only fsockopen \
                 block from the multi-tenant hardening preset, and that preset \
                 is not active here (needs sites_dir set and \
                 multi_tenant_hardening on)."
            );
        }
        return;
    }
    if egress_externally_managed {
        tracing::info!(
            "multi-tenant hardening ON: disabled pcntl_*/posix process control, \
             pfsockopen, SysV shm_*/sem_*/msg_*, dl, mail, and \
             opcache_reset/opcache_compile_file via disable_functions; \
             mysqli/pgsql/odbc.allow_persistent=0. network_egress_externally_managed=true, \
             so the reachability-only fsockopen block is LIFTED (egress is \
             enforced below PHP); pfsockopen stays disabled (persistence, not \
             reachability). Cost: persistent DB/socket connections are off \
             (Redis pconnect, mysqli p:, pfsockopen). Disable the whole preset \
             with [server.security] multi_tenant_hardening = false."
        );
    } else {
        tracing::info!(
            "multi-tenant hardening ON: disabled pcntl_*/posix process control, \
             pfsockopen/fsockopen, SysV shm_*/sem_*/msg_*, dl, mail, and \
             opcache_reset/opcache_compile_file via disable_functions; \
             mysqli/pgsql/odbc.allow_persistent=0. Cost: persistent DB/socket connections are \
             off (Redis pconnect, mysqli p:, pfsockopen); fsockopen is blocked as \
             a reachability control (set [server.security] \
             network_egress_externally_managed = true to lift it when egress is \
             enforced at the network/kernel layer). Disable with \
             [server.security] multi_tenant_hardening = false."
        );
    }
    if cluster_invalidation {
        tracing::warn!(
            "multi-tenant hardening: [opcache] cluster_invalidation is ON, so \
             opcache_invalidate / opcache_get_status stay callable by tenants \
             (ePHPm's own cluster invalidator needs them). Residual: a tenant \
             can invalidate cached scripts and read aggregate OPcache metadata. \
             opcache.restrict_api is NOT set for the same reason. Turn off \
             cluster_invalidation to fully lock down the OPcache API."
        );
    } else {
        tracing::info!(
            "multi-tenant hardening: OPcache userland API fully locked down \
             (opcache.restrict_api sentinel + opcache_invalidate/get_status/\
             get_configuration/is_script_cached disabled)."
        );
    }
}

/// Convert a PHP exit code (i32) to a Rust `ExitCode`.
fn exit_code_from(code: i32) -> ExitCode {
    if code == 0 { ExitCode::SUCCESS } else { ExitCode::from(u8::try_from(code).unwrap_or(1)) }
}

/// Initialize PHP and start the HTTP server.
///
/// PHP must be initialized BEFORE the tokio runtime is created. PHP's
/// `php_embed_init()` starts a SIGPROF timer for `max_execution_time`.
/// If tokio worker threads exist when the signal fires, it gets delivered
/// to a non-PHP thread whose signal handler dereferences NULL → SIGSEGV.
///
/// The sequence is:
/// 1. Load config + init tracing (no threads)
/// 2. Init PHP + disable SIGPROF timer (still single-threaded)
/// 3. Create tokio runtime (spawns worker threads — now safe)
/// 4. Run HTTP server
fn run_serve_sync(command: Option<Commands>) -> anyhow::Result<ExitCode> {
    // Load config first (before tracing) so we can use the configured log level.
    let (config, verbose) = load_serve_config(command)?;
    run_with_config(config, verbose, false)
}

/// Shared HTTP server startup path used by both `serve` (production) and
/// `dev` (developer) entry points. Initialises tracing, applies PHP ini
/// overrides, boots the embedded PHP runtime in single-threaded mode, then
/// hands off to the tokio-driven HTTP loop.
///
/// `dev_mode` is `true` on the `ephpm dev` / bare-`ephpm` path and `false` on
/// `ephpm serve`. It only affects the OPcache timestamp-validation default:
/// serve trusts the cache (`validate_timestamps=0`, invalidate via
/// `ephpm deploy`), dev stat-refreshes every request (`validate_timestamps=1`).
/// An explicit `[php] opcache_validate_timestamps` overrides either way.
fn run_with_config(
    config: ephpm_config::Config,
    verbose: u8,
    dev_mode: bool,
) -> anyhow::Result<ExitCode> {
    // Resolve log level: RUST_LOG > -v flag > config > "info"
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbose {
            0 => config.server.logging.level.as_str(),
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(level)
    });

    // If a service backend launched us (e.g. Windows SCM), it sets
    // EPHPM_SERVICE_LOG_FILE so the main tracing layer can be routed to disk
    // — without that, SCM-detached stdout swallows every event and `ephpm
    // logs` has nothing to read. Unix backends rely on systemd/launchd's
    // built-in stdout redirection, so this branch is effectively Windows-only.
    let (fmt_layer, _service_log_guard) = match std::env::var_os("EPHPM_SERVICE_LOG_FILE") {
        Some(raw) if !raw.is_empty() => {
            let path = PathBuf::from(raw);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let dir = path.parent().map_or_else(|| PathBuf::from("."), PathBuf::from);
            let file_name = path
                .file_name()
                .map_or_else(|| "ephpm.log".to_string(), |f| f.to_string_lossy().into_owned());
            let (writer, guard) =
                tracing_appender::non_blocking(tracing_appender::rolling::never(dir, file_name));
            let layer =
                tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(false).boxed();
            (layer, Some(guard))
        }
        _ => (tracing_subscriber::fmt::layer().boxed(), None),
    };

    // OTLP trace export (compiled with `--features otlp`; runtime activation
    // is opt-in via OTEL_EXPORTER_OTLP_TRACES_ENDPOINT /
    // OTEL_EXPORTER_OTLP_ENDPOINT or [server.diagnostics] otlp_endpoint, the
    // env vars winning). Resolved before the subscriber is built so the
    // export layer joins the same registry; when nothing requests it, no
    // exporter is built and no background thread is spawned.
    #[cfg(feature = "otlp")]
    let otlp = ephpm_server::otlp::init_layer(
        config.server.diagnostics.otlp_endpoint.as_deref(),
        config.server.diagnostics.otlp_protocol.as_deref(),
    )
    .context("failed to initialize OTLP trace export")?;

    // Layer stack. The env filter must be scoped to the fmt layer with
    // `with_filter`, never pushed into the Vec as a sibling layer: `Vec`'s
    // `Layer` impl merges per-layer `Interest` by taking the MAX, so an
    // unfiltered fmt layer (`Interest::always`) bypasses a sibling
    // `EnvFilter` entirely — RUST_LOG / -v / [server.logging] level were
    // silently inert in non-OTLP builds when the filter was a sibling.
    // (Scoping also keeps a global info-level filter from disabling the
    // DEBUG-level request spans the OTLP layer needs; that layer carries
    // its own target filter.)
    // Access records travel on their own tracing target (`access_log`, see
    // `ephpm_server::router`) to the dedicated file layer below. Silence that
    // target on the main fmt layer so an enabled access log is written only to
    // its file and never *also* echoed to stdout / the service log. Harmless
    // when access logging is off — the router emits nothing on that target.
    let env_filter = env_filter
        .add_directive("access_log=off".parse().expect("static access_log directive parses"));

    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> = Vec::new();
    layers.push(fmt_layer.with_filter(env_filter).boxed());

    // Set up access log file writer if configured. One structured-JSON record
    // per served request is emitted by the router on the `access_log` target;
    // this layer is the only subscriber to it and writes to the configured
    // file. JSON (rather than a combined/common text line) is deliberate: the
    // record carries attacker-influenced fields (request path, client IP via
    // X-Forwarded-For), and serde's escaping makes log-injection impossible,
    // matching ePHPm's other structured diagnostics (`/_ephpm/requests`).
    let _access_guard = if config.server.logging.access.is_empty() {
        None
    } else {
        let access_path = PathBuf::from(&config.server.logging.access);
        let access_dir = access_path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let access_file = access_path
            .file_name()
            .map_or_else(|| "access.log".to_string(), |f| f.to_string_lossy().into_owned());
        let (access_writer, guard) = tracing_appender::non_blocking(
            tracing_appender::rolling::never(access_dir, access_file),
        );
        let access_layer = tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(access_writer)
            .with_filter(EnvFilter::new("access_log=info"));
        layers.push(access_layer.boxed());
        Some(guard)
    };

    // The guard flushes the exporter's batch queue on drop — hold it until
    // the server has exited.
    #[cfg(feature = "otlp")]
    let _otlp_guard = otlp.map(|(otlp_layer, guard, info)| {
        layers.push(otlp_layer.boxed());
        (guard, info)
    });

    tracing_subscriber::registry().with(layers).init();

    #[cfg(feature = "otlp")]
    if let Some((_, ref info)) = _otlp_guard {
        tracing::info!(
            endpoint = %info.description,
            protocol = info.protocol,
            "OTLP trace export enabled"
        );
        // Legal-but-probably-wrong configuration (e.g. the other transport's
        // conventional port). Never fatal — see `otlp::port_mismatch_warning`.
        for warning in &info.warnings {
            tracing::warn!("{warning}");
        }
    }

    // add-config-knob: `otlp_protocol` only means anything alongside an
    // endpoint. Setting it alone exports nothing, which must not look like it
    // took effect.
    #[cfg(feature = "otlp")]
    if _otlp_guard.is_none() && config.server.diagnostics.otlp_protocol.is_some() {
        tracing::warn!(
            "[server.diagnostics] otlp_protocol is set but no OTLP endpoint is configured \
             (otlp_endpoint / OTEL_EXPORTER_OTLP_*), so no spans are exported and the \
             protocol has no effect."
        );
    }

    // add-config-knob: never let an OTLP request die silently in a binary
    // compiled without the exporter.
    #[cfg(not(feature = "otlp"))]
    {
        let env_requested = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"]
            .iter()
            .any(|var| std::env::var(var).is_ok_and(|v| !v.is_empty()));
        if config.server.diagnostics.otlp_endpoint.is_some() || env_requested {
            tracing::warn!(
                "OTLP trace export requested ([server.diagnostics] otlp_endpoint or an \
                 OTEL_EXPORTER_OTLP_* env var is set), but this binary was built without \
                 the `otlp` cargo feature — no spans are exported. Rebuild with \
                 `cargo build --features otlp`."
            );
        }
    }

    tracing::info!(
        listen = %config.server.listen,
        document_root = %config.server.document_root.display(),
        "starting ePHPm"
    );

    // Never let a parsed-but-unimplemented knob look like it took effect.
    warn_unimplemented_knobs(&config);

    // `EPHPM_SERVER_LISTEN` (single underscore) maps to no key and the root
    // config is lenient, so figment drops it silently. Say so. A variable the
    // config itself references as `env:NAME` (a middleware secret) is read,
    // just not as a key, so it is not reported.
    let env_refs = config.env_references();
    for var in ephpm_config::unmapped_env_vars(
        std::env::vars_os()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .filter(|k| !env_refs.iter().any(|r| r == k)),
    ) {
        match var.suggestion {
            Some(suggestion) => tracing::warn!(
                var = %var.name,
                "environment variable {} maps to no config key and is ignored — did you \
                 mean {suggestion}? Nested keys are separated by a double underscore",
                var.name
            ),
            None => tracing::warn!(
                var = %var.name,
                "environment variable {} maps to no config key and is ignored (config \
                 overrides look like EPHPM_<SECTION>__<KEY>, e.g. EPHPM_SERVER__LISTEN)",
                var.name
            ),
        }
    }

    // Build the effective PHP ini file. If the user specified ini_overrides
    // in the config, we have to materialize them on disk and load them via
    // PHP's normal ini path: setting them at runtime via zend_alter_ini_entry
    // only updates the calling thread's per-thread globals, which doesn't
    // propagate to tokio worker threads under ZTS. Loading a real .ini file
    // routes through MINIT, where values land in the shared ini directives
    // table that every new TSRM thread sees.
    // disable_functions only takes effect during PHP's MINIT
    // (zend_disable_functions reads the ini value once and removes the
    // entries from CG(function_table)). Setting it via runtime
    // zend_alter_ini_entry just changes the ini string and leaves the
    // functions callable, so vhost-mode disable_shell_exec needs to ride
    // along on the generated ini instead of the per-request ini hook.
    let vhost_disable_shell =
        config.server.sites_dir.is_some() && config.server.effective_disable_shell_exec();
    // Multi-tenant hardening preset: in vhost mode, extend the denylist to the
    // full set a hostile-tenant pentest proved closes every cross-tenant
    // read/write channel (SysV IPC, persistent-socket inheritance, pcntl/posix
    // process control, OPcache flush). Composed as a UNION with any operator
    // `disable_functions` (never clobbered — see `compose_disable_functions`).
    let vhost_hardening =
        config.server.sites_dir.is_some() && config.server.effective_multi_tenant_hardening();
    // When the operator asserts egress is enforced below PHP (network/kernel
    // layer), drop the reachability-only `fsockopen` block from the preset —
    // blocking it while `stream_socket_client`/`curl` stay open is inconsistent
    // and bypassable. Persistence (`pfsockopen`, `*.allow_persistent`) and the
    // non-reachability blocks are unaffected. Resolved independently of
    // `vhost_hardening` so we can warn when it is set but does nothing.
    let egress_externally_managed = config.server.effective_network_egress_externally_managed();
    let include_reachability_block = !egress_externally_managed;
    // ePHPm's own cluster OPcache invalidator (opcache.rs) calls the userland
    // `opcache_get_status`/`opcache_invalidate` through the function table, so
    // the hardening preset may only fully lock down the OPcache API
    // (opcache.restrict_api + disabling those two) when cluster invalidation is
    // OFF. When it is on, only the DoS-grade `opcache_reset`/`opcache_compile_file`
    // are disabled and the residual is logged. Resolve it here so the ini
    // builder and the startup log agree.
    let cluster_invalidation =
        config.opcache.effective_cluster_invalidation(config.cluster.enabled);
    // Per-site function capabilities (experimental, opt-in). Scan the operator's
    // per-site overrides directory ONCE, here, before PHP init: the set of
    // functions a grant moves from global-disable to per-request gating is fixed
    // at MINIT, so the grants that decide it are read now and frozen. Only active
    // in multi-tenant mode with an overrides directory configured; otherwise the
    // scan is skipped and everything below is a no-op (empty grants), keeping
    // `disable_functions` byte-identical to a build without this feature.
    let operator_df_for_caps = collect_operator_disable_functions(&config.php.ini_overrides);
    let capability_grants = if let Some(overrides_dir) =
        config.server.effective_site_overrides_dir().filter(|_| config.server.sites_dir.is_some())
    {
        ephpm_server::scan_site_capabilities(overrides_dir).map_err(|reason| {
            anyhow::anyhow!(
                "a per-site [security] capabilities grant names an unknown capability: {reason}. \
                 Accepted capabilities: {}",
                ephpm_config::GRANTABLE_CAPABILITIES.join(", ")
            )
        })?
    } else {
        std::collections::BTreeMap::new()
    };
    // Union of every site's grants, then partitioned against the effective
    // hardening baseline: `gated` is what actually changes (left registered,
    // gated per request); `redundant`/`operator_blocked` are logged so a grant
    // that does nothing is never silent.
    let granted_union: std::collections::BTreeSet<String> =
        capability_grants.values().flatten().cloned().collect();
    let gated_resolution = resolve_gated(
        &granted_union,
        &operator_df_for_caps,
        vhost_disable_shell,
        vhost_hardening,
        include_reachability_block,
        cluster_invalidation,
    );
    let gated_capabilities = gated_resolution.gated.clone();
    // Worker mode needs the generated ini for log_errors=On (seeded for every
    // mode now, see `error_display_ini_defaults`): a worker script that dies
    // during boot leaves no diagnostic otherwise — display_errors output is
    // captured into a buffer that is discarded when no request is in flight.
    let worker_mode = config.php.is_worker_mode();
    // OPcache timestamp-validation default is mode-dependent (off under serve,
    // on under dev) and is always emitted into the generated ini, so ini
    // generation is now unconditional. The other flags below still document
    // *why* generation would be needed even absent the opcache line.
    // Resolve the full resource-aware autotuning profile once: it feeds both
    // the generated php.ini (opcache/memory/realpath/assertions lines) and the
    // startup autotune summary log below.
    // The JIT defaults to `disable` in every mode, but the *reason* logged at
    // startup depends on tenancy (`sites_dir` set: per-vhost invalidation
    // never reclaims JIT buffer) — so autotune needs to know whether this is
    // a vhost run.
    let autotune = config.php.autotune(dev_mode, config.server.sites_dir.is_some());
    let opcache_ini_lines = autotune.ini_lines();
    let validate_timestamps = autotune.validate_timestamps.value;
    // [php] extensions also forces ini generation: `extension=` lines only
    // take effect when PHP parses them during MINIT, same as the overrides.
    // When PHP has per-thread execution timers, we always emit an ini so the
    // configured max_execution_time is authoritative (PHP arms its own timer
    // from it). cfg!() is a compile-time constant here — false in stub mode and
    // on builds without native timers, so this adds no ini on those targets.
    let want_generated_ini = !opcache_ini_lines.is_empty()
        || !config.php.ini_overrides.is_empty()
        || !config.php.extensions.is_empty()
        || vhost_disable_shell
        || vhost_hardening
        || worker_mode
        || cfg!(php_max_exec_timers)
        // The shared engine's extension preload rides the generated ini.
        || ephpm_php::shared::SharedRuntime::is_shared_build();

    let (effective_ini_path, _generated_ini_guard): (Option<PathBuf>, Option<tempfile::TempDir>) =
        if want_generated_ini {
            use std::fmt::Write as _;

            let mut content = String::new();
            // Error display/logging defaults, first so `ini_file` and
            // `ini_overrides` win.
            // Serve never renders PHP diagnostics into the HTTP response —
            // they carry absolute filesystem paths — and logs them to ePHPm's
            // own log instead. Dev keeps them on screen. Worker mode also
            // relies on log_errors: a worker script that dies during boot has
            // no response to display into.
            for (k, v) in error_display_ini_defaults(dev_mode) {
                let _ = writeln!(content, "{k}={v}");
            }
            // Windows shared-engine build: extension_dir + the extensions the
            // static build compiles in but the official php8ts.dll ships as
            // DLLs (mbstring, openssl, intl, ...). First, so `[php]
            // extensions` that depend on them load after them and every
            // operator setting below can still override. No-op elsewhere.
            if let Some(shared) = ephpm_php::shared::SharedRuntime::detect() {
                for line in shared
                    .ini_lines()
                    .context("the shared PHP engine's runtime files are incomplete")?
                {
                    let _ = writeln!(content, "{line}");
                }
            }
            // Shared extensions ([php] extensions) go first, before
            // ini_file/ini_overrides, so any extension ini settings that
            // follow apply to an already-declared extension. A bare name
            // rides PHP's extension_dir search (`extension=redis`); a path
            // loads verbatim (`extension=/path/to/imagick.so`). A library
            // that exports `zend_extension_entry` (Xdebug, OPcache-style) is
            // emitted as `zend_extension=` instead — with an absolute path,
            // since PHP resolves a relative zend_extension against
            // extension_dir (see `ext_ini`). ABI mismatches (PHP minor / ZTS
            // / libc) are rejected by PHP at startup with an explicit
            // API-version error.
            if !config.php.extensions.is_empty() {
                let abi = ephpm_php::PhpRuntime::abi();
                // The default Windows build links PHP statically: it cannot
                // load extension DLLs (they import php8ts.dll), and when some
                // php8ts.dll is reachable the load attaches a second PHP
                // engine and crashes the process. Refuse instead.
                if cfg!(windows)
                    && abi.is_some()
                    && !ephpm_php::shared::SharedRuntime::is_shared_build()
                {
                    anyhow::bail!(
                        "[php] extensions is not supported by this Windows build: it links PHP \
                         statically and cannot load extension DLLs. Use the shared-engine build \
                         (`cargo xtask release --target windows --variant shared`), or remove \
                         [php] extensions"
                    );
                }
                let cwd = std::env::current_dir().context("failed to read current directory")?;
                let ext_dir = ext_ini::effective_extension_dir(
                    &config.php.ini_overrides,
                    abi.as_ref().and_then(|a| a.default_extension_dir.as_deref()),
                );
                for ext in &config.php.extensions {
                    let line = ext_ini::resolve_extension_line(ext, &cwd, ext_dir.as_deref());
                    let ini_line = line.ini_line()?;
                    if line.directive == ext_ini::LoadDirective::ZendExtension {
                        tracing::info!(
                            entry = %ext,
                            line = %ini_line,
                            "[php] extensions: loading as a Zend extension (exports \
                             zend_extension_entry)"
                        );
                    }
                    let _ = writeln!(content, "{ini_line}");
                }
            }
            if let Some(base) = &config.php.ini_file {
                let base_content = std::fs::read_to_string(base).with_context(|| {
                    format!("failed to read php.ini file at {}", base.display())
                })?;
                content.push_str(&base_content);
                if !content.ends_with('\n') {
                    content.push('\n');
                }
            }
            // OPcache timestamp-validation default (mode-dependent) + optional
            // revalidate_freq. Before ini_overrides so an operator can still
            // force a different value through ini_overrides if they insist.
            for (k, v) in &opcache_ini_lines {
                let _ = writeln!(content, "{k}={v}");
            }
            // Native max_execution_time (per-thread execution timers only).
            // Written before ini_overrides, so an explicit override still wins.
            // PHP arms/disarms its own per-thread POSIX timer from this value on
            // each php_request_startup/shutdown; set_time_limit() re-arms it live.
            #[cfg(php_max_exec_timers)]
            {
                let _ = writeln!(content, "max_execution_time={}", config.php.max_execution_time);
            }
            // Emit every operator `ini_overrides` line EXCEPT `disable_functions`:
            // that one is folded into the composed union below so an operator's
            // additions can never be clobbered by ePHPm's baseline (PHP ini is
            // last-wins, and historically ePHPm appended its own
            // `disable_functions` line last — silently discarding the operator's).
            for [k, v] in &config.php.ini_overrides {
                if k.trim().eq_ignore_ascii_case("disable_functions") {
                    continue;
                }
                let _ = writeln!(content, "{k}={v}");
            }
            // Compose the effective `disable_functions` as the UNION of ePHPm's
            // baseline (shell family when disable_shell_exec is on, plus the
            // full hardening set when the preset is on) and any operator list.
            let operator_df = collect_operator_disable_functions(&config.php.ini_overrides);
            if let Some(df) = compose_disable_functions(
                &operator_df,
                vhost_disable_shell,
                vhost_hardening,
                include_reachability_block,
                cluster_invalidation,
                // Functions a per-site capability grant moves to per-request
                // gating are omitted from the baseline here so they stay
                // registered (empty set when no site granted anything).
                &gated_capabilities,
            ) {
                let _ = writeln!(content, "disable_functions={df}");
            }
            // Extra hardening ini (multi-tenant preset only). Persistent
            // connection handles live in `EG(persistent_list)`, keyed without a
            // tenant component and surviving request end on a shared ZTS worker,
            // so one tenant could inherit another's live handle — disable
            // persistence across every driver that exposes an ini for it
            // (mysqli, pgsql/`pg_pconnect`, odbc). PDO `ATTR_PERSISTENT` has no
            // global ini and cannot be forced off here (documented residual).
            // Also point the OPcache userland API at an unreachable sentinel so
            // a tenant cannot reset/inspect the shared cache — but only when
            // ePHPm's own cluster invalidator does not itself need that API.
            if vhost_hardening {
                let _ = writeln!(content, "mysqli.allow_persistent=0");
                let _ = writeln!(content, "pgsql.allow_persistent=0");
                let _ = writeln!(content, "odbc.allow_persistent=0");
                if !cluster_invalidation {
                    let _ =
                        writeln!(content, "opcache.restrict_api={OPCACHE_RESTRICT_API_SENTINEL}");
                }
            }
            // This file inlines the operator's entire `[php] ini_file` plus
            // every `ini_override`, and PHP reads it back during MINIT — as
            // root on most deployments. A fixed, PID-derived name under a
            // shared /tmp is attacker-reachable: `fs::write` follows symlinks
            // and truncates, so a pre-planted symlink turns startup into an
            // arbitrary-file truncation, and the window between the write and
            // PHP's read is enough to swap in `extension=` or
            // `auto_prepend_file=`.
            //
            // Same private-tempdir shape used elsewhere for extracting a
            // sensitive file: mkdtemp (O_EXCL, never a reused path), 0700, then `create_new`
            // so the open still refuses to follow a symlink or clobber an
            // existing file. The `TempDir` guard owns the cleanup and must
            // outlive PHP's read, so it is what the caller holds on to.
            use std::io::Write as _;

            let dir = tempfile::Builder::new()
                .prefix("ephpm-ini-")
                .tempdir()
                .context("failed to create a private temp dir for the generated php.ini")?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                    .context("failed to lock down the generated php.ini directory")?;
            }

            let temp_path = dir.path().join("overrides.ini");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
                .context("failed to create the generated php.ini")?;
            file.write_all(content.as_bytes()).context("failed to write the generated php.ini")?;
            file.sync_all().context("failed to flush the generated php.ini")?;
            drop(file);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))
                    .context("failed to lock down the generated php.ini")?;
            }

            tracing::debug!(path = %temp_path.display(), "wrote generated php.ini");
            (Some(temp_path), Some(dir))
        } else {
            (config.php.ini_file.clone(), None)
        };

    // Surface exactly what the multi-tenant hardening preset did (or its
    // residual when cluster invalidation forces the OPcache API to stay open).
    log_multi_tenant_hardening(vhost_hardening, egress_externally_managed, cluster_invalidation);

    // Resource-aware autotuning: log the detected CPU/memory budget and the
    // derived (or explicitly-pinned) PHP/OPcache profile at INFO. Trust
    // requires visibility — an operator must be able to see exactly what
    // ePHPm sized itself to and which values they overrode (marked `*`).
    tracing::info!("{}", autotune.summary_line());

    // State the JIT contract at startup: the default is shaped (tracing in
    // single-site serve, off in multi-tenant/worker/dev), so the effective
    // state and its reason must never be silent. A config that works but
    // carries a documented hazard (JIT forced on in multi-tenant mode; JIT on
    // with a zero buffer) additionally warns.
    tracing::info!("{}", autotune.jit_line());
    if let Some(warning) = autotune.jit_warning() {
        tracing::warn!("{warning}");
    }
    // Windows: an explicit opcache SHM size above the derived ceiling is
    // honoured, but its failure mode is a hard exit(-2) inside PHP's module
    // startup, so it must not be silent.
    if let Some(warning) = autotune.shm_warning() {
        tracing::warn!("{warning}");
    }

    // State the OPcache staleness contract at startup so operators know how
    // code changes reach a running server. Under serve mode with validation
    // off, the ONLY way to refresh cached code is `ephpm deploy` /
    // `ephpm cache reset` — which write through the RESP listener. If that
    // listener is disabled there is no invalidation lever at all, so warn
    // loudly rather than let an operator get stuck with frozen code.
    if validate_timestamps {
        tracing::info!(
            "opcache timestamp validation ON ({} mode); code changes are picked \
             up automatically",
            if dev_mode { "dev" } else { "serve" }
        );
    } else {
        tracing::info!(
            "opcache timestamp validation OFF (serve mode); code changes require \
             `ephpm deploy` or `ephpm cache reset`"
        );
        if !config.kv.redis_compat.enabled {
            tracing::warn!(
                "opcache timestamp validation is OFF but the RESP listener is \
                 disabled ([kv.redis_compat] enabled = false) — `ephpm deploy` / \
                 `ephpm cache reset` cannot reach this server, so cached code can \
                 only be refreshed by restarting. Enable the RESP listener, or set \
                 [php] opcache_validate_timestamps = true."
            );
        }
    }

    // Ceiling on runtime raises of memory_limit. Handed over before init: the
    // hook that enforces it is installed during module startup.
    ephpm_php::PhpRuntime::set_memory_limit_max(config.php.memory_limit_max.as_deref())
        .context("failed to apply [php] memory_limit_max")?;

    // Per-site function capabilities. Hand the gate list to the C wrapper and
    // the site→capabilities map to the request-time gate, BEFORE init: the
    // handler swap runs during module startup. Both are no-ops when no site
    // granted anything (empty list, empty map). Logged either way so the move
    // from global-disable to per-site gating, and who holds what, is never
    // silent — this widens the shared-process surface, so an operator must see
    // exactly what it bought.
    apply_per_site_capabilities(&gated_resolution, &capability_grants, worker_mode)?;

    // Initialize PHP BEFORE creating tokio runtime (single-threaded here).
    // finalize_for_http() disables SIGPROF so it can't crash worker threads.
    ephpm_php::PhpRuntime::init_with_ini_file(effective_ini_path.as_deref())
        .context("failed to initialize PHP runtime")?;
    check_memory_limit_ceiling(&config.php)?;
    ephpm_php::PhpRuntime::finalize_for_http()
        .context("failed to finalize PHP runtime for HTTP")?;
    // `ephpm dev --xdebug`: prove Xdebug registered as a Zend extension (PHP
    // only warns on a failed load) and print its banner. No-op otherwise.
    dev_xdebug::verify_after_init()?;
    // Arm value for PHP's per-thread execution timer. The request paths call
    // zend_set_timeout() with this each request because the embed SAPI zeroes
    // the max_execution_time ini entry at runtime. No-op on builds without
    // per-thread timers and in stub mode.
    ephpm_php::PhpRuntime::set_max_execution_time(config.php.max_execution_time);

    // Stack-overflow crash containment (experimental `[php] crash_containment`).
    //
    // Armed here, on the single-threaded startup path, before any PHP request
    // exists — and the recovery hook is registered BEFORE the guard is enabled,
    // so a guard can never be armed with no hook to recover it. Left entirely
    // alone when the knob is off: no hook is registered, no guard is ever armed,
    // and a C-stack overflow aborts the process exactly as it always has.
    //
    // Windows has no containment at all (the guard is POSIX-signal based and
    // Windows raises SEH exceptions), so there the knob is ignored — and said
    // so, rather than logging the "containment is ON" banner below for a
    // mechanism that cannot fire (issue #454).
    if config.php.crash_containment && !cfg!(unix) {
        tracing::warn!(
            "[php] crash_containment = true is IGNORED on Windows — stack-overflow \
             containment is Unix-only (it relies on POSIX signal handling; Windows \
             delivers the fault as an SEH exception). A PHP C-stack overflow will \
             abort the process as usual."
        );
    } else if config.php.is_crash_containment_active() {
        fatal_signal::set_recover_hook(ephpm_php::crash_guard::recover_hook());
        ephpm_php::crash_guard::set_enabled(true);
        tracing::warn!(
            "[php] crash_containment is ON (experimental): a PHP C-stack overflow \
             will be contained — the request gets a 500 and its pool thread is \
             retired — instead of killing the process. Heap corruption is NOT \
             contained and still aborts. Each contained crash leaks the poisoned \
             thread's PHP context and makes this process skip PHP module shutdown \
             at exit."
        );
    } else if config.php.crash_containment {
        tracing::warn!(
            "[php] crash_containment = true is IGNORED in worker mode — containment \
             must be able to retire the thread that crashed, and a persistent worker \
             thread holds framework state that cannot be abandoned mid-flight. \
             Crashes will abort the process as usual."
        );
    }

    // PHP's hard memory limit (PHP 8.3.36 / 8.4.27 / 8.5.12+) ends in
    // `_exit(137)`. The crash guard contains it on Linux in both modes, with
    // no knob; elsewhere it still ends the process. Say which, once.
    if ephpm_php::PhpRuntime::engine_has_hard_memory_limit() {
        if ephpm_php::crash_guard::exit_containment_available() {
            tracing::info!(
                "PHP hard memory limit (memory_hard_limit_percent): a request that crosses \
                 it is contained — answered 500, its PHP thread retired — instead of ending \
                 the process"
            );
        } else {
            tracing::warn!(
                "PHP hard memory limit (memory_hard_limit_percent) is NOT contained on this \
                 platform: a request that crosses it ends the whole ePHPm process (exit \
                 status 137). Budget memory against the hard limit and run under a \
                 supervisor that restarts the process"
            );
        }
    }

    // Now safe to create the multi-threaded tokio runtime.
    //
    // Note: [php] concurrency bounds ePHPm's own dedicated execution pool
    // (built in the router / worker pool), NOT tokio's blocking pool — that
    // pool is shared with static file I/O and other blocking work, and slow
    // PHP scripts must never starve it.
    //
    // Identical to `Runtime::new()` (multi-thread, all drivers enabled) except
    // for the thread hook, which gives every worker and blocking-pool thread an
    // alternate signal stack if it lacks one. Without an alternate stack the
    // fatal-signal handler cannot run for a stack-overflow fault — which is
    // precisely one of the faults worth diagnosing. PHP executes on this
    // runtime's blocking pool, so the hook must be on this runtime.
    //
    // `thread_stack_size` covers the blocking pool as well as the async
    // workers, and the blocking pool is where PHP runs in the default fpm
    // engine. PHP's own C-stack guard bounds recursion at the *thread's* stack
    // size, so leaving tokio's 2 MiB default would make deep-but-legal PHP
    // (a nested template/block render) fail at a quarter of the depth php-fpm
    // allows. See `ephpm_php::PHP_THREAD_STACK` (issue #116).
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .thread_stack_size(ephpm_php::PHP_THREAD_STACK)
        .on_thread_start(fatal_signal::install_thread_altstack);
    // `[server] io_threads`: a non-zero value sizes tokio's async worker pool
    // (and overrides TOKIO_WORKER_THREADS); `0` leaves tokio's own default.
    // The blocking pool and the PHP execution pool are unaffected.
    let io_threads = config.server.io_threads;
    if io_threads > 0 {
        builder.worker_threads(io_threads);
    }
    let rt = builder.build().context("failed to create tokio runtime")?;
    {
        let (effective, source) = effective_io_threads(io_threads);
        tracing::info!(io_threads = effective, source, "tokio async I/O threads");
    }
    let result = rt.block_on(async { ephpm_server::serve(config, dev_mode).await });

    // Shut the runtime down BEFORE tearing down PHP (issue #266): every
    // background task (cluster channel, replication drivers, DB listeners)
    // is dropped at its next await and the worker and blocking-pool threads
    // are joined first. Only after that is php_embed_shutdown() considered:
    // module shutdown's ts_free_id() walks all remaining TSRM entries on
    // *this* thread, and a still-live PHP thread's entry would make it free
    // another thread's request-lifetime allocations cross-heap — zend_mm_heap
    // corrupted → SIGABRT. PHP no longer runs on this runtime's threads (it
    // has its own pools, whose drain `serve` already waited for), and
    // `PhpRuntime::shutdown()` skips module teardown while any PHP-registered
    // thread is still live, so the join is not what keeps that safe.
    //
    // Bounded, unlike a plain `drop(rt)`, which waits for every blocking task
    // indefinitely: one stuck blocking call must not turn SIGTERM into a hang
    // that only SIGKILL ends (stress report F8). `serve` has already spent
    // the configured `[server.timeouts] shutdown` grace draining requests;
    // this is only the tail.
    let runtime_stop = std::time::Instant::now();
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    let stop_elapsed = runtime_stop.elapsed();
    if stop_elapsed >= RUNTIME_SHUTDOWN_GRACE {
        tracing::warn!(
            grace_secs = RUNTIME_SHUTDOWN_GRACE.as_secs(),
            "async runtime did not stop within its shutdown grace — exiting with blocking \
             work still running"
        );
    } else {
        tracing::info!(
            elapsed_ms = u64::try_from(stop_elapsed.as_millis()).unwrap_or(u64::MAX),
            "async runtime stopped"
        );
    }

    // Shutdown PHP runtime — all PHP threads are gone, only the main
    // thread's TSRM entry remains.
    ephpm_php::PhpRuntime::shutdown().context("failed to shutdown PHP runtime")?;

    result.map(|()| ExitCode::SUCCESS)
}

/// Resolve the tokio worker-thread count the runtime was built with, and why,
/// for the startup log line. Mirrors tokio's own resolution: an explicit
/// `[server] io_threads` wins, else a valid `TOKIO_WORKER_THREADS`, else the
/// host's available parallelism. (An *invalid* `TOKIO_WORKER_THREADS` makes
/// tokio panic at build time, before this runs.)
fn effective_io_threads(configured: usize) -> (usize, &'static str) {
    if configured > 0 {
        return (configured, "explicit");
    }
    if let Some(n) = std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return (n, "TOKIO_WORKER_THREADS");
    }
    (
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        "host_parallelism",
    )
}

/// Parse the Serve command and load configuration.
///
/// Called before tracing is initialized, so no logging here.
/// Returns `(config, verbose_level)`.
fn load_serve_config(command: Option<Commands>) -> anyhow::Result<(ephpm_config::Config, u8)> {
    let Commands::Serve { config, listen, document_root, verbose } =
        command.unwrap_or(Commands::Serve {
            config: PathBuf::from("ephpm.toml"),
            listen: None,
            document_root: None,
            verbose: 0,
        })
    else {
        unreachable!("load_serve_config called with non-Serve command");
    };

    let mut config = if config.exists() {
        ephpm_config::Config::load(&config).context("failed to load configuration")?
    } else {
        ephpm_config::Config::default_config()?
    };

    // CLI overrides take precedence
    if let Some(addr) = listen {
        config.server.listen = addr;
    }
    if let Some(root) = document_root {
        config.server.document_root = root;
    }

    // Relative `document_root` / `sites_dir` resolve against the working
    // directory, like every other relative path in the config — but they must
    // be made absolute here, because PHP resolves a relative script path
    // against its own per-thread working directory, not ours.
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    config.resolve_relative_roots(&cwd);

    // Validate cross-field invariants (e.g. the worker-mode script) AFTER
    // CLI overrides so document_root is final. Fails fast with a clear message.
    config.validate().context("invalid configuration")?;

    Ok((config, verbose))
}

// ─────────────────────────────────────────────────────────────────────────────
// KV Store CLI Subcommands
// ─────────────────────────────────────────────────────────────────────────────

/// RESP credentials used for the CLI's short-lived connections.
///
/// The server turns `AUTH` on when either `[kv.redis_compat] password` or
/// `[kv] secret` is set, and until a connection authenticates it answers every
/// command except `AUTH` and `QUIT` with `-NOAUTH Authentication required`.
/// There are two server-side modes:
///
/// - **Legacy single password** (`[kv.redis_compat] password`) — satisfied by
///   `--password` alone, which sends `AUTH <password>`.
/// - **Per-site HMAC** (`[kv] secret` together with `[server] sites_dir`) — the
///   server expects `AUTH <hostname> <HMAC-SHA256(secret, hostname)>` and
///   scopes the connection to that vhost's store. Satisfied by
///   `--user <hostname> --password <derived>`; the derived value is exactly
///   what ePHPm injects into PHP as `EPHPM_REDIS_PASSWORD`. Note that
///   `deploy` / `cache reset` write keys the server reads from the *default*
///   store, which a site-scoped connection cannot reach — those two commands
///   only work under legacy-password mode.
#[derive(Debug, Clone)]
struct KvAuth {
    /// First `AUTH` argument — the vhost hostname under per-site HMAC auth.
    user: Option<String>,
    /// The password. `None` means send no `AUTH` command at all.
    password: Option<String>,
}

impl KvAuth {
    /// Resolve credentials from the CLI flags, falling back to the
    /// `EPHPM_KV_PASSWORD` environment variable when `--password` is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when `--user` is given with no password, since the
    /// two-argument `AUTH` form cannot be built from a username alone.
    fn resolve(user: Option<String>, password: Option<String>) -> anyhow::Result<Self> {
        let password =
            password.or_else(|| std::env::var("EPHPM_KV_PASSWORD").ok()).filter(|p| !p.is_empty());
        if user.is_some() && password.is_none() {
            anyhow::bail!(
                "--user requires a password — pass --password, or set the \
                 EPHPM_KV_PASSWORD environment variable"
            );
        }
        Ok(Self { user, password })
    }

    /// The `AUTH` frame to send before the first command, or `None` when no
    /// credentials were supplied.
    fn frame(&self) -> Option<Frame> {
        let password = self.password.as_ref()?;
        let mut args = vec![Frame::bulk(b"AUTH".to_vec())];
        if let Some(user) = &self.user {
            args.push(Frame::bulk(user.as_bytes().to_vec()));
        }
        args.push(Frame::bulk(password.as_bytes().to_vec()));
        Some(Frame::Array(args))
    }
}

/// Dispatcher for all KV subcommands.
async fn run_kv(
    host: &str,
    port: u16,
    auth: &KvAuth,
    sub: KvSubcommand,
) -> anyhow::Result<ExitCode> {
    match sub {
        KvSubcommand::Ping => kv_ping(host, port, auth).await,
        KvSubcommand::Keys { pattern } => kv_keys(host, port, auth, &pattern).await,
        KvSubcommand::Get { key } => kv_get(host, port, auth, &key).await,
        KvSubcommand::Set { key, value, ttl } => kv_set(host, port, auth, &key, &value, ttl).await,
        KvSubcommand::Del { keys } => kv_del(host, port, auth, &keys).await,
        KvSubcommand::Incr { key, by } => kv_incr(host, port, auth, &key, by).await,
        KvSubcommand::Ttl { key } => kv_ttl(host, port, auth, &key).await,
        // Dispatched before the runtime is built (see `run`); unreachable here.
        KvSubcommand::Snapshot { .. } => {
            anyhow::bail!("kv snapshot is dispatched without a runtime")
        }
    }
}

/// TCP connection helper. Authenticates before returning, so every caller
/// gets a stream that is ready for real commands.
async fn kv_connect(host: &str, port: u16, auth: &KvAuth) -> anyhow::Result<TcpStream> {
    let mut stream = kv_tcp_connect(host, port)
        .await
        .with_context(|| format!("could not connect to KV server at {host}:{port}"))?;
    kv_authenticate(&mut stream, auth).await?;
    Ok(stream)
}

/// Open a TCP connection to `host:port`, where `host` is a hostname, an IPv4
/// literal, or an IPv6 literal with or without brackets (`::1` / `[::1]`).
///
/// This used to parse `"{host}:{port}"` as a `SocketAddr`, which accepts only
/// IP literals — so `--host localhost` or `--host kv.internal` failed with
/// "invalid address" — and mis-split a bare IPv6 literal on its colons. Passing
/// the `(host, port)` pair lets the resolver handle names and tries every
/// address it returns, so `localhost` works whether it resolves to `::1` or
/// `127.0.0.1` first.
async fn kv_tcp_connect(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    TcpStream::connect((host, port)).await
}

/// Send `AUTH` and check the reply.
///
/// A no-op when no password was supplied: the server only demands `AUTH` when
/// it is configured with credentials, and sending one unprompted is harmless
/// but pointless.
async fn kv_authenticate(stream: &mut TcpStream, auth: &KvAuth) -> anyhow::Result<()> {
    let Some(frame) = auth.frame() else {
        return Ok(());
    };
    kv_send(stream, &frame).await?;
    match kv_recv(stream).await? {
        Frame::Simple(_) => Ok(()),
        Frame::Error(e) => anyhow::bail!("KV authentication failed: {e}"),
        other => anyhow::bail!("unexpected AUTH response: {other}"),
    }
}

/// Send a RESP frame to the server.
async fn kv_send(stream: &mut TcpStream, frame: &Frame) -> anyhow::Result<()> {
    let bytes = frame.to_bytes();
    stream.write_all(&bytes).await.context("failed to write command to KV server")
}

/// Receive a RESP frame from the server.
async fn kv_recv(stream: &mut TcpStream) -> anyhow::Result<Frame> {
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        buf.reserve(512);
        let n = stream.read_buf(&mut buf).await.context("failed to read from KV server")?;
        if n == 0 {
            anyhow::bail!("KV server closed connection unexpectedly");
        }
        if let Some(frame) = parse_frame(&mut buf).context("invalid RESP data from KV server")? {
            return Ok(frame);
        }
    }
}

/// Send a command and receive the response in one connection.
async fn kv_roundtrip(host: &str, port: u16, auth: &KvAuth, cmd: Frame) -> anyhow::Result<Frame> {
    let mut stream = kv_connect(host, port, auth).await?;
    kv_send(&mut stream, &cmd).await?;
    kv_recv(&mut stream).await
}

/// PING command.
async fn kv_ping(host: &str, port: u16, auth: &KvAuth) -> anyhow::Result<ExitCode> {
    let cmd = Frame::Array(vec![Frame::bulk(b"PING".to_vec())]);
    match kv_roundtrip(host, port, auth, cmd).await? {
        Frame::Simple(s) => {
            println!("{s}");
            Ok(ExitCode::SUCCESS)
        }
        Frame::Error(e) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// KEYS command.
async fn kv_keys(host: &str, port: u16, auth: &KvAuth, pattern: &str) -> anyhow::Result<ExitCode> {
    let cmd =
        Frame::Array(vec![Frame::bulk(b"KEYS".to_vec()), Frame::bulk(pattern.as_bytes().to_vec())]);
    match kv_roundtrip(host, port, auth, cmd).await? {
        Frame::Array(items) => {
            if items.is_empty() {
                println!("(empty)");
            } else {
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Frame::Bulk(b) => println!("{}) {}", i + 1, String::from_utf8_lossy(b)),
                        other => println!("{}) {other}", i + 1),
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Frame::Error(e) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// GET command.
async fn kv_get(host: &str, port: u16, auth: &KvAuth, key: &str) -> anyhow::Result<ExitCode> {
    let cmd =
        Frame::Array(vec![Frame::bulk(b"GET".to_vec()), Frame::bulk(key.as_bytes().to_vec())]);
    match kv_roundtrip(host, port, auth, cmd).await? {
        Frame::Bulk(data) => {
            match std::str::from_utf8(&data) {
                Ok(s) => println!("{s}"),
                Err(_) => println!("<{} bytes of binary data>", data.len()),
            }
            Ok(ExitCode::SUCCESS)
        }
        Frame::Null => {
            println!("(nil)");
            Ok(ExitCode::SUCCESS)
        }
        Frame::Error(e) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// SET command.
async fn kv_set(
    host: &str,
    port: u16,
    auth: &KvAuth,
    key: &str,
    value: &str,
    ttl: Option<u64>,
) -> anyhow::Result<ExitCode> {
    let mut args = vec![
        Frame::bulk(b"SET".to_vec()),
        Frame::bulk(key.as_bytes().to_vec()),
        Frame::bulk(value.as_bytes().to_vec()),
    ];
    if let Some(secs) = ttl {
        args.push(Frame::bulk(b"EX".to_vec()));
        args.push(Frame::bulk(secs.to_string().into_bytes()));
    }
    let cmd = Frame::Array(args);
    match kv_roundtrip(host, port, auth, cmd).await? {
        Frame::Simple(s) => {
            println!("{s}");
            Ok(ExitCode::SUCCESS)
        }
        Frame::Null => {
            println!("(nil)");
            Ok(ExitCode::SUCCESS)
        }
        Frame::Error(e) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// DEL command.
async fn kv_del(host: &str, port: u16, auth: &KvAuth, keys: &[String]) -> anyhow::Result<ExitCode> {
    let mut args = vec![Frame::bulk(b"DEL".to_vec())];
    for key in keys {
        args.push(Frame::bulk(key.as_bytes().to_vec()));
    }
    let cmd = Frame::Array(args);
    match kv_roundtrip(host, port, auth, cmd).await? {
        Frame::Integer(n) => {
            println!("(integer) {n}");
            Ok(ExitCode::SUCCESS)
        }
        Frame::Error(e) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// INCR command.
async fn kv_incr(
    host: &str,
    port: u16,
    auth: &KvAuth,
    key: &str,
    by: i64,
) -> anyhow::Result<ExitCode> {
    let cmd = if by == 1 {
        Frame::Array(vec![Frame::bulk(b"INCR".to_vec()), Frame::bulk(key.as_bytes().to_vec())])
    } else {
        Frame::Array(vec![
            Frame::bulk(b"INCRBY".to_vec()),
            Frame::bulk(key.as_bytes().to_vec()),
            Frame::bulk(by.to_string().into_bytes()),
        ])
    };
    match kv_roundtrip(host, port, auth, cmd).await? {
        Frame::Integer(n) => {
            println!("(integer) {n}");
            Ok(ExitCode::SUCCESS)
        }
        Frame::Error(e) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// TTL command.
async fn kv_ttl(host: &str, port: u16, auth: &KvAuth, key: &str) -> anyhow::Result<ExitCode> {
    let mut stream = kv_connect(host, port, auth).await?;

    // Send TTL
    kv_send(
        &mut stream,
        &Frame::Array(vec![Frame::bulk(b"TTL".to_vec()), Frame::bulk(key.as_bytes().to_vec())]),
    )
    .await?;
    let ttl_frame = kv_recv(&mut stream).await?;

    // Send PTTL on the same connection
    kv_send(
        &mut stream,
        &Frame::Array(vec![Frame::bulk(b"PTTL".to_vec()), Frame::bulk(key.as_bytes().to_vec())]),
    )
    .await?;
    let pttl_frame = kv_recv(&mut stream).await?;

    match (ttl_frame, pttl_frame) {
        (Frame::Integer(ttl), Frame::Integer(pttl)) => {
            match ttl {
                -2 => println!("key does not exist"),
                -1 => println!("no expiry (persistent key)"),
                s => println!("expires in {s}s ({pttl}ms)"),
            }
            Ok(ExitCode::SUCCESS)
        }
        (Frame::Error(e), _) | (_, Frame::Error(e)) => {
            eprintln!("error: {e}");
            Ok(ExitCode::FAILURE)
        }
        (a, b) => anyhow::bail!("unexpected response: {a} / {b}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// OPcache Clustering CLI Subcommands (Phase 1)
// ─────────────────────────────────────────────────────────────────────────────

/// Vhost name used when neither `--site` nor `--all` is supplied. Mirrors the
/// server-side `ephpm_server::opcache::DEFAULT_VHOST`; kept in-lockstep
/// manually since the CLI does not depend on `ephpm-server`.
///
/// **Uppercase on purpose (issue #450).** [`resolve_target`] lowercases
/// `--site`, and the server lowercases every `Host`, so neither an operator
/// flag nor a `sites_dir` directory can reach this bucket — it is only ever
/// written by *omitting* `--site`. Before the fix it was `_default`, which a
/// tenant site could be named, and the two then shared one invalidation
/// counter on the server.
const OPCACHE_DEFAULT_VHOST: &str = "_DEFAULT";

/// Broadcast vhost name written by `--all`. Kept in-lockstep with the
/// server-side `ephpm_server::opcache::BROADCAST_VHOST`. Uppercase for the
/// same reason as [`OPCACHE_DEFAULT_VHOST`] (was `_all`).
const OPCACHE_BROADCAST_VHOST: &str = "_ALL";

/// KV key prefix for the per-vhost version counter.
const OPCACHE_VERSION_PREFIX: &str = "opcache:version:";

/// KV key prefix for the optional revision tag (informational only —
/// invalidation still keys off `opcache:version:*`).
const OPCACHE_REVISION_PREFIX: &str = "opcache:revision:";

/// Current wall-clock time in milliseconds since the UNIX epoch. Used as the
/// monotonically-nondecreasing version stamp. Any well-formed epoch_ms works
/// as a trigger; the actual value is opaque to the watcher.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Human-facing hint when the RESP listener refuses a TCP connection — the
/// server is either not running or has `[kv.redis_compat]` disabled.
fn resp_connect_hint(host: &str, port: u16) -> String {
    format!(
        "could not reach RESP listener at {host}:{port} — is ephpm running with \
         `[kv.redis_compat] enabled = true` in ephpm.toml?"
    )
}

/// Issue a RESP `SET key value` roundtrip, returning `Ok` on `+OK`.
async fn kv_set_raw(
    host: &str,
    port: u16,
    auth: &KvAuth,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    let mut stream =
        kv_tcp_connect(host, port).await.with_context(|| resp_connect_hint(host, port))?;
    kv_authenticate(&mut stream, auth).await?;
    let cmd = Frame::Array(vec![
        Frame::bulk(b"SET".to_vec()),
        Frame::bulk(key.as_bytes().to_vec()),
        Frame::bulk(value.as_bytes().to_vec()),
    ]);
    kv_send(&mut stream, &cmd).await?;
    match kv_recv(&mut stream).await? {
        Frame::Simple(_) => Ok(()),
        Frame::Error(e) if e.starts_with("NOAUTH") => anyhow::bail!(
            "KV server requires authentication ({e}) — pass --password or set \
             EPHPM_KV_PASSWORD to the value of [kv.redis_compat] password"
        ),
        Frame::Error(e) => anyhow::bail!("KV server returned error: {e}"),
        other => anyhow::bail!("unexpected RESP response: {other}"),
    }
}

/// `ephpm deploy`: write `opcache:version:<vhost> = <epoch_ms>` and, when
/// `--rev` is supplied, also `opcache:revision:<vhost> = <rev>`.
async fn run_deploy(
    host: &str,
    port: u16,
    auth: &KvAuth,
    site: Option<&str>,
    all: bool,
    rev: Option<&str>,
) -> anyhow::Result<ExitCode> {
    let vhost = resolve_target(site, all)?;
    let stamp = epoch_ms();
    let stamp_str = stamp.to_string();
    let version_key = format!("{OPCACHE_VERSION_PREFIX}{vhost}");
    kv_set_raw(host, port, auth, &version_key, &stamp_str).await?;
    if let Some(revision) = rev {
        let rev_key = format!("{OPCACHE_REVISION_PREFIX}{vhost}");
        kv_set_raw(host, port, auth, &rev_key, revision).await?;
    }
    if vhost == OPCACHE_BROADCAST_VHOST {
        println!("deployed: broadcast (every vhost) at {stamp_str}");
    } else {
        println!("deployed: vhost={vhost} at {stamp_str}");
    }
    if let Some(revision) = rev {
        println!("  revision: {revision}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `ephpm cache` subcommand dispatcher (currently only `reset`).
async fn run_cache(
    host: &str,
    port: u16,
    auth: &KvAuth,
    sub: CacheSubcommand,
) -> anyhow::Result<ExitCode> {
    match sub {
        CacheSubcommand::Reset { site, all } => {
            run_cache_reset(host, port, auth, site.as_deref(), all).await
        }
    }
}

/// `ephpm cache reset --site <name> | --all`
///
/// Wire-level identical to `deploy` (writes the same version key). The
/// separate command exists so operators can distinguish a local dev reset
/// from a deploy event in shell history / audit logs. On single-node
/// deployments the two commands behave identically; on a cluster, both
/// propagate via gossip because the KV write happens through the RESP
/// listener into the same in-process store.
async fn run_cache_reset(
    host: &str,
    port: u16,
    auth: &KvAuth,
    site: Option<&str>,
    all: bool,
) -> anyhow::Result<ExitCode> {
    let vhost = resolve_target(site, all)?;
    let stamp = epoch_ms();
    let stamp_str = stamp.to_string();
    let key = format!("{OPCACHE_VERSION_PREFIX}{vhost}");
    kv_set_raw(host, port, auth, &key, &stamp_str).await?;
    if vhost == OPCACHE_BROADCAST_VHOST {
        println!("cache reset: broadcast (every vhost) at {stamp_str}");
    } else {
        println!("cache reset: vhost={vhost} at {stamp_str}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve `--site <name>` / `--all` / neither into a vhost key.
fn resolve_target(site: Option<&str>, all: bool) -> anyhow::Result<String> {
    match (site, all) {
        (Some(_), true) => {
            anyhow::bail!("--site and --all are mutually exclusive")
        }
        (Some(name), false) => {
            let normalised = name.trim().to_ascii_lowercase();
            if normalised.is_empty() {
                anyhow::bail!("--site must not be empty");
            }
            Ok(normalised)
        }
        (None, true) => Ok(OPCACHE_BROADCAST_VHOST.to_string()),
        (None, false) => Ok(OPCACHE_DEFAULT_VHOST.to_string()),
    }
}

#[cfg(test)]
mod php_arg_tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn subcommand_names_come_from_the_real_cli() {
        // The php-personality rules defer to these names, so they must track
        // the actual clap tree — including hidden ones and clap's `help`.
        for name in ["serve", "dev", "php", "composer", "kv", "cache", "help"] {
            assert!(is_ephpm_subcommand(name), "{name} is an ephpm subcommand");
        }
        // Hidden, and only compiled into the Windows build.
        assert_eq!(is_ephpm_subcommand("service-run"), cfg!(windows));
        for name in ["script.php", "artisan", "-r", "", "vendor/bin/phpunit"] {
            assert!(!is_ephpm_subcommand(name), "{name:?} is not an ephpm subcommand");
        }
    }

    #[test]
    fn separator_clap_swallowed_is_restored() {
        // `… | ephpm php -- a b`: php-cli needs the `--` to know that `a` is an
        // argument and the program comes from stdin, not that `a` is a script.
        let raw = v(&["--", "a", "b"]);
        let parsed = v(&["a", "b"]);
        assert_eq!(restore_php_separator(Some(raw.clone()), &parsed), raw);
    }

    #[test]
    fn only_the_first_separator_is_clap_s() {
        // clap strips one `--`; a second is a real PHP argument and stays put.
        let raw = v(&["--", "-r", "echo 1;", "--", "x"]);
        let parsed = v(&["-r", "echo 1;", "--", "x"]);
        assert_eq!(restore_php_separator(Some(raw.clone()), &parsed), raw);
    }

    #[test]
    fn args_without_a_separator_are_unchanged() {
        let raw = v(&["-r", "echo 1;"]);
        assert_eq!(restore_php_separator(Some(raw.clone()), &raw), raw);
    }

    #[test]
    fn unexpected_argv_shape_keeps_the_parsed_view() {
        // Raw tail that is not "parsed plus one separator" is not trusted.
        let parsed = v(&["-r", "echo 1;"]);
        assert_eq!(restore_php_separator(Some(v(&["something", "else"])), &parsed), parsed);
        assert_eq!(restore_php_separator(None, &parsed), parsed);
    }

    // ── --site / --config prefix stripping (issue #471) ─────────────────────

    #[test]
    fn strips_leading_config_and_site_options() {
        // The ePHPm-owned options clap consumes are removed; the PHP args remain.
        assert_eq!(
            strip_leading_site_opts(v(&["--config", "e.toml", "--site", "shop", "-r", "echo 1;"])),
            v(&["-r", "echo 1;"])
        );
        assert_eq!(strip_leading_site_opts(v(&["--site", "shop"])), v(&[] as &[&str]));
    }

    #[test]
    fn strips_equals_form_options() {
        assert_eq!(
            strip_leading_site_opts(v(&["--site=shop", "--config=e.toml", "wp", "db", "query"])),
            v(&["wp", "db", "query"])
        );
    }

    #[test]
    fn strip_stops_at_the_first_php_argument() {
        // A `--site` appearing *after* a PHP token is the script's argument, not
        // ePHPm's — clap routes it into `args`, and stripping must leave it be.
        assert_eq!(
            strip_leading_site_opts(v(&["script.php", "--site", "bar"])),
            v(&["script.php", "--site", "bar"])
        );
        // No leading options at all → unchanged.
        assert_eq!(strip_leading_site_opts(v(&["-r", "echo 1;"])), v(&["-r", "echo 1;"]));
    }

    #[test]
    fn strip_then_restore_recovers_a_stdin_separator_after_site() {
        // `ephpm php --site shop -- a b`: after stripping `--site shop`, the
        // `--` clap swallowed is restored for php-cli's stdin semantics.
        let raw = v(&["--site", "shop", "--", "a", "b"]);
        let parsed = v(&["a", "b"]);
        assert_eq!(
            restore_php_separator(Some(strip_leading_site_opts(raw)), &parsed),
            v(&["--", "a", "b"])
        );
    }

    #[test]
    fn clap_separates_site_from_passthrough_but_leaves_bare_php_alone() {
        use clap::Parser;

        // --site/--config before the passthrough are ePHPm's; the rest is PHP's.
        let cli = Cli::try_parse_from(["ephpm", "php", "--site", "shop", "-r", "echo 1;"]).unwrap();
        let Some(Commands::Php { config, site, args }) = cli.command else {
            panic!("expected a php command");
        };
        assert_eq!(site.as_deref(), Some("shop"));
        assert!(config.is_none());
        assert_eq!(args, v(&["-r", "echo 1;"]));

        // No --site → behaves exactly as before: everything is PHP's.
        let cli = Cli::try_parse_from(["ephpm", "php", "-r", "echo 1;"]).unwrap();
        let Some(Commands::Php { site, args, .. }) = cli.command else {
            panic!("expected a php command");
        };
        assert!(site.is_none());
        assert_eq!(args, v(&["-r", "echo 1;"]));

        // A --site after a script name is the script's, captured into args.
        let cli = Cli::try_parse_from(["ephpm", "php", "script.php", "--site", "x"]).unwrap();
        let Some(Commands::Php { site, args, .. }) = cli.command else {
            panic!("expected a php command");
        };
        assert!(site.is_none());
        assert_eq!(args, v(&["script.php", "--site", "x"]));
    }

    #[test]
    fn analyze_parses_path_and_overrides() {
        let cli = Cli::try_parse_from([
            "ephpm",
            "analyze",
            "/srv/app",
            "--config",
            "gate.yml",
            "--format",
            "sarif",
            "--profile",
            "security",
            "--fail-on",
            "deny",
            "--level",
            "2",
            "--baseline",
            "base.json",
            "--since",
            "origin/main",
            "--cache-dir",
            "/var/cache/analyze",
            "--no-cache",
        ])
        .unwrap();
        let Some(Commands::Analyze {
            path,
            config,
            format,
            profile,
            fail_on,
            level,
            baseline,
            since,
            cache_dir,
            no_cache,
        }) = cli.command
        else {
            panic!("expected an analyze command");
        };
        assert_eq!(path.as_deref(), Some(std::path::Path::new("/srv/app")));
        assert_eq!(config.as_deref(), Some(std::path::Path::new("gate.yml")));
        assert_eq!(format.as_deref(), Some("sarif"));
        assert_eq!(profile.as_deref(), Some("security"));
        assert_eq!(fail_on.as_deref(), Some("deny"));
        assert_eq!(level, Some(2));
        assert_eq!(baseline.as_deref(), Some(std::path::Path::new("base.json")));
        assert_eq!(since.as_deref(), Some("origin/main"));
        assert_eq!(cache_dir.as_deref(), Some(std::path::Path::new("/var/cache/analyze")));
        assert!(no_cache);
    }

    #[test]
    fn analyze_rejects_out_of_range_level() {
        assert!(Cli::try_parse_from(["ephpm", "analyze", "--level", "4"]).is_err());
    }

    #[test]
    fn analyze_defaults_to_cwd_and_no_overrides() {
        let cli = Cli::try_parse_from(["ephpm", "analyze"]).unwrap();
        let Some(Commands::Analyze {
            path,
            config,
            format,
            profile,
            fail_on,
            level,
            baseline,
            since,
            cache_dir,
            no_cache,
        }) = cli.command
        else {
            panic!("expected an analyze command");
        };
        assert!(path.is_none());
        assert!(config.is_none());
        assert!(format.is_none());
        assert!(profile.is_none());
        assert!(fail_on.is_none());
        assert!(level.is_none());
        assert!(baseline.is_none());
        assert!(since.is_none());
        assert!(cache_dir.is_none());
        assert!(!no_cache);
    }
}

#[cfg(test)]
mod disable_functions_tests {
    use super::*;

    fn ov(pairs: &[(&str, &str)]) -> Vec<[String; 2]> {
        pairs.iter().map(|(k, v)| [(*k).to_string(), (*v).to_string()]).collect()
    }

    /// The common case for these legacy tests: no per-site capability gating, so
    /// `compose_disable_functions` must reproduce exactly the pre-feature output.
    fn no_gates() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn nothing_to_emit_without_operator_or_baseline() {
        assert_eq!(compose_disable_functions(&[], false, false, true, false, &no_gates()), None);
    }

    #[test]
    fn shell_only_matches_legacy_line() {
        let df = compose_disable_functions(&[], true, false, true, false, &no_gates()).unwrap();
        assert_eq!(df, "exec,passthru,shell_exec,system,proc_open,popen,pcntl_exec");
    }

    #[test]
    fn operator_additions_are_unioned_not_clobbered() {
        // Bug 1: an operator disabling pcntl_fork alongside disable_shell_exec
        // must keep BOTH blocked. The old code appended the shell line last and
        // PHP last-wins discarded the operator's list.
        let operator = collect_operator_disable_functions(&ov(&[(
            "disable_functions",
            "pcntl_fork,mkdir_helper",
        )]));
        let df =
            compose_disable_functions(&operator, true, false, true, false, &no_gates()).unwrap();
        // Operator entries come first and survive.
        assert!(df.starts_with("pcntl_fork,mkdir_helper,"));
        // Shell baseline is still present.
        assert!(df.split(',').any(|f| f == "system"));
        assert!(df.split(',').any(|f| f == "pcntl_fork"));
    }

    #[test]
    fn hardening_adds_the_proven_channels() {
        let df = compose_disable_functions(&[], true, true, true, false, &no_gates()).unwrap();
        for expected in [
            "system",        // shell family
            "pcntl_fork",    // pcntl
            "posix_kill",    // posix process control
            "pfsockopen",    // persistent socket inheritance
            "fsockopen",     // reachability (block on by default)
            "shm_attach",    // SysV shm
            "sem_get",       // SysV sem
            "msg_send",      // SysV msg
            "opcache_reset", // opcache flush
            "dl",
            "mail",
        ] {
            assert!(df.split(',').any(|f| f == expected), "missing {expected} in {df}");
        }
    }

    #[test]
    fn egress_externally_managed_lifts_only_fsockopen() {
        // reachability block OFF (network_egress_externally_managed = true):
        // fsockopen is lifted, but pfsockopen and every non-reachability block
        // stay in place — this is the whole point of the knob.
        let df = compose_disable_functions(&[], true, true, false, false, &no_gates()).unwrap();
        assert!(
            !df.split(',').any(|f| f == "fsockopen"),
            "fsockopen must be lifted when egress is externally managed: {df}"
        );
        for still_blocked in ["pfsockopen", "pcntl_fork", "shm_attach", "dl", "mail", "system"] {
            assert!(
                df.split(',').any(|f| f == still_blocked),
                "{still_blocked} must stay blocked regardless of egress management: {df}"
            );
        }

        // An operator who *also* lists fsockopen explicitly keeps it blocked —
        // the knob only drops ePHPm's own baseline entry, never the operator's.
        let operator =
            collect_operator_disable_functions(&ov(&[("disable_functions", "fsockopen")]));
        let with_operator =
            compose_disable_functions(&operator, true, true, false, false, &no_gates()).unwrap();
        assert!(with_operator.split(',').any(|f| f == "fsockopen"));
    }

    #[test]
    fn cluster_invalidation_keeps_opcache_introspection_callable() {
        // With cluster invalidation ON, ePHPm needs opcache_invalidate/status,
        // so they must NOT be in the denylist — but opcache_reset always is.
        let on = compose_disable_functions(&[], true, true, true, true, &no_gates()).unwrap();
        assert!(on.split(',').any(|f| f == "opcache_reset"));
        assert!(!on.split(',').any(|f| f == "opcache_invalidate"));
        assert!(!on.split(',').any(|f| f == "opcache_get_status"));

        // With it OFF, they are disabled too.
        let off = compose_disable_functions(&[], true, true, true, false, &no_gates()).unwrap();
        assert!(off.split(',').any(|f| f == "opcache_invalidate"));
        assert!(off.split(',').any(|f| f == "opcache_get_status"));
    }

    #[test]
    fn duplicate_names_are_deduplicated_case_insensitively() {
        let operator =
            collect_operator_disable_functions(&ov(&[("disable_functions", "System, PCNTL_FORK")]));
        let df =
            compose_disable_functions(&operator, true, true, true, false, &no_gates()).unwrap();
        assert_eq!(df.split(',').filter(|f| f.eq_ignore_ascii_case("system")).count(), 1);
        assert_eq!(df.split(',').filter(|f| f.eq_ignore_ascii_case("pcntl_fork")).count(), 1);
    }

    #[test]
    fn collect_splits_and_trims_multiple_entries() {
        let out = collect_operator_disable_functions(&ov(&[
            ("display_errors", "Off"),
            ("disable_functions", " a , b "),
            ("disable_functions", "c"),
        ]));
        assert_eq!(out, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    // ── Per-site function capabilities ──────────────────────────────────

    fn gates(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn granted(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    /// The hard requirement: with nothing granted, the composed denylist is
    /// byte-identical to the pre-feature output. `no_gates()` is what every
    /// legacy test above already passes, so this pins the equality directly.
    #[test]
    fn no_grant_leaves_disable_functions_byte_identical() {
        for (shell, harden, reach, cluster) in [
            (false, false, true, false),
            (true, false, true, false),
            (true, true, true, false),
            (true, true, false, false),
            (true, true, true, true),
        ] {
            let baseline =
                compose_disable_functions(&[], shell, harden, reach, cluster, &no_gates());
            let with_empty_gated =
                compose_disable_functions(&[], shell, harden, reach, cluster, &gates(&[]));
            assert_eq!(baseline, with_empty_gated, "an empty gate set must not change the output");
        }
    }

    /// A gated function is removed from ePHPm's baseline (left registered for the
    /// per-site gate) while the rest of the hardening set is untouched.
    #[test]
    fn a_gated_function_is_removed_from_the_denylist() {
        let full = compose_disable_functions(&[], true, true, true, false, &no_gates()).unwrap();
        assert!(full.split(',').any(|f| f == "mail"));
        assert!(full.split(',').any(|f| f == "fsockopen"));

        let gated =
            compose_disable_functions(&[], true, true, true, false, &gates(&["mail", "fsockopen"]))
                .unwrap();
        assert!(!gated.split(',').any(|f| f == "mail"), "a gated function must not be disabled");
        assert!(!gated.split(',').any(|f| f == "fsockopen"));
        // Everything else the preset disables is still there.
        assert!(gated.split(',').any(|f| f == "pcntl_fork"));
        assert!(gated.split(',').any(|f| f == "shm_attach"));
        assert!(gated.split(',').any(|f| f == "opcache_reset"));
    }

    /// An operator's explicit `disable_functions` entry always wins: compose
    /// never skips an operator name even if it appears in the gated set.
    #[test]
    fn gating_never_overrides_an_operator_explicit_disable() {
        let operator = collect_operator_disable_functions(&ov(&[("disable_functions", "mail")]));
        let df = compose_disable_functions(&operator, true, true, true, false, &gates(&["mail"]))
            .unwrap();
        assert!(df.split(',').any(|f| f == "mail"), "operator-disabled mail must stay disabled");
    }

    /// `resolve_gated` partitions a grant union into gated / redundant /
    /// operator-blocked against the effective baseline.
    #[test]
    fn resolve_gated_partitions_against_the_baseline() {
        // Hardening on, egress NOT externally managed (fsockopen is in the
        // reachability block), operator explicitly disabled pfsockopen.
        let operator = vec!["pfsockopen".to_string()];
        let res = resolve_gated(
            &granted(&["mail", "fsockopen", "pfsockopen"]),
            &operator,
            true,
            true,
            true,
            false,
        );
        assert_eq!(res.gated, gates(&["mail", "fsockopen"]));
        assert_eq!(res.operator_blocked, vec!["pfsockopen".to_string()]);
        assert_eq!(res.redundant, [] as [String; 0]);
    }

    /// With egress externally managed, `fsockopen` is not in the baseline at
    /// all, so granting it is redundant — not gated.
    #[test]
    fn resolve_gated_marks_already_callable_grants_redundant() {
        // include_reachability_block = false => fsockopen is not disabled.
        let res = resolve_gated(&granted(&["mail", "fsockopen"]), &[], true, true, false, false);
        assert_eq!(res.gated, gates(&["mail"]));
        assert_eq!(res.redundant, vec!["fsockopen".to_string()]);
        assert_eq!(res.operator_blocked, [] as [String; 0]);

        // Hardening entirely off => every grant is redundant (nothing disabled).
        let off = resolve_gated(&granted(&["mail"]), &[], false, false, true, false);
        assert!(off.gated.is_empty(), "nothing disabled means nothing to gate");
        assert_eq!(off.redundant, vec!["mail".to_string()]);
    }

    fn ini_value(defaults: &[(&str, &str)], key: &str) -> Option<String> {
        defaults.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_string())
    }

    #[test]
    fn serve_hides_php_errors_from_responses_and_logs_them() {
        // PHP's own default is display_errors=1: a warning would render the
        // script's absolute path into the HTTP body. Serve must override it.
        let serve = error_display_ini_defaults(false);
        assert_eq!(ini_value(&serve, "display_errors").as_deref(), Some("Off"));
        assert_eq!(ini_value(&serve, "display_startup_errors").as_deref(), Some("Off"));
        assert_eq!(ini_value(&serve, "log_errors").as_deref(), Some("On"));
        // No error_log file: the line must reach the SAPI log_message hook
        // (ePHPm's log), not a file nobody reads.
        assert!(ini_value(&serve, "error_log").is_none());
    }

    #[test]
    fn dev_keeps_php_errors_visible() {
        let dev = error_display_ini_defaults(true);
        assert_eq!(ini_value(&dev, "display_errors").as_deref(), Some("On"));
        assert_eq!(ini_value(&dev, "log_errors").as_deref(), Some("On"));
    }
}

#[cfg(test)]
mod exec_timeout_warning_tests {
    use super::php_timeout_preempted;

    #[test]
    fn php_limit_at_or_above_the_request_deadline_is_preempted() {
        assert!(php_timeout_preempted(30, 30));
        assert!(php_timeout_preempted(60, 30));
        assert!(!php_timeout_preempted(29, 30));
    }

    #[test]
    fn disabled_request_deadline_never_preempts() {
        // `[server.timeouts] request = 0` disables the deadline; PHP's own
        // timeout is then the only ceiling and fires normally.
        assert!(!php_timeout_preempted(30, 0));
        assert!(!php_timeout_preempted(1, 0));
    }

    #[test]
    fn disabled_php_limit_is_never_preempted() {
        assert!(!php_timeout_preempted(0, 30));
        assert!(!php_timeout_preempted(0, 0));
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn parses_install_subcommand() {
        let cli = Cli::try_parse_from(["ephpm", "install"]).unwrap();
        match cli.command {
            Some(Commands::Install { user, group }) => {
                assert!(user.is_none());
                assert!(group.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_install_with_user_and_group() {
        let cli =
            Cli::try_parse_from(["ephpm", "install", "--user", "ephpm-web", "--group", "www"])
                .unwrap();
        match cli.command {
            Some(Commands::Install { user, group }) => {
                assert_eq!(user.as_deref(), Some("ephpm-web"));
                assert_eq!(group.as_deref(), Some("www"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_install_with_user_only() {
        let cli = Cli::try_parse_from(["ephpm", "install", "--user", "ephpm-web"]).unwrap();
        match cli.command {
            Some(Commands::Install { user, group }) => {
                assert_eq!(user.as_deref(), Some("ephpm-web"));
                assert!(group.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rejects_install_group_without_user() {
        let err = Cli::try_parse_from(["ephpm", "install", "--group", "www"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_uninstall_with_keep_data_flag() {
        let cli = Cli::try_parse_from(["ephpm", "uninstall", "--keep-data"]).unwrap();
        match cli.command {
            Some(Commands::Uninstall { keep_data }) => assert!(keep_data),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_uninstall_default_keeps_no_data() {
        let cli = Cli::try_parse_from(["ephpm", "uninstall"]).unwrap();
        match cli.command {
            Some(Commands::Uninstall { keep_data }) => assert!(!keep_data),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_lifecycle_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["ephpm", "start"]).unwrap().command,
            Some(Commands::Start)
        ));
        assert!(matches!(
            Cli::try_parse_from(["ephpm", "stop"]).unwrap().command,
            Some(Commands::Stop)
        ));
        assert!(matches!(
            Cli::try_parse_from(["ephpm", "restart"]).unwrap().command,
            Some(Commands::Restart)
        ));
        assert!(matches!(
            Cli::try_parse_from(["ephpm", "status"]).unwrap().command,
            Some(Commands::Status)
        ));
    }

    #[test]
    fn parses_logs_with_follow() {
        let cli = Cli::try_parse_from(["ephpm", "logs", "--follow"]).unwrap();
        match cli.command {
            Some(Commands::Logs { follow }) => assert!(follow),
            other => panic!("unexpected: {other:?}"),
        }

        let cli = Cli::try_parse_from(["ephpm", "logs", "-f"]).unwrap();
        match cli.command {
            Some(Commands::Logs { follow }) => assert!(follow),
            other => panic!("unexpected: {other:?}"),
        }

        let cli = Cli::try_parse_from(["ephpm", "logs"]).unwrap();
        match cli.command {
            Some(Commands::Logs { follow }) => assert!(!follow),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_deploy_with_site() {
        let cli =
            Cli::try_parse_from(["ephpm", "deploy", "--site", "blog", "--rev", "abc123"]).unwrap();
        match cli.command {
            Some(Commands::Deploy { site, all, rev, .. }) => {
                assert_eq!(site.as_deref(), Some("blog"));
                assert!(!all);
                assert_eq!(rev.as_deref(), Some("abc123"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_deploy_with_all() {
        let cli = Cli::try_parse_from(["ephpm", "deploy", "--all"]).unwrap();
        match cli.command {
            Some(Commands::Deploy { site, all, rev, .. }) => {
                assert_eq!(site, None);
                assert!(all);
                assert_eq!(rev, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn deploy_site_and_all_are_mutex() {
        let err = Cli::try_parse_from(["ephpm", "deploy", "--site", "blog", "--all"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot be used"), "expected mutex error, got: {msg}");
    }

    #[test]
    fn parses_cache_reset_with_site() {
        let cli = Cli::try_parse_from(["ephpm", "cache", "reset", "--site", "shop"]).unwrap();
        match cli.command {
            Some(Commands::Cache { subcommand: CacheSubcommand::Reset { site, all }, .. }) => {
                assert_eq!(site.as_deref(), Some("shop"));
                assert!(!all);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_kv_password_flag() {
        // Parent-level flags come before the subcommand, same as --host/--port.
        let args = ["ephpm", "kv", "--password", "s3cr3t", "ping"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Some(Commands::Kv { password, user, .. }) => {
                assert_eq!(password.as_deref(), Some("s3cr3t"));
                assert_eq!(user, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_kv_user_flag() {
        let args = ["ephpm", "kv", "--user", "blog", "--password", "pw", "ping"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Some(Commands::Kv { user, .. }) => assert_eq!(user.as_deref(), Some("blog")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_deploy_password_flag() {
        let args = ["ephpm", "deploy", "--all", "--password", "pw"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Some(Commands::Deploy { password, .. }) => assert_eq!(password.as_deref(), Some("pw")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_cache_password_flag() {
        let args = ["ephpm", "cache", "--password", "pw", "reset", "--all"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Some(Commands::Cache { password, .. }) => assert_eq!(password.as_deref(), Some("pw")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn kv_auth_without_credentials_sends_no_auth_frame() {
        let auth = KvAuth { user: None, password: None };
        assert!(auth.frame().is_none());
    }

    #[test]
    fn kv_auth_password_only_uses_single_argument_form() {
        let auth = KvAuth::resolve(None, Some("pw".to_string())).unwrap();
        let want = Frame::Array(vec![Frame::bulk("AUTH"), Frame::bulk("pw")]);
        assert_eq!(auth.frame(), Some(want));
    }

    #[test]
    fn kv_auth_with_user_uses_two_argument_form() {
        // What the server's per-site HMAC mode expects on the wire:
        // AUTH <hostname> <HMAC-SHA256(secret, hostname)>.
        let auth = KvAuth::resolve(Some("h".to_string()), Some("p".to_string())).unwrap();
        let want = Frame::Array(vec![Frame::bulk("AUTH"), Frame::bulk("h"), Frame::bulk("p")]);
        assert_eq!(auth.frame(), Some(want));
    }

    #[test]
    fn kv_auth_rejects_user_without_password() {
        let err = KvAuth::resolve(Some("h".to_string()), None).unwrap_err();
        assert!(err.to_string().contains("--user requires a password"), "got: {err}");
    }

    /// A one-shot RESP responder: answers the first command on each of
    /// `conns` connections with `+PONG`.
    async fn pong_server(listener: tokio::net::TcpListener, conns: usize) {
        for _ in 0..conns {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"+PONG\r\n").await.unwrap();
        }
    }

    fn ping() -> Frame {
        Frame::Array(vec![Frame::bulk("PING")])
    }

    /// `--host` accepts a hostname. It used to be parsed as a `SocketAddr`, so
    /// anything but an IP literal (`localhost`, `kv.internal`) failed with
    /// "invalid address" before a connection was even attempted.
    #[tokio::test]
    async fn kv_host_resolves_hostnames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(pong_server(listener, 2));
        let auth = KvAuth { user: None, password: None };

        // `localhost` may resolve to `::1` first; the connect must fall through
        // to `127.0.0.1` rather than giving up on the first address.
        let reply = kv_roundtrip("localhost", port, &auth, ping()).await.unwrap();
        assert_eq!(reply, Frame::Simple("PONG".into()));
        // The deploy/cache write path shares the same connector.
        assert!(kv_tcp_connect("localhost", port).await.is_ok());
        server.await.unwrap();
    }

    /// IP literals keep working, including IPv6 with or without brackets (a
    /// bare `::1` used to be mis-split on its colons).
    #[tokio::test]
    async fn kv_host_accepts_ip_literals() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(pong_server(listener, 1));
        let auth = KvAuth { user: None, password: None };
        let reply = kv_roundtrip("127.0.0.1", port, &auth, ping()).await.unwrap();
        assert_eq!(reply, Frame::Simple("PONG".into()));
        server.await.unwrap();

        // IPv6 loopback is not configured on every CI host; skip that half
        // rather than fail on the environment.
        let Ok(listener6) = tokio::net::TcpListener::bind("[::1]:0").await else {
            eprintln!("skipping IPv6 half: no ::1 on this host");
            return;
        };
        let port6 = listener6.local_addr().unwrap().port();
        let server6 = tokio::spawn(pong_server(listener6, 2));
        for host in ["::1", "[::1]"] {
            let reply = kv_roundtrip(host, port6, &auth, ping()).await.unwrap();
            assert_eq!(reply, Frame::Simple("PONG".into()), "host {host}");
        }
        server6.await.unwrap();
    }

    /// An unresolvable name is a connection error naming the target — not a
    /// panic and not the old "invalid address" parse failure.
    #[tokio::test]
    async fn kv_host_unresolvable_name_is_a_clear_error() {
        let auth = KvAuth { user: None, password: None };
        let err = kv_connect("no-such-host.invalid", 6379, &auth).await.unwrap_err();
        assert!(
            err.to_string().contains("could not connect to KV server at no-such-host.invalid:6379"),
            "got: {err}"
        );
    }

    #[test]
    fn kv_auth_ignores_an_empty_password() {
        let auth = KvAuth::resolve(None, Some(String::new())).unwrap();
        assert!(auth.frame().is_none());
    }

    #[test]
    fn resolve_target_prefers_site() {
        assert_eq!(resolve_target(Some("Blog"), false).unwrap(), "blog");
        assert_eq!(resolve_target(None, true).unwrap(), OPCACHE_BROADCAST_VHOST);
        assert_eq!(resolve_target(None, false).unwrap(), OPCACHE_DEFAULT_VHOST);
    }

    #[test]
    fn resolve_target_rejects_empty_site() {
        assert!(resolve_target(Some("   "), false).is_err());
    }

    /// Issue #450, CLI half: `--site` lowercases, so no value an operator can
    /// type reaches a sentinel bucket. That is what makes the uppercase
    /// sentinels unreachable from *both* directions — the server refuses to
    /// derive them from a `Host`, and the CLI refuses to spell them.
    ///
    /// Also pins that a site genuinely *named* `_default` or `_all` now
    /// targets itself, which is the behaviour the collision used to steal.
    #[test]
    fn no_site_flag_can_name_a_sentinel_bucket() {
        for spelling in ["_DEFAULT", "_default", "_ALL", "_all", "_Default"] {
            let target = resolve_target(Some(spelling), false).unwrap();
            assert_ne!(
                target, OPCACHE_DEFAULT_VHOST,
                "`--site {spelling}` must not reach the default bucket — issue #450"
            );
            assert_ne!(
                target, OPCACHE_BROADCAST_VHOST,
                "`--site {spelling}` must not reach the broadcast bucket — issue #450"
            );
        }
        assert_eq!(resolve_target(Some("_default"), false).unwrap(), "_default");
        assert_eq!(resolve_target(Some("_all"), false).unwrap(), "_all");
    }

    #[test]
    fn resolve_target_rejects_mutex() {
        assert!(resolve_target(Some("blog"), true).is_err());
    }
}

/// Proof that the Linux `malloc-override` feature is live in this binary's
/// link: the C `malloc` the rest of the process sees is mimalloc's, not
/// glibc's. These unit tests are linked exactly like the `ephpm` binary (same
/// crate, same `libmimalloc-sys` features, same build.rs link args), so a
/// pass here means libphp.a and `dlopen`ed extensions get mimalloc too.
#[cfg(all(test, target_os = "linux", target_env = "gnu", feature = "malloc-override"))]
#[allow(unsafe_code)] // Raw libc allocation calls are the whole point.
mod malloc_override_tests {
    use std::ffi::{CStr, c_void};

    // Declared by hand rather than via `libmimalloc_sys` because the binding
    // lives behind that crate's `extended` feature (an extra `cty` dep). The
    // symbol itself is always in the linked mimalloc object.
    unsafe extern "C" {
        /// `true` when `p` lies in memory mimalloc manages.
        fn mi_is_in_heap_region(p: *const c_void) -> bool;
    }

    fn owned_by_mimalloc(p: *const c_void) -> bool {
        // SAFETY: mi_is_in_heap_region only inspects the address (a page-map
        // lookup); it never dereferences `p`, so any value is sound.
        unsafe { mi_is_in_heap_region(p) }
    }

    /// The check must be able to say "no", or the positive assertions below
    /// prove nothing.
    #[test]
    fn mimalloc_ownership_check_discriminates() {
        let on_stack = 0u64;
        assert!(!owned_by_mimalloc(std::ptr::from_ref(&on_stack).cast()));

        // SAFETY: anonymous private mapping of one page; checked for
        // MAP_FAILED and unmapped below with the same length.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mapped, libc::MAP_FAILED);
        assert!(!owned_by_mimalloc(mapped));
        // SAFETY: `mapped` is the live 4096-byte mapping created above.
        assert_eq!(unsafe { libc::munmap(mapped, 4096) }, 0);
    }

    /// A direct `malloc`/`calloc`/`realloc`/`posix_memalign` from this
    /// executable resolves to mimalloc.
    #[test]
    fn libc_malloc_family_is_mimalloc() {
        // SAFETY: plain C allocation calls; every pointer is null-checked and
        // released exactly once with `free` (the same, overridden, allocator).
        unsafe {
            let p = libc::malloc(64);
            assert!(!p.is_null());
            assert!(owned_by_mimalloc(p), "malloc() did not return mimalloc memory");

            let p = libc::realloc(p, 1 << 20);
            assert!(!p.is_null());
            assert!(owned_by_mimalloc(p), "realloc() did not return mimalloc memory");
            libc::free(p);

            let c = libc::calloc(16, 16);
            assert!(!c.is_null());
            assert!(owned_by_mimalloc(c), "calloc() did not return mimalloc memory");
            libc::free(c);

            let mut a: *mut c_void = std::ptr::null_mut();
            assert_eq!(libc::posix_memalign(&raw mut a, 4096, 100), 0);
            assert!(owned_by_mimalloc(a), "posix_memalign() did not return mimalloc memory");
            assert_eq!(a as usize % 4096, 0);
            libc::free(a);
        }
    }

    /// Allocations glibc makes *internally* — `realpath(.., NULL)` and
    /// `getcwd(NULL, 0)` are implemented inside libc.so and call `malloc`
    /// from there — also come from mimalloc, i.e. the executable's
    /// definitions preempt libc.so's own. That is what makes a pointer glibc
    /// hands to PHP or to an extension safe to `free` from either side.
    /// (`strdup` is checked too, but mimalloc's override defines `strdup`
    /// itself, so on its own it would not prove libc.so is interposed.)
    #[test]
    fn glibc_internal_allocations_are_mimalloc() {
        // SAFETY: each call returns a fresh malloc'd C string (null-checked)
        // that the caller owns and releases exactly once with `free`.
        unsafe {
            let r = libc::realpath(c"/".as_ptr(), std::ptr::null_mut());
            assert!(!r.is_null());
            assert!(owned_by_mimalloc(r.cast()), "glibc realpath() bypassed the override");
            assert_eq!(CStr::from_ptr(r).to_bytes(), b"/");
            libc::free(r.cast());

            let cwd = libc::getcwd(std::ptr::null_mut(), 0);
            assert!(!cwd.is_null());
            assert!(owned_by_mimalloc(cwd.cast()), "glibc getcwd() bypassed the override");
            libc::free(cwd.cast());

            let s = libc::strdup(c"mimalloc-override".as_ptr());
            assert!(!s.is_null());
            assert!(owned_by_mimalloc(s.cast()), "strdup() did not return mimalloc memory");
            assert_eq!(CStr::from_ptr(s).to_bytes(), b"mimalloc-override");
            libc::free(s.cast());
        }
    }
}
