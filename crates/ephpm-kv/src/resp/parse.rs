//! Streaming RESP2 protocol parser.
//!
//! Reads frames incrementally from a `BytesMut` buffer. Returns
//! `Ok(None)` when more data is needed (incomplete frame).
//!
//! Parsing is two-phase. `scan_frame` walks the buffer by index and
//! produces a `FrameSpec` tree in which bulk payloads are recorded as
//! byte ranges, without copying or consuming anything. Only once a whole
//! frame is known to be present does [`parse_frame`] split it off the
//! buffer and materialise the [`Frame`], slicing bulk payloads out of the
//! frozen `Bytes` (still zero-copy).
//!
//! Two properties fall out of that split, both of which the previous
//! recursive-on-`BytesMut` parser got wrong:
//!
//! - Array elements are scanned against an index, so an N-element array
//!   costs O(N) work. The old parser copied the entire remaining buffer
//!   into a fresh `BytesMut` per element — quadratic, and repeated on
//!   every socket read while the array was still incomplete.
//! - Nesting is capped at `MAX_ARRAY_DEPTH` (32). Without a cap, ~40 KiB of
//!   repeated `*1\r\n` (far below `max_input_buffer`) recursed thousands
//!   of frames deep and overflowed the tokio worker stack, taking down
//!   the whole process rather than the one connection.

use std::ops::Range;

use bytes::{Bytes, BytesMut};

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

/// Maximum nesting depth allowed for RESP arrays.
///
/// Mirrors Redis's hardcoded `PROTO_NESTED_MULTIBULK_DEPTH` (32). Nesting
/// is attacker-controlled and each level costs a stack frame in
/// [`scan_frame`], so without a cap a small packet of repeated `*1\r\n`
/// overflows the thread stack — which aborts the entire process, not just
/// the offending connection. Real RESP2 client traffic is one level deep.
const MAX_ARRAY_DEPTH: usize = 32;

/// Errors that can occur while parsing RESP frames.
///
/// An incomplete frame is not an error — [`parse_frame`] reports it as
/// `Ok(None)` so the caller reads more from the socket and retries.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The frame contains invalid data.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// A frame that has been located in the buffer but not yet materialised.
///
/// Bulk payloads are held as byte ranges into the buffer that was scanned,
/// so scanning allocates nothing per payload and copies nothing.
#[derive(Debug)]
enum FrameSpec {
    /// `+<line>\r\n`
    Simple(String),
    /// `-<line>\r\n`
    Error(String),
    /// `:<int>\r\n`
    Integer(i64),
    /// A null bulk string (`$-1\r\n`) or null array (`*-1\r\n`).
    Null,
    /// `$<len>\r\n<payload>\r\n` — the range covers `<payload>`.
    Bulk(Range<usize>),
    /// `*<count>\r\n<elements...>`
    Array(Vec<FrameSpec>),
}

/// Try to parse a complete RESP frame from `buf`.
///
/// On success, the consumed bytes are drained from `buf` and the parsed
/// [`Frame`] is returned. Returns `Ok(None)` when the buffer does not
/// yet contain a complete frame.
///
/// # Errors
///
/// Returns [`ParseError::Protocol`] when the buffer contains invalid RESP
/// data, including an array nested deeper than `MAX_ARRAY_DEPTH` (32).
pub fn parse_frame(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    // Phase 1: locate the frame without consuming or copying anything.
    let Some((spec, end)) = scan_frame(&buf[..], 0, 0)? else {
        return Ok(None);
    };

    // Phase 2: the frame is known-complete, so take exactly its bytes.
    // `split_to().freeze()` reuses the existing allocation, and every
    // bulk payload is a `Bytes` slice of it — no payload is ever copied.
    let data = buf.split_to(end).freeze();
    Ok(Some(materialize(spec, &data)))
}

/// Turn a scanned [`FrameSpec`] into a [`Frame`], slicing bulk payloads
/// out of `data` (the frozen bytes the spec was scanned against).
///
/// Recursion is bounded by [`MAX_ARRAY_DEPTH`], enforced during scanning.
/// Every range came from [`scan_bulk`], which only emits ranges fully
/// inside the scanned region, so the slices are always in bounds.
fn materialize(spec: FrameSpec, data: &Bytes) -> Frame {
    match spec {
        FrameSpec::Simple(s) => Frame::Simple(s),
        FrameSpec::Error(s) => Frame::Error(s),
        FrameSpec::Integer(n) => Frame::Integer(n),
        FrameSpec::Null => Frame::Null,
        FrameSpec::Bulk(range) => Frame::Bulk(data.slice(range)),
        FrameSpec::Array(items) => {
            Frame::Array(items.into_iter().map(|item| materialize(item, data)).collect())
        }
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

/// Read a line ending in `\r\n`, returning the index just past the CRLF
/// and the content between `start` and the `\r`.
///
/// Returns `None` when the buffer does not yet contain a full line.
fn read_line(buf: &[u8], start: usize) -> Option<(usize, &[u8])> {
    let crlf = find_crlf(buf, start)?;
    Some((crlf + 2, &buf[start..crlf]))
}

/// Parse the integer in a RESP line (used for lengths and `:` frames).
fn parse_line_int(line: &[u8]) -> Result<i64, ParseError> {
    let s = std::str::from_utf8(line)
        .map_err(|_| ParseError::Protocol("non-UTF-8 integer line".into()))?;
    s.parse::<i64>().map_err(|_| ParseError::Protocol(format!("invalid integer: {s}")))
}

/// Locate one frame starting at `start`, without consuming `buf`.
///
/// Returns the frame's spec and the index just past its last byte, or
/// `None` when the buffer does not yet hold the whole frame. `depth` is
/// the array nesting level of this frame (0 at the top level).
fn scan_frame(
    buf: &[u8],
    start: usize,
    depth: usize,
) -> Result<Option<(FrameSpec, usize)>, ParseError> {
    if start >= buf.len() {
        return Ok(None);
    }

    match buf[start] {
        b'+' => Ok(scan_line(buf, start).map(|(s, end)| (FrameSpec::Simple(s), end))),
        b'-' => Ok(scan_line(buf, start).map(|(s, end)| (FrameSpec::Error(s), end))),
        b':' => scan_integer(buf, start),
        b'$' => scan_bulk(buf, start),
        b'*' => scan_array(buf, start, depth),
        byte => Err(ParseError::Protocol(format!("unexpected type byte: {byte:#04x}"))),
    }
}

/// Scan a `\r\n`-terminated payload line (`+`/`-` frames).
fn scan_line(buf: &[u8], start: usize) -> Option<(String, usize)> {
    let (end, line) = read_line(buf, start + 1)?;
    Some((String::from_utf8_lossy(line).into_owned(), end))
}

fn scan_integer(buf: &[u8], start: usize) -> Result<Option<(FrameSpec, usize)>, ParseError> {
    let Some((end, line)) = read_line(buf, start + 1) else {
        return Ok(None);
    };
    let n = parse_line_int(line)?;
    Ok(Some((FrameSpec::Integer(n), end)))
}

fn scan_bulk(buf: &[u8], start: usize) -> Result<Option<(FrameSpec, usize)>, ParseError> {
    let Some((after_len_line, len_line)) = read_line(buf, start + 1) else {
        return Ok(None);
    };

    let len = parse_line_int(len_line)?;

    // Null bulk string: $-1\r\n
    if len < 0 {
        return Ok(Some((FrameSpec::Null, after_len_line)));
    }

    let len = usize::try_from(len)
        .map_err(|_| ParseError::Protocol("bulk string length out of range".into()))?;
    if len > MAX_BULK_LEN {
        return Err(ParseError::Protocol("invalid bulk length".into()));
    }

    let data_end = after_len_line + len;
    let end = data_end + 2; // data + \r\n
    if buf.len() < end {
        return Ok(None);
    }

    Ok(Some((FrameSpec::Bulk(after_len_line..data_end), end)))
}

fn scan_array(
    buf: &[u8],
    start: usize,
    depth: usize,
) -> Result<Option<(FrameSpec, usize)>, ParseError> {
    // Reject before recursing: this is the only thing standing between a
    // few kilobytes of `*1\r\n` and a stack overflow that kills the
    // process. See `MAX_ARRAY_DEPTH`.
    if depth >= MAX_ARRAY_DEPTH {
        return Err(ParseError::Protocol(format!(
            "array nesting deeper than {MAX_ARRAY_DEPTH} levels"
        )));
    }

    let Some((after_len_line, len_line)) = read_line(buf, start + 1) else {
        return Ok(None);
    };

    let count = parse_line_int(len_line)?;

    // Null array: *-1\r\n
    if count < 0 {
        return Ok(Some((FrameSpec::Null, after_len_line)));
    }

    let count = usize::try_from(count)
        .map_err(|_| ParseError::Protocol("array count out of range".into()))?;
    if count > MAX_ARRAY_LEN {
        return Err(ParseError::Protocol("invalid multibulk length".into()));
    }

    // Reserve only a bounded amount up front: the advertised `count` is
    // trusted only as far as `MAX_ARRAY_LEN`, and even then the array may
    // be incomplete, so we let the vector grow rather than allocating
    // `count` slots from an unverified header.
    let mut cursor = after_len_line;
    let mut items = Vec::with_capacity(count.min(MAX_ARRAY_PREALLOC));

    for _ in 0..count {
        // Elements are scanned in place against an index — no per-element
        // copy of the remaining buffer.
        let Some((item, end)) = scan_frame(buf, cursor, depth + 1)? else {
            return Ok(None);
        };
        items.push(item);
        cursor = end;
    }

    Ok(Some((FrameSpec::Array(items), cursor)))
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

    /// Nesting exactly at the cap still parses: 32 levels of single-element
    /// arrays wrapping a simple string.
    #[test]
    fn nesting_at_depth_cap_parses() {
        let mut input = "*1\r\n".repeat(MAX_ARRAY_DEPTH);
        input.push_str("+OK\r\n");
        let frame = parse(input.as_bytes()).unwrap();
        assert!(frame.is_some(), "nesting at the cap must parse");
    }

    /// One level past the cap is a protocol error, not a deeper recursion.
    #[test]
    fn nesting_past_depth_cap_rejected() {
        let mut input = "*1\r\n".repeat(MAX_ARRAY_DEPTH + 1);
        input.push_str("+OK\r\n");
        let err = parse(input.as_bytes()).unwrap_err();
        assert!(matches!(err, ParseError::Protocol(_)), "got {err:?}");
    }

    /// The DoS shape: ~40 KiB of repeated `*1\r\n` — well under the 1 MiB
    /// `max_input_buffer`, but ~10,000 frames deep. Before the depth cap
    /// this overflowed the tokio worker stack and SIGSEGV'd the whole
    /// process. It must now be a plain protocol error on one connection.
    #[test]
    fn deeply_nested_array_does_not_overflow_stack() {
        let input = "*1\r\n".repeat(10_000);
        let err = parse(input.as_bytes()).unwrap_err();
        assert!(matches!(err, ParseError::Protocol(_)), "got {err:?}");
    }

    /// Bulk payloads inside an array are slices of the input allocation,
    /// not copies — the property the index-based scan exists to preserve.
    #[test]
    fn nested_bulk_payloads_are_zero_copy_slices() {
        let mut buf = BytesMut::from(&b"*1\r\n$3\r\nabc\r\n"[..]);
        let base = buf.as_ptr() as usize;
        let frame = parse_frame(&mut buf).unwrap().unwrap();
        let Frame::Array(items) = frame else { panic!("expected an array") };
        let Frame::Bulk(ref data) = items[0] else { panic!("expected a bulk") };
        assert_eq!(&data[..], b"abc");
        // The payload points into the original buffer's allocation at the
        // offset where "abc" sits (4 bytes of "*1\r\n" + 4 of "$3\r\n").
        assert_eq!(data.as_ptr() as usize, base + 8, "bulk payload was copied");
    }

    /// A partially received array must not consume anything: the caller
    /// re-parses the same buffer after the next socket read.
    #[test]
    fn incomplete_array_leaves_buffer_untouched() {
        let mut buf = BytesMut::from(&b"*2\r\n$3\r\nGET\r\n"[..]);
        let before = buf.len();
        assert!(parse_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), before, "incomplete frame must not be consumed");

        // Completing it yields the whole array.
        buf.extend_from_slice(b"$3\r\nkey\r\n");
        let frame = parse_frame(&mut buf).unwrap().unwrap();
        assert_eq!(
            frame,
            Frame::Array(vec![
                Frame::Bulk(bytes::Bytes::from_static(b"GET")),
                Frame::Bulk(bytes::Bytes::from_static(b"key")),
            ])
        );
        assert!(buf.is_empty());
    }

    /// Nested arrays round-trip with their elements intact.
    #[test]
    fn nested_array_round_trip() {
        let frame = parse(b"*2\r\n*1\r\n:1\r\n$2\r\nhi\r\n").unwrap().unwrap();
        assert_eq!(
            frame,
            Frame::Array(vec![
                Frame::Array(vec![Frame::Integer(1)]),
                Frame::Bulk(bytes::Bytes::from_static(b"hi")),
            ])
        );
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
