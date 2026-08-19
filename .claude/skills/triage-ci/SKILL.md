---
name: triage-ci
description: Diagnose a failing ephpm CI run (E2E, unit, deny, release). Use whenever a GitHub Actions check is red or queued-forever - it encodes the diagnostic ladder for crashes vs test failures vs runner/infra problems, and the self-hosted (ephemerd) runner fleet quirks.
---

# Triage a failing ephpm CI run

Work the ladder top-down. Each step either identifies the failure class or rules it out.

## 0. No logs? Read the job's *annotation* before assuming a test failed

The fleet regularly returns `BlobNotFound` / `log not found` for a failed job. That absence is itself a signal, and there is a second channel that still works:

```bash
gh api repos/ephpm/ephpm/actions/runs/<RUN_ID>/jobs \
  --jq '.jobs[] | .name, ([.steps[] | "  \(.number). \(.name) => \(.conclusion//"null")"] | join("\n"))'
gh api repos/ephpm/ephpm/check-runs/<JOB_ID>/annotations --jq '.[] | "\(.annotation_level): \(.message)"'
```

**A step with `conclusion: null` and `completed_at: null` did not fail — it never finished.** GitHub only records a step conclusion when the runner reports one, so a null conclusion on the step that was executing means the runner process died underneath it. A genuine test failure always leaves `conclusion: failure` on that step plus a completed `Complete job` step.

- **Known signature — runner death**: annotation reads `The self-hosted runner lost communication with the server. Verify the machine is running and has a healthy network connection. Anything in your workflow that terminates the runner process, starves it for CPU/Memory, or blocks its network access can cause this error.` This is **infra, not your commit.** Missing logs follow from it: the runner never lived to upload them.
- Corroborate before believing it — the cheap checks, in order: does the same tree pass on other matrix legs? Did the PR's own run of the identical tree pass? How many heavy jobs were on the fleet in that window (`gh run list --workflow=e2e.yml --limit 15` and compare `created_at`)? Precedent (2026-08-19): four commits landed on main in 74 seconds, twelve E2E legs hit the fleet at once, and the last run's 8.3/8.5 legs died this way on two attempts while its 8.4 leg and all nine earlier legs passed. The commit was innocent; `.github/workflows/e2e.yml` now carries a concurrency group to cap the pile-up.
- Do **not** start bisecting a commit until this step rules runner death out. "Deterministic across re-runs" is not evidence of a code bug when the fleet stays saturated or degraded between attempts.

## 1. Read the failure summary FIRST (bottom of the E2E job log)

Default path is `cargo xtask e2e` (bare-process). On failure it dumps each node's stderr file and prints a `==== FAILED E2E SUITES (bare-process) ====` block with the per-suite names.

Opt-in Kind path is `cargo xtask k8s-e2e` (dispatched via `.github/workflows/k8s-e2e.yml`). It prints, as the LAST lines on failure (added in #105):
- `==== FAILED E2E TESTS ====` - the extracted `... FAILED` / `panicked at` lines. Read these before scrolling anywhere else.
- `--- ephpm pod: container exit code / signal ---` + a loud banner if the server crashed.

Fetch logs:
```bash
gh run view --job <JOB_ID> --log-failed          # only after the whole run completes
gh api repos/ephpm/ephpm/actions/jobs/<JOB_ID>/logs > tmp_job.log   # works while run is in progress
```
Never grep with `-E/-i/-v` flags through the PowerShell-backed shell (flags get eaten) - dump to a file and use the Grep tool.

## 2. Classify by exit code / pod state

| Signal | Meaning | It is a... |
|---|---|---|
| pod `RESTARTS>0`, exit `139` (=128+11 SIGSEGV) | server under test crashed | **server bug** (FFI/wrapper suspect first) |
| exit `137` (SIGKILL) | OOM-killed | resource bug or VM pressure |
| exit `134` (SIGABRT) | assertion/abort | server bug |
| pod healthy (`RESTARTS 0`, `Ready True`) + assertion diffs (`left: X right: Y`) | test-level failure | logic/regression |
| pod healthy + `error sending request` cascade + **readiness probe timing out mid-suite** | server alive but not answering | starvation (blocking pool / PHP worker cap / deadlock) |
| `NodeNotReady` / `MemoryPressure=True` in the pre-tilt baseline | cluster contention | infra, retry after checking the box |

Crash follow-up: on the current glibc (gnu) builds `backtrace()` works normally; on legacy/custom musl static builds it is a no-op stub - fall back to the container exit code, kernel `dmesg` (`error 4` = userspace read of unmapped memory = use-after-free), and `addr2line` on the unstripped binary to map the fault offset.

## 3. Job never starts (queued forever) = runner problem

The Windows/Linux runners are **ephemerd** JIT runners on Luther's box; macOS runs on native self-hosted runners (up to 4).

- Fleet state: `gh api 'repos/ephpm/ephpm/actions/runners?per_page=100&page=N'` - **paginate**; hundreds of `offline` ephemeral registrations are NORMAL. `offline` on an ephemeral runner means "not connected right now", not dead.
- ephemerd service log: `C:\ProgramData\ephemerd\ephemerd.log`. Per-runner logs: `C:\ProgramData\ephemerd\logs\<runner-name>-runner.log`.
- **Known signature - deprecated runner version**: ephemerd provisions a runner, it reaches "runner environment ready", then `runner exited exit_code=0` ~6s later without running the job; the per-runner log says `Runner version vX.Y.Z is deprecated and cannot receive messages`. The `ignoring duplicate queued event` spam in ephemerd.log is a *symptom* (dedup cache), not the cause. Fix: bump `RunnerVersion` in `~/ephemerd/mage/download/download.go`, delete the stale zip in `pkg/runner/embed/`, `mage build:windows`, stop service, swap `C:\Program Files\ephemerd\ephemerd.exe`, start service. A service restart alone does NOT help.
- macOS runner history: needed `brew install llvm@17` (release builds pin `LIBCLANG_PATH` to it; the workflow step masks a missing install with `|| true`) and a sandbox fix to allow loopback `bind()` (EPERM on `TcpListener::bind("127.0.0.1:0")` = seatbelt/launchd context, not a code bug).

## 4. Rerun rules

- `gh run rerun <RUN_ID> --failed` is refused while any job in the run is still running/queued. **Cancel first, then rerun-failed** - completed-successful jobs (Linux legs, Docker) are preserved, only failed/cancelled legs re-execute. This also applies to release runs: never re-tag to retry.
- E2E runs are serialized **per ref** via a concurrency group (`ephpm-e2e-${{ github.ref }}`) - "pending" E2E often just means an earlier run on that same ref holds the lock. Pushes to main queue (every commit keeps its own result, so CI history stays bisectable); pull-request runs cancel in progress when superseded.
- A re-run that dies the same way is not automatically a code bug. If step 0 said "lost communication", check the fleet box before re-running again - a host left short on memory or disk by an earlier burst will kill the next runner too. `.github/workflows/fleet-maintenance.yml` purges docker build cache/images on a node.

## 5. Repo-wide sudden failures

If Cargo Deny (or every PR) goes red simultaneously: new RUSTSEC advisory. `cargo update -p <crate>` on a tiny branch, merge first, then update other branches from main. Precedent: anyhow RUSTSEC-2026-0190 (#108); ignored-advisory precedent in `deny.toml` (proc-macro-error2).

## Known flakes
- `ephpm-config` tests mutate process-global `EPHPM_*` env vars. `crate::test_env` now serialises those mutations against every `Config::load`/`default_config`, so the suite is parallel-safe and `--test-threads=1` is no longer required (issue #235). If it flakes again, look for a test that touched the environment outside an `EnvVars` guard.
- sqlite e2e suite has a pre-existing parallel-isolation issue (shared table).
