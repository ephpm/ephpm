//! `[kv.snapshot] mode = "object"` without `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY` in the environment must fail at startup, naming
//! the variables — not at the first save, hours later.
//!
//! Its own test binary (its own process) because the sibling object-storage
//! tests set those variables, and process environment is shared across the
//! threads of one binary.

use ephpm_config::Config;
use ephpm_kv::store::{Store, StoreConfig};
use ephpm_server::kv_snapshot::KvSnapshotService;

#[test]
fn object_mode_without_credentials_fails_at_startup() {
    // SAFETY: the only thread in this process that touches the environment.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        std::env::remove_var("AWS_SESSION_TOKEN");
    }
    let config: Config = toml::from_str(
        "[kv]\nsecret = \"s3cret\"\n[kv.snapshot]\nmode = \"object\"\nurl = \"s3://kv-test\"\n\
         endpoint = \"http://127.0.0.1:1\"\n",
    )
    .unwrap();
    config.validate().expect("the config itself is valid; credentials are an environment matter");
    let store = Store::new(StoreConfig::default());
    let err =
        KvSnapshotService::from_config(&config, store, None, None, false).expect_err("no creds");
    let msg = format!("{err:#}");
    assert!(msg.contains("AWS_ACCESS_KEY_ID") && msg.contains("AWS_SECRET_ACCESS_KEY"), "{msg}");
}
