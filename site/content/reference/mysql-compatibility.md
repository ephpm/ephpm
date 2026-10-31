+++
title = "MySQL Compatibility: Named Locks"
weight = 8
+++

The embedded database speaks the MySQL wire protocol (through
[litewire](https://github.com/ephpm/litewire)) over the Turso engine, so
`pdo_mysql`, `mysqli` and every ORM built on them connect to it unchanged.
This page documents one MySQL feature whose semantics are *not* derivable
from "it's SQLite underneath": **named (user-level) locks** —
`GET_LOCK()`, `RELEASE_LOCK()`, `RELEASE_ALL_LOCKS()`, `IS_FREE_LOCK()`,
`IS_USED_LOCK()` — and the `DO <expr>` statement they are usually issued
through. Everything here is pinned by tests in litewire and ePHPm; where a
behaviour differs from MySQL 8, it says so.

Why these matter: cron-style workers use them to make sure a job runs once.
Sendy, for example, takes `SELECT GET_LOCK("sendy_campaign_send_<id>", 0) AS
lock_status` before sending a campaign and `DO RELEASE_LOCK(...)` after, and
relies on the lock disappearing if the worker dies mid-send.

## Semantics

| | ePHPm | Same as MySQL 8? |
|---|---|---|
| `GET_LOCK(name, timeout)` | `1` obtained, `0` timed out, `NULL` for a `NULL` timeout | yes |
| `timeout = 0` | one attempt, never waits | yes |
| `timeout > 0` | waits up to that many seconds; **woken by the release**, not polled | yes |
| `timeout < 0` | waits until obtained | yes (no deadlock detection — see below) |
| Re-entrancy | a session may take the same name repeatedly; it holds a count | yes |
| `RELEASE_LOCK(name)` | `1` released one acquisition, `0` held by another session (nothing released), `NULL` nobody holds it | yes |
| `RELEASE_ALL_LOCKS()` | the number of acquisitions released (a name held three times counts three) | yes |
| `IS_FREE_LOCK(name)` | `1` / `0` | yes |
| `IS_USED_LOCK(name)` | the holder's session id, or `NULL` | yes — the id is litewire's own per-connection counter, and a bare `SELECT CONNECTION_ID()` reports that same id, so the two agree as MySQL guarantees |
| `CONNECTION_ID()` | the connection's own session id (non-zero) | yes for a bare `SELECT CONNECTION_ID()`; a connection with no lock scope reports `0`, and the call embedded in a larger expression still rewrites to `0` |
| Lock names | case-insensitive (lowercased), at most 64 characters | yes |
| Empty / `NULL` name | error — `ER_USER_LOCK_WRONG_NAME` (3057) in the bridge, `ER_WRONG_ARGUMENTS` (1210) over the wire (the wire library's error table predates 3057) | code differs on the wire |
| Name over 64 characters | error — 3938 in the bridge, 1210 over the wire | code differs on the wire |
| Transactions | `COMMIT` / `ROLLBACK` do not touch named locks | yes |
| Release on disconnect | **released when the holding connection ends, however it ends** (clean `QUIT`, TCP reset after a killed process, read timeout, server-side error); `COM_RESET_CONNECTION` / `COM_CHANGE_USER` release too | yes |
| Deadlock detection | none: two sessions waiting on each other with negative timeouts wait forever. Use finite timeouts when taking more than one lock | MySQL raises `ER_USER_LOCK_DEADLOCK` |
| Evaluation | **once per statement, in source order**, before the statement runs. `SELECT GET_LOCK('x', 0) FROM t` takes the lock once | MySQL evaluates per row |

Lock functions work in any `SELECT` — aliased, alongside other columns, in
`WHERE` — and in prepared statements with `?` arguments
(`SELECT GET_LOCK(?, ?)`). An unaliased call keeps MySQL's column name, so
`SELECT GET_LOCK('x', 0)` returns a column named `GET_LOCK('x', 0)`. Arguments
must be literals or placeholders; `GET_LOCK(CONCAT(...), 0)` is a statement
error. `DO expr [, expr]` evaluates and discards, answering OK.

A negative timeout means a connection — and under the `ephpm_db_*` bridge a
PHP worker thread — waits as long as the lock is held. The `[server.timeouts]
request` ceiling still applies to the request, exactly as it would to any
slow statement.

## Scope: one lock table per site

MySQL's lock namespace is server-wide. ePHPm serves many tenants from one
process, so the namespace is **the site**:

- **Single-node, one database** (`[db.sqlite]` without `sites_dir`): one
  namespace. Every connection to the database — `pdo_mysql` and the
  `ephpm_db_*` bridge alike — shares one lock table.
- **Per-site databases** (`[server] sites_dir` + `[db.sqlite] dir`): one
  namespace **per virtual host**, derived from the connection's
  authenticated site key (the `DB_USER` it proved it holds the password
  for), never from anything the client claims. A tenant's `GET_LOCK('job',
  0)` excludes that tenant's other connections and is invisible to — and
  cannot block — every other tenant's `job`. `IS_FREE_LOCK` / `IS_USED_LOCK`
  likewise answer only about the asking site's locks.

Both routes into a site's database resolve to the same lock table: a site's
`pdo_mysql` connections, its bridge sessions, and (clustered) the statements
forwarded to it all see one set of locks.

## Clustered mode

### Per-site clustered (`[db.sqlite.replication] per_site = true`, experimental)

A site's statements run on the node that **owns** it by rendezvous hashing;
every other node forwards over the cluster channel. Named locks are session
state, so they live **on the owner**: a forwarded `GET_LOCK` is carried over
the connection's pinned forwarding stream and taken by the owner-side session
that stream holds. Two connections for one site therefore exclude each other
whichever node they landed on — the owner's local connections and every
forwarded one share one lock table, keyed by site.

Release is the stream's lifetime. The owner drops the stream's session — and
its locks — when the forwarding stream ends on any path: the client's
connection closed, the client's **node died** (TCP keepalive / reset), the
owner's 10-minute idle timeout, a transport error. A holder never has to send
a release it can no longer send.

**Ownership moves** (a node joins or dies) do not carry locks: the old owner's
sessions keep theirs until they end, the new owner starts with an empty table
for the site. What stops two holders is that the old owner refuses a pinned
stream once it is no longer the owner — it re-checks ownership at most every
second and **before every lock operation**, and on loss drops the stream's
session (releasing its locks) and answers with a connection error flagged
closed, so the holder's next statement fails and it must reconnect to the new
owner, where it has to take the lock again. The window this leaves: a holder
that issues **no** statement across the move keeps believing it holds a lock
the new owner may grant to someone else until its next statement fails.
Side effects performed in that window are not protected — no lock survives
its server, and MySQL has the same window across a failover. An owner that
dies takes every lock for its sites with it; clients get a connection error on
their next statement and re-resolve.

### Whole-database clustered (`[db.sqlite]` + `[cluster]`, no `per_site`)

There is no statement forwarding in this mode: a replica's wire frontend
serves its local copy, and writes made there are already documented as
unreplicated. A lock granted on a replica would be node-local, so a **replica
refuses every lock operation** with an error:

```
named locks (GET_LOCK and friends) are served by the primary only in clustered
mode and this node is a replica — point the client at the primary …
```

The elected primary grants them; it is the single writer, so its table is the
cluster-wide one. The check is per operation, so a connection that survives a
promotion starts working and one that survives a demotion stops. On failover
the old primary's locks die with its sessions and the new primary starts
empty — the same window as above.

## The `ephpm_db_*` bridge

The bridge keeps one litewire session **per PHP worker thread, across
requests**, so a lock's "connection" would outlive the request that took it.
ePHPm therefore releases every named lock a bridge session still holds **at
request end** (`RELEASE_ALL_LOCKS()`, alongside the request-end rollback) — a
bridge lock lives at most for the request that took it. A script that wants a
lock to outlive its request must use `pdo_mysql`, whose connection it
controls. `ephpm_db_query("SELECT GET_LOCK('job', 0)")` returns the row;
`ephpm_db_execute("DO RELEASE_LOCK('job')")` returns an OK.

`ephpm php --site <key>` against a running server goes over the wire, so a
CLI script's locks live until the CLI process disconnects. The **offline**
direct-open path (no server running) opens the site's file without a lock
scope, and `GET_LOCK` fails there with "named locks … are not available on
this connection" rather than granting a lock no other process would see.

## Fail closed

A connection whose backend has no lock scope — any route ePHPm did not wire
one for — makes the lock functions an **error**, never `1`. A lock that
appears to work but excludes nothing is the one answer a caller cannot
defend against.
