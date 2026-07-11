+++
title = "Benchmarking"
type = "docs"
weight = 10
+++

How ePHPm is measured, what the numbers actually mean, and the findings
that came out of measuring it. This section is descriptive, not
aspirational — every number here was measured on a real artifact, and
where something is unproven it says so.

## The discipline

Performance claims in ePHPm follow three rules, learned the hard way:

1. **Measured before merged.** A change is not "a 200× win" until a
   benchmark says so. Estimates are labeled as estimates.
2. **Verified on the artifact.** Headline numbers are re-measured on the
   *built release image*, not a dev build — because the two can differ in
   ways that silently erase the win (see
   [Findings → When measurement caught a bug](findings/#when-measurement-caught-a-bug)).
3. **Guarded after shipping.** Silent-regression classes get CI guards
   (the SHA-NI symbol check, the opcache-enabled e2e) so a win can't
   quietly evaporate in a later build.

## Pages

- **[Methodology](methodology/)** — the harness, the tools, container CPU
  quotas, and the traps that taint a run (rate limiters, the wrong image
  config, throughput-vs-latency confusion).
- **[Results](results/)** — the measured numbers across releases, with the
  before/after that each change produced.
- **[Findings](findings/)** — the technical discoveries: where latency
  hid, why some "obvious" wins didn't materialize, and what the data
  ruled out.

## The one-paragraph version

ePHPm is an all-in-one PHP application server, so its performance story
spans HTTP dispatch, PHP execution, an embedded database wire proxy, and
a KV store — each with its own hot path. The biggest single win to date
was **removing a ~44 ms-per-query Nagle/delayed-ACK stall** on the
database path (208× on point-SELECTs). The biggest *surprise* was that a
JIT and an allocator swap — both "obviously faster" — did little or
nothing for the workloads we tested, while a one-line socket option did
more. Measurement, not intuition, drove every one of those calls.
