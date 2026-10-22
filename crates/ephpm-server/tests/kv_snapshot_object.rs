//! Object-storage KV snapshots against an in-process S3 mock.
//!
//! The mock is a small hyper server holding objects in a map. It speaks the
//! five calls the client makes (PUT, GET, HEAD, DELETE, ListObjectsV2) and
//! **verifies every request's SigV4 signature**: it reconstructs the
//! canonical request from what arrived on the wire (decoded path and query,
//! the signed headers named in `Authorization`, the payload hash header) and
//! re-signs it with the same secret. A canonicalisation bug on the client
//! side — an unsorted query, an unencoded key, a header left out of
//! `SignedHeaders` — therefore fails here, not only against a real bucket.
//!
//! What it does not prove: that a real S3/MinIO/R2 agrees with our reading
//! of the spec. The AWS-documented test vector in `sigv4.rs` and the MinIO
//! lab run cover that.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use ephpm_config::Config;
use ephpm_kv::multi_tenant::MultiTenantStore;
use ephpm_kv::snapshot::{ColdReason, LoadOutcome};
use ephpm_kv::store::{Store, StoreConfig};
use ephpm_server::kv_snapshot::{KvSnapshotService, SaveTrigger, object_client_from_config};
use ephpm_server::sigv4::{self, Credentials, SignInput};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const ACCESS_KEY: &str = "AKIAMOCKMOCKMOCKMOCK";
const SECRET_KEY: &str = "mock-secret-key-not-for-production";

#[derive(Default)]
struct MockState {
    objects: BTreeMap<String, Vec<u8>>,
    /// Every request's `(method, key)` in order, for assertions.
    log: Vec<(String, String)>,
    bad_signatures: usize,
}

type Shared = Arc<Mutex<MockState>>;

/// Start the mock on a loopback port. Returns its address.
async fn start_mock(state: Shared) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let svc = service_fn(move |req| handle(Arc::clone(&state), req));
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
            });
        }
    });
    addr
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// Re-derive the signature from the wire and compare.
fn verify_signature(req: &Request<Incoming>, state: &Shared) -> bool {
    let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    let Some(rest) = auth.strip_prefix("AWS4-HMAC-SHA256 ") else { return false };
    let mut credential = "";
    let mut signed_headers = "";
    for part in rest.split(", ") {
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = v;
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = v;
        }
    }
    let mut scope = credential.split('/');
    let key_id = scope.next().unwrap_or("");
    let _date = scope.next();
    let region = scope.next().unwrap_or("");
    let service = scope.next().unwrap_or("");
    if key_id != ACCESS_KEY || service != "s3" {
        return false;
    }
    let amz_date = req.headers().get("x-amz-date").and_then(|v| v.to_str().ok()).unwrap_or("");
    let Some(unix_secs) = sigv4::parse_amz_date(amz_date) else { return false };
    let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let path = percent_decode(req.uri().path());
    let query_raw: Vec<(String, String)> = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    let query: Vec<(&str, &str)> =
        query_raw.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let payload_hash = req
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Extra signed headers = everything in SignedHeaders the signer does not
    // add itself.
    let extra_raw: Vec<(String, String)> = signed_headers
        .split(';')
        .filter(|h| {
            !matches!(*h, "host" | "x-amz-date" | "x-amz-content-sha256" | "x-amz-security-token")
        })
        .map(|h| {
            let v = req.headers().get(h).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            (h.to_string(), v)
        })
        .collect();
    let extra: Vec<(&str, &str)> =
        extra_raw.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let creds = Credentials {
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SECRET_KEY.into(),
        session_token: None,
    };
    let expected = sigv4::sign(
        &creds,
        &SignInput {
            method: req.method().as_str(),
            host: &host,
            path: &path,
            query: &query,
            extra_headers: &extra,
            payload_hash: &payload_hash,
            send_content_sha256: true,
            region,
            service: "s3",
            unix_secs,
        },
    );
    let ok = expected.authorization == auth;
    if !ok {
        eprintln!("signature mismatch\n  got:      {auth}\n  expected: {}", expected.authorization);
        state.lock().unwrap().bad_signatures += 1;
    }
    ok
}

fn text(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder().status(status).body(Full::new(body.into())).unwrap()
}

async fn handle(
    state: Shared,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if !verify_signature(&req, &state) {
        return Ok(text(
            StatusCode::FORBIDDEN,
            "<Error><Code>SignatureDoesNotMatch</Code><Message>bad signature</Message></Error>",
        ));
    }
    // Path-style: /bucket/key…
    let path = percent_decode(req.uri().path());
    let (bucket, key) =
        path.trim_start_matches('/').split_once('/').unwrap_or((path.trim_start_matches('/'), ""));
    if bucket != "kv-test" {
        return Ok(text(StatusCode::NOT_FOUND, "<Error><Code>NoSuchBucket</Code></Error>"));
    }
    let method = req.method().clone();
    let query = req.uri().query().unwrap_or("").to_string();
    state.lock().unwrap().log.push((method.to_string(), key.to_string()));
    match method {
        hyper::Method::PUT => {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            state.lock().unwrap().objects.insert(key.to_string(), body.to_vec());
            Ok(text(StatusCode::OK, ""))
        }
        hyper::Method::GET if key.is_empty() => {
            let prefix = query
                .split('&')
                .find_map(|p| p.strip_prefix("prefix="))
                .map(percent_decode)
                .unwrap_or_default();
            let st = state.lock().unwrap();
            let mut xml =
                String::from("<?xml version=\"1.0\"?><ListBucketResult><Name>kv-test</Name>");
            for (k, v) in st.objects.iter().filter(|(k, _)| k.starts_with(&prefix)) {
                use std::fmt::Write as _;
                let _ = write!(
                    xml,
                    "<Contents><Key>{k}</Key><LastModified>2026-10-22T00:00:00.000Z</LastModified><Size>{}</Size></Contents>",
                    v.len()
                );
            }
            xml.push_str("</ListBucketResult>");
            Ok(text(StatusCode::OK, xml))
        }
        hyper::Method::GET => match state.lock().unwrap().objects.get(key) {
            Some(v) => Ok(text(StatusCode::OK, v.clone())),
            None => Ok(text(StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code></Error>")),
        },
        hyper::Method::HEAD => match state.lock().unwrap().objects.get(key) {
            Some(v) => Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-length", v.len())
                .header("last-modified", "Thu, 22 Oct 2026 00:00:00 GMT")
                .body(Full::new(Bytes::new()))
                .unwrap()),
            None => Ok(text(StatusCode::NOT_FOUND, "")),
        },
        hyper::Method::DELETE => {
            state.lock().unwrap().objects.remove(key);
            Ok(text(StatusCode::NO_CONTENT, ""))
        }
        _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "")),
    }
}

/// Credentials are read from the process environment; these tests set them
/// once (they are process-global, so every test uses the same pair).
fn set_env_creds() {
    // SAFETY: tests in this binary run on separate threads but set the same
    // values, and nothing reads other AWS_* variables concurrently.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", ACCESS_KEY);
        std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET_KEY);
    }
}

fn object_config(endpoint: &str, staging: &std::path::Path, identity: &str) -> Config {
    toml::from_str(&format!(
        "[kv]\nsecret = \"s3cret\"\n[kv.snapshot]\nmode = \"object\"\nurl = \"s3://kv-test/prod/kv\"\n\
         endpoint = \"{endpoint}\"\nregion = \"us-east-1\"\npath = {:?}\nidentity = \"{identity}\"\n\
         load_timeout = \"20s\"\n",
        staging.to_string_lossy()
    ))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_uploads_and_a_fresh_node_loads_from_the_object_alone() {
    set_env_creds();
    let state: Shared = Arc::default();
    let addr = start_mock(Arc::clone(&state)).await;
    let endpoint = format!("http://{addr}");
    let staging = tempfile::tempdir().unwrap();
    let config = object_config(&endpoint, staging.path(), "web");

    let store = Store::new(StoreConfig::default());
    let sites = MultiTenantStore::new(Arc::clone(&store), StoreConfig::default());
    store.set("k".into(), b"default".to_vec(), None);
    sites.get_site_store("a.test").set(
        "k".into(),
        b"alice".to_vec(),
        Some(std::time::Duration::from_secs(600)),
    );
    let svc = KvSnapshotService::from_config(
        &config,
        Arc::clone(&store),
        Some(sites.clone()),
        Some("node-1"),
        None,
    )
    .unwrap()
    .expect("object mode enabled");
    assert_eq!(svc.object_url().as_deref(), Some("s3://kv-test/prod/kv/web.kvsnap"));

    // First load: nothing there yet.
    let cold = svc.load_now().await;
    assert_eq!(cold.outcome, LoadOutcome::Cold(ColdReason::Missing), "{cold:?}");

    let report = svc.save(SaveTrigger::Shutdown).await.expect("save + upload");
    assert_eq!(report.keys(), 2);
    {
        let st = state.lock().unwrap();
        assert_eq!(st.bad_signatures, 0);
        let obj = st.objects.get("prod/kv/web.kvsnap").expect("uploaded under the prefix");
        assert_eq!(obj.len() as u64, report.bytes);
        assert!(obj.starts_with(b"EPKVSNAP"));
    }
    assert!(!svc.file().exists(), "the staging copy is removed after the upload");

    // A new "pod" with an EMPTY staging directory warms from the object.
    let staging2 = tempfile::tempdir().unwrap();
    let config2 = object_config(&endpoint, staging2.path(), "web");
    let store2 = Store::new(StoreConfig::default());
    let sites2 = MultiTenantStore::new(Arc::clone(&store2), StoreConfig::default());
    let svc2 = KvSnapshotService::from_config(
        &config2,
        Arc::clone(&store2),
        Some(sites2.clone()),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    let load = svc2.load_at_startup().await.unwrap();
    assert_eq!(load.outcome, LoadOutcome::Ok, "{load:?}");
    assert_eq!(load.restored, 2);
    assert_eq!(store2.get("k").as_deref(), Some(&b"default"[..]));
    assert_eq!(sites2.get_site_store("a.test").get("k").as_deref(), Some(&b"alice"[..]));
    assert!(!svc2.file().exists(), "the downloaded copy is removed after the load");
    let status: serde_json::Value = serde_json::from_str(&svc2.status_json()).unwrap();
    assert_eq!(status["mode"], "object");
    assert_eq!(status["object"], "s3://kv-test/prod/kv/web.kvsnap");
    assert_eq!(status["last_load"]["written_by"], "node-1");

    // list / head / delete through the same client the CLI uses.
    let (client, prefix) = object_client_from_config(&config).unwrap();
    assert_eq!(prefix, "prod/kv");
    let listed = client.list("prod/kv/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "prod/kv/web.kvsnap");
    assert_eq!(listed[0].size, report.bytes);
    assert!(client.head("prod/kv/web.kvsnap").await.unwrap().is_some());
    client.delete("prod/kv/web.kvsnap").await.unwrap();
    assert!(client.head("prod/kv/web.kvsnap").await.unwrap().is_none());
    assert_eq!(state.lock().unwrap().bad_signatures, 0, "every request verified");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_upload_is_a_save_error_and_an_unreachable_endpoint_is_a_cold_start() {
    set_env_creds();
    let state: Shared = Arc::default();
    let addr = start_mock(Arc::clone(&state)).await;
    let staging = tempfile::tempdir().unwrap();

    // Wrong bucket in the URL → the mock answers NoSuchBucket.
    let config: Config = toml::from_str(&format!(
        "[kv]\nsecret = \"s3cret\"\n[kv.snapshot]\nmode = \"object\"\nurl = \"s3://other-bucket\"\n\
         endpoint = \"http://{addr}\"\npath = {:?}\nidentity = \"web\"\n",
        staging.path().to_string_lossy()
    ))
    .unwrap();
    let store = Store::new(StoreConfig::default());
    store.set("k".into(), b"v".to_vec(), None);
    let svc = KvSnapshotService::from_config(&config, Arc::clone(&store), None, None, None)
        .unwrap()
        .unwrap();
    let err = svc.save(SaveTrigger::Request).await.expect_err("upload refused");
    assert!(err.to_string().contains("NoSuchBucket"), "{err}");
    assert!(!svc.file().exists(), "no staging file left behind");
    let status: serde_json::Value = serde_json::from_str(&svc.status_json()).unwrap();
    assert_eq!(status["last_save"]["ok"], false);

    // Unreachable endpoint → cold start with reason io, never a panic.
    let config: Config = toml::from_str(&format!(
        "[kv]\nsecret = \"s3cret\"\n[kv.snapshot]\nmode = \"object\"\nurl = \"s3://kv-test\"\n\
         endpoint = \"http://127.0.0.1:1\"\npath = {:?}\nidentity = \"web\"\nload_timeout = \"3s\"\n",
        staging.path().to_string_lossy()
    ))
    .unwrap();
    let svc = KvSnapshotService::from_config(&config, Arc::clone(&store), None, None, None)
        .unwrap()
        .unwrap();
    let load = svc.load_now().await;
    assert_eq!(load.outcome, LoadOutcome::Cold(ColdReason::Io), "{load:?}");
}
