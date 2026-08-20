# CLI conformance corpus

Differential test corpus for `cargo xtask cli-conformance`: every case is run
against **both** `ephpm php` and an upstream `php` CLI binary of the same
minor version, with an identical minimal environment, and stdout / stderr /
exit code are compared **byte-for-byte**. Any unexplained mismatch fails the
run. The runner lives in `xtask/src/cli_conformance.rs`; the nightly workflow
is `.github/workflows/cli-conformance.yml`.

This corpus complements (does not duplicate) the `crates/ephpm-e2e/tests/cli.rs`
regression suite: that suite asserts specific known-good behavior of `ephpm php`
alone; this corpus asserts *sameness* against the genuine php-cli.

## Case format

One case = one stem `NNN-name`:

| File | Meaning |
|------|---------|
| `NNN-name.php` | the script (optional when `.args` exists) |
| `NNN-name.args` | CLI args, one per line; `{SCRIPT}` → absolute `.php` path, `{TMPDIR}` → per-side scratch dir; full-line `#` comments and blank lines ignored. Default when absent: `{SCRIPT}` |
| `NNN-name.stdin` | bytes piped to stdin (default: empty; stdin is always closed after writing) |
| `NNN-name.meta` | markers: `skip = "reason"`, `xfail = "reason (#issue)"`, `normalize = [...]`, `no_default_ini = true` |

Numbering: `0xx` = language/runtime surface, `1xx` = CLI-specific behavior.

## Ground rules for cases

- **Deterministic only.** No wall-clock time, no unseeded randomness, no
  network, no filesystem writes outside `getenv('CONFORMANCE_TMPDIR')`
  (which is also the working directory).
- Unless `no_default_ini = true`, the runner injects `-n` on **both** sides
  so compiled defaults are compared, not distro php.ini contents.
- Environment during runs: `TZ=UTC`, `LC_ALL=C`, `LANG=C`, `HOME=<tmpdir>`,
  `CONFORMANCE_TMPDIR=<tmpdir>`, `CONFORMANCE_TEST_VAR=hello-conformance`,
  plus the parent `PATH`. Nothing else.
- **`xfail` is for real divergences** and must cite the tracking issue. When
  the divergence is fixed the case reports XPASS, which fails the run until
  the stale marker is removed — markers cannot rot silently.
- **`skip` is for comparisons that are meaningless by definition**
  (e.g. `-m` extension inventories). The reason string must say why.
- **`normalize` is a last resort** with a deliberately tiny registry
  (see `NORMALIZERS` in the runner). Every use must be justified in a
  comment in the `.meta` file. Never use a normalizer to hide a behavior
  difference — that is exactly what this harness exists to catch.

## Out of scope (do not add must-pass cases for these)

- `-S` (built-in web server) and `-a` (interactive shell): intentionally not
  supported by `ephpm php` — both print an honest refusal.
- Windows fatal-error conformance: tracked separately (#328); the nightly
  runs on Linux. The runner itself is platform-neutral so a Windows leg can
  be added once #328 lands.
