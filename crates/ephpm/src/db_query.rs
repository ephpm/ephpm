//! `ephpm db query` — run one SQL statement (or a `;`-separated batch) against a
//! virtual host's embedded database and print the result set.
//!
//! The transport, dialect, screening and formatting all live in the shared
//! [`crate::db_console`] engine; this module is the one-shot front end: it reads
//! the SQL (a positional argument, `-e <SQL>`, or stdin), runs each statement,
//! prints each non-empty result set in the chosen `--format`, and returns a
//! non-zero exit code on the first failure.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};
use ephpm_dbimport::Dialect;

use crate::db_console::{Console, ConsoleConfig, Format, format_rows, split_statements};

/// One `ephpm db query` invocation, assembled from the CLI flags.
pub struct QueryRequest {
    /// The vhost key whose database to query.
    pub site: String,
    /// The ephpm config file.
    pub config: PathBuf,
    /// The dialect to speak (`sqlite` by default).
    pub dialect: Dialect,
    /// Output format.
    pub format: Format,
    /// Reject any statement that is not a pure read.
    pub read_only: bool,
    /// Offline-only, unscreened engine-maintenance mode.
    pub maintenance: bool,
    /// The SQL: a positional argument / `-e` value, or `None` to read stdin.
    pub sql: Option<String>,
}

/// Run `ephpm db query`.
///
/// # Errors
///
/// Returns an error when there is no SQL to run, the transport cannot be
/// established (see [`Console::connect`]'s fail-closed rules), or a statement is
/// refused or rejected by the backend.
pub fn run(req: QueryRequest) -> anyhow::Result<ExitCode> {
    let sql = match req.sql {
        Some(s) => s,
        None => read_stdin()?,
    };
    let statements = split_statements(&sql);
    if statements.is_empty() {
        bail!("no SQL to run — pass it as an argument, with -e <SQL>, or on stdin");
    }

    let mut console = Console::connect(&ConsoleConfig {
        site: req.site,
        config_path: req.config,
        dialect: req.dialect,
        read_only: req.read_only,
        maintenance: req.maintenance,
    })?;

    let result = run_statements(&mut console, &statements, req.format);
    // Always truncate the offline WAL before returning, success or not —
    // whatever committed is in the log (the SDK never checkpoints on drop).
    console.finish();
    result
}

/// Run each statement in order, printing non-empty result sets separated by a
/// blank line. A statement error stops the batch (nothing past it runs).
fn run_statements(
    console: &mut Console,
    statements: &[String],
    format: Format,
) -> anyhow::Result<ExitCode> {
    let mut printed = false;
    for stmt in statements {
        let rows =
            console.run_sql(stmt).with_context(|| format!("running: {}", first_line(stmt)))?;
        if rows.is_empty() {
            continue;
        }
        if printed {
            println!();
        }
        println!("{}", format_rows(&rows, format));
        printed = true;
    }
    Ok(ExitCode::SUCCESS)
}

/// The first line of a statement, for an error-context label (keeps a
/// multi-line statement from flooding the message).
fn first_line(stmt: &str) -> &str {
    stmt.lines().next().unwrap_or(stmt)
}

/// Read all of stdin as the SQL to run.
fn read_stdin() -> anyhow::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context("reading SQL from stdin")?;
    Ok(buf)
}
