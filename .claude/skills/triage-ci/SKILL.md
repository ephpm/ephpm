---
name: triage-ci
description: Diagnose a failing ephpm CI run (E2E, unit, deny, release). Use whenever a GitHub Actions check is red or queued-forever - it encodes the diagnostic ladder for crashes vs test failures vs runner/infra problems, and the self-hosted (ephemerd) runner fleet quirks.
---

# Triage a failing ephpm CI run

Work the ladder top-down. Each step either identifies the failure class or rules it out.

## 1. Read the failure summary FIRST (bottom of the E2E job log)

`cargo xtask e2e` prints, as the LAST lines on failure (added in #105):
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
- E2E runs are serialized repo-wide via a concurrency group - "pending" E2E often just means another run holds the lock.

## 5. Repo-wide sudden failures

If Cargo Deny (or every PR) goes red simultaneously: new RUSTSEC advisory. `cargo update -p <crate>` on a tiny branch, merge first, then update other branches from main. Precedent: anyhow RUSTSEC-2026-0190 (#108); ignored-advisory precedent in `deny.toml` (proc-macro-error2).

## Known flakes
- `ephpm-config` tests mutate process-global `EPHPM_*` env vars - parallel runs flake; `cargo test -p ephpm-config -- --test-threads=1` is authoritative.
- sqlite e2e suite has a pre-existing parallel-isolation issue (shared table).
