use std::path::PathBuf;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Default to `serve` if no subcommand given
    let Commands::Serve {
        config,
        listen,
        document_root,
    } = cli.command.unwrap_or(Commands::Serve {
        config: PathBuf::from("ephpm.toml"),
        listen: None,
        document_root: None,
    });

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

    // Initialize PHP runtime
    ephpm_php::PhpRuntime::init().context("failed to initialize PHP runtime")?;

    // Start HTTP server (blocks until shutdown signal)
    let result = ephpm_server::serve(config).await;

    // Shutdown PHP runtime
    ephpm_php::PhpRuntime::shutdown().context("failed to shutdown PHP runtime")?;

    result
}
