# Sessions

The documentation for ePHPm's native `ephpm` session save handler now lives on
the published site, at [`site/content/guides/sessions.md`](../../site/content/guides/sessions.md)
(rendered at `/guides/sessions/`).

That page covers how to enable it (it is **not** on by default, and a system
`php.ini` is not read, so use `[php] ini_overrides`, `[php] ini_file` or
`ini_set()`), the per-session locking, TTL and eviction, framework
compatibility, cluster behaviour, and the per-request-mode known issue that
makes `session_write_close()` important on releases through v0.11.2.

The implementation is the "Native session save handler" block in
`crates/ephpm-php/ephpm_wrapper.c`.
