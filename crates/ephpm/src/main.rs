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

mod service;

/// ePHPm — All-in-one PHP application server
#[derive(Parser, Debug)]
#[command(name = "ephpm", version, about)]
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
        Some(Commands::Kv { host, port, subcommand }) => {
            let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
            rt.block_on(run_kv(&host, port, subcommand))
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
        Some(Commands::Dev { listen, document_root, port, verbose }) => {
            run_dev(listen, document_root, port, verbose)
        }
        // Bare `ephpm` (no subcommand) is the dev-mode entry point. Service
        // backends always invoke the binary with explicit `serve --config`
        // arguments, so this default never executes under SCM/systemd/launchd.
        None => run_dev(None, None, 8080, 0),
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

    print_dev_banner(&config);
    run_with_config(config, verbose)
}

/// Pretty banner printed once at dev-server startup. Stdout, not tracing,
/// so it's stable across log-format changes and visible regardless of
/// `RUST_LOG`.
fn print_dev_banner(config: &ephpm_config::Config) {
    let version = env!("CARGO_PKG_VERSION");
    let url = format!("http://{}", config.server.listen);
    let doc_root = config.server.document_root.display();
    let php = ephpm_php::PhpRuntime::php_version();

    println!();
    println!("  ePHPm {version} — dev server");
    println!("    serving:  {doc_root}");
    println!("    url:      {url}");
    println!("    php:      {php}");
    println!("    press ctrl+c to stop");
    println!();
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
    // Windows: extract the embedded php8embed.dll before the first PHP call.
    // Guard deletes the temp directory when this function returns.
    #[cfg(all(php_linked, target_os = "windows"))]
    let _dll_guard = ephpm_php::windows_dll::extract_php_dll()
        .context("failed to extract embedded php8embed.dll")?;

    let exit_code = ephpm_php::PhpRuntime::cli_main(args).context("PHP CLI failed")?;
    let _ = ephpm_php::PhpRuntime::shutdown();
    Ok(exit_code_from(exit_code))
}

/// Convert a PHP exit code (i32) to a Rust `ExitCode`.
fn exit_code_from(code: i32) -> ExitCode {
    if code == 0 { ExitCode::SUCCESS } else { ExitCode::from(u8::try_from(code).unwrap_or(1)) }
}

/// Removes a temp file when dropped. Used to clean up the generated
/// php.ini we materialise from `[php] ini_overrides`.
struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
    run_with_config(config, verbose)
}

/// Shared HTTP server startup path used by both `serve` (production) and
/// `dev` (developer) entry points. Initialises tracing, applies PHP ini
/// overrides, boots the embedded PHP runtime in single-threaded mode, then
/// hands off to the tokio-driven HTTP loop.
fn run_with_config(config: ephpm_config::Config, verbose: u8) -> anyhow::Result<ExitCode> {
    // Resolve log level: RUST_LOG > -v flag > config > "info"
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbose {
            0 => config.server.logging.level.as_str(),
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(level)
    });

    let fmt_layer = tracing_subscriber::fmt::layer();

    // Set up access log file writer if configured.
    let _access_guard = if config.server.logging.access.is_empty() {
        tracing_subscriber::registry().with(env_filter).with(fmt_layer).init();
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
        tracing_subscriber::registry().with(env_filter).with(fmt_layer).with(access_layer).init();
        Some(guard)
    };

    tracing::info!(
        listen = %config.server.listen,
        document_root = %config.server.document_root.display(),
        "starting ePHPm"
    );

    // Windows: extract the embedded php8embed.dll before PHP init.
    // Declared here so it drops after `rt` (Rust drops locals in reverse
    // declaration order — `rt` is declared later, so it drops first, which
    // ensures the tokio runtime is fully shut down before we delete the DLL).
    #[cfg(all(php_linked, target_os = "windows"))]
    let _dll_guard = ephpm_php::windows_dll::extract_php_dll()
        .context("failed to extract embedded php8embed.dll")?;

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
        config.server.sites_dir.is_some() && config.server.security.disable_shell_exec;
    let want_generated_ini = !config.php.ini_overrides.is_empty() || vhost_disable_shell;

    let (effective_ini_path, _generated_ini_guard): (Option<PathBuf>, Option<TempFileGuard>) =
        if want_generated_ini {
            use std::fmt::Write as _;

            let mut content = String::new();
            if let Some(base) = &config.php.ini_file {
                let base_content = std::fs::read_to_string(base).with_context(|| {
                    format!("failed to read php.ini file at {}", base.display())
                })?;
                content.push_str(&base_content);
                if !content.ends_with('\n') {
                    content.push('\n');
                }
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
            let temp_path =
                std::env::temp_dir().join(format!("ephpm-{}-overrides.ini", std::process::id()));
            std::fs::write(&temp_path, content).with_context(|| {
                format!("failed to write generated php.ini at {}", temp_path.display())
            })?;
            (Some(temp_path.clone()), Some(TempFileGuard::new(temp_path)))
        } else {
            (config.php.ini_file.clone(), None)
        };

    // Initialize PHP BEFORE creating tokio runtime (single-threaded here).
    // finalize_for_http() disables SIGPROF so it can't crash worker threads.
    ephpm_php::PhpRuntime::init_with_ini_file(effective_ini_path.as_deref())
        .context("failed to initialize PHP runtime")?;
    ephpm_php::PhpRuntime::finalize_for_http()
        .context("failed to finalize PHP runtime for HTTP")?;

    // Now safe to create the multi-threaded tokio runtime
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let result = rt.block_on(async { ephpm_server::serve(config).await });

    // Shutdown PHP runtime
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

    Ok((config, verbose))
}

// ─────────────────────────────────────────────────────────────────────────────
// KV Store CLI Subcommands
// ─────────────────────────────────────────────────────────────────────────────

/// Dispatcher for all KV subcommands.
async fn run_kv(host: &str, port: u16, sub: KvSubcommand) -> anyhow::Result<ExitCode> {
    match sub {
        KvSubcommand::Ping => kv_ping(host, port).await,
        KvSubcommand::Keys { pattern } => kv_keys(host, port, &pattern).await,
        KvSubcommand::Get { key } => kv_get(host, port, &key).await,
        KvSubcommand::Set { key, value, ttl } => kv_set(host, port, &key, &value, ttl).await,
        KvSubcommand::Del { keys } => kv_del(host, port, &keys).await,
        KvSubcommand::Incr { key, by } => kv_incr(host, port, &key, by).await,
        KvSubcommand::Ttl { key } => kv_ttl(host, port, &key).await,
    }
}

/// TCP connection helper.
async fn kv_connect(host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid address: {host}:{port}"))?;
    TcpStream::connect(addr)
        .await
        .with_context(|| format!("could not connect to KV server at {host}:{port}"))
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
async fn kv_roundtrip(host: &str, port: u16, cmd: Frame) -> anyhow::Result<Frame> {
    let mut stream = kv_connect(host, port).await?;
    kv_send(&mut stream, &cmd).await?;
    kv_recv(&mut stream).await
}

/// PING command.
async fn kv_ping(host: &str, port: u16) -> anyhow::Result<ExitCode> {
    let cmd = Frame::Array(vec![Frame::bulk(b"PING".to_vec())]);
    match kv_roundtrip(host, port, cmd).await? {
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
async fn kv_keys(host: &str, port: u16, pattern: &str) -> anyhow::Result<ExitCode> {
    let cmd =
        Frame::Array(vec![Frame::bulk(b"KEYS".to_vec()), Frame::bulk(pattern.as_bytes().to_vec())]);
    match kv_roundtrip(host, port, cmd).await? {
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
async fn kv_get(host: &str, port: u16, key: &str) -> anyhow::Result<ExitCode> {
    let cmd =
        Frame::Array(vec![Frame::bulk(b"GET".to_vec()), Frame::bulk(key.as_bytes().to_vec())]);
    match kv_roundtrip(host, port, cmd).await? {
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
    match kv_roundtrip(host, port, cmd).await? {
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
async fn kv_del(host: &str, port: u16, keys: &[String]) -> anyhow::Result<ExitCode> {
    let mut args = vec![Frame::bulk(b"DEL".to_vec())];
    for key in keys {
        args.push(Frame::bulk(key.as_bytes().to_vec()));
    }
    let cmd = Frame::Array(args);
    match kv_roundtrip(host, port, cmd).await? {
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
async fn kv_incr(host: &str, port: u16, key: &str, by: i64) -> anyhow::Result<ExitCode> {
    let cmd = if by == 1 {
        Frame::Array(vec![Frame::bulk(b"INCR".to_vec()), Frame::bulk(key.as_bytes().to_vec())])
    } else {
        Frame::Array(vec![
            Frame::bulk(b"INCRBY".to_vec()),
            Frame::bulk(key.as_bytes().to_vec()),
            Frame::bulk(by.to_string().into_bytes()),
        ])
    };
    match kv_roundtrip(host, port, cmd).await? {
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
async fn kv_ttl(host: &str, port: u16, key: &str) -> anyhow::Result<ExitCode> {
    let mut stream = kv_connect(host, port).await?;

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
}
