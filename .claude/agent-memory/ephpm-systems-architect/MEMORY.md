# Memory Index

- [exit() is invisible after php_execute_script](php-exit-not-observable-after-execute-script.md) — run PHP's own compile/execute/inspect loop when a request executes more than one script, or short-circuits silently fall through
- [PHP middleware lane shape](php-middleware-lane-shape.md) — `php:` mounts run inside the app's request (not their own); two-phase chain, measured costs, why the phase boundary can't be fixed
- [Windows per-request chdir costs ~55us](windows-chdir-per-request-cost.md) — never add a cwd save/restore to the request path unmeasured; measure with a no-op version to split fixed from variable cost
- [Shared target dir replays stale rlibs](shared-target-stale-fingerprint.md) — "method not found" on a symbol you just added is a cache lie; touch the file, never `cargo clean`
- [Windows PHP-linked local builds](windows-php-linked-local-build.md) — vcvars64 + LIBCLANG_PATH + 8.5.7 SDK; test binaries need `/FORCE:MULTIPLE`; CI only `cargo check`s this lane
