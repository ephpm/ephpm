//! Request message-framing validation (HTTP request smuggling defence).
//!
//! Request smuggling needs two parties to disagree on where one request ends
//! and the next begins — typically a front proxy that frames by
//! `Content-Length` and a backend that frames by `Transfer-Encoding`, or the
//! reverse. hyper parses HTTP/1.x framing strictly, but it is *lenient* in the
//! cases RFC 9112 leaves to the server, and those are exactly the cases a
//! proxy in front of ePHPm may resolve differently:
//!
//! - a request carrying **both** `Content-Length` and `Transfer-Encoding` is
//!   accepted (TE wins) and the connection is kept alive. When `Content-Length`
//!   precedes `Transfer-Encoding` in the header block, both headers even reach
//!   the application. RFC 9112 §6.3 allows "TE wins" only if the server then
//!   closes the connection; rejecting is the safe choice behind a proxy.
//! - `Transfer-Encoding` is judged by its *last* coding only, so `gzip, chunked`
//!   or `, chunked` is accepted even though ePHPm decodes no coding but
//!   `chunked` — the application would receive a still-encoded body.
//! - an HTTP/1.1 request **without `Host`**, or with **several** `Host` fields,
//!   is accepted; RFC 9112 §3.2 requires a 400 for both. With several, which one
//!   names the virtual host depends on who you ask.
//! - an absolute-form target (`GET http://b/ HTTP/1.1`) whose authority differs
//!   from `Host` is accepted, and the router picks the vhost from `Host` while a
//!   proxy routes on the URI (RFC 9112 §3.2.2 says the URI wins).
//!
//! [`validate`] runs on every request **before** routing, middleware, the
//! reverse proxy or PHP see it (`serve_connection`, the HTTP→HTTPS redirect
//! listener and the HTTP/3 request handler). A violation is answered with
//! `400 Bad Request` and, on HTTP/1.x, `Connection: close` — hyper closes the
//! connection after writing it, so bytes the client pipelined behind a
//! malformed request are never parsed as a request.
//!
//! HTTP/2 and HTTP/3 have no `Transfer-Encoding` framing (the DATA frames carry
//! the body), so the TE/CL rules do not apply there. Instead `Transfer-Encoding`
//! and the other connection-specific fields make the request malformed (RFC
//! 9113 §8.2.2, RFC 9114 §4.2). The `h2` crate already rejects them, and it
//! checks `Content-Length` against the DATA frames. The `h3` crate (0.0.8) does
//! neither, so this module rejects the fields for HTTP/3, and
//! `http3::H3RequestBody` enforces the declared length.
//!
//! The check is O(number of header fields) and allocates nothing on the
//! accepting path.

use bytes::Bytes;
use http::header::{self, HeaderMap};
use http::{Request, Response, StatusCode, Uri, Version};
use http_body_util::Full;
use metrics::counter;

use crate::body::{self, ServerBody};

/// Why a request's framing was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingViolation {
    /// `Content-Length` and `Transfer-Encoding` on one HTTP/1.x request.
    ContentLengthWithTransferEncoding,
    /// A `Content-Length` value that is not a bare decimal number, or several
    /// `Content-Length` values that disagree.
    InvalidContentLength,
    /// A `Transfer-Encoding` other than exactly one `chunked` coding.
    UnsupportedTransferEncoding,
    /// `Transfer-Encoding` on an HTTP/1.0 request (RFC 9112 §6.1).
    TransferEncodingOnHttp10,
    /// An HTTP/1.1 request without a `Host` field (RFC 9112 §3.2).
    MissingHost,
    /// More than one `Host` field.
    MultipleHost,
    /// The request-target authority (absolute-form, or the HTTP/2 and HTTP/3
    /// `:authority`) names a different host than the `Host` field.
    HostAuthorityMismatch,
    /// A connection-specific field on an HTTP/2 or HTTP/3 request.
    ConnectionSpecificHeader,
}

impl FramingViolation {
    /// Stable label for the `reason` label of
    /// `ephpm_http_framing_rejected_total` and for logs.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::ContentLengthWithTransferEncoding => "cl_and_te",
            Self::InvalidContentLength => "invalid_content_length",
            Self::UnsupportedTransferEncoding => "unsupported_transfer_encoding",
            Self::TransferEncodingOnHttp10 => "te_on_http10",
            Self::MissingHost => "missing_host",
            Self::MultipleHost => "multiple_host",
            Self::HostAuthorityMismatch => "host_authority_mismatch",
            Self::ConnectionSpecificHeader => "connection_specific_header",
        }
    }
}

impl std::fmt::Display for FramingViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// Validate a request's message framing. See the module docs for the rules.
///
/// # Errors
///
/// Returns the first [`FramingViolation`] found.
pub fn validate<B>(req: &Request<B>) -> Result<(), FramingViolation> {
    validate_parts(req.version(), req.uri(), req.headers())
}

/// [`validate`] over the request's parts.
///
/// # Errors
///
/// Returns the first [`FramingViolation`] found.
pub fn validate_parts(
    version: Version,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), FramingViolation> {
    check_host(version, uri, headers)?;
    let has_content_length = check_content_length(headers)?;

    if version == Version::HTTP_2 || version == Version::HTTP_3 {
        return check_no_connection_specific_fields(headers);
    }

    if headers.contains_key(header::TRANSFER_ENCODING) {
        if version == Version::HTTP_10 || version == Version::HTTP_09 {
            return Err(FramingViolation::TransferEncodingOnHttp10);
        }
        if has_content_length {
            return Err(FramingViolation::ContentLengthWithTransferEncoding);
        }
        if !transfer_encoding_is_exactly_chunked(headers) {
            return Err(FramingViolation::UnsupportedTransferEncoding);
        }
    }
    Ok(())
}

/// At most one `Host`; exactly one on HTTP/1.1; and agreeing with the URI
/// authority when the target carries one.
fn check_host(version: Version, uri: &Uri, headers: &HeaderMap) -> Result<(), FramingViolation> {
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = hosts.next();
    if hosts.next().is_some() {
        return Err(FramingViolation::MultipleHost);
    }
    match (host, uri.authority()) {
        (None, _) if version == Version::HTTP_11 => Err(FramingViolation::MissingHost),
        // An exact (case-insensitive) comparison, port included. RFC 9112
        // §3.2.2 tells a server to *ignore* `Host` for an absolute-form target,
        // but ePHPm's vhost resolution reads `Host`; refusing the disagreement
        // is the one answer every hop in front of us agrees with.
        (Some(host), Some(authority))
            if !host.as_bytes().eq_ignore_ascii_case(authority.as_str().as_bytes()) =>
        {
            Err(FramingViolation::HostAuthorityMismatch)
        }
        _ => Ok(()),
    }
}

/// Every `Content-Length` value must be `1*DIGIT` and all values identical.
/// Returns whether any was present.
///
/// hyper already enforces this on HTTP/1.x (and `h2` on HTTP/2); it is checked
/// again because HTTP/3 reaches the router without it, and because the router's
/// body cap reads the header.
fn check_content_length(headers: &HeaderMap) -> Result<bool, FramingViolation> {
    let mut first: Option<&[u8]> = None;
    for value in headers.get_all(header::CONTENT_LENGTH) {
        let bytes = value.as_bytes();
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
            return Err(FramingViolation::InvalidContentLength);
        }
        match first {
            Some(prev) if prev != bytes => return Err(FramingViolation::InvalidContentLength),
            Some(_) => {}
            None => first = Some(bytes),
        }
    }
    Ok(first.is_some())
}

/// True when the `Transfer-Encoding` field(s), taken together, name exactly
/// one coding and it is `chunked`.
///
/// Only `chunked` is accepted because it is the only transfer coding hyper
/// decodes; anything layered under it (`gzip, chunked`) would reach PHP still
/// encoded, and a doubled `chunked, chunked` is invalid per RFC 9112 §7. Empty
/// list elements (`, chunked`) are refused rather than skipped, so a list a
/// lenient parser would read one way cannot be read another way here.
fn transfer_encoding_is_exactly_chunked(headers: &HeaderMap) -> bool {
    let mut seen_chunked = false;
    for value in headers.get_all(header::TRANSFER_ENCODING) {
        for element in value.as_bytes().split(|&b| b == b',') {
            let coding = trim_ows(element);
            if seen_chunked || !coding.eq_ignore_ascii_case(b"chunked") {
                return false;
            }
            seen_chunked = true;
        }
    }
    seen_chunked
}

/// RFC 9113 §8.2.2 / RFC 9114 §4.2: connection-specific fields make an HTTP/2
/// or HTTP/3 request malformed; `TE` is allowed only as `trailers`.
fn check_no_connection_specific_fields(headers: &HeaderMap) -> Result<(), FramingViolation> {
    const FORBIDDEN: [&str; 4] =
        ["transfer-encoding", "connection", "keep-alive", "proxy-connection"];
    if FORBIDDEN.iter().any(|name| headers.contains_key(*name))
        || headers.contains_key(header::UPGRADE)
    {
        return Err(FramingViolation::ConnectionSpecificHeader);
    }
    for value in headers.get_all(header::TE) {
        if !trim_ows(value.as_bytes()).eq_ignore_ascii_case(b"trailers") {
            return Err(FramingViolation::ConnectionSpecificHeader);
        }
    }
    Ok(())
}

/// Strip optional whitespace (SP / HTAB) from both ends.
fn trim_ows(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = bytes {
        bytes = rest;
    }
    bytes
}

/// The `400 Bad Request` answer to a framing violation. On HTTP/1.x it carries
/// `Connection: close` so hyper closes the connection after writing it; HTTP/2
/// and HTTP/3 must not carry that field (and a stream error there cannot
/// desynchronise other streams).
///
/// # Panics
///
/// Never: the status, headers and body are static and valid.
#[must_use]
pub fn bad_request(version: Version) -> Response<ServerBody> {
    let mut builder = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "text/plain");
    if version != Version::HTTP_2 && version != Version::HTTP_3 {
        builder = builder.header(header::CONNECTION, "close");
    }
    builder
        .body(body::buffered(Full::new(Bytes::from_static(b"400 Bad Request\n"))))
        .expect("static 400 response")
}

/// Whether the connection must be closed after answering `req`: any HTTP/1.x
/// request framed by `Transfer-Encoding`.
///
/// hyper *discards* a `Content-Length` that follows `Transfer-Encoding` in the
/// header block, so for that order [`validate`] cannot see the conflict and
/// the request is framed by TE. RFC 9112 §6.3 permits TE-wins only if the
/// server then closes the connection, and closing is also what defeats the
/// smuggling: bytes a `Content-Length`-honouring front end considered part of
/// this body are never parsed as a next request. The cost is that chunked
/// uploads do not reuse their connection (browsers never send chunked
/// requests; streaming upload clients rarely pipeline behind one).
#[must_use]
pub fn must_close_after<B>(req: &Request<B>) -> bool {
    req.version() != Version::HTTP_2
        && req.version() != Version::HTTP_3
        && req.headers().contains_key(header::TRANSFER_ENCODING)
}

/// Ask hyper to close the connection after writing `response`. Leaves a
/// `101 Switching Protocols` alone (the connection is handed off, not reused).
pub fn mark_connection_close(response: &mut Response<ServerBody>) {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        response.headers_mut().insert(header::CONNECTION, http::HeaderValue::from_static("close"));
    }
}

/// Validate `req`; on a violation record it and return the 400 to send.
///
/// The one call the listeners make, so the metric and log line are uniform.
#[must_use]
pub fn reject<B>(
    req: &Request<B>,
    remote_addr: std::net::SocketAddr,
) -> Option<Response<ServerBody>> {
    let violation = validate(req).err()?;
    counter!("ephpm_http_framing_rejected_total", "reason" => violation.reason()).increment(1);
    tracing::debug!(
        %remote_addr,
        reason = violation.reason(),
        version = ?req.version(),
        "rejected request with ambiguous message framing"
    );
    Some(bad_request(req.version()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(version: Version, uri: &str, headers: &[(&str, &str)]) -> Request<()> {
        let mut builder = Request::builder().version(version).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).expect("request")
    }

    fn h11(headers: &[(&str, &str)]) -> Result<(), FramingViolation> {
        validate(&req(Version::HTTP_11, "/", headers))
    }

    #[test]
    fn plain_requests_pass() {
        assert_eq!(h11(&[("host", "a")]), Ok(()));
        assert_eq!(h11(&[("host", "a"), ("content-length", "3")]), Ok(()));
        assert_eq!(h11(&[("host", "a"), ("transfer-encoding", "chunked")]), Ok(()));
        assert_eq!(h11(&[("host", "a"), ("transfer-encoding", " Chunked\t")]), Ok(()));
        // An empty Host is legal (RFC 9112 §3.2) when the target has no authority.
        assert_eq!(h11(&[("host", "")]), Ok(()));
        // HTTP/1.0 needs no Host.
        assert_eq!(validate(&req(Version::HTTP_10, "/", &[])), Ok(()));
    }

    #[test]
    fn content_length_with_transfer_encoding_is_refused() {
        assert_eq!(
            h11(&[("host", "a"), ("content-length", "3"), ("transfer-encoding", "chunked")]),
            Err(FramingViolation::ContentLengthWithTransferEncoding)
        );
    }

    #[test]
    fn content_length_values_must_agree_and_be_digits() {
        assert_eq!(h11(&[("host", "a"), ("content-length", "3"), ("content-length", "3")]), Ok(()));
        for bad in [
            &[("content-length", "3"), ("content-length", "4")][..],
            &[("content-length", "3, 3")],
            &[("content-length", "+3")],
            &[("content-length", "")],
            &[("content-length", "0x3")],
        ] {
            let mut headers = vec![("host", "a")];
            headers.extend_from_slice(bad);
            assert_eq!(h11(&headers), Err(FramingViolation::InvalidContentLength), "{bad:?}");
        }
    }

    #[test]
    fn only_a_single_chunked_coding_is_accepted() {
        for te in [
            "chunked, identity",
            "identity",
            "xchunked",
            "gzip, chunked",
            "chunked, chunked",
            ", chunked",
            "chunked,",
            "chunked;foo=bar",
            "",
        ] {
            assert_eq!(
                h11(&[("host", "a"), ("transfer-encoding", te)]),
                Err(FramingViolation::UnsupportedTransferEncoding),
                "{te:?}"
            );
        }
        // Split across two fields is still two codings.
        assert_eq!(
            h11(&[("host", "a"), ("transfer-encoding", "gzip"), ("transfer-encoding", "chunked")]),
            Err(FramingViolation::UnsupportedTransferEncoding)
        );
    }

    #[test]
    fn transfer_encoding_on_http10_is_refused() {
        assert_eq!(
            validate(&req(Version::HTTP_10, "/", &[("transfer-encoding", "chunked")])),
            Err(FramingViolation::TransferEncodingOnHttp10)
        );
    }

    #[test]
    fn host_rules() {
        assert_eq!(h11(&[]), Err(FramingViolation::MissingHost));
        assert_eq!(h11(&[("host", "a"), ("host", "b")]), Err(FramingViolation::MultipleHost));
        assert_eq!(h11(&[("host", "a"), ("host", "a")]), Err(FramingViolation::MultipleHost));
        assert_eq!(
            validate(&req(Version::HTTP_10, "/", &[("host", "a"), ("host", "b")])),
            Err(FramingViolation::MultipleHost)
        );
        assert_eq!(
            validate(&req(Version::HTTP_11, "http://b/", &[("host", "a")])),
            Err(FramingViolation::HostAuthorityMismatch)
        );
        assert_eq!(
            validate(&req(Version::HTTP_11, "http://A:8080/", &[("host", "a:8080")])),
            Ok(())
        );
    }

    #[test]
    fn http2_and_http3_refuse_connection_specific_fields() {
        for version in [Version::HTTP_2, Version::HTTP_3] {
            // No Host at all is normal: the authority is the `:authority`.
            assert_eq!(validate(&req(version, "https://a/", &[])), Ok(()));
            assert_eq!(validate(&req(version, "https://a/", &[("te", "trailers")])), Ok(()));
            assert_eq!(validate(&req(version, "https://a/", &[("content-length", "3")])), Ok(()));
            for (name, value) in [
                ("transfer-encoding", "chunked"),
                ("connection", "close"),
                ("keep-alive", "timeout=5"),
                ("proxy-connection", "close"),
                ("upgrade", "websocket"),
                ("te", "gzip"),
            ] {
                assert_eq!(
                    validate(&req(version, "https://a/", &[(name, value)])),
                    Err(FramingViolation::ConnectionSpecificHeader),
                    "{version:?} {name}"
                );
            }
            assert_eq!(
                validate(&req(version, "https://a/", &[("host", "b")])),
                Err(FramingViolation::HostAuthorityMismatch)
            );
        }
    }

    #[test]
    fn bad_request_closes_only_http1() {
        let r = bad_request(Version::HTTP_11);
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            r.headers().get(header::CONNECTION).map(http::HeaderValue::as_bytes),
            Some(&b"close"[..])
        );
        assert!(bad_request(Version::HTTP_2).headers().get(header::CONNECTION).is_none());
        assert!(bad_request(Version::HTTP_3).headers().get(header::CONNECTION).is_none());
    }
}
