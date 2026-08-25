---
name: shared-target-stale-fingerprint
description: The shared target dir replays stale rlibs across worktrees — a "method not found" on a symbol you just added is a cache lie; touch the source file, don't debug the code
metadata:
  type: project
---

In this repo every worktree shares one `target/` (`[build] target-dir` in the
root `.cargo/config.toml`). That cache periodically serves a **stale rlib** for
a path dependency, producing compile errors that contradict the source on disk.

Concretely, 2026-08-22: `cargo test -p ephpm-server --lib` failed with
`no method named php_script found for &ephpm_config::MiddlewareMount` while
`cargo test -p ephpm-config` compiled and passed tests that *call*
`php_script`, and `cargo clippy --workspace --all-targets` was green. Retrying
reproduced it identically. Fix was one line:

```powershell
(Get-Item crates\ephpm-config\src\lib.rs).LastWriteTime = Get-Date
```

after which the same command passed 384 tests.

**Why:** concurrent builds from sibling worktrees contend for the same
fingerprint database; a unit can be recorded fresh while its dependents keep a
stale view. The failure is indistinguishable from a real compile error, and
"green clippy, red test" is the tell.

**How to apply:**
* If a build error says a symbol you can *see* in the source does not exist —
  and especially if a different cargo command disagrees — bump the mtime of
  the defining file and re-run **before** editing anything.
* Never respond to this by deleting the cache (`cargo clean`, `rm -rf
  target/`): it is a 78 GB shared dir and a full rebuild costs tens of minutes
  for every agent using it.
* The inverse also happens: a stale cache can report **false green**. When a
  verdict matters (a gate before pushing), make sure the run actually
  recompiled the crate you changed.
