//! Preview-mode preset end-to-end tests (`[server] preview = true`).
//!
//! Runs ONLY against the dedicated preview node `cargo xtask e2e` spawns (see
//! `ISOLATED_CONFIG_SUITES` in `xtask/src/e2e_bare.rs`), which provides its
//! base URL via `EPHPM_PREVIEW_URL` — the suite self-skips when it is unset,
//! so it can never be pointed at a non-preview node by accident.
//!
//! That node runs multi-tenant (`sites_dir`, exported as `EPHPM_SITES_DIR`)
//! with `preview = true` and the template's EXPLICIT `[server.limits]` values
//! for `max_connections` (100) and the per-IP bucket (generous). Only the
//! per-site pair is left unset, so it comes from the preview preset:
//! `per_site_rate = 5.0`, `per_site_burst = 20`.
//!
//! Validates:
//! - `X-Ephpm-Preview: 1` is on every response (200, 404, and the 429s)
//! - a PHP burst against one site trips per-site 429s with `Retry-After`
//! - a sibling site — same client IP — is untouched right after, which
//!   simultaneously proves the 429s were per-site (not per-IP) and that the
//!   template's explicit generous per-IP values beat the preset's 10/s
//!   (explicit-beats-preset, end to end)
//! - a host that matches no site is never per-site-capped
//!
//! Environment variables:
//! - `EPHPM_PREVIEW_URL` — base URL of the preview-mode ephpm instance
//! - `EPHPM_SITES_DIR` — the node's sites directory (writable)

use std::path::PathBuf;
use std::time::Duration;

fn preview_url() -> Option<String> {
    std::env::var("EPHPM_PREVIEW_URL").ok().filter(|s| !s.is_empty())
}

fn sites_dir() -> PathBuf {
    PathBuf::from(ephpm_e2e::required_env("EPHPM_SITES_DIR"))
}

async fn get_with_host(
    client: &reqwest::Client,
    base_url: &str,
    host: &str,
    path: &str,
) -> reqwest::Response {
    client
        .get(format!("{base_url}{path}"))
        .header("Host", host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} with Host: {host} failed: {e}"))
}

fn assert_preview_marker(resp: &reqwest::Response, ctx: &str) {
    assert_eq!(
        resp.headers().get("x-ephpm-preview").and_then(|v| v.to_str().ok()),
        Some("1"),
        "X-Ephpm-Preview: 1 missing on {ctx} (status {})",
        resp.status()
    );
}

/// Deploy a minimal PHP site into `sites_dir` and poll until the router's
/// lazy vhost discovery serves it (the negative-lookup cache holds unknown
/// hosts for ~2s, so "live within seconds" is the documented contract).
async fn deploy_php_site(client: &reqwest::Client, base_url: &str, host: &str) -> PathBuf {
    let dir = sites_dir().join(host);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create site dir");
    std::fs::write(dir.join("index.php"), format!("<?php echo 'site:{host}';"))
        .expect("write index.php");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let resp = get_with_host(client, base_url, host, "/index.php").await;
        if resp.status().as_u16() == 200 {
            let body = resp.text().await.expect("read body");
            assert!(body.contains(&format!("site:{host}")), "wrong site content: {body}");
            return dir;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "site {host} not discovered within the lazy-discovery window \
             (last status {})",
            resp.status()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The "don't mistake this for prod" marker is on every response class the
/// router produces — success and error alike.
#[tokio::test]
async fn every_response_carries_the_preview_marker() {
    let Some(base_url) = preview_url() else {
        eprintln!("EPHPM_PREVIEW_URL unset — skipping preview marker test");
        return;
    };
    let client = reqwest::Client::new();

    // Static 200 from the default docroot.
    let resp = client
        .get(format!("{base_url}/test.html"))
        .send()
        .await
        .expect("GET /test.html failed");
    assert_eq!(resp.status().as_u16(), 200);
    assert_preview_marker(&resp, "a static 200");

    // A 404.
    let resp = client
        .get(format!("{base_url}/definitely-not-here.txt"))
        .send()
        .await
        .expect("GET missing file failed");
    assert_eq!(resp.status().as_u16(), 404);
    assert_preview_marker(&resp, "a 404");
}

/// One site's PHP burst trips the preset per-site cap (5/s, burst 20) with
/// 429 + `Retry-After`, while a sibling site — same client IP — stays fully
/// available, and an unmatched host is never per-site-capped.
#[tokio::test]
async fn per_site_php_burst_gets_429_and_siblings_are_unaffected() {
    let Some(base_url) = preview_url() else {
        eprintln!("EPHPM_PREVIEW_URL unset — skipping per-site rate cap test");
        return;
    };
    let client = reqwest::Client::new();
    let hot = "preview-hot.test";
    let quiet = "preview-quiet.test";

    let hot_dir = deploy_php_site(&client, &base_url, hot).await;
    let quiet_dir = deploy_php_site(&client, &base_url, quiet).await;

    // Burst well past the per-site burst of 20. Sequential over keep-alive is
    // plenty fast against loopback; at 5 tokens/s the refill contributes at
    // most a token or two over the burst's lifetime.
    let total = 40;
    let mut ok = 0;
    let mut limited = 0;
    let mut other = Vec::new();
    for _ in 0..total {
        let resp = get_with_host(&client, &base_url, hot, "/index.php").await;
        assert_preview_marker(&resp, "a burst response");
        match resp.status().as_u16() {
            200 => ok += 1,
            429 => {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                assert!(
                    retry_after.is_some_and(|s| s >= 1),
                    "per-site 429 must carry Retry-After >= 1, got {:?}",
                    resp.headers().get("retry-after")
                );
                limited += 1;
            }
            s => other.push(s),
        }
    }
    assert!(
        ok >= 1,
        "burst should start within the per-site budget; got ok={ok} 429={limited} other={other:?}"
    );
    assert!(
        limited >= 1,
        "a {total}-request PHP burst must trip the per-site cap (burst 20); \
         got ok={ok} 429={limited} other={other:?}"
    );

    // Same client IP, different site: full budget. This is also the
    // explicit-beats-preset proof — under the preset's per-IP 10/s the ~45
    // requests so far would have exhausted the per-IP bucket too, but the
    // node's explicit generous per-IP values won, so only the per-site
    // bucket of the hot site is empty.
    let resp = get_with_host(&client, &base_url, quiet, "/index.php").await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "sibling site must be unaffected by the hot site's exhaustion \
         (a 429 here would mean per-IP limiting, i.e. the preset overrode \
          explicit [server.limits] values)"
    );
    assert_preview_marker(&resp, "the sibling site's 200");

    // A host that names no site has no site key and is never per-site-capped.
    for _ in 0..5 {
        let resp =
            get_with_host(&client, &base_url, "nonexistent-site.example.com", "/test.html").await;
        assert_ne!(
            resp.status().as_u16(),
            429,
            "unmatched host must not be per-site rate limited"
        );
    }

    // Teardown.
    let _ = std::fs::remove_dir_all(hot_dir);
    let _ = std::fs::remove_dir_all(quiet_dir);
}
