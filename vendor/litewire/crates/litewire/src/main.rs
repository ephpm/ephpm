//! litewire CLI binary.

use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(
    name = "litewire",
    version,
    about = "SQL protocol proxy for SQLite"
)]
struct Cli {
    /// SQLite database file path.
    #[arg(long, default_value = "litewire.db")]
    db: String,

    /// MySQL frontend listen address.
    #[arg(long, default_value = "127.0.0.1:3306")]
    mysql_listen: Option<String>,

    /// Hrana HTTP frontend listen address.
    #[arg(long)]
    hrana_listen: Option<String>,

    /// Disable MySQL frontend.
    #[arg(long)]
    no_mysql: bool,

    /// Disable Hrana HTTP frontend.
    #[arg(long)]
    no_hrana: bool,

    /// PostgreSQL frontend listen address.
    #[arg(long)]
    postgres_listen: Option<String>,

    /// Disable PostgreSQL frontend.
    #[arg(long)]
    no_postgres: bool,

    /// TDS (SQL Server) frontend listen address.
    #[arg(long)]
    tds_listen: Option<String>,

    /// Disable TDS frontend.
    #[arg(long)]
    no_tds: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,litewire=debug".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    let backend = litewire::backend::Rusqlite::open(&cli.db)?;
    info!(db = %cli.db, "Opened SQLite database");

    let mut builder = litewire::LiteWire::new(backend);

    if !cli.no_mysql {
        if let Some(ref addr) = cli.mysql_listen {
            builder = builder.mysql(addr);
        }
    }

    if !cli.no_hrana {
        if let Some(ref addr) = cli.hrana_listen {
            builder = builder.hrana(addr);
        }
    }

    #[cfg(feature = "postgres")]
    if !cli.no_postgres {
        if let Some(ref addr) = cli.postgres_listen {
            builder = builder.postgres(addr);
        }
    }

    #[cfg(feature = "tds")]
    if !cli.no_tds {
        if let Some(ref addr) = cli.tds_listen {
            builder = builder.tds(addr);
        }
    }

    // Run until Ctrl+C or a frontend error.
    tokio::select! {
        result = builder.serve() => result,
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down");
            Ok(())
        }
    }
}
