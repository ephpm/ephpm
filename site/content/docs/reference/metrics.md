+++
title = "Metrics"
weight = 4
+++

> **Stub** — not yet written. See [Architecture → Metrics](/docs/architecture/metrics/) for the design.

## What this will cover

Exhaustive list of every metric exposed at `/metrics`:

- HTTP: request count, duration histograms, status code totals
- PHP: request execution time, errors, memory usage
- Database: connections active/idle, query digests, slow queries
- KV: operations per type, hit/miss, compression savings
- Cluster: gossip membership, KV replication lag, sqld primary election
- TLS: cert expiry, ACME renewal events
