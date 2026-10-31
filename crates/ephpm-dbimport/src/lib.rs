//! Dump-file sanitizer and compatibility reporter for `ephpm db import`.
//!
//! This crate turns a `mysqldump` or `pg_dump` SQL file into a stream of
//! statements an ePHPm per-site (Turso) database can accept, and reports —
//! honestly — everything SQLite/Turso cannot represent. It is deliberately
//! **pure**: no PHP, no network, no filesystem beyond the `BufRead` the caller
//! hands it. The only outbound edge is the [`StatementSink`] trait, so the
//! whole pipeline is exercised by fixture unit tests with an in-memory sink.
//!
//! # What this crate does and does not do
//!
//! It handles dump **file-format / dialect-shape** concerns only:
//!
//! * [`Dialect`] + [`sniff`] — decide MySQL vs PostgreSQL from a file header.
//! * [`DumpReader`] — a *streaming* statement splitter that respects quoting,
//!   comments, backtick identifiers, `DELIMITER` changes, and the Postgres
//!   `COPY … FROM stdin` … `\.` data block. It reads incrementally and never
//!   loads the whole file.
//! * [`Sanitizer`] — strips `/*! */` version gates, drops `LOCK TABLES` and
//!   session-setup `SET`s, strips unsupported `CREATE TABLE` options, rewrites
//!   `ENUM`/`SET` columns to `TEXT`, converts `COPY FROM stdin` to batched
//!   `INSERT`s, and classifies stored routines / triggers / events / sequences
//!   / full-text indexes as unsupported or downgraded.
//! * [`CompatReport`] — the ordered list of [`Finding`]s, with a [`summary`]
//!   and a JSON serialization.
//!
//! The actual MySQL/PostgreSQL → SQLite **type and dialect translation** is
//! *not* this crate's job — the sanitized statements are fed to litewire's
//! translating `Session` (or forwarded to a running server's wire listener)
//! by the `ephpm` binary. Keeping translation out of here is what lets the
//! sanitizer stay a pure, dependency-light, fully unit-tested library.
//!
//! [`summary`]: CompatReport::summary

mod dialect;
mod driver;
mod reader;
mod report;
mod sanitizer;
mod sink;

pub use dialect::{Dialect, sniff};
pub use driver::{ImportOptions, ImportOutcome, ImportStats, OnError, run_import};
pub use reader::{DumpReader, RawStatement, ReadError};
pub use report::{CompatReport, Finding, FindingKind, Summary};
pub use sanitizer::{SanitizeOutcome, Sanitizer};
pub use sink::{SinkError, StatementSink};
