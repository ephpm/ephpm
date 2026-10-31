//! `ephpm db shell` — an interactive REPL over a virtual host's embedded
//! database.
//!
//! The database transport, dialect handling, screening and formatting live in
//! the shared [`crate::db_console`] engine; this module adds the line editor and
//! the slash-prefixed meta-commands (`/dialect`, `/format`, `/tables`,
//! `/schema`, `/kv`, `/kv-put`, `/help`, `/quit`).
//!
//! **stdout vs stderr.** Query result sets, `/tables`/`/schema` output and `/kv`
//! replies go to **stdout**; the banner, the prompt, and status/feedback lines
//! go to **stderr**. A session can therefore be driven from a pipe and its
//! stdout asserted on without the prompt noise mixed in.
//!
//! SQL spans lines until a top-level `;`; a `/`-prefixed line is a meta-command
//! (only when no SQL is pending). End of input (Ctrl-D) or `/quit` exits.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ephpm_dbimport::Dialect;

use crate::db_console::{
    Console, ConsoleConfig, Format, format_rows, parse_dialect, split_statements,
};

/// One `ephpm db shell` invocation, assembled from the CLI flags.
pub struct ShellRequest {
    /// The vhost key whose database to open.
    pub site: String,
    /// The ephpm config file.
    pub config: PathBuf,
    /// The initial dialect (`sqlite` by default).
    pub dialect: Dialect,
    /// Whether `--dialect` was passed explicitly (vs the `sqlite` default). A
    /// default sqlite is auto-promoted to MySQL in live mode; an explicit one
    /// is refused there (see [`Console::connect`]).
    pub dialect_explicit: bool,
    /// Reject any statement that is not a pure read.
    pub read_only: bool,
    /// Offline-only, unscreened engine-maintenance mode.
    pub maintenance: bool,
}

/// A parsed REPL meta-command (everything after a leading `/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meta {
    /// `/dialect [sqlite|mysql|pg]` — show (no arg) or switch the dialect.
    Dialect(Option<String>),
    /// `/format [table|tsv|csv|json]` — show or set the output format.
    Format(Option<String>),
    /// `/tables` — list the user tables.
    Tables,
    /// `/schema <table>` — describe one table.
    Schema(String),
    /// `/kv <ARGS...>` — a RESP command passthrough to the live KV store.
    Kv(Vec<String>),
    /// `/kv-put '<key-template>' [value-col]` — pipe the last result set into KV.
    KvPut { template: String, value_col: Option<String> },
    /// `/help` — command help.
    Help,
    /// `/quit` / `/exit` — leave the REPL.
    Quit,
    /// An unrecognised `/command`.
    Unknown(String),
}

/// Run `ephpm db shell`.
///
/// # Errors
///
/// Returns an error only for a setup failure (see [`Console::connect`]). Once
/// the REPL is running, per-statement and per-command errors are printed and
/// the session continues.
pub fn run(req: ShellRequest) -> anyhow::Result<ExitCode> {
    let mut console = Console::connect(&ConsoleConfig {
        site: req.site,
        config_path: req.config,
        dialect: req.dialect,
        dialect_explicit: req.dialect_explicit,
        read_only: req.read_only,
        maintenance: req.maintenance,
    })?;

    let mut format = Format::Table;
    let mut last_rows = ephpm_dbimport::RowSet::empty();

    banner(&console, req.read_only, req.maintenance);

    let stdin = std::io::stdin();
    let lines = stdin.lock().lines();
    let mut pending = String::new();

    prompt(&console, false);
    for line in lines {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        };

        // A meta-command is only a meta-command at a statement boundary; mid
        // statement a '/' line is ordinary text (kept for the SQL buffer).
        if pending.trim().is_empty() && line.trim_start().starts_with('/') {
            pending.clear();
            match handle_meta(&mut console, &line, &mut format, &last_rows) {
                MetaResult::Continue => {}
                MetaResult::Quit => break,
            }
            prompt(&console, false);
            continue;
        }

        pending.push_str(&line);
        pending.push('\n');

        // Execute once the pending buffer carries a terminating ';'.
        if ends_statement(&pending) {
            run_buffer(&mut console, &pending, format, &mut last_rows);
            pending.clear();
            prompt(&console, false);
        } else {
            prompt(&console, true);
        }
    }

    // Flush any statement left without a trailing ';' at EOF.
    if !pending.trim().is_empty() {
        run_buffer(&mut console, &pending, format, &mut last_rows);
    }
    eprintln!();
    console.finish();
    Ok(ExitCode::SUCCESS)
}

/// Whether the outcome of a meta-command keeps the REPL running.
enum MetaResult {
    /// Keep looping.
    Continue,
    /// Leave the REPL.
    Quit,
}

/// Print the startup banner (to stderr).
fn banner(console: &Console, read_only: bool, maintenance: bool) {
    eprintln!();
    eprintln!("  ePHPm db shell — site {}", console.site());
    eprintln!("    connected: {} ({})", console.mode_label(), console.dialect().as_str());
    if read_only {
        eprintln!("    read-only: writes are refused");
    }
    if maintenance {
        eprintln!("    maintenance: statement screening is OFF (offline engine access)");
    }
    eprintln!("    type SQL ending in ';', or /help for commands, /quit to exit");
    eprintln!();
}

/// Write the prompt to stderr (`continuation` uses a shorter `-> ` prompt).
fn prompt(console: &Console, continuation: bool) {
    if continuation {
        eprint!("  -> ");
    } else {
        eprint!("{}", console.prompt());
    }
    let _ = std::io::stderr().flush();
}

/// Whether `buf` holds a complete, `;`-terminated statement — i.e. the text
/// after the last top-level `;` (one not inside a string literal or comment) is
/// only whitespace. Blank input counts as complete (nothing to run).
///
/// Quote-/comment-aware so a `;` inside a string (`SELECT ';'`) does not look
/// like a terminator.
fn ends_statement(buf: &str) -> bool {
    if buf.trim().is_empty() {
        return true;
    }
    let bytes = buf.as_bytes();
    let mut i = 0usize;
    let mut tail_start = None;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        if bytes.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b';' => {
                tail_start = Some(i + 1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    match tail_start {
        Some(start) => buf[start..].trim().is_empty(),
        None => false,
    }
}

/// Execute every statement in `buf`, printing result sets and remembering the
/// last non-empty one for `/kv-put`.
fn run_buffer(
    console: &mut Console,
    buf: &str,
    format: Format,
    last_rows: &mut ephpm_dbimport::RowSet,
) {
    for stmt in split_statements(buf) {
        match console.run_sql(&stmt) {
            Ok(rows) => {
                if rows.is_empty() {
                    eprintln!("OK");
                } else {
                    println!("{}", format_rows(&rows, format));
                    *last_rows = rows;
                }
            }
            Err(e) => eprintln!("error: {e:#}"),
        }
    }
}

/// Handle one meta-command line.
fn handle_meta(
    console: &mut Console,
    line: &str,
    format: &mut Format,
    last_rows: &ephpm_dbimport::RowSet,
) -> MetaResult {
    match parse_meta(line) {
        Meta::Quit => return MetaResult::Quit,
        Meta::Help => print_help(),
        Meta::Dialect(None) => println!("dialect: {}", console.dialect().as_str()),
        Meta::Dialect(Some(name)) => match parse_dialect(&name) {
            Some(d) => match console.set_dialect(d) {
                Ok(()) => eprintln!("dialect: {}", console.dialect().as_str()),
                Err(e) => eprintln!("error: {e:#}"),
            },
            None => eprintln!("error: unknown dialect {name:?} (want sqlite | mysql | pg)"),
        },
        Meta::Format(None) => println!("format: {}", format.as_str()),
        Meta::Format(Some(name)) => match Format::parse(&name) {
            Some(f) => {
                *format = f;
                eprintln!("format: {}", format.as_str());
            }
            None => eprintln!("error: unknown format {name:?} (want table | tsv | csv | json)"),
        },
        Meta::Tables => match console.list_tables() {
            Ok(rows) => println!("{}", format_rows(&rows, *format)),
            Err(e) => eprintln!("error: {e:#}"),
        },
        Meta::Schema(table) => match console.describe_table(&table) {
            Ok(rows) => println!("{}", format_rows(&rows, *format)),
            Err(e) => eprintln!("error: {e:#}"),
        },
        Meta::Kv(args) => match console.kv_command(&args) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("error: {e:#}"),
        },
        Meta::KvPut { template, value_col } => {
            match console.kv_put(last_rows, &template, value_col.as_deref()) {
                Ok(n) => eprintln!("/kv-put: wrote {n} key(s)"),
                Err(e) => eprintln!("error: {e:#}"),
            }
        }
        Meta::Unknown(cmd) => {
            eprintln!("error: unknown command /{cmd} (try /help)");
        }
    }
    MetaResult::Continue
}

/// Print the command help (to stdout — the user asked for it).
fn print_help() {
    println!(
        "commands:\n\
         \u{20}  /dialect [sqlite|mysql|pg]   show or switch the SQL dialect\n\
         \u{20}  /format  [table|tsv|csv|json] show or set the output format\n\
         \u{20}  /tables                      list the user tables\n\
         \u{20}  /schema <table>              describe one table\n\
         \u{20}  /kv <ARGS...>                run a RESP command against the live KV store\n\
         \u{20}  /kv-put '<key-tmpl>' [col]   write the last result set into the live KV\n\
         \u{20}  /help                        this help\n\
         \u{20}  /quit, /exit                 leave (Ctrl-D also works)\n\
         SQL ends at a ';'. /kv and /kv-put need live mode (a running server)."
    );
}

/// Parse a meta-command line (the leading `/` is stripped).
///
/// Tokenisation is shell-ish for `/kv-put`: a single- or double-quoted first
/// argument is read as one token (so a key template with spaces works), and the
/// remaining token, if any, is the value column.
#[must_use]
pub fn parse_meta(line: &str) -> Meta {
    let line = line.trim();
    let body = line.strip_prefix('/').unwrap_or(line);
    let (cmd, rest) = split_first_word(body);
    let rest = rest.trim();
    match cmd.to_ascii_lowercase().as_str() {
        "quit" | "exit" | "q" => Meta::Quit,
        "help" | "h" | "?" => Meta::Help,
        "dialect" | "d" => Meta::Dialect(opt(rest)),
        "format" | "f" => Meta::Format(opt(rest)),
        "tables" | "t" => Meta::Tables,
        "schema" | "s" => Meta::Schema(rest.to_string()),
        "kv" => Meta::Kv(tokenize(rest)),
        "kv-put" | "kvput" => {
            let tokens = tokenize(rest);
            let mut it = tokens.into_iter();
            let template = it.next().unwrap_or_default();
            let value_col = it.next();
            Meta::KvPut { template, value_col }
        }
        other => Meta::Unknown(other.to_string()),
    }
}

/// `None` for an empty string, else `Some(trimmed)`.
fn opt(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Split off the first whitespace-delimited word, returning `(word, rest)`.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// Split `s` into tokens, honoring single- and double-quoted spans (so a quoted
/// argument with spaces stays one token). A closing quote is optional at end of
/// input.
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_token = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                in_token = true;
                let quote = c;
                for q in chars.by_ref() {
                    if q == quote {
                        break;
                    }
                    cur.push(q);
                }
            }
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        tokens.push(cur);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_commands() {
        assert_eq!(parse_meta("/quit"), Meta::Quit);
        assert_eq!(parse_meta("/exit"), Meta::Quit);
        assert_eq!(parse_meta("/help"), Meta::Help);
        assert_eq!(parse_meta("/tables"), Meta::Tables);
        assert_eq!(parse_meta("/dialect"), Meta::Dialect(None));
        assert_eq!(parse_meta("/dialect mysql"), Meta::Dialect(Some("mysql".into())));
        assert_eq!(parse_meta("/format json"), Meta::Format(Some("json".into())));
        assert_eq!(parse_meta("/schema wp_posts"), Meta::Schema("wp_posts".into()));
        assert_eq!(parse_meta("/frobnicate"), Meta::Unknown("frobnicate".into()));
    }

    #[test]
    fn parses_kv_passthrough() {
        assert_eq!(
            parse_meta("/kv GET session:42"),
            Meta::Kv(vec!["GET".into(), "session:42".into()])
        );
        assert_eq!(
            parse_meta("/kv SET greeting \"hello world\""),
            Meta::Kv(vec!["SET".into(), "greeting".into(), "hello world".into()])
        );
    }

    #[test]
    fn parses_kv_put_with_quoted_template_and_value_col() {
        assert_eq!(
            parse_meta("/kv-put 'user:${id}' name"),
            Meta::KvPut { template: "user:${id}".into(), value_col: Some("name".into()) }
        );
        // No value column → whole-row JSON mode.
        assert_eq!(
            parse_meta("/kv-put 'user:${id}:${name}'"),
            Meta::KvPut { template: "user:${id}:${name}".into(), value_col: None }
        );
        // Unquoted single-word template works too.
        assert_eq!(
            parse_meta("/kv-put prefix value"),
            Meta::KvPut { template: "prefix".into(), value_col: Some("value".into()) }
        );
    }

    #[test]
    fn statement_termination_detection() {
        assert!(ends_statement("SELECT 1;"));
        assert!(ends_statement("SELECT 1;\n"));
        assert!(!ends_statement("SELECT 1"));
        assert!(!ends_statement("SELECT ';'"));
        assert!(ends_statement("SELECT ';';"));
        // A bare terminator / only comments is "complete" (nothing to run).
        assert!(ends_statement("   "));
    }

    #[test]
    fn tokenize_handles_quotes() {
        assert_eq!(tokenize("a b c"), vec!["a", "b", "c"]);
        assert_eq!(tokenize("'one two' three"), vec!["one two", "three"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
    }
}
