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
        Some(Commands::Php { args }) => run_php(&php_cli_args(&args)),
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

/// The verbatim argument list for `ephpm php`, including any `--` separator.
///
/// `--` is *meaningful* to php-cli: it ends option parsing, so
/// `… | php -- a b` reads the program from stdin and passes `a b` to it
/// rather than running a script named `a`. clap consumes the first `--` it
/// sees, so the parsed `Vec<String>` cannot express the difference. `ephpm`
/// declares no global options before its subcommand, so argv[1] is always the
/// `php` token and argv[2..] is exactly what the user typed after it.
///
/// Falls back to clap's view for any argv shape that doesn't match that
/// expectation (including non-UTF-8 arguments), so this can only ever restore
/// a separator — never invent arguments.
fn php_cli_args(parsed: &[String]) -> Vec<String> {
    let raw =
        std::env::args_os().skip(2).map(|a| a.into_string().ok()).collect::<Option<Vec<String>>>();
    restore_php_separator(raw, parsed)
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
fn run_php(args: &[String]) -> anyhow::Result<ExitCode> {
    let exit_code = ephpm_php::PhpRuntime::cli_main(args).context("PHP CLI failed")?;
    let _ = ephpm_php::PhpRuntime::shutdown();
    Ok(exit_code_from(exit_code))
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
/// when `max_execution_time >= request` (excluding `0`, which means "no PHP
/// limit" — the request backstop is then the only ceiling, by design).
#[cfg(php_max_exec_timers)]
fn warn_max_execution_time(config: &ephpm_config::Config) {
    let inner = config.php.max_execution_time;
    let outer = config.server.timeouts.request;
    if inner != 0 && u64::from(inner) >= outer {
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
        tracing::warn!(
            max_execution_time = config.php.max_execution_time,
            request_timeout = config.server.timeouts.request,
            "[php] max_execution_time is not natively enforced on this build \
             (the linked PHP has no per-thread execution timers; its process-wide \
             SIGPROF timer is unsafe under tokio and stays disabled). The \
             per-request deadline actually in force is [server.timeouts] request."
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
    // Persistent raw sockets: `EG(persistent_list)` is keyed `host:port` with
    // no tenant component, so a tenant reusing another's host:port inherits its
    // live, authenticated socket.
    "pfsockopen",
    "fsockopen",
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
/// - `cluster_invalidation` — when `false`, also disable the OPcache
///   introspection/invalidation functions that ePHPm otherwise needs.
///
/// Returns `None` when the union is empty (nothing to emit).
fn compose_disable_functions(
    operator: &[String],
    include_shell: bool,
    include_hardening: bool,
    cluster_invalidation: bool,
) -> Option<String> {
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Operator list first: preserve their explicit intent and ordering.
    for f in operator {
        push_unique(&mut ordered, &mut seen, f);
    }
    if include_shell || include_hardening {
        for f in SHELL_EXEC_FUNCTIONS {
            push_unique(&mut ordered, &mut seen, f);
        }
    }
    if include_hardening {
        for f in HARDENING_FUNCTIONS {
            push_unique(&mut ordered, &mut seen, f);
        }
        for f in HARDENING_OPCACHE_ALWAYS {
            push_unique(&mut ordered, &mut seen, f);
        }
        if !cluster_invalidation {
            for f in HARDENING_OPCACHE_WHEN_NO_CLUSTER_INVALIDATION {
                push_unique(&mut ordered, &mut seen, f);
            }
        }
    }

    if ordered.is_empty() { None } else { Some(ordered.join(",")) }
}

/// Log, at startup, exactly what the multi-tenant hardening preset did — the
/// denylist it added, the performance it cost, and any residual it could not
/// close in this configuration. Never silent: an operator who relies on the
/// preset must be able to see precisely what it bought them.
fn log_multi_tenant_hardening(vhost_hardening: bool, cluster_invalidation: bool) {
    if !vhost_hardening {
        return;
    }
    tracing::info!(
        "multi-tenant hardening ON: disabled pcntl_*/posix process control, \
         pfsockopen/fsockopen, SysV shm_*/sem_*/msg_*, dl, mail, and \
         opcache_reset/opcache_compile_file via disable_functions; \
         mysqli.allow_persistent=0. Cost: persistent DB/socket connections are \
         off (Redis pconnect, mysqli p:, pfsockopen). Disable with \
         [server.security] multi_tenant_hardening = false."
    );
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

/// Say plainly, at startup, when the code bundle cannot reach the one function a
/// real Composer autoloader probes with.
///
/// `file_exists` does not go through the stream wrapper, so without the
/// internal-function handler override the bundle is measurably indistinguishable
/// from `code_bundle = "off"` on a Composer application (1223 filesystem
/// syscalls per request either way). A feature that silently does nothing is
/// worse than one that says so.
fn warn_if_file_exists_unfronted(fronted: &[&'static str]) {
    if fronted.contains(&"file_exists") {
        return;
    }
    tracing::warn!(
        "[php] code_bundle is NOT fronting file_exists — the function a real Composer \
         autoloader probes with. It does not go through the stream wrapper, so the bundle \
         will accelerate almost nothing on a Composer application (measured: \
         indistinguishable from code_bundle = \"off\"). Unset \
         EPHPM_BUNDLE_FRONT_FILE_EXISTS=0 to re-enable it."
    );
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

    // Layer stack. The env filter must be scoped to the fmt layer with
    // `with_filter`, never pushed into the Vec as a sibling layer: `Vec`'s
    // `Layer` impl merges per-layer `Interest` by taking the MAX, so an
    // unfiltered fmt layer (`Interest::always`) bypasses a sibling
    // `EnvFilter` entirely — RUST_LOG / -v / [server.logging] level were
    // silently inert in non-OTLP builds when the filter was a sibling.
    // (Scoping also keeps a global info-level filter from disabling the
    // DEBUG-level request spans the OTLP layer needs; that layer carries
    // its own target filter.)
    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> = Vec::new();
    layers.push(fmt_layer.with_filter(env_filter).boxed());

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
    // Multi-tenant hardening preset: in vhost mode, extend the denylist to the
    // full set a hostile-tenant pentest proved closes every cross-tenant
    // read/write channel (SysV IPC, persistent-socket inheritance, pcntl/posix
    // process control, OPcache flush). Composed as a UNION with any operator
    // `disable_functions` (never clobbered — see `compose_disable_functions`).
    let vhost_hardening =
        config.server.sites_dir.is_some() && config.server.effective_multi_tenant_hardening();
    // ePHPm's own cluster OPcache invalidator (opcache.rs) calls the userland
    // `opcache_get_status`/`opcache_invalidate` through the function table, so
    // the hardening preset may only fully lock down the OPcache API
    // (opcache.restrict_api + disabling those two) when cluster invalidation is
    // OFF. When it is on, only the DoS-grade `opcache_reset`/`opcache_compile_file`
    // are disabled and the residual is logged. Resolve it here so the ini
    // builder and the startup log agree.
    let cluster_invalidation =
        config.opcache.effective_cluster_invalidation(config.cluster.enabled);
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
    // The JIT default is shaped by tenancy: tracing in single-site serve,
    // disable when `sites_dir` is set (per-vhost invalidation never reclaims
    // JIT buffer) — so autotune needs to know whether this is a vhost run.
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
        || cfg!(php_max_exec_timers);

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
            // Native max_execution_time (per-thread execution timers only).
            // Written before ini_overrides and before any per-site ini_overrides
            // are replayed at request time, so an explicit override still wins.
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
                cluster_invalidation,
            ) {
                let _ = writeln!(content, "disable_functions={df}");
            }
            // Extra hardening ini (multi-tenant preset only). mysqli persistent
            // handles are keyed without a tenant component, so disable them; and
            // point the OPcache userland API at an unreachable sentinel so a
            // tenant cannot reset/inspect the shared cache — but only when
            // ePHPm's own cluster invalidator does not itself need that API.
            if vhost_hardening {
                let _ = writeln!(content, "mysqli.allow_persistent=0");
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
    log_multi_tenant_hardening(vhost_hardening, cluster_invalidation);

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
    // Arm value for PHP's per-thread execution timer. The request paths call
    // zend_set_timeout() with this each request because the embed SAPI zeroes
    // the max_execution_time ini entry at runtime. No-op on builds without
    // per-thread timers and in stub mode.
    ephpm_php::PhpRuntime::set_max_execution_time(config.php.max_execution_time);

    // In-memory code bundle (experimental `[php] code_bundle`).
    //
    // Two steps with very different threading requirements:
    //
    //  1. The C hooks are armed HERE, on the single-threaded startup path, right
    //     after PHP init — they overwrite PHP's global function pointers, which
    //     must not race a reader. They are inert while no index is published:
    //     every hook delegates to the saved original, i.e. exactly
    //     `code_bundle = "off"` behaviour.
    //  2. The scan itself runs on ONE background thread and publishes the
    //     finished index with a single atomic `set`. Startup never blocks on it
    //     (the scan measured 45 ms warm but 3.7 s cold on Windows), and there is
    //     no half-built state — which is what keeps sealed roots safe, since a
    //     partially scanned index would report "does not exist" for files it had
    //     not reached yet.
    //
    // Every pre-final state is fail-safe: not-ready → fall through to disk;
    // scan complete → overlay; sealed roots declared → authoritative, and only
    // then. A scan failure never publishes, so the process simply stays on the
    // fall-through path.
    if config.php.is_code_bundle_enabled() {
        if dev_mode {
            // The bundle freezes source bytes AND mtimes at scan time, so an
            // in-place edit is invisible for the life of the process — the exact
            // opposite of what `ephpm dev` is for. Hard error rather than a
            // silent downgrade: a dev server that ignores your edits is a much
            // worse experience than a startup message telling you why.
            anyhow::bail!(
                "[php] code_bundle = {:?} is not supported by `ephpm dev`. The bundle serves \
                 source and mtimes captured at startup, so edits to .php files would be \
                 invisible until restart. Remove the setting (or set code_bundle = \"off\") \
                 for development.",
                config.php.code_bundle_label()
            );
        }
        if config.server.sites_dir.is_some() {
            tracing::warn!(
                "[php] code_bundle is set but multi-site mode ([server] sites_dir) is \
                 active — the code bundle is single-docroot only in this POC and is \
                 IGNORED. Code reads fall through to disk."
            );
        } else if config.php.is_code_bundle_lazy() {
            // ── code_bundle = "lazy": read-through cache ──────────────────
            //
            // Nothing is indexed up front. The hooks are armed and an EMPTY
            // cache is published immediately: a lookup that misses does exactly
            // the filesystem operation PHP was about to do, answers from it, and
            // keeps the result. There is no "not ready yet" state to wait out
            // and no all-or-nothing size cliff — `code_bundle_max_bytes` evicts
            // instead of refusing.
            //
            // Authoritative negatives are structurally impossible here, so
            // `code_bundle_sealed_paths` is rejected rather than ignored.
            if !config.php.code_bundle_sealed_paths.is_empty() {
                anyhow::bail!(
                    "[php] code_bundle = \"lazy\" cannot be combined with \
                     code_bundle_sealed_paths. Sealing declares that absence from the \
                     index PROVES a file does not exist — a lazily populated, evictable \
                     cache can never prove that, because \"never populated\" and \
                     \"populated then evicted\" are the same state. Use code_bundle = \
                     \"sealed\" (a complete, immutable startup scan) or drop \
                     code_bundle_sealed_paths."
                );
            }
            if config.php.code_bundle_verify_negatives {
                tracing::warn!(
                    "[php] code_bundle_verify_negatives is IGNORED with code_bundle = \
                     \"lazy\" — lazy mode never answers a negative from the index, only \
                     from a live syscall, so there is nothing to verify."
                );
            }
            let algo = ephpm_php::code_bundle::BundleCompression::parse(
                &config.php.code_bundle_compression,
            )
            .with_context(|| {
                format!(
                    "invalid [php] code_bundle_compression = {:?} (expected \
                     none|gzip|zstd|brotli)",
                    config.php.code_bundle_compression
                )
            })?;
            let docroot = config.server.document_root.clone();
            let max = usize::try_from(config.php.code_bundle_max_bytes).unwrap_or(usize::MAX);
            let boot_scan = config.php.code_bundle_boot_scan;

            ephpm_php::PhpRuntime::arm_code_bundle_hooks()
                .context("failed to arm code bundle hooks")?;
            let index = ephpm_php::code_bundle::LazyIndex::new(&docroot, algo, max);
            let published = ephpm_php::code_bundle::publish_lazy(index);
            if published.is_none() {
                tracing::error!(
                    "failed to publish the lazy code cache — code reads fall through to \
                     disk for the life of this process"
                );
            }
            let fronted = ephpm_php::code_bundle::function_overrides();
            tracing::info!(
                compression = algo.label(),
                max_bytes = max,
                boot_scan,
                document_root = %docroot.display(),
                validate_timestamps,
                function_overrides = ?fronted,
                "code bundle: lazy read-through cache active. Misses fall through to the \
                 filesystem and populate the cache; nothing is ever answered \"missing\" \
                 from the cache itself."
            );
            warn_if_file_exists_unfronted(&fronted);
            if validate_timestamps {
                tracing::warn!(
                    "[php] code_bundle is ON but opcache.validate_timestamps is ON — the \
                     cache serves the mtime it observed when it first read each file, so \
                     OPcache's revalidation cannot see a later edit. Use `ephpm deploy` / \
                     `ephpm cache reset` after replacing files, or set [php] \
                     opcache_validate_timestamps = false to stop paying for the stat."
                );
            }

            // The boot scan is now an OPTIMIZATION, not a correctness step: it
            // bulk-fills the likely working set while the server is already
            // answering requests from the same cache, publishing entries as it
            // finds them. One thread on purpose — a fan-out would win a second
            // of wall time and spend it competing with the first real requests.
            if boot_scan && let Some(idx) = published {
                let builder = std::thread::Builder::new().name("ephpm-code-bundle".into());
                let spawned = builder.spawn(move || {
                    let started = std::time::Instant::now();
                    let (files, bytes) = idx.boot_scan(true);
                    tracing::info!(
                        entries = files,
                        resident_bytes = bytes,
                        load_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "code bundle: boot scan finished pre-filling the lazy cache"
                    );
                });
                if let Err(e) = spawned {
                    tracing::warn!(
                        error = %e,
                        "could not spawn the code bundle boot scan — the cache still fills \
                         lazily on first use, which is the normal path"
                    );
                }
            }
        } else {
            let algo = ephpm_php::code_bundle::BundleCompression::parse(
                &config.php.code_bundle_compression,
            )
            .with_context(|| {
                format!(
                    "invalid [php] code_bundle_compression = {:?} (expected \
                     none|gzip|zstd|brotli)",
                    config.php.code_bundle_compression
                )
            })?;
            let docroot = config.server.document_root.clone();
            let max = usize::try_from(config.php.code_bundle_max_bytes).unwrap_or(usize::MAX);

            if !config.php.code_bundle_boot_scan {
                tracing::warn!(
                    "[php] code_bundle_boot_scan = false is IGNORED with code_bundle = \
                     {:?} — that mode IS its startup scan and cannot skip it. The setting \
                     only applies to code_bundle = \"lazy\".",
                    config.php.code_bundle_label()
                );
            }
            // `sealed` promotes a bundle MISS from "ask the filesystem" to
            // "answer 'no such file' from RAM" — but only inside the subtrees
            // named by code_bundle_sealed_paths, which is empty by default. It
            // is the one setting that can change an answer rather than just its
            // latency, hence the explicit startup line stating what is live.
            let semantics = if config.php.is_code_bundle_sealed() {
                ephpm_php::code_bundle::BundleSemantics::Sealed
            } else {
                ephpm_php::code_bundle::BundleSemantics::Overlay
            };
            // The verify knob only has anything to confirm in sealed mode. Never
            // silently ignore it: say so at startup.
            let verify_negatives =
                config.php.code_bundle_verify_negatives && config.php.is_code_bundle_sealed();
            if config.php.code_bundle_verify_negatives && !verify_negatives {
                tracing::warn!(
                    "[php] code_bundle_verify_negatives is set but code_bundle is not \
                     \"sealed\" — there are no authoritative negatives to verify, so the \
                     setting is IGNORED."
                );
            }
            if !config.php.code_bundle_sealed_paths.is_empty()
                && !config.php.is_code_bundle_sealed()
            {
                tracing::warn!(
                    "[php] code_bundle_sealed_paths is set but code_bundle is not \
                     \"sealed\" — the setting is IGNORED and every miss still falls \
                     through to the filesystem."
                );
            }
            // Sealed roots are validated NOW, on the startup path, so a bad path
            // is a startup error rather than a surprise from a background thread.
            let spec = ephpm_php::code_bundle::BundleSpec::new(
                docroot.clone(),
                algo,
                max,
                semantics,
                &config.php.code_bundle_sealed_paths,
                verify_negatives,
            )
            .context("invalid [php] code_bundle_sealed_paths")?;

            ephpm_php::PhpRuntime::arm_code_bundle_hooks()
                .context("failed to arm code bundle hooks")?;

            let sealed_roots: Vec<String> = spec.sealed_roots().to_vec();
            let fronted = ephpm_php::code_bundle::function_overrides();
            tracing::info!(
                compression = algo.label(),
                semantics = semantics.label(),
                verify_negatives,
                sealed_roots = ?sealed_roots,
                document_root = %docroot.display(),
                validate_timestamps,
                function_overrides = ?fronted,
                "code bundle: scanning in the background; until it completes, code reads \
                 fall through to the filesystem exactly as with code_bundle = \"off\""
            );
            warn_if_file_exists_unfronted(&fronted);
            if verify_negatives {
                tracing::warn!(
                    "[php] code_bundle_verify_negatives is ON — every authoritative \
                     negative is confirmed against disk and mismatches are logged. This \
                     gives back the syscalls sealed mode removes; it is a DIAGNOSTIC \
                     mode, not a production setting."
                );
            }
            if validate_timestamps {
                tracing::warn!(
                    "[php] code_bundle is ON but opcache.validate_timestamps is ON — the \
                     bundle serves the mtime captured at scan time, so OPcache's \
                     revalidation can never observe an edit. Code changes are invisible \
                     until restart either way; set [php] opcache_validate_timestamps = \
                     false to at least stop paying for the stat."
                );
            }

            // ONE background thread — not a rayon fan-out, which would compete
            // with early requests for CPU and IO.
            let builder = std::thread::Builder::new().name("ephpm-code-bundle".into());
            // (No thread-priority call: `std::thread` exposes none portably, and
            // the `nice`/`SetThreadPriority` FFI is not worth an `unsafe` block
            // in the binary crate. The substance is that this is ONE thread, not
            // a fan-out competing with early requests for CPU and IO.)
            let spawned = builder.spawn(move || {
                let started = std::time::Instant::now();
                match ephpm_php::code_bundle::Bundle::from_scan(&spec) {
                    Ok(bundle) => {
                        let files = bundle.file_count();
                        let raw = bundle.raw_bytes();
                        let resident = bundle.resident_bytes();
                        let semantics_note = if sealed_roots.is_empty() {
                            "overlay — hits are served from memory, misses fall through to \
                             the filesystem unchanged"
                        } else {
                            "SEALED roots armed — an unindexed .php path inside a sealed \
                             root is reported MISSING without touching disk, until the \
                             first write into that root permanently disarms it"
                        };
                        let load_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                        match ephpm_php::PhpRuntime::publish_code_bundle(bundle) {
                            Ok(()) => tracing::info!(
                                entries = files,
                                raw_bytes = raw,
                                resident_bytes = resident,
                                sealed_roots = ?sealed_roots,
                                load_ms,
                                "code bundle published: {files} .php files, {resident} bytes \
                                 resident; {semantics_note}"
                            ),
                            Err(e) => tracing::error!(
                                error = %e,
                                "failed to publish code bundle — code reads keep falling \
                                 through to disk"
                            ),
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "code bundle NOT built — code reads keep falling through to normal \
                         disk access for the life of this process"
                    ),
                }
            });
            if let Err(e) = spawned {
                tracing::warn!(
                    error = %e,
                    "could not spawn the code bundle scan thread — code reads fall through \
                     to disk"
                );
            }
        }
    }

    // Stack-overflow crash containment (experimental `[php] crash_containment`).
    //
    // Armed here, on the single-threaded startup path, before any PHP request
    // exists — and the recovery hook is registered BEFORE the guard is enabled,
    // so a guard can never be armed with no hook to recover it. Left entirely
    // alone when the knob is off: no hook is registered, no guard is ever armed,
    // and a C-stack overflow aborts the process exactly as it always has.
    if config.php.is_crash_containment_active() {
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
            "[php] crash_containment = true is IGNORED unless [php] fpm_engine = \
             \"pool\" in fpm mode — containment must be able to retire the thread \
             that crashed, which only ePHPm's own FPM pool can do. Crashes will \
             abort the process as usual."
        );
    }

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
}

#[cfg(test)]
mod disable_functions_tests {
    use super::*;

    fn ov(pairs: &[(&str, &str)]) -> Vec<[String; 2]> {
        pairs.iter().map(|(k, v)| [(*k).to_string(), (*v).to_string()]).collect()
    }

    #[test]
    fn nothing_to_emit_without_operator_or_baseline() {
        assert_eq!(compose_disable_functions(&[], false, false, false), None);
    }

    #[test]
    fn shell_only_matches_legacy_line() {
        let df = compose_disable_functions(&[], true, false, false).unwrap();
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
        let df = compose_disable_functions(&operator, true, false, false).unwrap();
        // Operator entries come first and survive.
        assert!(df.starts_with("pcntl_fork,mkdir_helper,"));
        // Shell baseline is still present.
        assert!(df.split(',').any(|f| f == "system"));
        assert!(df.split(',').any(|f| f == "pcntl_fork"));
    }

    #[test]
    fn hardening_adds_the_proven_channels() {
        let df = compose_disable_functions(&[], true, true, false).unwrap();
        for expected in [
            "system",        // shell family
            "pcntl_fork",    // pcntl
            "posix_kill",    // posix process control
            "pfsockopen",    // persistent socket inheritance
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
    fn cluster_invalidation_keeps_opcache_introspection_callable() {
        // With cluster invalidation ON, ePHPm needs opcache_invalidate/status,
        // so they must NOT be in the denylist — but opcache_reset always is.
        let on = compose_disable_functions(&[], true, true, true).unwrap();
        assert!(on.split(',').any(|f| f == "opcache_reset"));
        assert!(!on.split(',').any(|f| f == "opcache_invalidate"));
        assert!(!on.split(',').any(|f| f == "opcache_get_status"));

        // With it OFF, they are disabled too.
        let off = compose_disable_functions(&[], true, true, false).unwrap();
        assert!(off.split(',').any(|f| f == "opcache_invalidate"));
        assert!(off.split(',').any(|f| f == "opcache_get_status"));
    }

    #[test]
    fn duplicate_names_are_deduplicated_case_insensitively() {
        let operator =
            collect_operator_disable_functions(&ov(&[("disable_functions", "System, PCNTL_FORK")]));
        let df = compose_disable_functions(&operator, true, true, false).unwrap();
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
}

#[cfg(test)]
mod cli_tests {
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
