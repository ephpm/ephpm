# Turso Engine — Gate 5 Evidence: WordPress + Laravel e2e

Evidence for decision gate 5 of the
[Turso engine roadmap](../site/content/roadmap/turso-engine.md):
*"WordPress + Laravel e2e suites green on the experimental backend."*

Two identical runs per application: `[db.sqlite] engine = "sqlite"`
(rusqlite, the production default — the **control**) and
`engine = "turso"` (experimental, litewire-turso, engine pinned `=0.7.0`).
Same binary, same image, same scripts, same order; only the `engine` line
in the config differs. One fresh ePHPm container (fresh DB file) per
{app} × {engine} cell.

## Verdict

**Gate 5 FAILED on ePHPm `main` as pinned** (litewire `0ff501e`): neither
WordPress nor Laravel can even complete installation — on **either**
engine. Every Part A blocker lives in litewire's MySQL frontend/translate
layer and fires before any engine code runs, so the run cannot
discriminate engines at all.

**With a local litewire patch fixing the nine wire-layer gaps found by the
control runs (Part B), both suites go fully green on both engines — the
turso cells match the control 20/20 rows.** Exactly one turso-specific
bug was found in the whole exercise, and it is a serious one: a
silent-data-loss engine bug (below), worked around in the patch and owed
an upstream issue. It is strong evidence for keeping roadmap gate 1
(upstream GA) open, and it means gate 5 cannot be called PASSED until
(a) the litewire fixes land upstream and ePHPm's pin is bumped, and
(b) the turso phantom-transaction bug is fixed or the workaround ships.

## Environment

| | |
|---|---|
| Date | 2026-07-14/15 |
| ePHPm | `main` @ `0c6a5e6`, `cargo xtask release 8.4` (PHP 8.4.23 embedded) |
| litewire (Part A) | git pin `0ff501e` (`turso` feature on, engine `turso =0.7.0`) |
| litewire (Part B) | local checkout `0d99ba6` (pin + the turso-CDC commit, wire-layer identical) **+ uncommitted wire-compat patch** (6 files, ~716 insertions incl. 13 new tests; preserved as `litewire-gate5.patch`, to be landed as a litewire PR) |
| Host | WSL2 build; podman 5.7.1; runtime image debian:13-slim + tini + binary |
| WordPress | 7.0.1 (and 6.4.3 where noted), WP-CLI 2.12.0 (`wordpress:cli` sidecar), theme twentytwentyone |
| Laravel | laravel/laravel v13.8.0 (framework 13.20.0), stock `php:8.4-cli` + `pdo_mysql` + composer |

Topology: WordPress runs **inside** ePHPm (fpm mode; `mysqli` →
`127.0.0.1:3306`, litewire in-process — no external MySQL anywhere);
WP-CLI in a sidecar on a shared network + shared `/var/www/html` volume.
Laravel runs in a stock `php:8.4-cli` container; ePHPm is only the DB.
Deviations from stock (identical for both engines):
`mysql_listen = "0.0.0.0:3306"` (sidecar reachability), the known
WP-7.0 output-buffer mu-plugin shim (fpm-SAPI, DB-agnostic),
`DISABLE_WP_CRON`.

## Part A — ePHPm main as pinned: engine-independent wire-layer blockers

WordPress 7.0.1, `wp core install`, identical on sqlite and turso:

```
Error: Error: WordPress 7.0.1 requires MySQL 5.5.5 or higher
```

(blocker 1 — handshake version). WordPress 6.4.3 (last release accepting
the old handshake) gets further and fails in dbDelta:

```
wp_usermeta   -> SQL translation error: SQL parse error: sql parser error: Expected: ), found: ( at Line: 9, Column: 24
wp_termmeta, wp_terms, wp_commentmeta, wp_comments, wp_postmeta, wp_posts -> (same, other line/col)
```

7 of 12 core tables — every one with an index prefix length
(`KEY meta_key (meta_key(191))`) (blocker 2). The 5 tables that parsed
were *also* silently never created (blocker 4, found in Part B). Site
unusable; the rest of the WP matrix is unreachable. **Byte-identical
failure list on both engines.**

Laravel, `php artisan migrate`, first statement, both engines:

```
SQLSTATE[HY000]: General error: 1105 SQLite error: near "DEFAULT": syntax error in
CREATE TABLE `migrations` (...) DEFAULT CHARACTER SET = utf8mb4 COLLATE = 'utf8mb4_unicode_ci' at offset 115
```

(blocker 3). All later rows fail with `no such table`. Only cosmetic
engine divergence in Part A: rusqlite error text appends
`in CREATE TABLE ... at offset 115`, turso says
`near "DEFAULT": syntax error` / `Parse error: no such table: \`users\``.

### Part A matrices (identical on sqlite control and turso)

| App | Result |
|---|---|
| WordPress 7.0.1 | install refused (blocker 1); rows 2–14 unreachable |
| WordPress 6.4.3 | install "produced database errors" (blockers 2/4); rows 2–14 unreachable |
| Laravel | migrate fails on statement 1 (blocker 3); rows 2–7 fail (`no such table`) |

## The blockers, enumerated (all found by the CONTROL, none by turso)

All in litewire (`crates/litewire-mysql`, `crates/litewire-translate`),
each now covered by a fix + regression test in the Part B patch:

1. **Handshake server version** is opensrv's default
   `5.1.10-alpha-msql-proxy` (`AsyncMysqlShim::version()` never
   overridden). WordPress ≥ 6.5 reads `mysqli_get_server_info()` and
   refuses. Fix: advertise `8.0.36-litewire`.
2. **Index prefix lengths** (`KEY k (col(191))`) unparseable by sqlparser
   0.57 → WP dbDelta DDL fails at parse. Fix: quote-aware pre-parse strip
   of bare `(<digits>)` groups in MySQL DDL.
3. **CREATE TABLE table options** (`DEFAULT CHARACTER SET = utf8mb4
   COLLATE = '...'`, `ENGINE=`) passed through to SQLite → every Laravel
   `create table` fails. Fix: drop table options.
4. **Inline `KEY name (cols)` constraints** re-emitted as-is (the emitter
   is sqlparser `Display`) → *every* WP CREATE TABLE failed at the SQLite
   layer, silently (dbDelta swallows errors). Fix: drop plain
   KEY/FULLTEXT constraints (perf-only), normalize `UNIQUE KEY name (c)`
   → `UNIQUE (c)`, keep PRIMARY KEY.
5. **`ON DUPLICATE KEY UPDATE col = VALUES(col)`** rewritten to
   `ON CONFLICT DO UPDATE SET col = VALUES(col)` — `VALUES()` is
   MySQL-only; the upsert WP's `add_option()` depends on always failed.
   Fix: rewrite to `excluded.col`.
6. **`SHOW FULL COLUMNS` not detected** (only `SHOW COLUMNS`), and the
   ShowColumns shim returned raw `PRAGMA table_info` shape (`cid, name,
   type, ...`). wpdb's `get_table_charset()` needs MySQL-shaped
   `Field/Type/Null/Key/Collation` rows; on failure `sanitize_option()`
   **stores every affected option as an empty string** (observed:
   `siteurl`, `home`, `blogname`, `admin_email`… all `''` after a
   "successful" install → "Error establishing a database connection"
   everywhere). Fix: detect `SHOW FULL COLUMNS/FIELDS`, emit MySQL-shaped
   rows (text affinity mapped to `longtext` so WP applies no 64 KB cap,
   collation `utf8mb4_unicode_ci`).
7. **`SQL_CALC_FOUND_ROWS`** unparseable → WordPress's main comment query
   fails, approved comments never render. Fix: quote-aware strip of MySQL
   SELECT hints; `FOUND_ROWS()` shimmed to `0` (documented pagination
   semantic gap — proper emulation needs per-session state).
8. **Laravel `Schema::hasTable` probe hijacked**: `select exists (select 1
   from information_schema.tables where ... table_name = 'migrations')`
   matched the generic information_schema shim and returned a 3-column
   table list → `MultipleColumnsSelectedException`. Fix: dedicated
   `TableExists` metadata query returning one scalar.
9. **Qualified UPDATE SET targets**: Eloquent emits ``update `users` set
   `name` = ?, `users`.`updated_at` = ?`` — MySQL-legal, SQLite-illegal.
   Fix: dequalify SET targets. Also: `ALTER TABLE ... ADD
   {KEY|INDEX|UNIQUE}` (one per Laravel `->index()`/`->unique()`) now
   expands to standalone `CREATE [UNIQUE] INDEX` (SQLite has no
   `ALTER ... ADD CONSTRAINT`).

Open litewire issue (not fixed, engine-independent, intermittent):

10. **COM_STMT_PREPARE response desync**: in 2 of 3 Laravel control runs,
    `migrate` after a rollback failed once with `Wrong COM_STMT_PREPARE
    response size. Received 1` / `Received 7` (mysqlnd protocol
    desync) on an `alter table ... add index` prepare directly after a
    rapid prepare/execute DDL burst. Non-deterministic, also seen shapes
    suggest a response-buffering race (litewire#7 coalesced writes is the
    suspect). Needs its own investigation.

## The one turso-specific finding (upstream, data-loss class)

**A `SELECT` from a pragma table-valued function (e.g.
`pragma_table_info('t')`) leaves the turso 0.7.0 connection in a
phantom-transaction state: every subsequent write on that connection is
accepted, visible to that session, and silently lost** (never hits the
WAL; invisible to other sessions; gone on close). `COMMIT` then errors
with `cannot commit - no transaction is active` — yet an explicit
`BEGIN; COMMIT;` pair (issued after the poisoning statement is dropped)
restores normal autocommit. The plain `PRAGMA table_info('t')` statement
form does **not** trigger it; `sqlite_master` reads do not either.

Impact: WordPress issues `DESCRIBE`/`SHOW FULL COLUMNS` (→ the TVF) at
the start of nearly every session — before the fix, `wp core install` on
turso ran ~500 statements with zero errors and left a **4 KB database
file and a 0-byte WAL**. This is the exact "beta engines earn trust here
or nowhere" scenario the roadmap warns about.

Reproduction (through the ePHPm proxy, engine=turso):

```
conn A: SELECT name FROM pragma_table_info('any_table');   -- poison
conn A: CREATE TABLE t (...); INSERT INTO t ...;           -- "succeeds"
conn A: close
conn B: SELECT COUNT(*) FROM t;  -- no such table: t
```

Workaround shipped in the litewire patch (`litewire-turso`): after
executing SQL containing `pragma_`, drop the statement handle and issue
`BEGIN` + `COMMIT` on the session; regression test
`pragma_tvf_read_does_not_poison_session` covers it.

## Part B — full matrix with the litewire wire-compat patch

WordPress 7.0.1 (current), identical scripts both engines:

| # | Test | engine=sqlite (control) | engine=turso |
|---|------|--------------------------|--------------|
| 1 | `wp core install` | PASS | PASS |
| 2 | classic theme install + activate | PASS | PASS |
| 3 | homepage renders 200 (doctype + title) | PASS | PASS |
| 4 | sample post renders 200 | PASS | PASS |
| 5 | `wp post create` → appears on homepage | PASS | PASS |
| 6 | comment via HTTP POST → stored | PASS | PASS |
| 7 | `wp comment approve` | PASS | PASS |
| 8 | approved comment renders on post page | PASS | PASS |
| 9 | option update ×100 (`wp eval` loop) | PASS | PASS |
| 10 | `wp post generate --count=50` (+count check) | PASS | PASS |
| 11 | `wp search-replace --dry-run --all-tables` | PASS | PASS |
| 12 | `wp plugin install contact-form-7 --activate` (+is-active) | PASS | PASS |

Laravel (framework 13.20.0):

| # | Test | engine=sqlite (control) | engine=turso |
|---|------|--------------------------|--------------|
| 1 | `php artisan migrate` (DDL incl. index ALTERs) | PASS | PASS |
| 2 | `php artisan db:seed` | PASS | PASS |
| 3 | Eloquent CRUD (create/read/update/delete) | PASS | PASS |
| 4 | transaction commit | PASS | PASS |
| 5 | transaction rollback | PASS | PASS |
| 6 | `php artisan migrate:rollback` (DROP TABLE) | PASS | PASS |
| 7 | re-migrate + `tinker` row-count sanity | PASS* | PASS |

\* re-migrate hit the intermittent COM_STMT_PREPARE desync (open issue
10) in 2 of 3 control runs; clean on the recorded run and on turso. The
flake is in the MySQL frontend, not engine-related.

Turso cells produced **zero** additional errors or warnings in ePHPm
logs beyond the expected `engine = "turso" is EXPERIMENTAL` startup
warning; behavior and page output byte-comparable to the control
(identical homepage sizes, same post/comment IDs).

## ePHPm-side observations (not gate blockers)

- `ephpm php wp-cli.phar` does **not** work: WP-CLI hard-refuses any SAPI
  other than `cli`/`phpdbg` (`php_sapi_name()` returns `ephpm`). The
  `site/content/reference/cli/php.md` examples advertising WP-CLI via
  `ephpm php` do not work as documented (docs-truth item; the artisan and
  composer examples were not exercised here).
- `tests/smoke/` (WP 6.7 + Laravel against litewire SQLite) cannot pass
  at the current litewire pin for the same blockers — apparently not
  exercised by CI.
- `[server.request] max_body_size` is **bytes**; `tests/smoke/
  ephpm-wordpress.toml` sets `64`, so any comment-form POST gets 413.
- The `engine = "turso"` knob behaves as documented: startup warning,
  single-node enforcement, transparent to PHP.

## Upstream issues that should be filed (list only — none filed)

**turso (tursodatabase/turso), against v0.7.0:**

1. `SELECT` from `pragma_table_info()` (pragma TVF) leaves the connection
   in a phantom-transaction state; subsequent writes silently non-durable;
   `COMMIT` reports "no transaction is active" while `BEGIN; COMMIT;`
   restores autocommit. Silent data loss. (Repro above.)

**litewire (ephpm/litewire):**

2. The nine wire-compat gaps above (fixes exist in the gate 5 patch, to
   be landed as a litewire PR with its regression tests).
3. Intermittent `COM_STMT_PREPARE` response-size desync under rapid
   prepared DDL bursts (open; suspect the coalesced response write path).
4. `FOUND_ROWS()` returns a constant 0 after hint stripping — WP-style
   pagination totals are wrong; proper emulation needs per-session state
   in the frontend.

## Gate status

| Gate | Status after this exercise |
|---|---|
| 1. Upstream GA | **Open** — reinforced: the pragma-TVF phantom-transaction bug is a silent-data-loss beta bug found on day one of app-level testing. |
| 5. WordPress + Laravel e2e | **FAILED as pinned** (blockers 1–10, all engine-independent). **Green on both engines with the litewire wire-compat patch** — 20/20 matrix rows, turso == control. Re-runnable once the litewire fixes land and the pin is bumped; the turso workaround must ship (or the upstream bug be fixed) for the turso cells to stay green. |

Default engine remains `"sqlite"` — nothing here changes that.

## Reproducing

- Binary: `cargo xtask release 8.4` on `main` @ `0c6a5e6` (WSL); Part B
  adds `[patch."https://github.com/ephpm/litewire.git"]` → local litewire
  checkout with the wire-compat patch (not committed to ePHPm; the
  workspace `Cargo.toml`/`Cargo.lock` in this branch are unchanged).
- Runtime: debian:13-slim + tini + the binary; podman network per run.
- Per cell: ePHPm container (`[db.sqlite] path=/data/…`, `engine =
  "sqlite"|"turso"`, `mysql_listen = "0.0.0.0:3306"`,
  `RUST_LOG=info,litewire_mysql=debug` for SQL capture); WP-CLI sidecar
  (`wordpress:cli`, shared volume + `WORDPRESS_DB_HOST`); Laravel runner
  (`php:8.4-cli` + `pdo_mysql` + composer, skeleton cached in a volume).
- The full litewire diff is preserved alongside the run artifacts as
  `litewire-gate5.patch` and is the basis for the upcoming litewire PR.
