use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// ePHPm — All-in-one PHP application server
#[derive(Parser, Debug)]
#[command(name = "ephpm", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the PHP application server (default)
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
    },

    /// Run PHP CLI commands using the embedded PHP runtime
    #[command(disable_help_flag = true)]
    Php {
        /// Arguments to pass to the PHP interpreter
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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
        other => run_serve_sync(other),
    }
}

/// Run the `ephpm php` subcommand — pass args through to the embedded PHP CLI.
fn run_php(args: &[String]) -> anyhow::Result<ExitCode> {
    let exit_code = ephpm_php::PhpRuntime::cli_main(args)
        .context("PHP CLI failed")?;
    let _ = ephpm_php::PhpRuntime::shutdown();
    Ok(exit_code_from(exit_code))
}

/// Convert a PHP exit code (i32) to a Rust `ExitCode`.
fn exit_code_from(code: i32) -> ExitCode {
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(u8::try_from(code).unwrap_or(1))
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
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = load_serve_config(command)?;

    // Initialize PHP BEFORE creating tokio runtime (single-threaded here).
    // finalize_for_http() disables SIGPROF so it can't crash worker threads.
    ephpm_php::PhpRuntime::init().context("failed to initialize PHP runtime")?;
    ephpm_php::PhpRuntime::finalize_for_http()
        .context("failed to finalize PHP runtime for HTTP")?;

    // Now safe to create the multi-threaded tokio runtime
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let result = rt.block_on(async {
        ephpm_server::serve(config).await
    });

    // Shutdown PHP runtime
    ephpm_php::PhpRuntime::shutdown().context("failed to shutdown PHP runtime")?;

    result.map(|()| ExitCode::SUCCESS)
}

/// Parse the Serve command and load configuration.
fn load_serve_config(command: Option<Commands>) -> anyhow::Result<ephpm_config::Config> {
    let Commands::Serve {
        config,
        listen,
        document_root,
    } = command.unwrap_or(Commands::Serve {
        config: PathBuf::from("ephpm.toml"),
        listen: None,
        document_root: None,
    }) else {
        unreachable!("load_serve_config called with non-Serve command");
    };

    let mut config = if config.exists() {
        tracing::info!(path = %config.display(), "loading configuration");
        ephpm_config::Config::load(&config).context("failed to load configuration")?
    } else {
        tracing::info!("no config file found, using defaults");
        ephpm_config::Config::default_config()?
    };

    // CLI overrides take precedence
    if let Some(addr) = listen {
        config.server.listen = addr;
    }
    if let Some(root) = document_root {
        config.server.document_root = root;
    }

    tracing::info!(
        listen = %config.server.listen,
        document_root = %config.server.document_root.display(),
        "starting ePHPm"
    );

    Ok(config)
}
