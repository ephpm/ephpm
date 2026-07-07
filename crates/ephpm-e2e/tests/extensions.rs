//! Shared PHP extension loading E2E.
//!
//! Proves the glibc-dynamic Linux binary can `dlopen` standard shared PHP
//! extensions at startup via `[php] extensions = [...]` — the payoff of the
//! dynamic baseline (`--export-dynamic` exports the Zend API so an out-of-tree
//! `.so` binds against the running binary). The extensions come from the
//! ePHPm ZTS extension catalog published per PHP minor at
//! `github.com/ephpm/php-sdk` (release tag `ext-<version>`), built ABI-matched
//! to the SDK: same PHP minor + ZTS + glibc + non-debug.
//!
//! Like the other worker/wordpress E2E, this needs a dedicated server instance
//! with the catalog mounted and referenced from config. The harness provides
//! its base URL via `EPHPM_EXT_URL`; the test self-skips (passes) when unset.
//!
//! ## Fixture
//!
//! Any glibc-dynamic ePHPm release image (PHP 8.5.x), plus the matching
//! catalog tarball. Locally:
//!
//! ```sh
//! # 1. fetch + extract the catalog for the image's PHP minor + arch
//! curl -fsSL -o ext.tgz \
//!   https://github.com/ephpm/php-sdk/releases/download/ext-8.5.7/ephpm-ext-8.5.7-linux-x86_64-gnu.tar.gz
//! mkdir ext && tar xzf ext.tgz -C ext
//!
//! # 2. serve a docroot containing tests/fixtures/ext-probe.php with a config:
//! #      [php]
//! #      extensions = ["/ext/igbinary.so","/ext/msgpack.so","/ext/apcu.so",
//! #                    "/ext/redis.so","/ext/mongodb.so"]
//! podman run -d -p 8110:8080 \
//!   -v "$PWD/docroot:/app:ro" -v "$PWD/ext:/ext:ro" \
//!   -v "$PWD/ephpm.toml:/etc/ephpm/ephpm.toml:ro" <ephpm-image>
//!
//! EPHPM_EXT_URL=http://127.0.0.1:8110 cargo test --test extensions
//! ```
//!
//! The probe script (`tests/fixtures/ext-probe.php`, served as `/ext-probe.php`)
//! reports which of the catalog extensions loaded and whether each is
//! functional (a real round-trip, not just `extension_loaded`).

use std::collections::BTreeMap;

/// Base URL of the extension-loaded instance, or `None` to skip.
fn ext_url() -> Option<String> {
    std::env::var("EPHPM_EXT_URL").ok().filter(|s| !s.is_empty())
}

/// The catalog extensions the fixture is expected to load, each paired with a
/// probe key the script sets `true` when the extension is not just loaded but
/// functionally exercised.
const EXPECTED: &[(&str, &str)] = &[
    ("igbinary", "igbinary"),   // serialize/unserialize round-trip
    ("msgpack", "msgpack"),     // pack/unpack round-trip
    ("apcu", "apcu"),           // store/fetch round-trip
    ("redis", "redis_class"),   // Redis class present
    ("mongodb", "mongodb_class"), // MongoDB\Driver\Manager present
];

#[tokio::test]
async fn shared_extensions_load_and_function() {
    let Some(base) = ext_url() else {
        eprintln!("EPHPM_EXT_URL unset — skipping extension E2E");
        return;
    };

    let body = reqwest::get(format!("{base}/ext-probe.php"))
        .await
        .expect("probe request failed")
        .error_for_status()
        .expect("probe returned non-2xx")
        .text()
        .await
        .expect("probe body read failed");

    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("probe not JSON ({e}): {body}"));

    let loaded: Vec<String> = json["loaded"]
        .as_array()
        .expect("`loaded` missing")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();

    let functional: BTreeMap<String, bool> = json["functional"]
        .as_object()
        .expect("`functional` missing")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false)))
        .collect();

    for (ext, fkey) in EXPECTED {
        assert!(
            loaded.iter().any(|l| l == ext),
            "extension `{ext}` did not load (catalog .so dlopen via [php] extensions); loaded = {loaded:?}",
        );
        assert_eq!(
            functional.get(*fkey),
            Some(&true),
            "extension `{ext}` loaded but its functional probe `{fkey}` was not true; functional = {functional:?}",
        );
    }
}
