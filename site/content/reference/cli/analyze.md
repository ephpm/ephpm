+++
title = "ephpm analyze"
weight = 6
+++

Statically analyze a PHP application and gate on the result. `ephpm analyze` runs a set of analyzers over a directory — external security tools wrapped as subprocesses, plus native passes built into the binary — merges their findings, and applies a **fail-closed** policy that produces one of three verdicts: `allow`, `quarantine`, or `deny`. The exit code reflects the verdict, so the command works directly as a CI step or deploy gate.

## Synopsis

```bash
ephpm analyze [PATH] [--config FILE] [--format FORMAT] [--profile NAME] [--fail-on VERDICT]
```

| Flag | Default | Purpose |
|------|---------|---------|
| `PATH` | current directory | Directory to analyze |
| `--config` | `<PATH>/.ephpm-analyze.yml` when present | Analysis configuration file. An explicit `--config` that doesn't exist is an error, never a silent fallback to defaults. |
| `--format` | `text` | `text` or `sarif` (SARIF v2.1.0 JSON). Overrides the config file. |
| `--profile` | `security` | `security` or `none`. Overrides the config file. |
| `--fail-on` | `quarantine` | Least severe verdict that fails the exit code: `allow` (report-only), `quarantine`, or `deny`. Overrides the config file. |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The verdict passed the `fail_on` gate |
| `2` | A `quarantine` verdict failed the gate |
| `3` | A `deny` verdict failed the gate |
| `1` | Internal error (unreadable target, invalid configuration) |

`fail_on` semantics: `quarantine` (the default) fails on `quarantine` **or** `deny`; `deny` fails only on `deny`; `allow` is report-only — the verdict never fails the exit code.

## Analyzers (Phase 1)

External tools are **not bundled** — ePHPm shells out to them. An analyzer whose tool (or required input) is absent is reported as *skipped* and does not gate, unless listed in `analyzers.required` (see below).

| Analyzer | Kind | Category | Needs | What it does |
|----------|------|----------|-------|--------------|
| `composer-audit` | external | supply-chain | `composer` on `PATH`, `composer.json` + `composer.lock` in the target | Runs `composer audit --no-scripts --format=json`; each advisory becomes a finding (severity from the advisory, `critical` hard-denies), abandoned packages become `info` findings. |
| `semgrep-php` | external | security | `semgrep` on `PATH` (registry configs also need network) | Runs `semgrep --config <analyzers.semgrep_config> --sarif` and maps its SARIF results to findings (`error` → high, `warning` → medium, `note` → low). |
| `malware-yara` | external | malware | `yara` on `PATH` **and** a ruleset at `analyzers.yara_rules` (none ships with ePHPm — unset means skipped) | Runs `yara -r -w <rules> <target>`; each rule match is a `high` finding. A *configured but missing* ruleset path is an error (fail-closed), not a skip. |
| `dangerous-sinks` | native | security | nothing | **Phase-1 placeholder**: a naive line/token pass over `*.php` / `*.phtml` flagging direct calls to the `eval` and `system` sinks (high) and `assert` (medium). Deliberately simple — matches inside comments/strings are false positives; dynamic calls are missed. It will be superseded by the opcode-level analyzer (planned). |

## The verdict model

Three layers; the final verdict is the most severe any layer demands:

1. **Hard denies.** A finding whose rule id matches an `analyzers.deny_hard` entry (exactly, or under it as a `/`-prefix — `malware-yara` matches every `malware-yara/<Rule>`), or any finding of `critical` severity, forces `deny`.
2. **Fail-closed floor.** Any analyzer *failure* — tool error, timeout, unparseable output, panic, or a `required` analyzer that could not run — floors the verdict at `quarantine`. An analyzer that errored might have been about to find something, so the run can never be `allow`. A plain skip of an optional analyzer does not floor.
3. **Weighted score.** Findings score by severity — info 0, low 1, medium 4, high 10, critical 40 — and the total is compared against `policy.quarantine_score` (default 10) and `policy.deny_score` (default 50). The weights are super-additive on purpose: volume of low-severity noise cannot outrank one serious finding class, while a large pile of mediums still escalates.

The text output prints one reason line per layer that fired.

## Configuration: `.ephpm-analyze.yml`

All keys with their defaults. **Every section rejects unknown keys** — a typo fails the run naming the key rather than silently doing nothing.

```yaml
profile: security          # security | none — supplies the analyzer set when
                           # analyzers.enable is not given. `security` enables
                           # all four Phase-1 analyzers; `none` enables nothing.
fail_on: quarantine        # allow | quarantine | deny (see exit codes above)
output: text               # text | sarif

engine:                    # Planned: not yet implemented — parsed but not
  detonate: false          # acted upon (engine-in-the-loop analysis, Phase 4).
  timeout_ms: null         # Setting either logs a startup warning.

analyzers:
  enable: null             # explicit analyzer list; null = use the profile.
                           # Unknown ids are a hard error.
  required: []             # analyzers that MUST produce a result: a skip
                           # (tool absent) of a required analyzer gates like a
                           # failure. Must be a subset of the enabled set.
  deny_hard: []            # rule ids (or `/`-prefixes) that force deny
  yara_rules: null         # YARA ruleset path for malware-yara (relative
                           # paths resolve against the analyzed directory)
  semgrep_config: p/php    # semgrep --config value
  tool_timeout_ms: 300000  # wall-clock budget per external tool; exceeding
                           # it kills the tool and gates (fail-closed)

policy:
  quarantine_score: 10     # score at/above which the verdict is >= quarantine
  deny_score: 50           # score at/above which the verdict is deny
```

## Output formats

**`text`** — per-analyzer status (completed / skipped / FAILED), findings sorted most severe first, then the score, verdict, and reason lines.

**`sarif`** — a SARIF v2.1.0 document with one run. Findings map to `results` (severity → level: high/critical → `error`, medium → `warning`, low/info → `note`; the exact severity and category ride in each result's `properties`). Skipped analyzers appear as `note` tool-execution notifications, failed analyzers as `error` notifications with `executionSuccessful: false`, and the verdict and score are on the run's `properties` as `ephpm/verdict` / `ephpm/score`.

## Examples

```bash
# Gate a checkout in CI with the defaults (security profile, fail on quarantine)
ephpm analyze /srv/app

# Report-only: always exit 0, just print the report
ephpm analyze /srv/app --fail-on allow

# SARIF for upload to a code-scanning UI
ephpm analyze /srv/app --format sarif > analysis.sarif

# Strict deploy gate: composer-audit must actually run, and any YARA hit
# or eval() call is an instant deny
cat > /srv/app/.ephpm-analyze.yml <<'EOF'
analyzers:
  required: [composer-audit]
  deny_hard: [malware-yara, dangerous-sinks/eval]
  yara_rules: /etc/ephpm/yara/webshells.yar
EOF
ephpm analyze /srv/app
```

## Phase-1 scope

This is the analyzer *framework* release: the aggregator, the fail-closed policy engine, the config/gating surface, both output formats, and the four analyzers above. Planned — not yet implemented: opcode-level analysis of compiled PHP (which replaces the naive `dangerous-sinks` pass), engine-in-the-loop detonation of suspicious inputs (the `engine:` section), and ePHPm-specific rules. The `Analyzer` trait, finding shape, policy engine, and output formats are designed to be stable across those additions.
