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

The `security` profile enables the first five analyzers below (`composer-audit`, `semgrep-php`, `malware-yara`, `dangerous-sinks`, `suppression-scan`). `phpstan`, `psalm-taint`, `progpilot`, `phpcs`, `phpmd`, `rector`, `opcode-scan`, `php-lint`, and `wp-vuln` are registered but **opt-in** — they run only when named explicitly in `analyzers.enable`, so the default gate behaviour is unchanged. `opcode-scan` and `php-lint` additionally **require a PHP-linked build** of ePHPm (the release binaries; a stub build reports them as skipped — list them in `analyzers.required` to make that skip gate instead).

| Analyzer | Kind | Category | Confidence | Needs | What it does |
|----------|------|----------|------------|-------|--------------|
| `composer-audit` | external | supply-chain | confirmed | `composer` on `PATH`, `composer.json` + `composer.lock` in the target | Runs `composer audit --no-scripts --format=json`; each advisory becomes a finding (severity from the advisory, `critical` hard-denies), abandoned packages become `info` findings. |
| `semgrep-php` | external | security | suspected | `semgrep` on `PATH` (registry configs also need network) | Runs `semgrep --config <analyzers.semgrep_config> --sarif --disable-nosem` and maps its SARIF results to findings (`error` → high, `warning` → medium, `note` → low). `--disable-nosem` means inline `// nosemgrep` comments in the analyzed code are **never honored**. |
| `malware-yara` | external | malware | confirmed | `yara` on `PATH` **and** a ruleset at `analyzers.yara_rules` (none ships with ePHPm — unset means skipped) | Runs `yara -r -w <rules> <target>`; each rule match is a `high` finding. A *configured but missing* ruleset path is an error (fail-closed), not a skip. |
| `progpilot` | external | security | confirmed | `progpilot` on `PATH` or at `vendor/bin/progpilot` in the target | A PHP **taint-analysis** security scanner: it tracks data flow from request sources to dangerous sinks and reports injection-class bugs (`sql_injection`, `xss`, `file_inclusion`, `command_injection`, …). Runs `progpilot .` and parses its default JSON output; each vulnerability is a `high` finding with rule id `progpilot/<vuln_name>`, anchored at the sink. **Opt-in** — *not* in the default `security` profile; enable it with `analyzers.enable`. progpilot's non-zero "found something" exit is not treated as a failure (the JSON body is authoritative); unparseable output gates. |
| `dangerous-sinks` | native | security | suspected | nothing | A naive line/token pass over `*.php` / `*.phtml` flagging direct calls to the `eval` and `system` sinks (high) and `assert` (medium). Deliberately simple — matches inside comments/strings are false positives (hence *suspected*); dynamic calls are missed. It will eventually be superseded by the opt-in `opcode-scan` analyzer below; both run today. |
| `psalm-taint` | external | security | confirmed | `psalm` on `PATH` (or `vendor/bin/psalm`) **and** a `psalm.xml` in the target | **Opt-in — not in the default `security` set.** Runs `psalm --taint-analysis --output-format=sarif` and maps each traced source→sink taint flow (`TaintedSql`, `TaintedHtml`, `TaintedShell`, `TaintedInclude`, …) to a `high` finding. A missing `psalm.xml` (Psalm cannot run without one) is a *skip*, not an error; auto-discovers `psalm.xml`/`psalm.xml.dist` or takes `analyzers.psalm_config`. Enable via `analyzers.enable`. |
| `suppression-scan` | native | security | confirmed | nothing | Flags tenant-authored suppression-shaped comments (`ephpm-analyze-ignore`, `@phpstan-ignore*`, `@psalm-suppress`, `phpcs:ignore`/`phpcs:disable`, `nosemgrep`, `noqa`, `NOSONAR`, `@codingStandardsIgnore`) as `suppression-scan/tenant-suppression-attempt` (medium) — see [Operator-only suppressions](#operator-only-suppressions-suppress). One finding per (file, marker), anchored at the first occurrence. |
| `phpstan` | external, **opt-in** | quality | confirmed | `phpstan` on `PATH` **or** a vendored `vendor/bin/phpstan`; a rule level (from `analyzers.phpstan_level` or a `phpstan.neon`/`phpstan.neon.dist` in the target) | Runs `phpstan analyse --error-format=json --no-progress --no-interaction`; each per-file message becomes a `medium` **quality** finding (`rule_id` = `phpstan/<identifier>` when PHPStan emits one, else `phpstan/analyse`). A non-empty top-level `errors` array (bad config, internal error) is a broken run and **gates** (fail-closed), never a clean result. Not in the `security` profile — enable it with `analyzers.enable: [phpstan]`. |
| `phpcs` | external, **opt-in** | quality (security for `.Security.` sniffs) | confirmed | `phpcs` on `PATH` **or** a vendored `vendor/bin/phpcs` | **Opt-in — not in the default `security` set.** Runs `phpcs --report=json -q .` and maps each coding-standard message to a finding: `type = ERROR` → `medium`, `WARNING` → `low`; `rule_id = phpcs/<source>` (the sniff code, e.g. `Generic.Files.LineLength.TooLong`). Category is `quality`, upgraded to `security` when the sniff `source` names a `Security` segment (`WordPress.Security.*`, VIP). `analyzers.phpcs_standard` overrides the standard (`--standard`); unset uses PHPCS's own configured/discovered standard. Non-zero exit (violations found) is not a failure — the JSON body is authoritative; unparseable output gates. |
| `phpmd` | external, **opt-in** | quality | suspected | `phpmd` on `PATH` **or** a vendored `vendor/bin/phpmd` | **Opt-in — not in the default `security` set.** Runs `phpmd . json <ruleset>` (PHPMD's positional-argument CLI — the report format and ruleset are positional, there is no `--report-format` flag) and maps each violation to a `quality` finding: `priority = 1` → `medium`, lower → `low`; `rule_id = phpmd/<rule>`. Heuristic smell/metrics detector, so findings are **suspected**. `analyzers.phpmd_ruleset` overrides the comma-separated ruleset (default `cleancode,codesize,controversial,design,naming,unusedcode`). Non-zero "found violations" exit is not a failure (JSON body authoritative); a non-empty top-level `errors` array (files PHPMD could not parse) gates, and unparseable output gates. |
| `rector` | external, **opt-in** | quality | suspected | `rector` on `PATH` (or `vendor/bin/rector`) **and** a `rector.php` in the target | **Opt-in — not in the default `security` set.** Runs `rector process . --dry-run --output-format=json --no-progress-bar` — **always in dry-run, never modifying code** — and reports one `low` `quality` finding per file Rector would change (`rule_id = rector/<first-applied-rector-short-name>`, or `rector/process`). Refactor suggestions, so **suspected**. A missing `rector.php` (Rector cannot run without one) is a *skip*, not an error; `analyzers.rector_config` sets an explicit `--config`. Non-zero exit (changes found) is not a failure (JSON body authoritative); a non-empty top-level `errors` array gates, and unparseable output gates. |
| `opcode-scan` | native (engine-backed), **opt-in** | security | confirmed | a **PHP-linked build** of ePHPm (skipped on stub builds) | Compiles each `*.php` / `*.phtml` file with the **embedded Zend compiler** — never executing it — and detects dangerous call sites in the opcode stream: the `eval` construct plus statically-named calls to `system`, `exec`, `shell_exec`, `passthru`, `proc_open`, `popen`, `create_function` (all `high`) and `assert`, `unserialize` (`medium`), including inside functions, methods, closures, and conditional declarations; the backtick operator is caught as the `shell_exec` call the compiler lowers it to. Opcodes have no comments or strings-as-code, so hits are `confirmed` — unlike the `dangerous-sinks` token pass, which it will eventually replace (both run today). Known false negatives: dynamic calls (`$f()`, `call_user_func`), concatenated names. A file that does not compile **gates the run** (fail-closed) — code the engine cannot compile cannot be vouched for. Results are never cached (they depend on the embedded PHP version). See the [roadmap](/roadmap/opcode-analysis/) for the taint/detonation phases. |
| `php-lint` | native (engine-backed), **opt-in** | quality | confirmed | a **PHP-linked build** of ePHPm (skipped on stub builds) | `php -l` done in-process: compiles each `*.php` / `*.phtml` file with the **embedded Zend compiler** — never executing it — and reports every file that fails to compile as one `high` **quality** finding (`rule_id = php-lint/syntax-error`, message = PHP's own diagnostic, line parsed from PHP's ` on line N`). A file that compiles produces nothing. Findings are `confirmed` — the engine's own compiler rejected the file. Reuses the `opcode-scan` compile path with an empty sink list (no separate FFI surface). A plain parse error keeps linting the rest of the tree; a rarer *fatal* compile error / bailout (e.g. an in-file redeclaration) may leave the engine degraded and instead **gates the run** (fail-closed), like `opcode-scan`. Results are never cached (they depend on the embedded PHP version). |
| `wp-vuln` | native (offline, feed-backed), **opt-in** | supply-chain | confirmed | a downloaded vulnerability feed at `analyzers.wp_vuln_feed` (none ships with ePHPm — unset means skipped) | The WordPress analogue of `composer-audit`: reads the installed **version** of every plugin (`wp-content/plugins/<slug>/` main file `Version:` header), theme (`wp-content/themes/<slug>/style.css` `Version:`), and core (`wp-includes/version.php` `$wp_version`), and matches each `(type, slug, version)` against a downloaded **Wordfence Intelligence** vulnerability feed. Every installed version that falls inside a known vulnerability's affected-version range becomes one `SupplyChain` finding: `rule_id = wp-vuln/<slug>/<CVE-or-feed-id>`, severity from the feed's CVSS rating (`critical` hard-denies), message `"<type> <slug> <version> — <CVE>: <title>; fixed in <patched>"`. **No network calls at scan time** — the operator refreshes the feed out of band (see [Downloading the feed](#wp-vuln-downloading-the-vulnerability-feed)), exactly like `malware-yara`'s ruleset. A *configured but missing / unparseable* feed is an error (fail-closed), not a skip; a tree that is not WordPress (no `wp-content`) yields no findings and no error. Version comparison uses PHP `version_compare` semantics (not strict semver), matching Wordfence's own scanner. Not cached — a cache hit could hide a newly published advisory against an unchanged install. |

### wp-vuln: downloading the vulnerability feed

`wp-vuln` is **offline at scan time** — it never talks to the network during a run. Instead the operator downloads the [Wordfence Intelligence](https://www.wordfence.com/help/wordfence-intelligence/) vulnerability feed out of band and points `analyzers.wp_vuln_feed` at the resulting JSON file, exactly the way `malware-yara` consumes a downloaded YARA ruleset. Refresh it on whatever cadence you trust (a nightly cron, a CI cache) — the analyzer reads whatever is on disk.

The current feed API is **v3**, which requires a free API key (a Bearer token generated in the *Integrations* section of your Wordfence account). Download the **production** feed (fully analyzed records with CVE/CVSS) like this:

```bash
curl -H "Authorization: Bearer $WORDFENCE_API_KEY" \
     "https://www.wordfence.com/api/intelligence/v3/vulnerabilities/production" \
     -o wordfence-feed.json
```

Then reference it from the config (relative paths resolve against the analyzed directory):

```yaml
analyzers:
  enable: [wp-vuln]        # opt-in — not in the default security profile
  wp_vuln_feed: wordfence-feed.json
```

The default response is a JSON **object keyed by vulnerability UUID**, which is the shape `wp-vuln` parses. The lighter **scanner** feed (`.../v3/vulnerabilities/scanner`) shares the same base schema and also works — it carries detection ranges but no CVE/CVSS, so those findings report the Wordfence record id in place of a CVE and default to `medium` severity. Only the fields `wp-vuln` actually reads are required: each record's `id`, `title`, `cve`, `cvss` (`score`, `rating`), and a `software` array of `{ type, slug, affected_versions: { <label>: { from_version, from_inclusive, to_version, to_inclusive } }, patched_versions }`.

Limitations to be aware of: the free feed covers wp.org plugins/themes and core — a **premium/renamed plugin** whose slug is not in the feed cannot be matched; `wp-vuln` reads versions from the standard layout (`wp-content/plugins`, `wp-content/themes`, `wp-includes/version.php`), so a **relocated `wp-content`** is not discovered; and results are only as fresh as the feed file you last downloaded.

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
                           # Unknown ids are a hard error. Opt-in extras
                           # (never enabled by a profile): phpstan,
                           # opcode-scan / php-lint (need a PHP-linked build).
  required: []             # analyzers that MUST produce a result: a skip
                           # (tool absent) of a required analyzer gates like a
                           # failure. Must be a subset of the enabled set.
  deny_hard: []            # rule ids (or `/`-prefixes) that force deny —
                           # regardless of confidence
  yara_rules: null         # YARA ruleset path for malware-yara (relative
                           # paths resolve against the analyzed directory)
  semgrep_config: p/php    # semgrep --config value
  phpstan_config: null     # phpstan `analyse -c <path>` (relative to the
                           # target). null = PHPStan auto-discovers
                           # phpstan.neon / phpstan.neon.dist
  phpstan_level: null      # phpstan `analyse --level <n>` (0..=9). null =
                           # level comes from the target's PHPStan config
  psalm_config: null       # psalm --config path for the opt-in psalm-taint
                           # analyzer; null = auto-discover psalm.xml /
                           # psalm.xml.dist (skip if neither is present)
  progpilot_config: null   # optional progpilot --configuration file (custom
                           # sources/sinks/rules); relative to the target.
                           # Must leave progpilot's default JSON output on.
  phpcs_standard: null     # phpcs --standard for the opt-in phpcs analyzer
                           # (a standard name or a phpcs.xml path). null =
                           # PHPCS's own configured/discovered standard
  phpmd_ruleset: null      # phpmd positional ruleset for the opt-in phpmd
                           # analyzer (comma-separated names/paths). null =
                           # cleancode,codesize,controversial,design,naming,unusedcode
  rector_config: null      # rector process --config for the opt-in rector
                           # analyzer (relative to the target). null =
                           # auto-discover rector.php (skip if absent)
  wp_vuln_feed: null       # downloaded Wordfence Intelligence vuln feed for
                           # the opt-in wp-vuln analyzer (relative to the
                           # target). null = wp-vuln skips. Missing/unparseable
                           # path = hard error (fail-closed). No network calls.
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

Shipped: the aggregator, the fail-closed policy engine (strictness levels, confidence model), the config/gating surface, baselines, diff-aware scanning, the incremental cache, operator-only suppressions with tenant-suppression detection, both output formats, the five default-profile analyzers above, and the opt-in `phpstan`, `psalm-taint`, `progpilot`, `phpcs`, `phpmd`, `rector`, `opcode-scan`, and `php-lint` analyzers (`opcode-scan` — compile-to-opcode sink detection via the embedded engine — is the first slice of [engine-backed analysis](/roadmap/opcode-analysis/); it coexists with `dangerous-sinks` for now, and `php-lint` reuses the same compile path for an in-process `php -l` syntax check). `phpcs`/`phpmd`/`rector` are external-tool quality wrappers (`rector` runs strictly in `--dry-run`, never modifying code). `wp-vuln` is a native, offline supply-chain analyzer that matches installed WordPress plugin/theme/core versions against a downloaded Wordfence Intelligence feed — the WordPress counterpart to `composer-audit`; it is opt-in today, a candidate for the default `security` profile once a first-class feed-refresh workflow ships. Planned — not yet implemented: superglobal→sink taint tracking on the opcode stream, engine-in-the-loop detonation of suspicious inputs (the `engine:` section), and ePHPm-specific rules. The `Analyzer` trait, finding shape, policy engine, and output formats are designed to be stable across those additions.
