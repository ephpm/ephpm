# `ephpm analyze` demo — acme-portal

A tiny, **deliberately-insecure** PHP app used to show what `ephpm analyze`
reports and how the fail-closed deploy gate behaves. **Do not model real code on
this** — every "finding" below is an intentional plant.

## Run it

From this directory (or point `ephpm analyze` at the path):

```console
$ ephpm analyze .
```

`ephpm analyze` reads `.ephpm-analyze.yml` here (or falls back to built-in
defaults), runs the configured analyzers, prints a report, and **exits non-zero
when the verdict trips the gate** — so it drops straight into a CI or deploy
step.

## What you get

With the bundled config (`profile: security`, `fail_on: quarantine`), a run on a
box where the external tools aren't installed looks like this:

```text
  composer-audit   skipped: no composer.lock in target (audit needs a lockfile)
  semgrep-php      skipped: `semgrep` not found on PATH
  malware-yara     skipped: no ruleset configured (set analyzers.yara_rules)
  dangerous-sinks  completed (3 finding(s))
  suppression-scan completed (2 finding(s))

  [high]   dangerous-sinks/eval    public/index.php:18: call to eval() — dangerous sink [suspected]
  [high]   dangerous-sinks/system  public/index.php:13: call to system() — dangerous sink [suspected]
  [medium] dangerous-sinks/assert  src/Router.php:9:    call to assert() — dangerous sink [suspected]
  [medium] suppression-scan/tenant-suppression-attempt  src/Auth.php:7:  marker "nosemgrep" — inline suppressions are never honored
  [medium] suppression-scan/tenant-suppression-attempt  src/Auth.php:12: marker "@phpstan-ignore" — inline suppressions are never honored

score:   32
verdict: quarantine
  - score 32 >= quarantine threshold 10

$ echo $?
2
```

### Reading it

- **`dangerous-sinks`** flags the planted RCE-shaped calls — `system(...)` and
  `eval(...)` in `public/index.php`, and `assert(...)` in `src/Router.php`.
  These are `[suspected]` because `dangerous-sinks` is a fast *token* scan: it
  can match a sink name inside a comment or string. (Enable the opt-in
  `opcode-scan` analyzer — which compiles each file with ePHPm's embedded Zend
  engine and inspects the *opcodes* — to get the same hits as `[confirmed]`,
  with comment/string false positives eliminated.) Note what it correctly does
  **not** flag: `subsystem_boot()` (contains "system" but isn't the sink) and
  `Auth::assert_state()` / `$this->assert_state()` (a method, not the bare
  builtin).
- **`suppression-scan`** reports the `// nosemgrep` and `@phpstan-ignore` markers
  in `src/Auth.php`. ePHPm-analyze **never honors inline suppressions** — it
  reports the *attempt* instead. Only operator config (`suppress:`) can waive a
  finding.
- **`skipped`** lines are graceful degradation: an analyzer whose tool/config is
  absent is skipped, not failed (unless you list it under `analyzers.required`).
  Install `composer` + a lockfile, `semgrep`, and a `yara` ruleset to light those
  up; add `analyzers.enable: [phpstan, psalm-taint, progpilot]` for the opt-in
  security wrappers.
- **The verdict** comes from the weighted score (2 high + 3 medium = 32) crossing
  the `quarantine` threshold (10). With `fail_on: quarantine`, that makes the
  command exit **2** — the gate blocks the deploy.

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | verdict below `fail_on` — clean/allowed |
| `2`  | verdict is `quarantine` (and `fail_on` ≤ quarantine) |
| `3`  | verdict is `deny` (hard-deny, or above `deny_score`) |
| `1`  | the analysis itself errored (a required analyzer failed, bad config) |

## Try the knobs

```console
$ ephpm analyze . --fail-on deny        # report-and-warn: only a hard deny fails the build
$ ephpm analyze . --level 3             # strict: quarantine any medium+ finding
$ ephpm analyze . --format sarif        # SARIF v2.1.0 for code-scanning dashboards
$ ephpm analyze . --baseline base.json  # write a baseline; later runs gate only on NEW findings
$ ephpm analyze . --since origin/main   # diff-aware: scan only files changed vs a git ref
```

See the full reference at [`reference/cli/analyze`](/reference/cli/analyze/).
