//! Baseline authentication for the **shared** SQL wire listeners.
//!
//! # What this closes (multi-tenant breakout audit, finding F4)
//!
//! Three listeners historically authenticated nobody — any process that could
//! reach the loopback port (a tenant's own PHP included) connected
//! credential-free and got full read/write access to the database:
//!
//! * the `[db.mysql]` forwarding proxy (it read and discarded the handshake);
//! * single-node `[db.sqlite]` (one litewire `MySQL` frontend over Turso);
//! * whole-database clustered `[db.sqlite]` (the same frontend, CDC-replicated).
//!
//! Only the per-site (multi-tenant) MySQL listener gated anything, and it still
//! does — per virtual host, via [`crate::site_wire_auth`]. This module is the
//! *shared* equivalent: one server-level credential for the listeners above.
//!
//! # Secure by default, with an explicit opt-out
//!
//! Authentication is required by default. [`SharedWireAuth::resolve`] produces
//! [`SharedWireAuth::Required`] unless `[db.wire_auth] allow_insecure_no_auth =
//! true`, which restores the old credential-free behavior and logs a loud WARN.
//!
//! The credential is derived exactly like the per-site one — a configured
//! `[db.wire_auth] password`, else `HMAC-SHA256([kv] secret, "ephpm-db-wire")`
//! (stable across restarts), else a fresh per-process random value. It is
//! injected into each request's `$_SERVER` by the router (never into the
//! process environment, so `getenv()` cannot read it; never logged), so stock
//! `pdo_mysql` keeps working transparently while an external credential-free
//! client is refused at the handshake.
//!
//! # Why the MySQL frontend only (litewire constraint)
//!
//! litewire resolves a backend per connection only on its MySQL frontend; a
//! builder constructed [`with_authenticator`](litewire::LiteWire::with_authenticator)
//! refuses to start if a Hrana/PostgreSQL/TDS listener is also configured. The
//! `[db.sqlite]` startup paths therefore fail closed with a clear message when
//! authentication is required and one of those extra frontends is configured —
//! the operator removes it or opts out. This is not a regression: those
//! frontends never authenticated anything either, and refusing beats serving
//! them an unauthenticated backend behind an otherwise-authenticated listener.

use std::sync::Arc;

use ephpm_config::Config;
use litewire::backend::{
    AuthOutcome, AuthRequest, ConnectionAuthenticator, SharedAuthenticator, SharedBackend,
};
use litewire::litewire_mysql::native_password;

/// The username injected for the shared credential. The password is the
/// secret; the username is cosmetic (the authenticator does not check it), so
/// it is a fixed, recognizable value rather than leaking a backend account.
pub(crate) const WIRE_USERNAME: &str = "ephpm";

/// Length of the per-process random master credential, in bytes (256 bits).
const RANDOM_CREDENTIAL_BYTES: usize = 32;

/// The resolved authentication policy for the shared SQL wire listeners.
#[derive(Clone)]
pub(crate) enum SharedWireAuth {
    /// Clients must present this password; it is injected into PHP's `$_SERVER`.
    Required(Arc<str>),
    /// `allow_insecure_no_auth = true`: credential-free (the explicit opt-out).
    Disabled,
}

impl SharedWireAuth {
    /// Resolve the policy from config, logging the outcome once.
    ///
    /// # Errors
    ///
    /// Fails only when a random credential is required (no configured password,
    /// no `[kv] secret`) and the OS entropy source is unavailable — deliberately
    /// fatal rather than deriving a predictable credential.
    pub(crate) fn resolve(config: &Config) -> anyhow::Result<Self> {
        let wa = &config.db.wire_auth;
        if wa.allow_insecure_no_auth {
            tracing::warn!(
                "[db.wire_auth] allow_insecure_no_auth = true — the shared SQL wire listeners \
                 ([db.mysql] proxy, single-node / whole-database clustered [db.sqlite]) accept \
                 ANY client with NO credential. Any process that can reach the listen port gets \
                 full read/write access to the database. This is an explicit opt-out; remove it \
                 to require authentication."
            );
            return Ok(Self::Disabled);
        }

        let password: Arc<str> = if let Some(p) = wa.password.as_deref().filter(|p| !p.is_empty()) {
            tracing::info!(
                "shared SQL wire listeners require authentication (credential from the configured \
                 [db.wire_auth] password)"
            );
            Arc::from(p)
        } else if let Some(secret) = config.kv.secret.as_deref().filter(|s| !s.is_empty()) {
            tracing::info!(
                "shared SQL wire listeners require authentication (credential derived from [kv] \
                 secret; stable across restarts, so a co-located CLI can authenticate)"
            );
            Arc::from(ephpm_kv::auth::derive_wire_password(secret).as_str())
        } else {
            let mut raw = [0u8; RANDOM_CREDENTIAL_BYTES];
            getrandom::fill(&mut raw).map_err(|e| {
                anyhow::anyhow!(
                    "failed to generate the shared SQL wire credential from OS entropy: {e}. \
                     Refusing to start rather than leave the listeners unauthenticated; set \
                     [db.wire_auth] password, [kv] secret, or (to accept the risk) \
                     [db.wire_auth] allow_insecure_no_auth = true."
                )
            })?;
            tracing::info!(
                "shared SQL wire listeners require authentication (credential generated \
                 per-process — no [db.wire_auth] password or [kv] secret set). In-process PHP \
                 connects via the injected $_SERVER DB_*; external clients and the CLI wire path \
                 cannot authenticate, and the credential rotates on restart. Set [db.wire_auth] \
                 password or [kv] secret for a stable credential."
            );
            Arc::from(hex_encode(&raw).as_str())
        };

        Ok(Self::Required(password))
    }

    /// Whether authentication is required (not opted out).
    pub(crate) fn is_required(&self) -> bool {
        matches!(self, Self::Required(_))
    }

    /// The `(username, password)` to inject into PHP's `$_SERVER`, or `None`
    /// when authentication is disabled (nothing to inject).
    pub(crate) fn injected_credential(&self) -> Option<(&str, &str)> {
        match self {
            Self::Required(password) => Some((WIRE_USERNAME, password)),
            Self::Disabled => None,
        }
    }

    /// A litewire authenticator that binds `backend` to every connection which
    /// proves the shared credential, or `None` when disabled (the caller then
    /// serves `backend` with a fixed, unauthenticated frontend).
    pub(crate) fn authenticator(&self, backend: SharedBackend) -> Option<SharedAuthenticator> {
        match self {
            Self::Required(password) => {
                Some(Arc::new(FixedBackendAuth { password: Arc::clone(password), backend }))
            }
            Self::Disabled => None,
        }
    }
}

/// litewire `ConnectionAuthenticator` that grants one fixed backend to any
/// connection presenting the shared credential.
///
/// Verification happens *before* the backend is handed out, so an
/// unauthenticated client never reaches the database. The username is not
/// checked — the password is the secret.
struct FixedBackendAuth {
    password: Arc<str>,
    backend: SharedBackend,
}

#[litewire::async_trait]
impl ConnectionAuthenticator for FixedBackendAuth {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> AuthOutcome {
        let expected = native_password::password_hash(self.password.as_bytes());
        if native_password::verify(&expected, req.salt, req.auth_response) {
            AuthOutcome::Granted(Arc::clone(&self.backend))
        } else {
            tracing::warn!(
                peer = %req.peer_addr,
                "shared SQL wire auth: password verification failed — refusing the connection"
            );
            AuthOutcome::Denied
        }
    }
}

/// Lowercase-hex encode, matching the per-site credential's encoding.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use litewire::backend::{Backend, BackendConn, BackendError};

    use super::*;

    fn config_from(toml: &str) -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ephpm.toml");
        std::fs::write(&path, toml).expect("write config");
        Config::load(&path).expect("load config")
    }

    #[test]
    fn default_is_required_with_a_credential() {
        let auth = SharedWireAuth::resolve(&config_from("")).expect("resolve");
        assert!(auth.is_required(), "authentication must be required by default");
        let (user, pw) = auth.injected_credential().expect("a credential to inject");
        assert_eq!(user, WIRE_USERNAME);
        assert!(!pw.is_empty(), "a password must be injected");
    }

    #[test]
    fn opt_out_disables_and_injects_nothing() {
        let auth = SharedWireAuth::resolve(&config_from(
            "[db.wire_auth]\nallow_insecure_no_auth = true\n",
        ))
        .expect("resolve");
        assert!(!auth.is_required());
        assert!(auth.injected_credential().is_none(), "nothing to inject when disabled");
        assert!(auth.authenticator(marker_backend()).is_none(), "no authenticator when disabled");
    }

    #[test]
    fn kv_secret_gives_a_stable_derived_credential() {
        let toml = "[kv]\nsecret = \"shared-secret\"\n";
        let a = SharedWireAuth::resolve(&config_from(toml)).expect("resolve");
        let b = SharedWireAuth::resolve(&config_from(toml)).expect("resolve");
        let pa = a.injected_credential().unwrap().1;
        let pb = b.injected_credential().unwrap().1;
        assert_eq!(pa, pb, "the [kv] secret derivation must be stable across resolutions");
        assert_eq!(pa, ephpm_kv::auth::derive_wire_password("shared-secret"));
    }

    #[test]
    fn configured_password_wins_over_secret() {
        let auth = SharedWireAuth::resolve(&config_from(
            "[kv]\nsecret = \"s\"\n[db.wire_auth]\npassword = \"explicit\"\n",
        ))
        .expect("resolve");
        assert_eq!(auth.injected_credential().unwrap().1, "explicit");
    }

    #[test]
    fn two_processes_without_a_secret_differ() {
        // No secret, no password → a fresh random credential each time.
        let a = SharedWireAuth::resolve(&config_from("")).expect("resolve");
        let b = SharedWireAuth::resolve(&config_from("")).expect("resolve");
        assert_ne!(
            a.injected_credential().unwrap().1,
            b.injected_credential().unwrap().1,
            "the random fallback must not derive the same credential twice"
        );
    }

    struct MarkerBackend;

    #[litewire::async_trait]
    impl Backend for MarkerBackend {
        async fn connect(&self) -> Result<Box<dyn BackendConn>, BackendError> {
            Err(BackendError::Other("marker".into()))
        }
    }

    fn marker_backend() -> SharedBackend {
        Arc::new(MarkerBackend) as SharedBackend
    }

    fn client_response(password: &str, salt: &[u8]) -> Vec<u8> {
        use sha1::{Digest, Sha1};
        let stage1: [u8; 20] = Sha1::digest(password.as_bytes()).into();
        let stage2: [u8; 20] = Sha1::digest(stage1).into();
        let mut h = Sha1::new();
        h.update(salt);
        h.update(stage2);
        let mask = h.finalize();
        stage1.iter().zip(mask).map(|(s, m)| s ^ m).collect()
    }

    const SALT: &[u8] = b"abcdefghijklmnopqrst";

    fn request(response: &[u8]) -> AuthRequest<'_> {
        AuthRequest {
            auth_plugin: "mysql_native_password",
            username: b"anything",
            salt: SALT,
            auth_response: response,
            local_addr: "127.0.0.1:3306".parse::<SocketAddr>().unwrap(),
            peer_addr: "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        }
    }

    #[tokio::test]
    async fn authenticator_grants_only_the_right_password() {
        let auth = SharedWireAuth::Required(Arc::from("s3cret"));
        let authn = auth.authenticator(marker_backend()).expect("an authenticator");

        // Correct password → Granted (the marker backend).
        let ok = authn.authenticate(&request(&client_response("s3cret", SALT))).await;
        assert!(matches!(ok, AuthOutcome::Granted(_)), "the right password must authenticate");

        // Wrong password and an empty (credential-free) response → Denied.
        let wrong = authn.authenticate(&request(&client_response("nope", SALT))).await;
        assert!(matches!(wrong, AuthOutcome::Denied), "a wrong password must be denied");
        let empty = authn.authenticate(&request(b"")).await;
        assert!(matches!(empty, AuthOutcome::Denied), "a credential-free client must be denied");
    }
}
