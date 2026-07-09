//! Streaming RESP2 protocol parser.
//!
//! Reads frames incrementally from a `BytesMut` buffer. Returns
//! `Ok(None)` when more data is needed (incomplete frame).

use bytes::BytesMut;

use super::frame::Frame;

/// Maximum number of elements allowed in a RESP array (`*<count>\r\n`).
///
/// Mirrors Redis's hardcoded multibulk limit (`1024 * 1024`). The count is
/// attacker-controlled and read before any payload, so without this bound a
/// single small packet claiming a huge count would make `Vec::with_capacity`
/// overflow `isize::MAX` and panic (`RawVec` capacity overflow) — a trivial
/// remote DoS against the KV port.
const MAX_ARRAY_LEN: usize = 1024 * 1024;

/// Maximum number of bytes allowed in a RESP bulk string (`$<len>\r\n`).
///
/// Mirrors Redis's default `proto-max-bulk-len` (512 MiB). Bounds how large a
/// single claimed bulk can be before we reject the connection, so a client
/// cannot make us buffer unboundedly waiting on an absurd advertised length.
const MAX_BULK_LEN: usize = 512 * 1024 * 1024;

/// Upper bound on the speculative `Vec::with_capacity` for array parsing.
///
/// Even within `MAX_ARRAY_LEN` we don't trust the advertised count to size the
/// allocation up front — the array may be a fraction of that once parsed (or
/// incomplete). Pre-reserve only a small amount and let the vector grow.
const MAX_ARRAY_PREALLOC: usize = 1024;

/// Errors that can occur while parsing RESP frames.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The frame is incomplete — need more data from the socket.
    #[error("incomplete frame")]
    Incomplete,
    /// The frame contains invalid data.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Try to parse a complete RESP frame from `buf`.
///
/// On success, the consumed bytes are drained from `buf` and the parsed
/// [`Frame`] is returned. Returns `Ok(None)` when the buffer does not
/// yet contain a complete frame.
///
/// # Errors
///
/// Returns [`ParseError::Protocol`] when the buffer contains invalid RESP data.
pub fn parse_frame(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    if buf.is_empty() {
        return Ok(None);
    }

    // Peek at the type byte without consuming.
    match buf[0] {
        b'+' => parse_simple(buf),
        b'-' => parse_error(buf),
        b':' => parse_integer(buf),
        b'$' => parse_bulk(buf),
        b'*' => parse_array(buf),
        byte => Err(ParseError::Protocol(format!("unexpected type byte: {byte:#04x}"))),
    }
}

/// Find `\r\n` in `buf` starting at `offset`. Returns the index of `\r`.
fn find_crlf(buf: &[u8], offset: usize) -> Option<usize> {
    // memchr would be faster but we keep deps minimal.
    let data = &buf[offset..];
    for i in 0..data.len().saturating_sub(1) {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            return Some(offset + i);
        }
    }
    None
}

/// Parse a line ending in `\r\n`, returning the content between `start`
/// and the `\r`. Returns `Incomplete` if no CRLF found yet.
fn read_line(buf: &[u8], start: usize) -> Result<(usize, &[u8]), ParseError> {
    let crlf = find_crlf(buf, start).ok_or(ParseError::Incomplete)?;
    Ok((crlf + 2, &buf[start..crlf]))
}

/// Parse the integer in a RESP line (used for lengths and `:` frames).
fn parse_line_int(line: &[u8]) -> Result<i64, ParseError> {
    let s = std::str::from_utf8(line)
        .map_err(|_| ParseError::Protocol("non-UTF-8 integer line".into()))?;
    s.parse::<i64>().map_err(|_| ParseError::Protocol(format!("invalid integer: {s}")))
}

fn parse_simple(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    match read_line(buf, 1) {
        Ok((end, line)) => {
            let s = String::from_utf8_lossy(line).into_owned();
            let _ = buf.split_to(end);
            Ok(Some(Frame::Simple(s)))
        }
        Err(ParseError::Incomplete) => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_error(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    match read_line(buf, 1) {
        Ok((end, line)) => {
            let s = String::from_utf8_lossy(line).into_owned();
            let _ = buf.split_to(end);
            Ok(Some(Frame::Error(s)))
        }
        Err(ParseError::Incomplete) => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_integer(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    match read_line(buf, 1) {
        Ok((end, line)) => {
            let n = parse_line_int(line)?;
            let _ = buf.split_to(end);
            Ok(Some(Frame::Integer(n)))
        }
        Err(ParseError::Incomplete) => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_bulk(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    let (after_len_line, len_line) = match read_line(buf, 1) {
        Ok(v) => v,
        Err(ParseError::Incomplete) => return Ok(None),
        Err(e) => return Err(e),
    };

    let len = parse_line_int(len_line)?;

    // Null bulk string: $-1\r\n
    if len < 0 {
        let _ = buf.split_to(after_len_line);
        return Ok(Some(Frame::Null));
    }

    let len = usize::try_from(len)
        .map_err(|_| ParseError::Protocol("bulk string length out of range".into()))?;
    if len > MAX_BULK_LEN {
        return Err(ParseError::Protocol("invalid bulk length".into()));
    }
    let total = after_len_line + len + 2; // data + \r\n

    if buf.len() < total {
        return Ok(None);
    }

    // Drain the bulk payload straight out of the input buffer as a
    // zero-copy `Bytes` slice. `BytesMut::split_to` reuses the
    // underlying allocation, so we avoid the `.to_vec()` memcpy the old
    // path did — matters for large PUT bodies from PHP session writes.
    let head = buf.split_to(after_len_line);
    let data = buf.split_to(len).freeze();
    let _ = buf.split_to(2); // trailing \r\n
    drop(head);
    Ok(Some(Frame::Bulk(data)))
}

fn parse_array(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    let (after_len_line, len_line) = match read_line(buf, 1) {
        Ok(v) => v,
        Err(ParseError::Incomplete) => return Ok(None),
        Err(e) => return Err(e),
    };

    let count = parse_line_int(len_line)?;

    // Null array: *-1\r\n
    if count < 0 {
        let _ = buf.split_to(after_len_line);
        return Ok(Some(Frame::Null));
    }

    let count = usize::try_from(count)
        .map_err(|_| ParseError::Protocol("array count out of range".into()))?;
    if count > MAX_ARRAY_LEN {
        return Err(ParseError::Protocol("invalid multibulk length".into()));
    }

    // We need to speculatively parse sub-frames without consuming `buf`
    // until we know the entire array is complete. Work on a snapshot of
    // the remaining bytes. Reserve only a bounded amount up front: the
    // advertised `count` is trusted only as far as `MAX_ARRAY_LEN`, and even
    // then the array may be incomplete, so we let the vector grow rather than
    // allocating `count` slots from an unverified header.
    let mut cursor = after_len_line;
    let mut items = Vec::with_capacity(count.min(MAX_ARRAY_PREALLOC));

    for _ in 0..count {
        if cursor >= buf.len() {
            return Ok(None);
        }

        let mut sub = BytesMut::from(&buf[cursor..]);
        match parse_frame(&mut sub) {
            Ok(Some(frame)) => {
                let consumed = buf.len() - cursor - sub.len();
                cursor += consumed;
                items.push(frame);
            }
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        }
    }

    let _ = buf.split_to(cursor);
    Ok(Some(Frame::Array(items)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8]) -> Result<Option<Frame>, ParseError> {
        let mut buf = BytesMut::from(input);
        parse_frame(&mut buf)
    }

    #[test]
    fn simple_string() {
        let frame = parse(b"+OK\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Simple("OK".into()));
    }

    #[test]
    fn error_string() {
        let frame = parse(b"-ERR bad\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Error("ERR bad".into()));
    }

    #[test]
    fn integer() {
        let frame = parse(b":42\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Integer(42));
    }

    #[test]
    fn negative_integer() {
        let frame = parse(b":-7\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Integer(-7));
    }

    #[test]
    fn bulk_string() {
        let frame = parse(b"$6\r\nfoobar\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Bulk(bytes::Bytes::from_static(b"foobar")));
    }

    #[test]
    fn null_bulk() {
        let frame = parse(b"$-1\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Null);
    }

    #[test]
    fn array() {
        let frame = parse(b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n").unwrap().unwrap();
        assert_eq!(
            frame,
            Frame::Array(vec![
                Frame::Bulk(bytes::Bytes::from_static(b"GET")),
                Frame::Bulk(bytes::Bytes::from_static(b"key")),
            ])
        );
    }

    #[test]
    fn null_array() {
        let frame = parse(b"*-1\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Null);
    }

    #[test]
    fn empty_array() {
        let frame = parse(b"*0\r\n").unwrap().unwrap();
        assert_eq!(frame, Frame::Array(vec![]));
    }

    #[test]
    fn incomplete_returns_none() {
        assert!(parse(b"$6\r\nfoo").unwrap().is_none());
        assert!(parse(b"+OK").unwrap().is_none());
        assert!(parse(b"*2\r\n$3\r\nGET\r\n").unwrap().is_none());
        assert!(parse(b"").unwrap().is_none());
    }

    #[test]
    fn invalid_type_byte() {
        assert!(parse(b"!bad\r\n").is_err());
    }

    #[test]
    fn buffer_consumption() {
        let mut buf = BytesMut::from(&b"+OK\r\n+PONG\r\n"[..]);
        let f1 = parse_frame(&mut buf).unwrap().unwrap();
        assert_eq!(f1, Frame::Simple("OK".into()));
        let f2 = parse_frame(&mut buf).unwrap().unwrap();
        assert_eq!(f2, Frame::Simple("PONG".into()));
        assert!(buf.is_empty());
    }

    /// Regression for the fuzzer-found capacity-overflow panic
    /// (crash-b641210242cd3065794a600ec1da035d934c6c51): an array header
    /// claiming a count that fits in i64 but overflows `isize::MAX` when
    /// multiplied by `size_of::<Frame>()`. Must reject as a protocol error,
    /// never panic in `Vec::with_capacity`.
    #[test]
    fn huge_array_count_rejected_not_panicked() {
        // ~5.5e18 fits in i64 (< i64::MAX) but * size_of::<Frame>() overflows.
        let err = parse(b"*5555555555555554359\r\n").unwrap_err();
        assert!(matches!(err, ParseError::Protocol(_)), "got {err:?}");
    }

    /// An array count just over the multibulk cap is rejected; one at the cap
    /// with no payload is treated as incomplete (needs more data), not a panic.
    #[test]
    fn array_count_at_and_over_cap() {
        let over = format!("*{}\r\n", MAX_ARRAY_LEN + 1);
        assert!(matches!(parse(over.as_bytes()), Err(ParseError::Protocol(_))));

        // At the cap, with no element bytes available, the parser asks for
        // more data rather than allocating MAX_ARRAY_LEN slots up front.
        let at = format!("*{MAX_ARRAY_LEN}\r\n");
        assert!(parse(at.as_bytes()).unwrap().is_none());
    }

    /// A bulk length over `proto-max-bulk-len` is rejected as a protocol error
    /// instead of letting the connection buffer toward an absurd size.
    #[test]
    fn huge_bulk_len_rejected() {
        let over = format!("${}\r\n", MAX_BULK_LEN + 1);
        assert!(matches!(parse(over.as_bytes()), Err(ParseError::Protocol(_))));
    }

    /// The exact fuzzer crash input must parse without panicking — every
    /// frame either parses, is rejected, or asks for more data.
    #[test]
    fn fuzz_crash_input_does_not_panic() {
        let crash: &[u8] = &[
            0x2a, 0x31, 0x0d, 0x0a, 0x2a, 0x31, 0x0d, 0x0a, 0x2b, 0x51, 0x0d, 0x00, 0x51, 0x00,
            0x51, 0x00, 0x2a, 0x00, 0x3a, 0x21, 0x24, 0x35, 0xff, 0xd7, 0xd7, 0xd7, 0x5b, 0x51,
            0x51, 0xd3, 0x51, 0x51, 0x2a, 0x31, 0x60, 0x0d, 0x0a, 0x2a, 0x31, 0x0d, 0x0a, 0x2a,
            0x31, 0x34, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35,
            0x35, 0x35, 0x35, 0x34, 0x33, 0x35, 0x39, 0x0d, 0x0a, 0x43, 0x0d, 0x0a, 0x0d, 0xff,
            0x0d, 0x0a, 0x0d, 0xff, 0xff, 0x0d, 0x0a, 0xd7, 0xff, 0x03,
        ];
        let mut buf = BytesMut::from(crash);
        // Drain frames until the parser stops yielding complete ones; the
        // assertion under test is simply that none of these calls panics.
        while let Ok(Some(_)) = parse_frame(&mut buf) {}
    }
}
