+++
title = "ephpm analyze"
weight = 6
+++

Statically analyze a PHP application and gate on the result. `ephpm analyze` runs a set of analyzers over a directory — external security tools wrapped as subprocesses, plus native passes built into the binary — merges their findings, and applies a **fail-closed** policy that produces one of three verdicts: `allow`, `quarantine`, or `deny`. The exit code reflects the verdict, so the command works directly as a CI step or deploy gate.

## Synopsis

```bash
ephpm analyze [PATH] [--config FILE] [--format FORMAT] [--profile NAME] [--fail-on VERDICT]
              [--level N] [--since GIT_REF] [--baseline FILE] [--cache-dir DIR] [--no-cache]
```

| Flag | Default | Purpose |
|------|---------|---------|
| `PATH` | current directory | Directory to analyze |
| `--config` | `<PATH>/.ephpm-analyze.yml` when present | Analysis configuration file. An explicit `--config` that doesn't exist is an error, never a silent fallback to defaults. |
| `--format` | `text` | `text` or `sarif` (SARIF v2.1.0 JSON). Overrides the config file. |
| `--profile` | `security` | `security` or `none`. Overrides the config file. |
| `--fail-on` | `quarantine` | Least severe verdict that fails the exit code: `allow` (report-only), `quarantine`, or `deny`. Overrides the config file. |
| `--level` | `1` | Strictness level `0..=3` — see [Strictness levels](#strictness-levels). Overrides the config file. |
| `--since` | — | Diff-aware scan versus a git ref — see [Diff-aware scanning](#diff-aware-scanning-since). Overrides the config file. |
| `--baseline` | — | **Write** a baseline of the current findings to FILE and exit 0 — see [Baseline](#baseline-gate-only-on-new-findings). |
| `--cache-dir` | `ephpm-analyze-cache` under the system temp dir | Directory for the incremental result cache. Overrides the config file. |
| `--no-cache` | off | Disable the incremental cache for this run. |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The verdict passed the `fail_on` gate (always, for a `--baseline` write run) |
| `2` | A `quarantine` verdict failed the gate |
| `3` | A `deny` verdict failed the gate |
| `1` | Internal error (unreadable target, invalid configuration, git failure on a `--since` run, missing configured baseline) |

`fail_on` semantics: `quarantine` (the default) fails on `quarantine` **or** `deny`; `deny` fails only on `deny`; `allow` is report-only — the verdict never fails the exit code.

## Analyzers

External tools are **not bundled** — ePHPm shells out to them. An analyzer whose tool (or required input) is absent is reported as *skipped* and does not gate, unless listed in `analyzers.required` (see below). Each finding carries a **confidence**: `confirmed` (the analyzer is sure) or `suspected` (a hotspot worth review) — see [Hotspots vs findings](#hotspots-vs-findings-confidence).

| Analyzer | Kind | Category | Confidence | Needs | What it does |
|----------|------|----------|------------|-------|--------------|
| `composer-audit` | external | supply-chain | confirmed | `composer` on `PATH`, `composer.json` + `composer.lock` in the target | Runs `composer audit --no-scripts --format=json`; each advisory becomes a finding (severity from the advisory, `critical` hard-denies), abandoned packages become `info` findings. |
| `semgrep-php` | external | security | suspected | `semgrep` on `PATH` (registry configs also need network) | Runs `semgrep --config <analyzers.semgrep_config> --sarif --disable-nosem` and maps its SARIF results to findings (`error` → high, `warning` → medium, `note` → low). `--disable-nosem` means inline `// nosemgrep` comments in the analyzed code are **never honored**. |
| `malware-yara` | external | malware | confirmed | `yara` on `PATH` **and** a ruleset at `analyzers.yara_rules` (none ships with ePHPm — unset means skipped) | Runs `yara -r -w <rules> <target>`; each rule match is a `high` finding. A *configured but missing* ruleset path is an error (fail-closed), not a skip. |
| `dangerous-sinks` | native | security | suspected | nothing | A naive line/token pass over `*.php` / `*.phtml` flagging direct calls to the `eval` and `system` sinks (high) and `assert` (medium). Deliberately simple — matches inside comments/strings are false positives (hence *suspected*); dynamic calls are missed. It will be superseded by the opcode-level analyzer (planned). |
| `suppression-scan` | native | security | confirmed | nothing | Flags tenant-authored suppression-shaped comments (`ephpm-analyze-ignore`, `@phpstan-ignore*`, `@psalm-suppress`, `phpcs:ignore`/`phpcs:disable`, `nosemgrep`, `noqa`, `NOSONAR`, `@codingStandardsIgnore`) as `suppression-scan/tenant-suppression-attempt` (medium) — see [Operator-only suppressions](#operator-only-suppressions-suppress). One finding per (file, marker), anchored at the first occurrence. |

## The verdict model

Four layers; the final verdict is the most severe any layer demands:

1. **Hard denies.** A finding whose rule id matches an `analyzers.deny_hard` entry (exactly, or under it as a `/`-prefix — `malware-yara` matches every `malware-yara/<Rule>`) forces `deny` regardless of confidence — `deny_hard` is an explicit operator instruction. A **confirmed** finding of `critical` severity also forces `deny`; a *suspected* critical floors the verdict at `quarantine` instead.
2. **Fail-closed floor.** Any analyzer *failure* — tool error, timeout, unparseable output, panic, or a `required` analyzer that could not run — floors the verdict at `quarantine`. An analyzer that errored might have been about to find something, so the run can never be `allow`. A plain skip of an optional analyzer does not floor. This layer is active at **every** strictness level.
3. **Weighted score** *(levels 1+)*. Findings score by severity — info 0, low 1, medium 4, high 10, critical 40. The **total** score (confirmed + suspected) is compared against `policy.quarantine_score` (default 10); only the **confirmed** portion is compared against `policy.deny_score` (default 50) — suspected findings never add up to a hard stop. The weights are super-additive on purpose: volume of low-severity noise cannot outrank one serious finding class, while a large pile of mediums still escalates.
4. **Severity floors** *(levels 2+)*. Level 2: any single high-or-worse finding floors at `quarantine` regardless of score. Level 3: any medium-or-worse finding does.

The text output prints one reason line per layer that fired.

## Strictness levels

`level:` in the config (or `--level`) scales *how strictly* findings gate; it composes with `profile`, which picks *which* analyzers run.

| Level | Name | Score layer | Severity floor |
|-------|------|-------------|----------------|
| `0` | permissive | off | none — only `deny_hard` matches and confirmed criticals gate (plus the fail-closed floor, which no level relaxes) |
| `1` | standard *(default)* | on | none |
| `2` | strict | on | any `high`+ finding ⇒ at least `quarantine` |
| `3` | paranoid | on | any `medium`+ finding ⇒ at least `quarantine` |

At level 0 the score is still computed and reported — it just does not gate.

## Baseline: gate only on new findings

Adopting the gate on an existing codebase (the PHPStan model): record the current findings once, then fail only on *new* ones.

```bash
# 1. Generate: runs the analysis and writes the baseline (exit 0).
ephpm analyze /srv/app --baseline .ephpm-analyze-baseline.json

# 2. Consume: reference it from the config.
cat >> /srv/app/.ephpm-analyze.yml <<'EOF'
baseline: .ephpm-analyze-baseline.json
EOF

# 3. Later runs suppress the recorded findings and gate only on new ones.
ephpm analyze /srv/app
```

Each baseline entry is keyed by a stable fingerprint — SHA-256 over the finding's rule id, `/`-normalized path, and message, deliberately **not** the line number, so reformatting that shifts a finding vertically does not resurrect it. The file also records the human-readable fields, so a baseline diff is reviewable. Fail-closed rules: a configured-but-missing, corrupt, or wrong-version baseline is a hard error (exit 1), never a silent full or over-suppressed run. A `--baseline` write run ignores any configured `baseline:` (so the written file is complete) but does apply `suppress:` rules first (permanently waived findings don't belong in a baseline). Suppressed counts appear in the report (`baseline: N known finding(s) suppressed`; SARIF `ephpm/baselineSuppressed`).

## Diff-aware scanning: `since`

`--since <git-ref>` (or `since:` in the config) restricts the run to files changed versus that ref — a fast per-push gate:

```bash
ephpm analyze /srv/app --since origin/main
```

The changed set is `git diff --name-only --relative <ref>` **plus** untracked files (`git ls-files --others --exclude-standard`), so a brand-new uncommitted file cannot dodge the gate. **Fail-closed:** any git failure — no git binary, not a repository, unknown ref — is a hard error (exit 1), never a silent full or empty scan.

Enforcement: the native per-file analyzers skip out-of-scope files up front (the speedup); findings from external tools — which currently still scan the whole tree — are filtered to the changed set afterwards (dropped findings are counted as `out of scope`). Findings with no file anchor are always kept: an unattributable finding must not be droppable by scoping.

## Incremental cache

Results of the native per-file analyzers (`dangerous-sinks`, `suppression-scan`) are cached keyed on the file's content hash, the analyzer id, the effective configuration hash, and the crate version — so unchanged files reuse cached findings, and **any** config change (file or CLI override) invalidates everything. Hashes are SHA-256: the analyzed tree is untrusted input, and a weak content hash would be collision-attackable. External-tool analyzers are deliberately never cached — their results depend on state outside the tree (advisory databases, remote rule packs, ruleset files), so a cache hit could hide a newly published advisory.

On by default (`cache.enabled: true`), directory `ephpm-analyze-cache` under the system temp dir (created `0700` on Unix), overridable with `cache.dir` / `--cache-dir`, off with `cache.enabled: false` / `--no-cache`. Cache read failures are misses and write failures are ignored — the cache can only cost a rescan, never change a verdict, **provided the directory is operator-trusted**: whoever can write it can plant results, so on shared hosts point `--cache-dir` at a path only the operator can write.

## Operator-only suppressions: `suppress`

Findings are waived **only** from the operator-side config — never from comments inside the analyzed (tenant-controlled) code:

```yaml
suppress:
  - rule: dangerous-sinks/assert      # exact id, or a /-prefix (dangerous-sinks waives all its rules)
    path: legacy/compat.php           # optional: waive only at this exact tree-relative path
    reason: vetted 2026-09 — assert() behind a debug flag, not reachable
```

`reason` is required and must be non-empty — a waiver without a recorded why is a config error. Waived counts appear in the report (`waived: N finding(s)`; SARIF `ephpm/waived`).

The inversion: a tenant-authored suppression-shaped comment (`// ephpm-analyze-ignore`, `@phpstan-ignore`, `// nosemgrep`, `# noqa`, …) is **never honored** — nothing in the pipeline reads suppression directives from the scanned code, and `semgrep-php` runs with `--disable-nosem` so Semgrep can't be muted inline either. Instead the `suppression-scan` analyzer emits the comment itself as a finding (`suppression-scan/tenant-suppression-attempt`, medium): an attempt to silence the scanner is itself a signal. Vendored code legitimately full of `@phpstan-ignore` / `phpcs:ignore` markers is tuned the operator way — a `suppress:` rule scoped to those paths.

## Hotspots vs findings: confidence

Every finding carries a `confidence`:

- **`confirmed`** — the analyzer is sure (a published advisory against a pinned lockfile, a YARA signature match, the factual presence of a suppression marker). Confirmed findings drive the automatic `deny` escalations: a confirmed critical hard-denies, and only confirmed weight counts toward `policy.deny_score`.
- **`suspected`** — a heuristic hit that may be a false positive (a semgrep pattern, the naive `dangerous-sinks` token pass). Suspected findings are review signals: they count toward the quarantine threshold and severity floors — they can `quarantine`, but never force `deny` on their own.

The exception is `analyzers.deny_hard`: an explicit operator entry fires regardless of confidence — listing a heuristic rule there means accepting its false positives.

Surfacing: the text output tags suspected findings with `[suspected]`; SARIF results carry `rank` (confirmed 80, suspected 40) and `properties["ephpm/confidence"]`.

## Configuration: `.ephpm-analyze.yml`

All keys with their defaults. **Every section rejects unknown keys** — a typo fails the run naming the key rather than silently doing nothing.

```yaml
profile: security          # security | none — supplies the analyzer set when
                           # analyzers.enable is not given. `security` enables
                           # all five analyzers; `none` enables nothing.
level: 1                   # 0..3 strictness (see the table above)
fail_on: quarantine        # allow | quarantine | deny (see exit codes above)
output: text               # text | sarif
baseline: null             # baseline file to READ (suppress known findings);
                           # generate one with `--baseline`. Missing file =
                           # hard error. Relative paths resolve against PATH.
since: null                # git ref for diff-aware scanning (fail-closed)

engine:                    # Planned: not yet implemented — parsed but not
  detonate: false          # acted upon (engine-in-the-loop analysis, Phase 4).
  timeout_ms: null         # Setting either logs a startup warning.

analyzers:
  enable: null             # explicit analyzer list; null = use the profile.
                           # Unknown ids are a hard error.
  required: []             # analyzers that MUST produce a result: a skip
                           # (tool absent) of a required analyzer gates like a
                           # failure. Must be a subset of the enabled set.
  deny_hard: []            # rule ids (or `/`-prefixes) that force deny —
                           # regardless of confidence
  yara_rules: null         # YARA ruleset path for malware-yara (relative
                           # paths resolve against the analyzed directory)
  semgrep_config: p/php    # semgrep --config value
  tool_timeout_ms: 300000  # wall-clock budget per external tool; exceeding
                           # it kills the tool and gates (fail-closed)

policy:
  quarantine_score: 10     # total score at/above which the verdict is
                           # >= quarantine (levels 1+)
  deny_score: 50           # CONFIRMED score at/above which the verdict is
                           # deny (levels 1+)

cache:
  enabled: true            # incremental cache for the native per-file passes
  dir: null                # null = ephpm-analyze-cache under the system temp dir

suppress: []               # operator-side waivers — the ONLY suppression
                           # mechanism; see above. Each entry:
                           #   rule (required), path (optional), reason (required)
```

## Output formats

**`text`** — per-analyzer status (completed / skipped / FAILED), findings sorted most severe first (suspected ones tagged `[suspected]`), post-processing counters when non-zero (`out of scope`, `waived`, `baseline`), then the score, verdict, and reason lines.

**`sarif`** — a SARIF v2.1.0 document with one run. Findings map to `results` (severity → level: high/critical → `error`, medium → `warning`, low/info → `note`; each result carries `rank` from confidence plus the exact severity, category, and confidence in its `properties`). Skipped analyzers appear as `note` tool-execution notifications, failed analyzers as `error` notifications with `executionSuccessful: false`. The run's `properties` carry `ephpm/verdict`, `ephpm/score`, `ephpm/outOfScope`, `ephpm/waived`, and `ephpm/baselineSuppressed`.

## Examples

```bash
# Gate a checkout in CI with the defaults (security profile, level 1,
# fail on quarantine)
ephpm analyze /srv/app

# Report-only: always exit 0, just print the report
ephpm analyze /srv/app --fail-on allow

# Fast per-push gate: only files changed vs main, paranoid strictness
ephpm analyze /srv/app --since origin/main --level 3

# Adopt on a legacy codebase: record today's findings, then gate on new ones
ephpm analyze /srv/app --baseline .ephpm-analyze-baseline.json
echo 'baseline: .ephpm-analyze-baseline.json' >> /srv/app/.ephpm-analyze.yml

# SARIF for upload to a code-scanning UI
ephpm analyze /srv/app --format sarif > analysis.sarif

# Strict deploy gate: composer-audit must actually run, and any YARA hit
# or eval() call is an instant deny; one vetted legacy assert() is waived
cat > /srv/app/.ephpm-analyze.yml <<'EOF'
analyzers:
  required: [composer-audit]
  deny_hard: [malware-yara, dangerous-sinks/eval]
  yara_rules: /etc/ephpm/yara/webshells.yar
suppress:
  - rule: dangerous-sinks/assert
    path: legacy/compat.php
    reason: vetted 2026-09 — debug assertion, unreachable in production
EOF
ephpm analyze /srv/app
```

## Scope

Shipped: the aggregator, the fail-closed policy engine (strictness levels, confidence model), the config/gating surface, baselines, diff-aware scanning, the incremental cache, operator-only suppressions with tenant-suppression detection, both output formats, and the five analyzers above. Planned — not yet implemented: opcode-level analysis of compiled PHP (which replaces the naive `dangerous-sinks` pass), engine-in-the-loop detonation of suspicious inputs (the `engine:` section), and ePHPm-specific rules. The `Analyzer` trait, finding shape, policy engine, and output formats are designed to be stable across those additions.
