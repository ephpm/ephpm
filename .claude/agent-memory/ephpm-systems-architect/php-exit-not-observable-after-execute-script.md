---
name: php-exit-not-observable-after-execute-script
description: php_execute_script/zend_execute_scripts destroy the exit() signal — check EG(exception) yourself between zend_compile_file and zend_execute, or a short-circuit silently becomes a fall-through
metadata:
  type: project
---

`php_execute_script()` and `zend_execute_scripts()` **cannot tell you that a
script called `exit()`**. Both funnel every pending exception through
`zend_exception_error()`, which sets `EG(exception) = NULL` and returns
`SUCCESS` for PHP 8's unwind-exit. On return, "the script called `exit()`" and
"the script ran to completion" are byte-identical states.

Corollaries, both verified against php-src (`main/main.c`, `Zend/zend.c`) and
by running it:

* Any ePHPm code that runs **more than one script per request** and needs to
  short-circuit on `exit()` must run PHP's own loop body itself —
  `zend_compile_file` → `zend_execute` → inspect `EG(exception)` with
  `zend_is_unwind_exit` → then `zend_destroy_static_vars` / `destroy_op_array`
  / `efree_size`. That is what `ephpm_run_one_middleware` does.
* The existing `EG(exception) && zend_is_unwind_exit(...)` check *after*
  `php_execute_script(app_script)` in `ephpm_execute_request` is therefore
  effectively dead — `exit()` in an application script returns
  `EPHPM_EXEC_OK`, not `EPHPM_EXEC_SCRIPT_EXIT`. Harmless today (both deliver
  the captured response) but do not build on it.
* A second fail-open trap sits next to it: an uncaught `Throwable` is reported
  with `E_DONT_BAIL`, so execution returns **normally** and only
  `PG(last_error_type)` records it. A loop that does not check that mask will
  print a fatal and then keep going.
* A *parse* error is a thrown `ParseError`, not a bailout —
  `zend_compile_file` returns NULL with the exception still pending and
  `PG(last_error_type)` unset. It must be reported explicitly or the request
  answers 200 with an empty body and the exception leaks into shutdown
  functions.

**Why:** discovered building the `php:` middleware lane (PR #382). The first
implementation used `php_execute_script` per mount; its `exit()`
short-circuit tests failed with the application script's output appended to
the middleware's — i.e. a middleware that rejected a request still ran the app.

**How to apply:** whenever adding a code path that executes a PHP file outside
the single primary-script call, do not trust the return value or
`EG(exception)` after a php-src wrapper. Inspect the exception between execute
and teardown yourself, and treat `E_DONT_BAIL` fatals as a separate signal from
bailouts. See [[php-middleware-lane-shape]].
