---
name: add-config-knob
description: Checklist for adding or changing an ephpm config field so it can never become a silent no-op. Use whenever a PR adds a key to ephpm-config, changes a default, or defers a knob's implementation.
---

# Add a config knob (no-silent-no-ops checklist)

Every item, same PR. History: `[server.timeouts].idle` shipped parsed-but-discarded and `[php].workers` shipped never-read; both were documented as working (fixed in #106).

1. **Field** in the right struct in `crates/ephpm-config/src/lib.rs` with `#[serde(default = "...")]` and a `default_*` fn.
2. **Enforcement**: code actually reads the field and changes behavior. Grep for the field name outside ephpm-config - if the only hit is the struct, it's a no-op.
   - If implementation is genuinely deferred: the doc comment MUST say `Planned: not yet implemented - parsed but not acted upon` (established phrasing, grep for it), AND startup must `tracing::warn!` when the knob is set to a non-default value.
3. **Doc comment** (`///`): semantics, unit, default, and what `0`/absent means. Units in the name where ambiguous (`ttl_secs` not `ttl`).
4. **Reference docs row** in `site/content/reference/config.md` - key, type, real default, one-line semantics. If it affects guides, update them too.
5. **Default-choice review**: think about worst-case, not typical. A CPU-count default for a concurrency cap starved the whole server when PHP blocked (the `workers` lesson: blocking tasks can't be cancelled; permits/threads held past the request timeout). Prefer "0 = unlimited/disabled, opt into limits explicitly" unless there's a strong reason.
6. **Resource-sharing check** for anything limiting concurrency/pools: never cap a shared resource (tokio blocking pool, connection pool) to enforce a per-subsystem limit - use a dedicated semaphore/limiter scoped to the subsystem.
7. **Tests**: default value test in ephpm-config; behavior test where enforced (ephpm-config tests must pass with `--test-threads=1` - they mutate global env vars).
8. **Env override sanity**: the key is reachable as `EPHPM_<SECTION>__<FIELD>` (double underscore). Mention it in docs only if users will need it.
9. **Security defaults**: if the knob gates isolation/auth, remember serde section-level defaults - a `#[derive(Default)]` on the parent struct can silently zero your carefully chosen field default when the whole section is absent (the `[server.security]` lesson). Test both "section present" and "section absent".
