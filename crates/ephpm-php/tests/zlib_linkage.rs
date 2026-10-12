//! Linked tests for the zlib PHP's `zlib` extension and libzip run on.
//!
//! PHP must run on the PHP SDK's `libz.a`. A Rust crate that compiles its own
//! zlib exports the same `inflate*`/`deflate*` symbols and can win the link.
//! That happened through `libz-sys` (pulled in by `mysql_common`'s default
//! `flate2/zlib` feature, via opensrv-mysql): on a build host with no system
//! zlib, libz-sys compiles a bundled zlib with `Z_SOLO`, which has no default
//! allocator and no gz* API. PHP's gz* file functions (from the SDK) then
//! called the `Z_SOLO` `inflateInit2_`/`deflateInit2_`, which return
//! `Z_STREAM_ERROR` for the `NULL` `zalloc` those callers pass. `gzdecode()`
//! kept working (PHP supplies its own allocator), but `gzopen`,
//! `compress.zlib://` and `ZipArchive` all broke: the v0.11.3 Docker image
//! could not run symfony/intl or unpack a zip.
//!
//! `deny.toml` bans the C zlib crates; this test checks the resulting binary
//! from the PHP side, through every path that broke.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --release --test zlib_linkage -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::sync::OnceLock;

use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use serial_test::serial;
use tempfile::TempDir;

static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

fn init_once() {
    INIT.get_or_init(|| {
        PhpRuntime::init().expect("php_embed_init");
        PhpRuntime::finalize_for_http().expect("finalize_for_http");
    });
}

fn run_php(name: &str, body: &str) -> String {
    init_once();
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path();
    let script = dir.join(name);
    std::fs::write(&script, body).expect("write test script");
    let resp = PhpRuntime::execute(PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script,
        headers: vec![("host".into(), "localhost".into())],
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: Vec::new(),
        middleware: Vec::new(),
    })
    .expect("execute");
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    String::from_utf8_lossy(&resp.body).trim().to_string()
}

/// Each line is `<check>=<result>`; every result must be `ok`.
const ZLIB_PATHS: &str = r#"<?php
$dir = __DIR__ . '/zlib-' . getmypid();
@mkdir($dir);
$payload = str_repeat("hello zlib ", 1000);
$r = static fn(bool $ok, string $why = '') => $ok ? 'ok' : "FAIL $why";
$last = static fn() => error_get_last()['message'] ?? '';

echo 'gzdecode=', $r(gzdecode(gzencode($payload)) === $payload), "\n";

file_put_contents("$dir/a.gz", gzencode($payload));
$g = @gzopen("$dir/a.gz", 'rb');
$got = $g ? gzread($g, 1 << 20) : false;
if ($g) { gzclose($g); }
echo 'gzopen_read=', $r($got === $payload, var_export(is_string($got) ? strlen($got) : $got, true)), "\n";

$g = @gzopen("$dir/b.gz", 'wb');
$wrote = $g && gzwrite($g, $payload) === strlen($payload);
if ($g) { gzclose($g); }
echo 'gzopen_write=', $r($wrote && gzdecode((string) file_get_contents("$dir/b.gz")) === $payload), "\n";

$got = @file_get_contents("compress.zlib://$dir/a.gz");
echo 'compress_zlib_read=', $r($got === $payload, $last()), "\n";

$n = @file_put_contents("compress.zlib://$dir/c.gz", $payload);
echo 'compress_zlib_write=', $r($n === strlen($payload) && gzdecode((string) file_get_contents("$dir/c.gz")) === $payload, $last()), "\n";

// The SDK compiles ext/zip in (libzip, deflate through the same zlib).
if (!class_exists('ZipArchive')) {
    echo "zip_deflate_write=FAIL ZipArchive missing\nzip_deflate_read=FAIL ZipArchive missing\n";
} else {
    $z = new ZipArchive();
    $z->open("$dir/d.zip", ZipArchive::CREATE | ZipArchive::OVERWRITE);
    $z->addFromString('x.txt', $payload);
    $z->setCompressionName('x.txt', ZipArchive::CM_DEFLATE);
    $closed = @$z->close();
    echo 'zip_deflate_write=', $r($closed, $last()), "\n";
    $z = new ZipArchive();
    $got = $closed && $z->open("$dir/d.zip") === true ? $z->getFromName('x.txt') : false;
    echo 'zip_deflate_read=', $r($got === $payload, $closed ? $z->getStatusString() : 'no archive'), "\n";
}

array_map('unlink', glob("$dir/*"));
rmdir($dir);
"#;

#[test]
#[serial]
fn php_zlib_streams_and_libzip_work() {
    let out = run_php("zlib_paths.php", ZLIB_PATHS);
    let failures: Vec<&str> = out.lines().filter(|l| !l.ends_with("=ok")).collect();
    assert_eq!(out.lines().count(), 7, "unexpected script output:\n{out}");
    assert!(
        failures.is_empty(),
        "zlib paths failed (a second zlib, e.g. libz-sys's bundled Z_SOLO build, \
         is shadowing the PHP SDK's libz.a):\n{}",
        failures.join("\n")
    );
}
