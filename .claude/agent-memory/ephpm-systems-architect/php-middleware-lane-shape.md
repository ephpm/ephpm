---
name: php-middleware-lane-shape
description: Why `php:` middleware runs inside the app's PHP request rather than its own, what that costs, and the two-phase chain that follows from it
metadata:
  type: project
---

The `php:` middleware lane (PR #382, experimental) runs a mount **inside the
same PHP request as the application script**, immediately before it — not as
its own `ephpm_execute_request`.

**Why:** `ephpm_execute_request` is a full php-fpm-shaped cycle (lazy
`php_request_shutdown`, `php_request_startup`, superglobal construction,
per-request INI apply, exec-timer arm, capture). A mount with its own cycle
would pay all of that per mount per request, could not see `$_POST` (the
native chain runs before the body read), and would have its state torn down
before the app script started. Running in-request makes the mount inherit the
request's superglobals, `open_basedir`, temp/session paths, OPcache vhost,
per-site DB session, KV keyspace, execution timer and crash guard **by
construction** — there is no second PHP context to configure, so none to get
wrong. That is also the entire multi-tenancy story: paths resolve against the
request's own document root, so a mount has no more reach than that tenant's
`index.php`.

**The unavoidable consequence:** a `php:` mount runs *after* the body is read
and after every native module. It cannot reject before the body transfer,
which is the main reason the native lane exists. The two lanes are two
**phases**; `order` sorts within a phase and physically cannot interleave
them. Do not "fix" this — no ordering can put PHP before the body read.

**Measured cost** (Windows, PHP 8.5, release, OPcache on, HTTP, best of 11 x
1000 keep-alive requests): baseline 365.7 us; +3.5 us for a Rust builtin
mount; **+11.9 us for one `php:` mount**; +21.8 us for three (~7 us/mount).
The Rust builtin's marginal cost measured in isolation is **476 ns**. So a PHP
mount is ~20x a builtin and ~3% of a trivial request. An *empty* middleware
file costs the same as a working one — the cost is engaging the lane, not the
PHP in it.

**How to apply:** reuse this shape for any future "run extra PHP around the
request" feature (response cache warmers, per-vhost preload hooks): prepend
inside the live request, never a second request. When someone asks why `php:`
mounts cannot rate-limit before an upload, the answer is the phase boundary,
not a missing feature. See
[[php-exit-not-observable-after-execute-script]] for the engine trap this
design walks into, and [[windows-chdir-per-request-cost]] for the perf trap.
