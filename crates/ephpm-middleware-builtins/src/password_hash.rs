//! PBKDF2-HMAC-SHA256 password hashing for the `basic-auth` middleware.
//!
//! # Why PBKDF2, and why from `aws-lc-rs`
//!
//! ePHPm already links `aws-lc-rs` — it is the crypto provider behind
//! `rustls-acme` and the RSA-OAEP implementation the MySQL
//! `caching_sha2_password` handshake uses — so PBKDF2 costs **no new
//! dependency**: no new crate in `Cargo.toml`, no new entry in `Cargo.lock`,
//! nothing new for `cargo deny` to judge. That was the deciding factor over
//! adding `argon2`/`bcrypt`.
//!
//! A plain keyed hash (the [`ephpm_kv::auth::derive_site_password`] shape,
//! HMAC-SHA256) would have been cheaper still, and is adequate for the
//! *generated* high-entropy passwords a preview-hosting control plane issues.
//! It is not adequate for the human-chosen password an operator types into
//! `ephpm.toml`, which is the other half of this middleware's audience. A
//! deliberately slow KDF is the difference between "the leaked hash is
//! useless" and "the leaked hash is `hunter2` in a second", so the slow KDF
//! wins and the per-request cost is dealt with by caching successful
//! verifications (see [`crate::basicauth`]).
//!
//! # Encoding
//!
//! Hashes are stored as a PHC-style string:
//!
//! ```text
//! $pbkdf2-sha256$i=100000$cmFuZG9tc2FsdDEyMzQ1$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2g
//! ```
//!
//! `i=` is the iteration count, then the salt and the derived key, both in
//! unpadded standard base64. The iteration count travels **with the hash**
//! rather than in configuration so a credential can be re-hashed at a higher
//! cost without an `ephpm.toml` edit, and so two credentials generated years
//! apart both keep verifying.
//!
//! Iteration counts outside [`MIN_ITERATIONS`]`..=`[`MAX_ITERATIONS`] are
//! rejected at parse time. The ceiling matters: a stored hash is attacker-
//! controlled data the moment an operator's credential store is writable, and
//! `i=4000000000` would otherwise be a one-line denial of service.

use std::fmt::Write as _;
use std::num::NonZeroU32;

use aws_lc_rs::pbkdf2;
use base64ct::{Base64Unpadded, Encoding};

/// Algorithm identifier at the head of every hash string.
const ALG: &str = "pbkdf2-sha256";

/// Derived-key length in bytes (SHA-256 output).
const KEY_LEN: usize = 32;

/// Salt length in bytes. 16 bytes is the NIST SP 800-132 recommendation and
/// what `aws-lc-rs`'s own PBKDF2 documentation asks for.
pub const SALT_LEN: usize = 16;

/// Lowest iteration count a stored hash may declare.
///
/// Below this the KDF stops being a KDF. Rejected at parse time so a
/// downgraded credential fails loudly instead of verifying quickly.
pub const MIN_ITERATIONS: u32 = 1_000;

/// Highest iteration count a stored hash may declare.
///
/// A stored hash is data, and data can be hostile: without a ceiling, one
/// credential reading `i=4000000000` turns every login attempt into a CPU
/// exhaustion attack against the server.
pub const MAX_ITERATIONS: u32 = 5_000_000;

/// Iteration count used when generating a new hash (`ephpm hash-password`).
///
/// Roughly 50–100 ms on 2020s server hardware. The cost is paid once per
/// distinct credential presented, not once per request — see the verification
/// cache in [`crate::basicauth`].
pub const DEFAULT_ITERATIONS: u32 = 100_000;

thread_local! {
    /// PBKDF2 derivations performed on **this thread**.
    ///
    /// Exists so tests can assert the *uniform work* property that actually
    /// matters for a login endpoint: an unknown username must cost the same
    /// derivation a known one does, or response latency becomes a
    /// user-enumeration oracle. A test reads this counter around a call and
    /// fails the moment someone adds an early return on the "no such user"
    /// path.
    ///
    /// Thread-local rather than a global atomic on purpose: libtest runs each
    /// test on its own thread, and a process-wide counter would be raced by
    /// every sibling test in the same binary — the exact shape of flake this
    /// repo has been bitten by before. A `Cell<u64>` with `const` init
    /// registers no TLS destructor, so it cannot participate in
    /// thread-teardown ordering hazards.
    static KDF_INVOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Record one PBKDF2 derivation on this thread.
fn note_kdf_invocation() {
    // `try_with` rather than `with`: accessing a TLS key during another key's
    // destruction panics, and a diagnostic counter must never be the thing
    // that takes a request down.
    let _ = KDF_INVOCATIONS.try_with(|c| c.set(c.get().saturating_add(1)));
}

/// PBKDF2 derivations performed on this thread so far. Test-support only.
#[doc(hidden)]
#[must_use]
pub fn kdf_invocations() -> u64 {
    KDF_INVOCATIONS.try_with(std::cell::Cell::get).unwrap_or(0)
}

/// A parsed PBKDF2-HMAC-SHA256 password hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordHash {
    iterations: NonZeroU32,
    salt: Vec<u8>,
    key: Vec<u8>,
}

impl PasswordHash {
    /// Parse a `$pbkdf2-sha256$i=<n>$<salt>$<key>` string.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when the string is not a hash of this
    /// form, when the base64 is invalid, when the derived key is not
    /// [`KEY_LEN`] bytes, or when the iteration count is outside
    /// [`MIN_ITERATIONS`]`..=`[`MAX_ITERATIONS`].
    pub fn parse(s: &str) -> Result<Self, String> {
        // Leading `$` then exactly four fields: alg, params, salt, key.
        let rest = s.strip_prefix('$').ok_or(
            "not a password hash (expected a leading `$`); \
                    generate one with `ephpm hash-password`",
        )?;
        let mut parts = rest.split('$');
        let (Some(alg), Some(params), Some(salt_b64), Some(key_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!(
                "malformed password hash: expected `${ALG}$i=<iterations>$<salt>$<hash>`"
            ));
        };
        if alg != ALG {
            return Err(format!("unsupported password hash algorithm `{alg}` (only `{ALG}`)"));
        }

        let iterations: u32 = params
            .strip_prefix("i=")
            .ok_or("malformed password hash: expected an `i=<iterations>` parameter field")?
            .parse()
            .map_err(|_| "malformed password hash: `i=` is not an integer".to_owned())?;
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return Err(format!(
                "password hash iteration count {iterations} is outside the accepted range \
                 {MIN_ITERATIONS}..={MAX_ITERATIONS}"
            ));
        }
        let iterations = NonZeroU32::new(iterations)
            .ok_or_else(|| "password hash iteration count must be > 0".to_owned())?;

        let salt = Base64Unpadded::decode_vec(salt_b64)
            .map_err(|_| "malformed password hash: salt is not unpadded base64".to_owned())?;
        if salt.len() < SALT_LEN {
            return Err(format!(
                "password hash salt is {} bytes; at least {SALT_LEN} are required",
                salt.len()
            ));
        }
        let key = Base64Unpadded::decode_vec(key_b64)
            .map_err(|_| "malformed password hash: hash is not unpadded base64".to_owned())?;
        if key.len() != KEY_LEN {
            return Err(format!(
                "password hash is {} bytes; PBKDF2-HMAC-SHA256 produces {KEY_LEN}",
                key.len()
            ));
        }

        Ok(Self { iterations, salt, key })
    }

    /// Derive a fresh hash for `password` with a random salt.
    ///
    /// `iterations` is clamped into [`MIN_ITERATIONS`]`..=`[`MAX_ITERATIONS`]
    /// so a caller cannot mint a credential this crate would then refuse to
    /// parse.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS entropy source is unavailable. Deriving a
    /// salt from anything predictable would defeat the point, so this fails
    /// rather than falling back.
    pub fn generate(password: &str, iterations: u32) -> Result<Self, String> {
        let mut salt = vec![0u8; SALT_LEN];
        getrandom::fill(&mut salt)
            .map_err(|e| format!("failed to draw a password salt from OS entropy: {e}"))?;
        let iterations = iterations.clamp(MIN_ITERATIONS, MAX_ITERATIONS);
        let iterations = NonZeroU32::new(iterations).unwrap_or(NonZeroU32::MIN);

        let mut key = vec![0u8; KEY_LEN];
        note_kdf_invocation();
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations,
            &salt,
            password.as_bytes(),
            &mut key,
        );
        Ok(Self { iterations, salt, key })
    }

    /// Check `password` against this hash in constant time.
    ///
    /// The comparison is [`aws_lc_rs::pbkdf2::verify`], which derives into a
    /// scratch buffer, compares with `constant_time::verify_slices_are_equal`
    /// and zeroizes. This function never has the stored key and a freshly
    /// derived key in scope as two comparable values, so there is no `==` for
    /// a later edit to reach for.
    #[must_use]
    pub fn verify(&self, password: &str) -> bool {
        note_kdf_invocation();
        pbkdf2::verify(
            pbkdf2::PBKDF2_HMAC_SHA256,
            self.iterations,
            &self.salt,
            password.as_bytes(),
            &self.key,
        )
        .is_ok()
    }

    /// The iteration count this hash declares.
    #[must_use]
    pub fn iterations(&self) -> u32 {
        self.iterations.get()
    }

    /// Render back to the `$pbkdf2-sha256$i=…$…$…` storage form.
    #[must_use]
    pub fn to_phc_string(&self) -> String {
        let mut out = String::with_capacity(96);
        // Writing to a String is infallible; the Result is discarded on
        // purpose rather than unwrapped.
        let _ = write!(out, "${ALG}$i={}$", self.iterations);
        out.push_str(&Base64Unpadded::encode_string(&self.salt));
        out.push('$');
        out.push_str(&Base64Unpadded::encode_string(&self.key));
        out
    }
}

impl std::fmt::Display for PasswordHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_phc_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fast enough for tests, still a legal iteration count.
    const TEST_ITERS: u32 = MIN_ITERATIONS;

    #[test]
    fn generate_then_verify_round_trips() {
        let h = PasswordHash::generate("correct horse battery staple", TEST_ITERS).expect("gen");
        assert!(h.verify("correct horse battery staple"));
        assert!(!h.verify("Correct horse battery staple"));
        assert!(!h.verify(""));
    }

    #[test]
    fn phc_string_round_trips_through_parse() {
        let h = PasswordHash::generate("s3cret", TEST_ITERS).expect("gen");
        let encoded = h.to_phc_string();
        let parsed = PasswordHash::parse(&encoded).expect("parse");
        assert_eq!(parsed, h);
        assert_eq!(parsed.to_phc_string(), encoded);
        assert!(parsed.verify("s3cret"));
    }

    #[test]
    fn salt_is_random_per_generation() {
        let a = PasswordHash::generate("same", TEST_ITERS).expect("gen");
        let b = PasswordHash::generate("same", TEST_ITERS).expect("gen");
        assert_ne!(a, b, "two hashes of the same password must not collide");
        assert!(a.verify("same") && b.verify("same"));
    }

    #[test]
    fn parse_rejects_junk_and_wrong_algorithms() {
        for bad in [
            "",
            "hunter2",
            "$argon2id$v=19$m=65536,t=3,p=4$c2FsdA$aGFzaA",
            "$pbkdf2-sha256$i=100000$onlythreefields",
            "$pbkdf2-sha256$i=100000$c2FsdHNhbHRzYWx0c2FsdA$aGFzaA$extra",
            "$pbkdf2-sha256$100000$c2FsdHNhbHRzYWx0c2FsdA$aGFzaA",
            "$pbkdf2-sha256$i=notanumber$c2FsdHNhbHRzYWx0c2FsdA$aGFzaA",
        ] {
            assert!(PasswordHash::parse(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn parse_enforces_the_iteration_bounds() {
        let h = PasswordHash::generate("x", TEST_ITERS).expect("gen");
        let good = h.to_phc_string();
        assert!(PasswordHash::parse(&good).is_ok());

        // A ceiling is what stops a hostile stored credential from turning
        // one login attempt into a CPU exhaustion attack.
        let too_slow = good.replace("i=1000$", &format!("i={}$", u64::from(MAX_ITERATIONS) + 1));
        let err = PasswordHash::parse(&too_slow).expect_err("must reject");
        assert!(err.contains("outside the accepted range"), "{err}");

        let too_fast = good.replace("i=1000$", "i=999$");
        assert!(PasswordHash::parse(&too_fast).is_err(), "must reject a downgraded cost");
    }

    #[test]
    fn parse_rejects_a_truncated_key_or_salt() {
        let h = PasswordHash::generate("x", TEST_ITERS).expect("gen");
        let phc = h.to_phc_string();
        let mut fields: Vec<&str> = phc.split('$').collect();
        // fields = ["", alg, params, salt, key]
        let short_key = format!("{}${}${}$aGFzaA", fields[1], fields[2], fields[3]);
        assert!(PasswordHash::parse(&format!("${short_key}")).is_err());
        fields[3] = "c2hvcnQ"; // "short" — 5 bytes, under SALT_LEN
        assert!(PasswordHash::parse(&format!("${}", fields[1..].join("$"))).is_err());
    }

    #[test]
    fn generate_clamps_out_of_range_iteration_counts() {
        let low = PasswordHash::generate("x", 1).expect("gen");
        assert_eq!(low.iterations(), MIN_ITERATIONS);
        // Round-trips, which is the property the clamp exists to protect.
        assert!(PasswordHash::parse(&low.to_phc_string()).is_ok());
    }

    #[test]
    fn every_verify_attempt_runs_the_kdf() {
        // The uniform-work property this counter exists for: a *failed*
        // verify must cost the same derivation a successful one does.
        let h = PasswordHash::generate("right", TEST_ITERS).expect("gen");
        let before = kdf_invocations();
        assert!(!h.verify("wrong"));
        assert_eq!(kdf_invocations(), before + 1);
        assert!(h.verify("right"));
        assert_eq!(kdf_invocations(), before + 2);
    }
}
