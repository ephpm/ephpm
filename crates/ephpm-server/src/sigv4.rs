//! AWS Signature Version 4, shared by the Route 53 DNS-01 provider and the
//! S3-compatible KV snapshot store.
//!
//! Implemented directly on the RustCrypto `hmac`/`sha2` crates already in the
//! tree rather than the AWS SDK: it is ~100 lines, pulls in no `ring` (the
//! workspace's single-crypto-provider rule, #241), and its one non-trivial
//! piece — the signing-key derivation — is pinned to the test vector AWS
//! publishes. Request canonicalisation follows the "Create a canonical
//! request" steps of the SigV4 specification: percent-encoded path, sorted
//! and encoded query, lowercase sorted headers, payload hash.

use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// The `x-amz-content-sha256` value for a payload the signer does not hash —
/// large uploads and downloads, where hashing a gigabyte twice buys nothing
/// over TLS. Accepted by S3, MinIO, R2 and GCS's S3 API.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// Static credentials: an access key pair and, for temporary credentials,
/// the session token sent as `x-amz-security-token`.
#[derive(Clone)]
pub struct Credentials {
    /// `AWS_ACCESS_KEY_ID`.
    pub access_key_id: String,
    /// `AWS_SECRET_ACCESS_KEY`.
    pub secret_access_key: String,
    /// `AWS_SESSION_TOKEN`, when present.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the secret; the key id alone identifies the principal.
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("session_token", &self.session_token.as_ref().map(|_| "<set>"))
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Read the standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
    /// `AWS_SESSION_TOKEN` variables. `None` when either key is unset or
    /// empty.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let access_key_id =
            std::env::var("AWS_ACCESS_KEY_ID").ok().filter(|s| !s.trim().is_empty())?;
        let secret_access_key =
            std::env::var("AWS_SECRET_ACCESS_KEY").ok().filter(|s| !s.trim().is_empty())?;
        let session_token =
            std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.trim().is_empty());
        Some(Self { access_key_id, secret_access_key, session_token })
    }
}

/// What to sign: everything the canonical request is built from.
pub struct SignInput<'a> {
    /// HTTP method, uppercase.
    pub method: &'a str,
    /// The `Host` header value (authority) the signature binds to.
    pub host: &'a str,
    /// Request path, unencoded (`/bucket/key with spaces`).
    pub path: &'a str,
    /// Query parameters, unencoded; the signer sorts and encodes them.
    pub query: &'a [(&'a str, &'a str)],
    /// Extra headers to sign, lowercase names, **sorted by name**, excluding
    /// `host`, `x-amz-date` and `x-amz-security-token` (added here).
    pub extra_headers: &'a [(&'a str, &'a str)],
    /// The payload hash the canonical request carries: a hex SHA-256 of the
    /// body ([`EMPTY_SHA256`] for none), or [`UNSIGNED_PAYLOAD`].
    pub payload_hash: &'a str,
    /// Also send and sign it as the `x-amz-content-sha256` header. S3
    /// requires the header; Route 53 does not use it.
    pub send_content_sha256: bool,
    /// Credential scope region, e.g. `us-east-1`.
    pub region: &'a str,
    /// Credential scope service, e.g. `s3` or `route53`.
    pub service: &'a str,
    /// Signing time, Unix seconds.
    pub unix_secs: u64,
}

/// The headers a signed request must carry: `x-amz-date`, `Authorization`,
/// and (when applicable) `x-amz-content-sha256` and `x-amz-security-token`.
#[derive(Debug, Clone)]
pub struct Signed {
    /// `x-amz-date`.
    pub amz_date: String,
    /// `Authorization`.
    pub authorization: String,
    /// Header pairs to add verbatim (the content hash and session token).
    pub headers: Vec<(&'static str, String)>,
    /// The canonical query string (already encoded), to append to the URL
    /// after `?` when non-empty.
    pub canonical_query: String,
}

/// Sign a request. Pure; the caller adds the returned headers.
#[must_use]
pub fn sign(creds: &Credentials, input: &SignInput<'_>) -> Signed {
    let (amz_date, datestamp) = aws_dates(input.unix_secs);
    let canonical_uri = uri_encode(input.path, false);
    let canonical_query = canonical_query_string(input.query);

    // Headers: host + extras + x-amz-* in lexical order of lowercase name.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(input.extra_headers.len() + 4);
    headers.push(("host".into(), input.host.to_string()));
    for (k, v) in input.extra_headers {
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    if input.send_content_sha256 {
        headers.push(("x-amz-content-sha256".into(), input.payload_hash.to_string()));
    }
    headers.push(("x-amz-date".into(), amz_date.clone()));
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();
    let mut canonical_headers = String::new();
    for (k, v) in &headers {
        let _ = writeln!(canonical_headers, "{k}:{v}");
    }
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let payload_hash = input.payload_hash;

    let canonical_request = format!(
        "{}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        input.method
    );
    let scope = format!("{datestamp}/{}/{}/aws4_request", input.region, input.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let key = derive_signing_key(&creds.secret_access_key, &datestamp, input.region, input.service);
    let signature = hex_lower(&hmac_sha256(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );
    let mut out_headers = Vec::new();
    if input.send_content_sha256 {
        out_headers.push(("x-amz-content-sha256", input.payload_hash.to_string()));
    }
    if let Some(token) = &creds.session_token {
        out_headers.push(("x-amz-security-token", token.clone()));
    }
    Signed { amz_date, authorization, headers: out_headers, canonical_query }
}

/// SHA-256 of the empty string, the payload hash of a bodiless request.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// `HMAC-SHA256(key, data)`.
///
/// # Panics
///
/// Never in practice: HMAC accepts a key of any length.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag);
    out
}

/// Lowercase hex of `SHA-256(data)`.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex_lower(&Sha256::digest(data))
}

/// Lowercase hex encoding of a byte slice.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Derive the SigV4 signing key: a four-step HMAC chain over the secret,
/// date, region, service and the literal `aws4_request`.
#[must_use]
pub fn derive_signing_key(secret: &str, datestamp: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// AWS URI-encoding: every byte outside the RFC 3986 unreserved set is
/// percent-encoded (uppercase hex). `/` is preserved when `encode_slash` is
/// false (used for the path) and encoded otherwise (used for query components).
#[must_use]
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Build the SigV4 canonical query string: each key and value URI-encoded, the
/// pairs sorted by encoded key (then value), joined with `&`.
#[must_use]
pub fn canonical_query_string(params: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> =
        params.iter().map(|(k, v)| (uri_encode(k, true), uri_encode(v, true))).collect();
    encoded.sort();
    encoded.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Split a Unix timestamp into the two SigV4 date strings: the amz timestamp
/// `YYYYMMDDTHHMMSSZ` and the credential-scope date stamp `YYYYMMDD`, both UTC.
#[must_use]
pub fn aws_dates(unix_secs: u64) -> (String, String) {
    let total = i64::try_from(unix_secs).unwrap_or(i64::MAX);
    let days = total.div_euclid(86_400);
    let tod = total.rem_euclid(86_400);
    let (hour, minute, second) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, day) = civil_from_days(days);
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → `(year, month,
/// day)` in the proleptic Gregorian calendar. Pure integer arithmetic, so it
/// needs no date crate.
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era, [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month, shifted so March = 0, [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Inverse of [`civil_from_days`]: days since 1970-01-01 for a proleptic
/// Gregorian date.
#[must_use]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an `x-amz-date` (`YYYYMMDDTHHMMSSZ`) back into Unix seconds.
#[must_use]
pub fn parse_amz_date(amz_date: &str) -> Option<u64> {
    let bytes = amz_date.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| amz_date[range].parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (hour, minute, second) = (num(9..11)?, num(11..13)?, num(13..15)?);
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(secs).ok()
}

/// Current time as Unix seconds (0 before the epoch).
#[must_use]
pub fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_preserves_unreserved_and_path_slash() {
        assert_eq!(uri_encode("_acme-challenge.a.b~c", true), "_acme-challenge.a.b~c");
        assert_eq!(uri_encode("/a/b", false), "/a/b");
        assert_eq!(uri_encode("/a/b", true), "%2Fa%2Fb");
        assert_eq!(uri_encode("a b+c", true), "a%20b%2Bc");
    }

    #[test]
    fn canonical_query_is_sorted_and_encoded() {
        let q =
            canonical_query_string(&[("type", "TXT"), ("name", "_x.example"), ("maxitems", "1")]);
        assert_eq!(q, "maxitems=1&name=_x.example&type=TXT");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 19723 days after the epoch is 2024-01-01 (54*365 + 13 leap days).
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn amz_date_round_trips() {
        for secs in [0u64, 1_369_353_600, 1_700_000_000, 4_102_444_800] {
            let (amz, _) = aws_dates(secs);
            assert_eq!(parse_amz_date(&amz), Some(secs), "{amz}");
        }
        assert_eq!(parse_amz_date("20130524T000000"), None);
        assert_eq!(days_from_civil(2024, 1, 1), 19_723);
    }

    #[test]
    fn aws_dates_formats_epoch() {
        let (amz, stamp) = aws_dates(0);
        assert_eq!(amz, "19700101T000000Z");
        assert_eq!(stamp, "19700101");
    }

    /// The signing-key derivation is validated against the vector AWS publishes
    /// in "Examples of how to derive a version 4 signing key".
    #[test]
    fn signing_key_matches_aws_documented_vector() {
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex_lower(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    /// The whole flow against the AWS "GET object" example from the S3
    /// SigV4 documentation (`examplebucket`, `test.txt`, 2013-05-24,
    /// `Range: bytes=0-9`, empty-payload hash).
    #[test]
    fn s3_get_object_matches_aws_documented_signature() {
        let creds = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let signed = sign(
            &creds,
            &SignInput {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
                extra_headers: &[("range", "bytes=0-9")],
                payload_hash: EMPTY_SHA256,
                send_content_sha256: true,
                region: "us-east-1",
                service: "s3",
                unix_secs: 1_369_353_600, // 2013-05-24T00:00:00Z
            },
        );
        assert_eq!(signed.amz_date, "20130524T000000Z");
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn session_token_is_signed_and_returned_as_a_header() {
        let creds = Credentials {
            access_key_id: "AKID".into(),
            secret_access_key: "secret".into(),
            session_token: Some("tok".into()),
        };
        let signed = sign(
            &creds,
            &SignInput {
                method: "PUT",
                host: "h",
                path: "/b/k",
                query: &[("list-type", "2"), ("prefix", "a b")],
                extra_headers: &[],
                payload_hash: UNSIGNED_PAYLOAD,
                send_content_sha256: true,
                region: "auto",
                service: "s3",
                unix_secs: 0,
            },
        );
        assert!(
            signed.authorization.contains(
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"
            )
        );
        assert_eq!(signed.canonical_query, "list-type=2&prefix=a%20b");
        assert_eq!(signed.headers.len(), 2);
        assert_eq!(signed.headers[1], ("x-amz-security-token", "tok".to_string()));
    }

    #[test]
    fn credentials_debug_never_prints_the_secret() {
        let creds = Credentials {
            access_key_id: "AKID".into(),
            secret_access_key: "hunter2".into(),
            session_token: Some("tok".into()),
        };
        let s = format!("{creds:?}");
        assert!(s.contains("AKID") && !s.contains("hunter2") && !s.contains("tok\""), "{s}");
    }
}
