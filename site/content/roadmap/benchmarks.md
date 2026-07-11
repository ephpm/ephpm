# Benchmarks as a Release Artifact

> **Status: DESIGN — not yet implemented.** The tooling exists in
> scattered, proven form (the ePHPm-lab k6 profiles, the opcache e2e);
> this page designs its consolidation into a per-release discipline.
>
> For the numbers and findings measured **so far**, see the
> [Benchmarking](/benchmarking/) section — this page is the plan for
> making that a repeatable per-release artifact.

## Why this page exists

Two facts from July 2026:

1. **Every ePHPm 8.3/8.4 binary ever shipped before 0.3.0 ran with
   OPcache silently disabled** — the embed SAPI wasn't on OPcache's
   allowlist, so drop-in mode was benchmarked (by us and by users) at
   roughly 17× its intended latency on framework workloads. No test
   caught it, because no test asserted a *performance property*.
2. The bug was found by an outside user's benchmark report — which also
   showed that a motivated evaluator will test the wrong mode, get the
   wrong numbers, and publish them. Their (fair) ask: *"publish
   benchmark recipes that compare against realistic PHP-FPM baselines
   with OPcache and Redis."*

Correctness CI cannot catch a 17× regression that returns correct
bytes. Only a measured baseline can.

## Design

### 1. `bench/` — recipes in-tree

A directory of self-contained, pinned benchmark profiles (k6 scripts +
fixtures + compose/k8s manifests), each with a named baseline pairing:

| Profile | ePHPm side | Baseline side |
|---|---|---|
| `tiny-scripts` | fpm mode, defaults | nginx + php-fpm (OPcache on) |
| `laravel-worker` | worker mode + native KV | nginx + php-fpm + Redis (Predis) |
| `static-files` | fpm mode | nginx alone |
| `deploy-blip` | 2-node cluster + `ephpm deploy` | 2-replica fpm + rolling restart |

The first three reproduce the shapes from the external lab report; the
fourth is the cluster demo already validated in ePHPm-lab. Recipes
double as user documentation — "here is exactly how to reproduce our
numbers."

### 2. Per-release numbers in the release notes

The release pipeline (or a manually-triggered `bench.yml`) runs the
profiles against the release candidate image on a pinned runner class
and emits a comparison table. Numbers are published with hardware
context and honest caveats — the July lab exchange demonstrated that
disclosed-limitation numbers build more trust than cherry-picked ones.

### 3. The regression gate (the actual point)

A stored baseline JSON per profile (median + p95, refreshed each
release). CI compares the candidate against the previous release's
numbers on identical hardware:

- **> 20% median regression on any profile → red check.** Wide enough
  to ignore runner noise, narrow enough that opcache-off (≈ 300%+)
  or a static-file path regression (≈ 10×) can never ship silently.
- Improvements auto-update the stored baseline on release.

The performance-*property* assertions stay in e2e where they belong
(`opcache_is_enabled_over_http` already guards the specific July
failure); the gate guards the failures nobody has imagined yet.

### 4. Hardware honesty

Self-hosted runner numbers are only comparable to themselves. The gate
compares same-runner-class release-over-release deltas, never absolute
numbers across environments. Published tables state the runner spec.

## Sizing

Most of this exists: the k6 profiles, fixtures, and manifests were
built and validated for the ePHPm-lab PRs; the summary-extraction
tooling was written for the deploy-blip driver. Remaining work is
consolidation into `bench/`, a workflow, and the baseline-comparison
script — days, not weeks. Highest value-to-effort item on this page's
shelf.
