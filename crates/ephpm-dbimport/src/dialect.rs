//! Dump-dialect detection.
//!
//! [`sniff`] reads a header heuristic off the first kibibyte of a dump so the
//! importer can pick the right statement-splitter escaping rules and the right
//! litewire translation dialect without the operator naming `--dialect`.

/// The SQL dialect a dump was produced in.
///
/// This selects the [`DumpReader`](crate::DumpReader)'s string-escaping rules
/// (MySQL treats `\` as an escape inside string literals and allows
/// double-quoted strings and backtick identifiers; PostgreSQL does neither in
/// standard strings) and the litewire translation dialect the `ephpm` binary
/// feeds the sanitized statements to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// A `mysqldump` file (MySQL / MariaDB).
    MySql,
    /// A `pg_dump` file (PostgreSQL).
    Pg,
    /// SQLite / Turso — the embedded engine's own dialect.
    ///
    /// Never produced by [`sniff`] (no dump tool emits a SQLite banner here)
    /// and never used by `db import` (which only loads MySQL/PostgreSQL dumps).
    /// It exists for the interactive `ephpm db query` / `ephpm db shell`
    /// consoles, whose default dialect is the engine's native tongue: a
    /// `sqlite` statement is run straight against the Turso engine with **no**
    /// litewire translation, whereas `mysql`/`pg` go through the translating
    /// [`Session`](https://docs.rs/litewire).
    Sqlite,
}

impl Dialect {
    /// The lowercase name used on the CLI and in diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MySql => "mysql",
            Self::Pg => "pg",
            Self::Sqlite => "sqlite",
        }
    }
}

/// Guess the dialect of a dump from its leading bytes (pass at least the first
/// kibibyte). Returns `None` when no header marker matches, in which case the
/// caller must fall back to an explicit `--dialect`.
///
/// The markers are the stable preambles both tools emit:
///
/// * MySQL — `-- MySQL dump`, the `/*!40101` version-gate family, the
///   `/*M!` MariaDB variant, or a backtick-quoted identifier in the DDL.
/// * PostgreSQL — `-- PostgreSQL database dump`, `SET statement_timeout`,
///   `SELECT pg_catalog.set_config`, or a `COPY … FROM stdin` block.
///
/// Detection is order-independent and case-sensitive to the exact strings the
/// tools write (both emit fixed ASCII preambles), so a value *inside* the data
/// — e.g. a row whose text happens to contain `COPY` — cannot flip the verdict
/// as long as a real header marker appears earlier, which it always does.
#[must_use]
pub fn sniff(head: &[u8]) -> Option<Dialect> {
    let text = String::from_utf8_lossy(head);

    // PostgreSQL markers first: a pg_dump header is unambiguous and never
    // contains a backtick, whereas the MySQL backtick fallback is broad.
    const PG_MARKERS: [&str; 4] = [
        "-- PostgreSQL database dump",
        "SET statement_timeout",
        "SELECT pg_catalog.set_config",
        "FROM stdin;",
    ];
    if PG_MARKERS.iter().any(|m| text.contains(m)) {
        return Some(Dialect::Pg);
    }

    const MYSQL_MARKERS: [&str; 4] = ["-- MySQL dump", "/*!40", "/*M!", "-- Server version"];
    if MYSQL_MARKERS.iter().any(|m| text.contains(m)) || text.contains('`') {
        return Some(Dialect::MySql);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_a_mysqldump_header() {
        let head = b"-- MySQL dump 10.13  Distrib 8.0.36\n-- \n/*!40101 SET @x=@@y */;\n";
        assert_eq!(sniff(head), Some(Dialect::MySql));
    }

    #[test]
    fn sniffs_a_mariadb_gate() {
        assert_eq!(sniff(b"/*M!999999\\- enable the sandbox mode */\n"), Some(Dialect::MySql));
    }

    #[test]
    fn sniffs_backtick_ddl_without_a_banner() {
        assert_eq!(sniff(b"CREATE TABLE `wp_posts` (\n  `ID` bigint\n);\n"), Some(Dialect::MySql));
    }

    #[test]
    fn sniffs_a_pg_dump_header() {
        let head = b"--\n-- PostgreSQL database dump\n--\nSET statement_timeout = 0;\n";
        assert_eq!(sniff(head), Some(Dialect::Pg));
    }

    #[test]
    fn sniffs_pg_from_a_copy_block() {
        assert_eq!(sniff(b"COPY public.users (id, name) FROM stdin;\n"), Some(Dialect::Pg));
    }

    #[test]
    fn returns_none_for_an_ambiguous_header() {
        assert_eq!(sniff(b"INSERT INTO t VALUES (1);\n"), None);
    }

    #[test]
    fn dialect_names_are_stable() {
        assert_eq!(Dialect::MySql.as_str(), "mysql");
        assert_eq!(Dialect::Pg.as_str(), "pg");
        assert_eq!(Dialect::Sqlite.as_str(), "sqlite");
    }

    #[test]
    fn sniff_never_returns_sqlite() {
        // No dump tool emits a SQLite banner, so `sniff` must never produce the
        // console-only `Sqlite` variant — `db import`'s auto path would then
        // feed it to a MySQL/PG translator. Exercise both real banners plus the
        // ambiguous fallback.
        assert_ne!(sniff(b"-- MySQL dump 10.13\n"), Some(Dialect::Sqlite));
        assert_ne!(sniff(b"-- PostgreSQL database dump\n"), Some(Dialect::Sqlite));
        assert_eq!(sniff(b"INSERT INTO t VALUES (1);\n"), None);
    }
}
