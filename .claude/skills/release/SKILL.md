---
name: release
description: Cut and shepherd an ephpm release (tag, CI matrix, publish, verify). Use when the user wants to release a new version, an RC, or to rescue a stuck/partially-failed release run.
---

# Cut an ephpm release

## 1. Pre-flight (do NOT tag until all pass)

- `main` is green: latest CI **and E2E** runs on main succeeded (`gh run list --branch main --limit 4`).
- Runner fleet can serve the whole matrix: Linux x64 + arm64, **macOS arm64** (native runners), **Windows x64** (ephemerd). A dead leg doesn't fail the release - it parks it forever, because the `Create Release` job `needs:` every build job. See the `triage-ci` skill for runner checks.
- Docs version pins bumped on main first (grep `site/content/` + `docs/` + `examples/` for `ephpm/ephpm:vX` image pins and `ePHPm X.Y.Z` banners).
- Confirm the version with the user (patch vs minor is their call).

## 2. Tag

```bash
git tag -a vX.Y.Z <main-head-sha> -m "Release vX.Y.Z - <one-line summary>"
git push origin vX.Y.Z
```
The `v*` push triggers `.github/workflows/release.yml`. `EPHPM_RELEASE_VERSION` is derived from the tag (leading `v` stripped) and baked into `ephpm --version`.

## 3. What the matrix produces

- Binaries: `ephpm-vX.Y.Z+php<FULL>-<os>-<arch>.tar.gz` for linux-x86_64, linux-aarch64, macos-aarch64, windows-x86_64 x PHP pins (see `matrix.php` in release.yml; keep in sync with `xtask::PHP_SDK_VERSIONS`). Plus `SHA256SUMS`.
- Docker: `ephpm/ephpm:vX.Y.Z-php<FULL>`, `:vX.Y.Z-php<MINOR>`, `:<MINOR>`, `:vX.Y.Z`, `:latest` (rolling tags skip pre-releases; a `-` in the tag = prerelease, marked so on GitHub).
- `Create Release` publishes ONLY after build-linux + build-macos + build-windows + docker-image all succeed.

## 4. Monitor and rescue

```bash
gh run list --workflow=release.yml --limit 3
gh run watch <RUN_ID> --exit-status --interval 45   # background it
```
If legs fail: **fix the cause, then `gh run cancel <RUN_ID>` (if still running) and `gh run rerun <RUN_ID> --failed`.** Green legs (e.g. Linux + Docker) are reused; only failed/cancelled legs re-run. **Never re-tag to retry** - rerun-failed on the same run publishes the same tag. GitHub refuses rerun while the run is in progress, hence cancel-first.

Known leg-specific failures: see `triage-ci` (macOS llvm@17/libclang, Windows ephemerd runner version).

## 5. Verify the release (always, before announcing)

```bash
gh release view vX.Y.Z --json isDraft,isPrerelease,assets   # expect 13 assets (12 tarballs + SHA256SUMS)
gh release download vX.Y.Z --pattern "*php<DEFAULT_PHP>-linux-x86_64.tar.gz" --pattern "*windows-x86_64.tar.gz"
```
Smoke each downloaded binary: `ephpm --version` reports the tag; serve a one-line `<?php echo "PHPOK ".PHP_VERSION;` docroot and curl it (Linux binary is static musl - run it in `alpine:3.21` via podman; Windows runs natively). Expect HTTP 200 + `PHPOK <php-version>`.

## 6. After

- Update any remaining guide pins to the new version (straight to main per repo owner's preference, or PR).
- If this was a stable release, confirm `:latest`/`:<minor>` Docker tags moved.
