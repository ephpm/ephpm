# `ephpm forge` — Build-Time Composition of PHP Extensions and Middleware

> **Status: DESIGN ONLY. Nothing on this page is implemented or shipped.**
> This document describes a proposed build pipeline. Where it references
> existing code or the open PR #88, that is labeled explicitly; everything
> else is future work.
>
> **Superseded for the common case (3.0 dynamic pivot):** Linux releases
> are now **glibc-dynamic** (`<arch>-unknown-linux-gnu`, `--export-dynamic`),
> so runtime `dlopen` loading of shared PHP extensions (`[php] extensions`,
> see `site/content/guides/php-extensions.md`) and middleware `.so` files
> works out of the box — the "static by mandate" premise below no longer
> describes the shipped product. `forge` remains relevant **only** as
> future tooling for users who want a custom fully static binary (where
> dlopen is genuinely unavailable) with extra extensions/middleware baked
> in at build time.

## 1. Problem and constraint

> *Historical premise — see the superseded note above. Kept because it
> still applies to self-built fully static musl binaries.*

ePHPm's original release binaries were **fully static by mandate** (musl,
`crt-static`): one file, runs anywhere, no loader, no shared-library
surface. It is empirically proven that a fully static musl binary
**cannot `dlopen()` anything** — the middleware dlopen lane fails at
startup with `Dynamic loading not supported`, and the same fate awaits any
`extension=`-based shared PHP extension load.

For such a binary, runtime loading cannot be the extensibility mechanism.
The mechanism must be **build-time composition**, the way
[xcaddy](https://github.com/caddyserver/xcaddy) composes Caddy: the user
declares what they want, a tool generates a thin package that imports the
plugins, and an ordinary compiler run produces a custom static binary.
ePHPm needs that for **two axes at once**:

- **(a) extra PHP extensions** — baked into `libphp.a` by
  static-php-cli (spc);
- **(b) extra middleware** — Rust crates implementing
  `ephpm_middleware::Middleware`, linked as rlibs and registered into the
  static builtin registry (`crates/ephpm-server/src/middleware.rs::builtin`,
  shipped in PR #123).

The dlopen lanes stay as escape hatches for dynamically linked builds;
they are not this document's subject.

## 2. What exists today (verified in source)

### 2.1 The stock release pipeline

`cargo xtask release` (`xtask/src/main.rs`):

- `PHP_SDK_VERSIONS` (line ~12) pins one full PHP version per minor
  (8.3/8.4/8.5). `ensure_php_sdk_for()` downloads
  `php-sdk-<ver>-<os>-<arch>[-gnu].tar.gz` from `github.com/ephpm/php-sdk`
  releases into `php-sdk/<ver>-<os>-<arch>[-gnu]/` (the `-gnu` libc suffix
  applies on Linux) and `release_native()` builds
  `-p ephpm --target <arch>-unknown-linux-gnu` with `PHP_SDK_PATH` set;
  `SQLD_BINARY_PATH` embeds sqld the same way.
- The SDK's extension set is **fixed at php-sdk build time** (~45
  extensions listed in the roadmap doc). Changing it today means: edit
  `PHP_EXTENSIONS` in the php-sdk repo workflow → cut an SDK release →
  bump the pin in xtask → rebuild. No per-user variation exists.

### 2.2 The static middleware registry (PR #123, this branch)

- `ephpm-middleware-builtins` holds the four in-tree middleware as plain
  Rust types; `ephpm-server` links it as an rlib and maps names via
  `fn builtin(name) -> Option<BuiltinBuilder>`; execution goes through
  `ephpm_middleware::builtin::BuiltinModule` (no FFI, works fully static).
- **Hard-won constraint:** the C-ABI `declare!` exports
  (`ephpm_middleware_init` etc.) are identical `no_mangle` symbols in
  every module cdylib. Linking two such crates into one binary is a
  guaranteed duplicate-symbol link error (observed: MSVC LNK2005; GNU ld
  equivalent). **Third-party middleware intended for static composition
  must therefore be consumed as rlibs exposing a `Middleware` type — never
  through `declare!`.** The in-tree crates already model this split
  (implementation crate + cdylib shell).

### 2.3 PR #88 (open): "ext build CLI + Ubuntu builder"

Studied via `gh pr view/diff 88`. It contains, working end-to-end on
x86_64 and aarch64 (per its `builder-smoke.yml` CI and its PR body's
tested list: redis, intl, imagick+full codec chain, grpc+abseil):

- `crates/ephpm/src/commands/ext.rs` (~577 lines, new):
  - `ephpm ext list|info` — introspection by shelling out to
    `<self> php -m` / `php -r`.
  - `ephpm ext search` — a **hardcoded snapshot** (`SPC_REGISTRY`, ~95
    entries with per-OS support flags) of the spc extension catalog.
  - `ephpm ext build --add X --suite core|wordpress|laravel|full` —
    validates names (anti-injection), detects podman/docker, auto-builds
    the builder image from `docker/Dockerfile.builder` if neither a local
    tag nor `ghcr.io/ephpm/builder:<ver>` exists, then runs the container
    with `EXTENSIONS=...`, an spc cache volume (`ephpm-spc-cache:/build`)
    and the current checkout mounted at `/src/ephpm`.
- `docker/Dockerfile.builder`: Ubuntu 24.04 (glibc host so
  bindgen/libclang work — Alpine ruled out precisely because static musl
  blocks the dlopen *libclang* needs), rustup + per-arch musl target, spc
  nightly binary, musl.cc cross toolchain in `/opt/<arch>-linux-musl-cross`.
- `docker/builder-entrypoint.sh`: `spc doctor --auto-fix` → `spc download
  --with-php=$PHP_VERSION --for-extensions=$EXTENSIONS` → `spc build
  $EXTENSIONS --build-embed --enable-zts` → `PHP_SDK_PATH=/build/buildroot
  cargo build --release -p ephpm --target <arch>-unknown-linux-musl` →
  copy binary to `/output`.
- `crates/ephpm/build.rs` hardening for arbitrary C++ extensions:
  musl-native `libstdc++.a` from musl.cc pulled in with
  `--whole-archive`; `libc.a`/`libm.a`/`libgcc.a` moved *inside* the
  `--start-group` (single-pass ld can't resolve mid-group references
  otherwise); a superset static-lib list (ImageMagick codec chain, gRPC,
  protobuf, ~90 abseil sublibs) guarded by existence checks so each build
  links only what spc actually produced.

What PR #88 does **not** have: a manifest (imperative flags only), any
middleware story, ePHPm version pinning (it builds whatever checkout is
mounted), sqld embedding in the container build, macOS/Windows lanes, or
a published `ghcr.io/ephpm/builder` image (the CLI probes the tag, but no
workflow pushes it).

## 3. Proposed design

### 3.1 One command, one manifest

A new top-level command, **`ephpm forge`** (working name — it builds
custom binaries in a controlled fire; `ephpm ext build` from PR #88
becomes an alias/subset). Input is either xcaddy-style flags or a
manifest, `forge.toml`:

```toml
# forge.toml — everything pinned, reproducible
[build]
php      = "8.5"                       # minor → resolved to the pinned full version
ephpm    = "v0.2.0"                    # tag/rev of ephpm itself to build
target   = "x86_64-unknown-linux-musl" # default: host arch, musl

[extensions]
suite = "wordpress"                    # PR #88's core|wordpress|laravel|full
add   = ["imagick", "grpc"]

[[middleware]]
crate  = "ephpm-geoip"
source = { git = "https://github.com/acme/ephpm-geoip", rev = "abc123" }
# or: source = { version = "1.2" }  (crates.io) / { path = "../geoip" }
```

Flag form mirrors xcaddy: `ephpm forge --with-ext imagick
--with-mw git=https://github.com/acme/ephpm-geoip@abc123 --output ./ephpm-custom`.

### 3.2 Lane (a): PHP extensions

Two resolution strategies, tried in order:

1. **Prebuilt variant matrix** (new php-sdk work): the php-sdk repo grows
   a small matrix of *suite variants* per release —
   `php-sdk-<ver>-<os>-<arch>-<suite>.tar.gz` for `core`, `wordpress`,
   `laravel`, `full` (suites exactly as defined in PR #88's
   `suite_extensions()`). `forge` canonicalises the requested extension
   set (sorted, deduped, hashed); if it equals a published variant, the
   SDK is just downloaded — seconds, not the ~30-60 min spc build. The
   cache key extends `php_sdk_dir_for()` to
   `php-sdk/<ver>-<os>-<arch>-<set-hash>/`.
2. **On-demand spc build**: anything else runs PR #88's builder container
   essentially verbatim (`Dockerfile.builder` + entrypoint), except the
   entrypoint is split: it stops after `spc build --build-embed
   --enable-zts` and exports `/build/buildroot` as an SDK directory. The
   Rust link step moves out of the entrypoint and into the common
   composition step (3.4), because middleware composition changes *what*
   gets cargo-built.

The ZTS constraint from the roadmap doc applies unchanged: only ZTS-safe
extensions are eligible; `SPC_REGISTRY` grows a `zts` flag and `forge`
refuses known-NTS-only extensions instead of shipping a crashy binary.

### 3.3 Lane (b): middleware — the generated crate

Exactly xcaddy's move, in cargo terms. `forge` generates a scratch
workspace member (under `target/forge/<hash>/`):

```
ephpm-forge-registry/
├── Cargo.toml      # deps: ephpm-middleware, + every [[middleware]] crate (rlib)
└── src/lib.rs      # generated:
                    # pub fn extra(name: &str) -> Option<BuiltinBuilder> {
                    #     match name {
                    #         "geoip" => Some(BuiltinModule::init::<ephpm_geoip::GeoIp>),
                    #         ...
                    #         _ => None,
                    #     }
                    # }
```

Registration seam in `ephpm-server` (new, small): `builtin()` first
consults a process-global extra table. Two candidate wirings:

- **Preferred: `linkme` distributed slice.** `ephpm-server` declares
  `#[distributed_slice] pub static EXTRA_BUILTINS: [(&str, BuiltinBuilder)]`;
  the generated crate (or the middleware crates themselves, via a tiny
  `register_builtin!(name, Type)` macro in `ephpm-middleware`)
  contributes entries. Merely depending on the crate registers it —
  the closest analogue of xcaddy's "import = plugged in". Cost: a new
  dependency (`linkme`), MSRV/platform check needed (it supports the
  tier-1 targets and musl; verify against Rust 1.85 before committing).
- **Fallback: generated `fn main`.** `crates/ephpm` exposes its `main` as
  `ephpm::run(extra: Option<ExtraRegistry>)`; forge generates a two-line
  binary crate `ephpm-custom` calling
  `ephpm::run(Some(ephpm_forge_registry::extra))`. No new dependency, but
  requires the binary crate to become a lib+bin pair.

Either way, third-party middleware authors publish an ordinary rlib crate
with a `pub struct X; impl Middleware for X` — the same trait the four
in-tree modules implement — and optionally the cdylib shell for the
dynamic lane. `declare!` must NOT be in the rlib path (see §2.2); docs
and a `cargo forge check` lint enforce it (reject crates whose rlib
exports `ephpm_middleware_init`).

### 3.4 Composition: one build

```
forge.toml
   │  resolve + pin (php full version, ephpm rev, middleware revs)
   ▼
[1] SDK: variant download ──or── spc container build   → PHP_SDK_PATH
[2] generate ephpm-forge-registry (+ ephpm-custom if fallback wiring)
[3] sqld: download_sqld() as today                      → SQLD_BINARY_PATH
[4] cargo build --release -p ephpm-custom --target <musl>
      (inside the builder container on Linux — same toolchain that
       built libphp.a; natively on macOS)
   ▼
single static binary + build-info.json
```

`build-info.json` is the manifest already designed in
`dynamic-extensions.md` (php version, ZTS, API numbers, target, spc
version) **extended with** the compiled-in middleware list
(name/crate/rev) so `ephpm forge verify ./bin` and support requests can
see exactly what a custom binary contains.

### 3.5 Caching and pinning

- spc artifacts: keep PR #88's named volume (`ephpm-spc-cache:/build`);
  spc's own download cache makes rebuild-with-one-more-extension
  incremental.
- SDKs: content-addressed by `(php, os, arch, ext-set-hash)` as in §3.2.
- Cargo: the generated workspace gets a committed-to-output `Cargo.lock`;
  `forge.lock` records the resolved php full version + all git revs so a
  re-run is byte-comparable.
- ePHPm rev: forge builds from a git checkout of the pinned `[build].ephpm`
  rev (not "whatever is in cwd" — PR #88's main reproducibility gap).

### 3.6 CI implications

- **Publish the builder image**: a workflow pushing
  `ghcr.io/ephpm/builder:<version>` per release (PR #88's CLI already
  prefers it; today the tag 404s and every user pays the image build).
- **php-sdk suite variants**: extend the php-sdk repo's matrix
  (version × os × arch × suite). Watch the known spc Linux
  `lib/lib/libphp.a` double-nesting bug — the workaround lives in
  php-sdk's build.yml and must apply to variants too.
- **Smoke**: extend PR #88's `builder-smoke.yml` with one forge run that
  adds a sample out-of-tree middleware and asserts (1) the binary is
  static, (2) `ext list` shows the extension, (3) a request hits the
  composed middleware. Self-hosted x64+arm64 runners already exist.

## 4. Phasing (realistic)

| Phase | Scope | Builds on |
|---|---|---|
| 0 | Land PR #88 (rebased): `ext list/info/search/build`, builder image, smoke CI. Publish `ghcr.io/ephpm/builder`. | PR #88 as-is |
| 1 | Registration seam in `ephpm-server` (linkme or `run(extra)`), `register_builtin!` macro, docs for rlib-style middleware authoring. Small, pure-Rust. | PR #123 registry |
| 2 | `ephpm forge` + `forge.toml`: generated registry crate, pinned checkout, composition build reusing the Phase-0 container for the SDK **and** the final link. | 0 + 1 |
| 3 | php-sdk suite-variant matrix + variant selection fast path; `forge.lock`; `build-info.json` + `forge verify`. | 2 |
| 4 | Graduate docs out of roadmap; optional macOS lane (native, no container); catalog of known-good third-party middleware crates. | 3 |

## 5. Reusability verdict on PR #88

Directly reusable (~80% of its non-lockfile diff): `Dockerfile.builder`
and the musl.cc/spc toolchain choices; the entrypoint's
spc-download→build→embed sequence (needs splitting at the SDK boundary);
the whole `ext.rs` surface — name validation, suites, engine detection,
image resolution/auto-build, `SPC_REGISTRY` (needs a ZTS flag and a
drift-check against spc); `crates/ephpm/build.rs` C++ static-link
hardening (prerequisite for imagick/grpc-class extensions); the smoke
workflow. Not reusable / missing: no manifest or pinning, no middleware
axis, entrypoint couples SDK build to the final cargo build, registry
image unpublished.

---

## Settled direction: toolchain distribution (2026-07-06)

Decision from design review (static-only releases are a hard constraint;
"container has the tools" UX preferred, but it must work on macOS and
Windows, where extracting a Linux builder image is a non-starter — OCI
images hold platform-specific binaries, and Docker Desktop only runs them
inside a hidden Linux VM):

**Per-OS toolchain bundles published as OCI artifacts** (ORAS — the registry
as a CDN, not a runtime): `ghcr.io/ephpm/toolchain:<ver>-<os>-<arch>`
containing spc, LLVM/libclang, and the auxiliary build tools for that
platform. `ephpm forge` pulls the host-platform artifact, extracts to
`~/.ephpm/toolchain/<digest>/`, prepends it to PATH for the build, and
deletes it afterwards when `--ephemeral` is set (default keeps the
digest-keyed cache; bundles are ~1–2 GB).

Per-OS notes:
- **Linux** — bundle path AND the true-container backend (`podman run` the
  builder image) both supported; container stays the convenience default
  where a runtime exists.
- **Windows** — MSVC Build Tools are not redistributable and not needed:
  the bundle ships clang-cl + lld-link, and `xwin` fetches the MSVC CRT +
  Windows SDK from Microsoft's CDN on first use (cached; same packages the
  VS installer downloads).
- **macOS** — Apple CLT (system SDK, ld) is not redistributable: it stays a
  one-time `xcode-select --install` prerequisite, checked by
  `cargo xtask doctor`. Everything else rides in the bundle; extraction
  clears quarantine attributes (or the bundle is signed).

`cargo xtask doctor` (shipped) is the preflight for exactly the two
non-redistributable prerequisites plus the local toolchain; forge reuses it.
Container-centric sections above should be read through this lens: the
builder image remains the Linux packaging of the same bundle contents.
