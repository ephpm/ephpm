+++
title = "MySQL Compatibility"
weight = 7
+++

With `[db.sqlite]` configured, PHP applications talk MySQL to ePHPm
(`pdo_mysql` / `mysqli` on `127.0.0.1:3306`) and litewire translates each
statement for the embedded Turso engine. This page lists what that
translation covers, what it does differently from MySQL, and how to move an
existing MySQL database over. The statement-by-statement reference is
litewire's [`docs/sql-translation.md`](https://github.com/ephpm/litewire/blob/main/docs/sql-translation.md).

The coverage below was checked against a real application. Sendy 7.1.6 is
a self-migrating PHP mail app written for MySQL 8.4. Its schema updater
was run against MySQL 8.4 and against ePHPm from the same starting schema,
and the resulting tables, columns and indexes were compared. About 1,000 of
its distinct statement shapes were also replayed on both, with the results
compared.

## Supported

**Schema probes.** These are answered from the SQLite catalog in MySQL's
column shape:

- `SHOW [FULL] TABLES`, `SHOW [FULL] COLUMNS|FIELDS`,
  `SHOW INDEX|INDEXES|KEYS`, `SHOW TABLE STATUS`, `SHOW CREATE TABLE`,
  `DESCRIBE t [col]`
- `information_schema.tables|columns|statistics|schemata`

Their `LIKE` and `WHERE` filters are applied. Self-migrating applications
decide whether to add a column or index from exactly these probes, for
example `SHOW INDEX FROM t WHERE Key_name = 'x'`. A `SHOW` form or filter
that is not supported (`SHOW VARIABLES`, `SHOW INDEX ... LIKE`, a system
schema) returns an **error**, never an unfiltered result.

**DDL:**

- `CREATE TABLE` with MySQL types, table and column options, inline
  `KEY` / `UNIQUE KEY` (created as real indexes), and charset and collation
  clauses (accepted and dropped).
- Multi-operation `ALTER TABLE` and `ADD (a ..., b ...)`.
- `ADD ... FIRST|AFTER`, `MODIFY`, `CHANGE`, and `ALTER COLUMN ...
  DEFAULT|NOT NULL`. SQLite cannot change a column in place, so ePHPm
  rebuilds the table in one transaction. Data, keys, indexes and triggers
  carry over, but the whole table is copied.
- `ADD/DROP INDEX|KEY` and `CREATE/DROP INDEX ... ON t`.
- `ALTER TABLE t ENGINE=|ROW_FORMAT=|CONVERT TO CHARACTER SET ...` alone is
  accepted and changes nothing.
- `TRUNCATE`.

**Index names** stay per-table, as in MySQL. Index `k` on table `t` is
stored as SQLite index `t__k` and reported as `k`.

**DML:**

- `INSERT ... ON DUPLICATE KEY UPDATE` and `INSERT IGNORE`.
- `INSERT ... SELECT ... FROM DUAL WHERE NOT EXISTS (...)`, which reports
  0 affected rows when the guard suppresses the insert.
- `UPDATE` / `DELETE ... LIMIT`.
- Multi-table `DELETE t1 FROM t t1 JOIN ...`, `UPDATE ... JOIN`, and
  `UPDATE a, b SET ...` (when each value reads its own table).
- `LIMIT offset, count` and `LIMIT 18446744073709551615`.
- Index hints (ignored).
- `FOR UPDATE`, `FOR SHARE` and `LOCK IN SHARE MODE` are accepted (see the
  differences below).
- `SQL_CALC_FOUND_ROWS` / `FOUND_ROWS()`.

**Expressions:**

- MySQL `"double-quoted"` strings, and `||` / `&&` as `OR` / `AND`.
- The implicit `LIKE` escape `\`.
- `IF`, `IFNULL`, `LEAST` / `GREATEST`, `CHAR_LENGTH`, and `CONCAT`.
- `NOW`, `UNIX_TIMESTAMP`, `FROM_UNIXTIME`, `DATE_ADD` / `DATE_SUB` /
  `± INTERVAL n unit`, and `YEAR` / `MONTH` / `DAY`.
- `FIND_IN_SET`, `SUBSTRING_INDEX` (count ±1), and `REGEXP` / `RLIKE`.

**Session:**

- `SET NAMES` and `SET time_zone` are accepted as no-ops.
- `SET sql_mode` is recorded and read back by `SELECT @@sql_mode` (see
  below).
- `GET_LOCK` / `RELEASE_LOCK`.
- `START TRANSACTION`, `COMMIT` and `ROLLBACK`. A `COMMIT` or `ROLLBACK`
  outside a transaction returns OK, as in MySQL.

## Behaviour differences

These are accepted but behave differently from MySQL. Each was measured
against MySQL 8.4.11.

| Area | MySQL 8.4 | ePHPm (Turso) |
|---|---|---|
| String comparison | `_ci` collations are case-insensitive | Binary (case-sensitive) for `=`, `UNIQUE`, `ORDER BY`. `LIKE` is case-insensitive for ASCII on both |
| `''` into an INT column (non-strict mode) | stored as `0` | stored as `''` (text). `n = 0` is false, `ORDER BY` puts it after every number, `SUM` / `n + 1` count it as 0 |
| `sql_mode` | enforced (default `ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,...`) | **reported, not enforced**. Default `''`. A session `SET sql_mode` is read back by `@@sql_mode` |
| Bare columns next to `GROUP BY` | error under `ONLY_FULL_GROUP_BY`; otherwise an indeterminate row | first row in scan order. With `MIN()`/`MAX()` in the same select, the rusqlite engine picks the min/max row and Turso does not. Don't rely on it |
| `SELECT ... FOR UPDATE` | row locks; a second claimer waits | no row locks (single writer). A second transaction writing the same row gets a busy error instead of waiting |
| Affected rows of an `UPDATE` that changes nothing | 0 (rows *changed*) | rows *matched* (as with `CLIENT_FOUND_ROWS`) |
| Affected rows of an `ON DUPLICATE KEY UPDATE` that updates | 2 | 1 |
| `TRUNCATE` affected rows | 0 | rows deleted |
| Date/time functions | session `time_zone` | UTC |
| `ON UPDATE CURRENT_TIMESTAMP` | column refreshed on every update | not emulated |
| `SHOW COLUMNS` `Type` | declared type (`varchar(100)`, `int`) | affinity (`longtext`, `bigint`, `double`, `longblob`) |
| `SHOW` / `information_schema` on a missing table | error 1146 | empty result |
| `SHOW DATABASES LIKE` / `WHERE` | supported | error (one database per connection) |
| Column order after a column add with no position | appended | appended (same). With `FIRST` / `AFTER`, the table is rebuilt to match |

Unsupported, and an error rather than silently wrong:

- Stored procedures, triggers in MySQL syntax, and events.
- `SHOW VARIABLES` / `SHOW STATUS` / `SHOW WARNINGS`.
- `information_schema` tables other than the four above.
- `ALTER COLUMN ... SET DATA TYPE`.
- `FROM_UNIXTIME` with a format argument, and `SUBSTRING_INDEX` with a
  count other than ±1.
- `UPDATE a, b` where a value reads the other table.

## Restoring a mysqldump

A MySQL 8.4 `mysqldump` restores statement by statement through the `mysql`
client or any MySQL driver connected to ePHPm:

```sh
mysqldump --hex-blob --single-transaction mydb > dump.sql
mysql -h 127.0.0.1 -P 3306 -u root mydb < dump.sql
```

- **Use `--hex-blob`** when the dump has BLOB or BINARY data. Without it,
  binary values are raw bytes inside string literals and the statement is
  not valid UTF-8.
- `/*!NNNNN ... */` wrappers (`SET @OLD_...`, `SET NAMES`, `DISABLE KEYS`)
  are skipped. **Views, triggers and routines that mysqldump wraps in
  `/*!50001 */` / `/*!50003 */` are skipped too**, and must be recreated by
  hand.
- `LOCK TABLES` / `UNLOCK TABLES` are accepted. Mixed `utf8mb3` / `utf8mb4`
  charsets and collations are accepted (all text is UTF-8).
- Inline keys become indexes, and `AUTO_INCREMENT` continues after the
  highest restored id.
- Extended INSERTs of several hundred KB are tested. Statements above 16 MiB
  are untested.
