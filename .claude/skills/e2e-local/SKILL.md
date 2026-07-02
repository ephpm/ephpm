---
name: e2e-local
description: Reproduce ephpm E2E test failures locally against a containerized release build (podman), without the Kind/Tilt cluster. Use when CI E2E is red and you need fast iteration, or to verify a server-side fix before pushing.
---

# Run E2E tests against a local container

The CI E2E harness is Kind + Tilt (`cargo xtask e2e`), but 90% of failures reproduce against a plain container plus the pre-built test binaries. Iteration: ~10 min image build, then seconds per test run.

## 1. Build the release image

```bash
podman build -f docker/Dockerfile -t ephpm:<tag> .        # full PHP-linked release build
podman tag docker.io/library/ephpm:<tag> localhost/ephpm:<tag>   # REQUIRED: podman run resolves localhost/, docker-style builds land in docker.io/library/
```
The image bakes `tests/docroot/` -> `/var/www/html` and `tests/ephpm-test.toml` -> `/etc/ephpm/ephpm.toml`.

## 2. Run it

```bash
podman run -d --name ephpm-verify -p 18088:8080 \
  -e EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED=true \
  localhost/ephpm:<tag>
```
Wait for ready: curl `http://127.0.0.1:18088/` until non-000. Config overrides use `EPHPM_` + double-underscore nesting.

## 3. Point the test binaries at it

The e2e crate is workspace-excluded; its tests are plain HTTP clients driven by env vars:
```bash
export EPHPM_URL="http://127.0.0.1:18088"
export EXPECTED_PHP_VERSION=<php full version>    # phpinfo suite fails without it
# pre-built binaries (from a previous cargo test --no-run) live at:
crates/ephpm-e2e/target/debug/deps/<suite>-<hash>.exe --test-threads=4
```
Pick the newest binary per suite name. Server-side fixes do NOT require rebuilding the test binaries - only the image.

## 4. Crash / regression verification loop

After each suite, check the server survived:
```bash
podman inspect -f 'running={{.State.Running}} restarts={{.RestartCount}}' ephpm-verify
```
For crash bugs, run the full suite list 3 passes and require `running=true restarts=0` throughout AND rc=0 per binary. The `kv`/`concurrency` suites are the historical SIGSEGV triggers; `http` covers `$_POST`, `php` covers status-code leaks.

## Known local-only artifacts (not real failures)

- `etag_cache::php_etag_different_query_strings_independent` fails on pass 2+ against a long-lived container: the KV etag cache persists across passes; CI uses a fresh container. Restart the container for a clean pass.
- `phpinfo::php_version_matches` needs `EXPECTED_PHP_VERSION` exported.
- PHP etag 304 tests need `EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED=true` (default off, and the baked toml doesn't set it).
- sqlite suites have a pre-existing parallel-isolation issue (shared table) - compare against a v-tag image before blaming your change.
- vhost/multi-tenant tests (security_p0, vhosts) need site dirs provisioned inside the container:
  `podman exec ephpm-verify sh -c 'mkdir -p /var/www/sites/<host> && ...'` and requests sent with `-H "Host: <host>"` (the baked toml trusts `basedir-a.test`, `site-a.preview.ephpm.dev`, etc.).

## Full-fidelity fallback

If the failure needs the real cluster (readiness gates, sqld, gossip): `cargo xtask e2e` (Kind + Tilt, needs podman machine + privileged dind on ephemerd hosts). On failure it dumps pod logs, cluster diagnostics, container exit codes, and a FAILED-tests summary at the end of the output.
