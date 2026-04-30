+++
title = "Query Stats with Prometheus"
weight = 7
+++

ePHPm tracks every SQL query that flows through it — to a real MySQL/Postgres backend or to embedded SQLite — and exposes per-digest timing, throughput, and error-rate as Prometheus metrics. No APM agent, no database plugin.

## Turn it on

Query stats are **on by default**, but you also need a metrics endpoint:

```toml
[server.metrics]
enabled = true
# path = "/metrics"            # default

[db.analysis]
query_stats = true             # set false to disable (zero overhead)
slow_query_threshold = "500ms" # queries slower than this are logged at WARN
```

That's it. PHP keeps using `pdo_mysql` against `127.0.0.1:3306`. ePHPm intercepts the wire, normalizes each statement, hashes it to a digest, and updates the metrics.

## What you get

`/metrics` exposes (among others):

```
# Query duration histogram, by digest and kind
ephpm_query_duration_seconds_bucket{digest="SELECT * FROM users WHERE id = ?",kind="query",le="0.01"} 4521

# Total count by status
ephpm_query_total{digest="SELECT * FROM users WHERE id = ?",kind="query",status="ok"}    4520
ephpm_query_total{digest="SELECT * FROM users WHERE id = ?",kind="query",status="error"} 1

# Rows returned/affected
ephpm_query_rows_total{digest="SELECT * FROM users WHERE id = ?",kind="query"} 4520

# Slow query counter
ephpm_query_slow_total 3

# Active digest cardinality
ephpm_query_active_digests 47
```

`digest` is the **normalized SQL** — literals replaced with `?`, whitespace collapsed. So `SELECT * FROM users WHERE id = 1` and `SELECT * FROM users WHERE id = 2` aggregate together, while `SELECT count(*) FROM users` is a separate digest.

## Slow-query log

Queries exceeding `slow_query_threshold` log at `WARN` with the normalized SQL and digest ID:

```
WARN ephpm_query_stats: slow query digest=a3f9b2e1 elapsed_ms=731 sql="SELECT * FROM orders WHERE status = ?"
```

Tail with `journalctl -u ephpm -f` (systemd) or whatever you point ePHPm at.

## Useful PromQL

p99 latency per digest:

```promql
histogram_quantile(0.99,
  sum by (digest, le) (rate(ephpm_query_duration_seconds_bucket[5m]))
)
```

Top 10 hottest queries:

```promql
topk(10, sum by (digest) (rate(ephpm_query_total[5m])))
```

Error rate by digest:

```promql
sum by (digest) (rate(ephpm_query_total{status="error"}[5m]))
  /
sum by (digest) (rate(ephpm_query_total[5m]))
```

Slow queries per second:

```promql
rate(ephpm_query_slow_total[5m])
```

Active digest cardinality (helpful for catching a digest-explosion when a query template stops normalizing cleanly):

```promql
ephpm_query_active_digests
```

## Cardinality is bounded

Digests are normalized — literals become `?`, so the cardinality is roughly the number of *distinct query shapes* in your app, not the number of *executions*. ePHPm caps this at `digest_store_max_entries` (default 100,000) and evicts oldest entries on overflow.

If your cardinality is climbing unexpectedly, look for queries with non-literal pieces that shouldn't vary: dynamic table names, raw SQL fragments built per request, etc.

## Grafana

Point a Grafana datasource at your Prometheus, then build the usual: throughput, p50/p95/p99 latency, error rate, slow-query rate. The metrics are vanilla Prometheus types — anything that speaks PromQL works.

## Disable it

Either knob silences the cost:

```toml
[server.metrics]
enabled = false               # turn off /metrics entirely

# OR, keep /metrics but don't track per-digest stats:
[db.analysis]
query_stats = false
```

## See also

- [Architecture → Query Stats](/architecture/query-stats/) — how the normalizer works
- [Reference → Metrics](/reference/metrics/) — every metric ePHPm exposes
- [Reference → Configuration `[db.analysis]`](/reference/config/)
