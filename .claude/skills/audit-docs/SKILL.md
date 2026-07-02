---
name: audit-docs
description: Run a claims-vs-code truth audit of ephpm documentation (site/, docs/, examples/, README). Use before releases, after large refactors, or when the user asks whether the docs match reality. Finds documented features that don't exist, wrong defaults, and undersold shipped features.
---

# Docs truth audit

Cross-check every user-facing claim against its source of truth. The first full audit (fixed in #106/#107) found ~25 fictional CLI commands, silently-ignored config knobs, and false security claims - assume drift accumulates and re-run periodically.

## Source-of-truth map (verify claims ONLY against these)

| Claim type | Authoritative source |
|---|---|
| CLI subcommands / flags | `crates/ephpm/src/main.rs` (clap derive) + `crates/ephpm/src/service/` |
| Config keys, defaults, semantics | `crates/ephpm-config/src/lib.rs` structs + `default_*` fns; note fields whose own doc comment says "Planned: not yet implemented" |
| PHP SAPI functions (names, arities, units) | `crates/ephpm-php/ephpm_wrapper.c` - check `ZEND_PARSE_PARAMETERS_START(min, max)` and unit conversions (e.g. `ttl * 1000LL` means the arg is SECONDS) |
| RESP commands | `crates/ephpm-kv/src/command.rs` match arms |
| Metric names / labels / buckets | `counter!`/`histogram!`/`gauge!` call sites (router.rs, lib.rs, query-stats) + `metrics.rs` bucket registration. A registered bucket with no recording call site = phantom metric |
| HTTP behavior / lifecycle | `crates/ephpm-server/src/router.rs` + `lib.rs`; PHP lifecycle in `ephpm_wrapper.c` |
| Cluster capabilities | `crates/ephpm-cluster/src/` - a config field existing does NOT mean the feature exists (grep for where it's read) |
| Release assets / Docker tags | `.github/workflows/release.yml` |

## Severity rubric

- **BROKEN** - following the doc as written fails, or a promised security/durability property doesn't exist. Fix first.
- **WRONG** - behavior/default/name/label differs from the doc.
- **STALE** - doc lags code (includes underselling: "planned" for shipped features - check both directions!).

## Procedure

1. Fan out parallel research agents by doc surface (they don't fit one context): (a) `site/content/reference/`, (b) `site/content/guides/` + `migration/` + `getting-started/`, (c) `site/content/architecture/` + `developer/`, (d) README + `examples/` + `docs/`. Give each the source-of-truth map and rubric; require file:line for every finding.
2. **Spot-verify the most damning findings yourself** before reporting (grep the cited code) - agents converging independently on a finding is good signal, but security claims deserve direct verification.
3. Report grouped: security-critical false claims -> broken-if-followed -> silent no-ops -> wrong-mechanism -> underselling.
4. Fixing: docs-only corrections in one PR; code changes (making a no-op knob real) in a separate PR. When both are in flight, coordinate which PR owns each truth (don't document a knob as dead while another PR implements it).

## Recurring traps

- Guides inventing config keys (`acme_domains` vs the real `domains`) - serde silently ignores unknown keys, so wrong keys "work" by doing nothing.
- Unit errors in examples (ms vs seconds TTLs).
- `blocked_paths` globs shown without the leading `/` (never match).
- Reference pages that were actually design docs (aspirational CLI/API surfaces).
- Compatibility floors ("needs v0.1.2+") are usually CORRECT - don't bump them to the current version.
