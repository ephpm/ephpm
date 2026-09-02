---
name: wsl-linked-build-and-bench
description: How to build/run php_linked tests from the Windows worktree inside WSL, the pre-existing php_middleware SIGSEGV on the Linux 8.5.7 SDK, and how to benchmark on this noisy shared box
metadata:
  type: reference
---

Building `php_linked` code on this Windows box is easiest via WSL Ubuntu —
no vcvars64/LIBCLANG/FORCE:MULTIPLE dance (that is the Windows-native lane,
see [[windows-php-linked-local-build]]):

```
wsl -d Ubuntu -u root -- bash -lc 'cd /mnt/c/Users/luther/ephpm/<worktree> \
  && export CARGO_TARGET_DIR=/root/ephpm-target \
     PHP_SDK_PATH=/mnt/c/Users/luther/ephpm/php-sdk/8.5.7-linux-x86_64-gnu \
  && cargo test -p ephpm-php --release --test <name> -- --test-threads=1'
```

- Work from the /mnt/c worktree; do NOT create another /root/<task>-src
  checkout (there are already ~40 of them wasting disk).
- Git operations on a Windows-created worktree must run from the Windows
  side (`.git` file holds a Windows gitdir path WSL git can't resolve);
  WSL only builds/runs.
- Quoting: `wsl -- bash -lc '...'` from PowerShell mangles `$(...)` and
  nested double quotes. Put anything non-trivial in a .sh file on /mnt/c
  and run `wsl -- bash /mnt/c/.../script.sh`.

**Pre-existing crash:** `cargo test -p ephpm-php --test php_middleware` on
the Linux 8.5.7 SDK SIGSEGVs on the FIRST test (verified on a clean
baseline commit, 2026-09-02) — same category as the sessions.rs failures on
Windows. Do not attribute it to your branch; `tests/perf_linked.rs` and
`tests/response_headers.rs` run fine in the same harness, and
`cargo xtask e2e` against the release binary is the reliable functional
gate.

**Benchmarking on this box:** other agents' builds/containers create load
spikes (observed 42us -> 166us on the same binary minutes apart). Never
compare two sequential builds' single runs. Stash each build's test binary
(`/root/perf-<tag>`, delete after), interleave runs round-robin, and take
min-of-N per build. Useful anchors (release, no OPcache, Linux, 2026-09):
trivial fpm request ≈ 42us baseline; $_SERVER Rust-side build+CString
conversion ≈ 4.4-6.1us before #133, ≈ 1.1-2.1us after.
