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

/// Global allocator override for the ephpm binary (Unix only).
///
/// mimalloc gives lower allocator contention and better locality than the
/// default system allocator under the ePHPm workload profile (many small,
/// short-lived allocs from HTTP frames, RESP payloads, and query buffers).
/// Applied only in the binary so unit tests and other tooling stay on the
/// standard allocator.
///
/// Memory-footprint watch item: mimalloc is more aggressive about
/// retaining freed pages for reuse than the default allocator, so RSS
/// looks larger at steady state. This is normal — track it against the
/// 320 MiB target and adjust with `MIMALLOC_PURGE_DELAY` if needed.
///
/// Windows uses the system allocator — see the Cargo.toml comment on the
/// mimalloc dep for the MSVC `/MD` vs PHP-SDK `/MT` link mismatch.
#[cfg(unix)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod fatal_signal;
mod service;

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
    },

    /// Run PHP CLI commands using the embedded PHP runtime
    #[command(disable_help_flag = true)]
    Php {
        /// Arguments to pass to the PHP interpreter
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    Install,

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
    /// `opcache:version:_all`) via the running server's RESP listener; gossip
    /// replicates the write to every peer within seconds.
    ///
    /// Requires the running server to have `[kv.redis_compat] enabled = true`
    /// so the CLI (a separate process) can reach the in-process KV store.
    Deploy {
        /// Invalidate a specific vhost. Mutually exclusive with `--all`.
        #[arg(long, group = "target")]
        site: Option<String>,

        /// Invalidate every vhost via the broadcast key (`opcache:version:_all`).
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

    /// Cache management subcommands (OPcache introspection and local reset).
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

#[derive(Subcommand, Debug)]
enum CacheSubcommand {
    /// Local-only OPcache reset (bypasses KV — does not propagate to peers).
    /// Behaves identically to `deploy` on a single-node server; use `deploy`
    /// on a cluster to broadcast the reset.
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
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Php { args }) => run_php(&args),
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
        Some(Commands::Install) => run_service_cmd(service::install),
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
        Some(Commands::Dev { listen, document_root, port, sites, verbose }) => {
            run_dev(listen, document_root, port, sites, verbose)
        }
        // Bare `ephpm` (no subcommand) is the dev-mode entry point. Service
        // backends always invoke the binary with explicit `serve --config`
        // arguments, so this default never executes under SCM/systemd/launchd.
        None => run_dev(None, None, 8080, None, 0),
        other @ Some(Commands::Serve { .. }) => run_serve_sync(other),
    }
}

/// Initialise a small tracing subscriber for service-management commands so
/// `tracing::info!` calls in the `service` module show up on the console.
fn ensure_cli_tracing() {
    use std::sync::Once;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(tracing_subscriber::fmt::layer().with_target(false))
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
fn run_dev(
    listen: Option<String>,
    document_root: Option<PathBuf>,
    port: u16,
    sites: Option<PathBuf>,
    verbose: u8,
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

    // Resolve document root — CLI override, else CWD.
    config.server.document_root = match document_root {
        Some(root) => root,
        None => std::env::current_dir().context("failed to read current directory")?,
    };

    // When --sites is provided, point the vhost machinery at it and enable
    // the `.localhost` suffix-stripping so on-disk dirs are short names
    // (`blog/`) while browsers use `blog.localhost:<port>`.
    if let Some(sites_path) = sites {
        let canonical = sites_path.canonicalize().unwrap_or_else(|_| sites_path.clone());
        config.server.sites_dir = Some(canonical);
        config.server.sites_domain_suffix = Some(".localhost".into());
    }

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

/// Run the `ephpm php` subcommand — pass args through to the embedded PHP CLI.
fn run_php(args: &[String]) -> anyhow::Result<ExitCode> {
    warn_if_runtime_extension_flag(args);
    let exit_code = ephpm_php::PhpRuntime::cli_main(args).context("PHP CLI failed")?;
    let _ = ephpm_php::PhpRuntime::shutdown();
    Ok(exit_code_from(exit_code))
}

/// Warn when a runtime `-d extension=` is passed to `ephpm php`.
///
/// The embedded PHP runtime initialises before it parses `-d` directives, so
/// `-d extension=…` is silently ignored (extensions must be registered at
/// startup). Rather than let the knob be a silent no-op, warn once and point
/// the user at the config equivalent.
fn warn_if_runtime_extension_flag(args: &[String]) {
    // Match `-d extension=…` (split), `-dextension=…`, and `-d=extension=…`.
    let mut prev_was_d = false;
    for arg in args {
        let is_ext_directive = if prev_was_d {
            arg.starts_with("extension=")
        } else if let Some(rest) = arg.strip_prefix("-d") {
            // `-dextension=…` or `-d=extension=…`
            rest.strip_prefix('=').unwrap_or(rest).starts_with("extension=")
        } else {
            false
        };
        if is_ext_directive {
            tracing::warn!(
                "`-d extension=` is ignored by `ephpm php` — the embedded PHP \
                 runtime loads extensions before parsing `-d`. Register shared \
                 extensions via `[php] extensions` in ephpm.toml instead."
            );
            return;
        }
        prev_was_d = arg == "-d";
    }
}

/// Warn once at startup for every config knob that is parsed but not acted
/// upon, so a silently-ignored setting can never look like it took effect.
///
/// Each field is compared against its own section's `Default`, so an untouched
/// config stays quiet and only a deliberate override produces a line. Whenever
/// one of these knobs gains a real implementation, delete its branch here and
/// the matching "Planned: not yet implemented" doc comment in `ephpm-config`.
fn warn_unimplemented_knobs(config: &ephpm_config::Config) {
    let php_defaults = ephpm_config::PhpConfig::default();
    if config.php.max_execution_time != php_defaults.max_execution_time {
        tracing::warn!(
            max_execution_time = config.php.max_execution_time,
            request_timeout = config.server.timeouts.request,
            "[php] max_execution_time is not enforced — the value is not \
             written into the generated php.ini, and PHP's own SIGPROF-based \
             timer is deliberately disabled because that handler crashes when \
             the signal lands on a tokio worker thread. The per-request \
             deadline actually in force is [server.timeouts] request."
        );
    }

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
    let otlp = ephpm_server::otlp::init_layer(config.server.diagnostics.otlp_endpoint.as_deref())
        .context("failed to initialize OTLP trace export")?;

    #[cfg(feature = "otlp")]
    let otlp_active = otlp.is_some();
    #[cfg(not(feature = "otlp"))]
    let otlp_active = false;

    // Layer stack. Without OTLP the env filter is installed globally (its
    // `Layer` impl), exactly as before. With the OTLP layer active it must be
    // scoped to the fmt layer instead: a global info-level filter would
    // disable the DEBUG-level request spans for every layer, including the
    // OTLP one (which carries its own target filter).
    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> = Vec::new();
    if otlp_active {
        layers.push(fmt_layer.with_filter(env_filter).boxed());
    } else {
        layers.push(env_filter.boxed());
        layers.push(fmt_layer);
    }

    // Set up access log file writer if configured.
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
            .with_writer(access_writer)
            .with_target(true)
            .with_filter(EnvFilter::new("access_log=info"));
        layers.push(access_layer.boxed());
        Some(guard)
    };

    // The guard flushes the exporter's batch queue on drop — hold it until
    // the server has exited.
    #[cfg(feature = "otlp")]
    let _otlp_guard = otlp.map(|(otlp_layer, guard, description)| {
        layers.push(otlp_layer.boxed());
        (guard, description)
    });

    tracing_subscriber::registry().with(layers).init();

    #[cfg(feature = "otlp")]
    if let Some((_, ref description)) = _otlp_guard {
        tracing::info!(endpoint = %description, "OTLP trace export enabled (http/protobuf)");
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
    // Always generate the ini in worker mode: log_errors must default On there
    // (see below) or a worker script that dies during boot leaves no
    // diagnostic anywhere — display_errors output is captured into a buffer
    // that is discarded when no request is in flight.
    let worker_mode = config.php.mode == "worker";
    // OPcache timestamp-validation default is mode-dependent (off under serve,
    // on under dev) and is always emitted into the generated ini, so ini
    // generation is now unconditional. The other flags below still document
    // *why* generation would be needed even absent the opcache line.
    // Resolve the full resource-aware autotuning profile once: it feeds both
    // the generated php.ini (opcache/memory/realpath/assertions lines) and the
    // startup autotune summary log below.
    let autotune = config.php.autotune(dev_mode);
    let opcache_ini_lines = autotune.ini_lines();
    let validate_timestamps = autotune.validate_timestamps.value;
    // [php] extensions also forces ini generation: `extension=` lines only
    // take effect when PHP parses them during MINIT, same as the overrides.
    let want_generated_ini = !opcache_ini_lines.is_empty()
        || !config.php.ini_overrides.is_empty()
        || !config.php.extensions.is_empty()
        || vhost_disable_shell
        || worker_mode;

    let (effective_ini_path, _generated_ini_guard): (Option<PathBuf>, Option<tempfile::TempDir>) =
        if want_generated_ini {
            use std::fmt::Write as _;

            let mut content = String::new();
            // Server-sane default, before ini_file/ini_overrides so either can
            // override it: fatals must reach the engine log ([PHP] lines).
            if worker_mode {
                content.push_str("log_errors=On\n");
            }
            // Shared extensions ([php] extensions) go first, before
            // ini_file/ini_overrides, so any extension ini settings that
            // follow apply to an already-declared extension. A bare name
            // rides PHP's extension_dir search (`extension=redis`); a path
            // loads verbatim (`extension=/path/to/imagick.so`). ABI
            // mismatches (PHP minor / ZTS / libc) are rejected by PHP at
            // startup with an explicit API-version error.
            for ext in &config.php.extensions {
                let _ = writeln!(content, "extension={ext}");
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
            for [k, v] in &config.php.ini_overrides {
                let _ = writeln!(content, "{k}={v}");
            }
            if vhost_disable_shell {
                let _ = writeln!(
                    content,
                    "disable_functions=exec,passthru,shell_exec,system,proc_open,popen,pcntl_exec"
                );
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
            // Same shape as the sqld binary extraction in `ephpm-sqld`:
            // mkdtemp (O_EXCL, never a reused path), 0700, then `create_new`
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

    // Resource-aware autotuning: log the detected CPU/memory budget and the
    // derived (or explicitly-pinned) PHP/OPcache profile at INFO. Trust
    // requires visibility — an operator must be able to see exactly what
    // ePHPm sized itself to and which values they overrode (marked `*`).
    tracing::info!("{}", autotune.summary_line());

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

    // Initialize PHP BEFORE creating tokio runtime (single-threaded here).
    // finalize_for_http() disables SIGPROF so it can't crash worker threads.
    ephpm_php::PhpRuntime::init_with_ini_file(effective_ini_path.as_deref())
        .context("failed to initialize PHP runtime")?;
    ephpm_php::PhpRuntime::finalize_for_http()
        .context("failed to finalize PHP runtime for HTTP")?;

    // Now safe to create the multi-threaded tokio runtime.
    //
    // Note: [php].workers is enforced by a semaphore around PHP execution in
    // the router, NOT by capping tokio's blocking pool — that pool is shared
    // with static file I/O and other blocking work, and slow PHP scripts must
    // never starve it.
    if config.php.workers > 0 {
        tracing::info!(workers = config.php.workers, "concurrent PHP executions capped");
    }
    //
    // Identical to `Runtime::new()` (multi-thread, all drivers enabled) except
    // for the thread hook, which gives every worker and blocking-pool thread an
    // alternate signal stack if it lacks one. Without an alternate stack the
    // fatal-signal handler cannot run for a stack-overflow fault — which is
    // precisely one of the faults worth diagnosing. PHP executes on this
    // runtime's blocking pool, so the hook must be on this runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_start(fatal_signal::install_thread_altstack)
        .build()
        .context("failed to create tokio runtime")?;
    let result = rt.block_on(async { ephpm_server::serve(config, dev_mode).await });

    // Drop the runtime BEFORE tearing down PHP (issue #266). Dropping joins
    // every worker and blocking-pool thread; each PHP-registered blocking
    // thread's TLS guard then runs php_request_shutdown + ts_free_thread on
    // the thread itself, freeing its per-thread Zend state into its own heap.
    // Only after that is php_embed_shutdown() safe: module shutdown's
    // ts_free_id() walks all remaining TSRM entries on *this* thread, and any
    // still-live worker entry would make it free another thread's
    // request-lifetime allocations cross-heap (pcre's cached named-subpattern
    // strings after WordPress traffic) — zend_mm_heap corrupted → SIGABRT.
    drop(rt);

    // Shutdown PHP runtime — all PHP threads are gone, only the main
    // thread's TSRM entry remains.
    ephpm_php::PhpRuntime::shutdown().context("failed to shutdown PHP runtime")?;

    result.map(|()| ExitCode::SUCCESS)
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

    // Validate cross-field invariants (e.g. worker-mode worker_script) AFTER
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
    }
}

/// TCP connection helper. Authenticates before returning, so every caller
/// gets a stream that is ready for real commands.
async fn kv_connect(host: &str, port: u16, auth: &KvAuth) -> anyhow::Result<TcpStream> {
    let addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid address: {host}:{port}"))?;
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("could not connect to KV server at {host}:{port}"))?;
    kv_authenticate(&mut stream, auth).await?;
    Ok(stream)
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
/// server-side `crate::opcache::DEFAULT_VHOST`; kept in-lockstep manually since
/// the CLI does not depend on `ephpm-server`.
const OPCACHE_DEFAULT_VHOST: &str = "_default";

/// Broadcast vhost name written by `--all`. Kept in-lockstep with the
/// server-side `crate::opcache::BROADCAST_VHOST`.
const OPCACHE_BROADCAST_VHOST: &str = "_all";

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
    let stream = TcpStream::connect(format!("{host}:{port}"))
        .await
        .with_context(|| resp_connect_hint(host, port))?;
    let mut stream = stream;
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

/// `ephpm cache reset` / `ephpm cache status` dispatcher.
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
mod cli_tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn parses_install_subcommand() {
        let cli = Cli::try_parse_from(["ephpm", "install"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Install)));
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

    #[test]
    fn resolve_target_rejects_mutex() {
        assert!(resolve_target(Some("blog"), true).is_err());
    }
}
