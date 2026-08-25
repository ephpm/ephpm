---
name: windows-php-linked-local-build
description: How to actually build and RUN php_linked code locally on this Windows box — vcvars64, LIBCLANG_PATH, the 8.5.7 SDK, and the /FORCE:MULTIPLE needed for test binaries
metadata:
  type: reference
---

CI's Windows PHP job only runs `cargo check -p ephpm-php` — it never links.
So linking and running `php_linked` code locally is untested territory and
needs three things this box does not provide by default:

1. **MSVC env**: `call "C:\Program Files (x86)\Microsoft Visual
   Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"` — without
   `INCLUDE`/`LIB` the build fails in bindgen and then in the linker.
2. **`LIBCLANG_PATH`** = `...\BuildTools\VC\Tools\Llvm\x64\bin` (there is no
   standalone LLVM install here; bindgen finds nothing otherwise).
3. **`PHP_SDK_PATH`** = `C:\Users\luther\ephpm\php-sdk\8.5.7-windows-x86_64`.
   **Not the 8.3.31 SDK** — it ships `libintl_a.lib`, which collides with
   `libiconv_a.lib` on `locale_charset`; both are linked `+whole-archive`, so
   `LNK2005`/`LNK1169` and nothing links.

Even on 8.5.7 the same collision surfaces for **test** binaries. Add the
override to the one target rather than to `RUSTFLAGS` (which would invalidate
the whole shared cache):

```
cargo rustc -p ephpm-php --test <name> -- -C link-arg=/FORCE:MULTIPLE
```

then run `target/{debug,release}/deps/<name>-*.exe --test-threads=1` directly
(`serial_test` + one `php_embed_init` per process).

**Also known:** enabling OPcache in the bare embed test harness
(`init_with_ini_file` with `opcache.enable=1`) **crashes** with an access
violation on Windows — the private-SHM setup the server does at startup is not
reproduced by the harness. Benchmarks that need OPcache must run against a real
`ephpm serve`, not the test harness. The `crates/ephpm-php/tests/sessions.rs`
suite also already fails/crashes on the 8.5.7 SDK **on `main`** (PHP 8.4+
deprecated `session.use_only_cookies=0`); verified by stashing changes, so do
not attribute it to your branch.
