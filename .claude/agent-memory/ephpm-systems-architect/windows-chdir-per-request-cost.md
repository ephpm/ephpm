---
name: windows-chdir-per-request-cost
description: VCWD_GETCWD + VCWD_CHDIR_FILE + VCWD_CHDIR costs ~55us per request on Windows — never add a cwd save/restore to the request path without measuring
metadata:
  type: project
---

One `VCWD_GETCWD` + `VCWD_CHDIR_FILE` + `VCWD_CHDIR` triple on the PHP request
path measured **~55 us per request on Windows** — roughly 5x the cost of
executing a whole extra OPcache-cached PHP script (~7-12 us), and ~15% of a
trivial request end-to-end.

**Why:** added to the `php:` middleware chain (PR #382) to mirror
`php_execute_script`'s chdir and match `auto_prepend_file` semantics exactly.
Benchmarking showed an *empty* middleware file cost the same as a working one,
which is what exposed it: the cost was fixed per request, not per mount. An
A/B behind a temporary env switch confirmed the chdir was ~55 of the ~60 us.
It was removed; a relative `include`/`require` still resolves against the
including file's own directory (PHP checks that before the cwd), so only bare
relative paths to `fopen()` etc. are affected — documented as "use `__DIR__`".

**How to apply:**
* Treat any per-request cwd manipulation on Windows as expensive until proven
  otherwise. `VCWD_CHDIR_FILE` does realpath expansion plus a
  `SetCurrentDirectory` syscall.
* More generally: when a per-request feature's cost does not scale with the
  work it nominally does, the cost is fixed overhead somewhere else. Measure
  with an empty/no-op version of the feature to separate fixed from variable
  before optimising the wrong thing.
* `php_execute_script` still does its own chdir for the application script, so
  application behaviour is unaffected by not doing one ourselves.
