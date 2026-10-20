//! Streaming RESP2 protocol parser.
//!
//! Reads frames incrementally from a `BytesMut` buffer. Returns
//! `Ok(None)` when more data is needed (incomplete frame).
//!
//! Parsing is two-phase. Scanning walks the buffer by index and produces a
//! `FrameSpec` tree in which bulk payloads are recorded as byte ranges,
//! without copying or consuming anything. Only once a whole frame is known
//! to be present does the [`Decoder`] split it off the buffer and
//! materialise the [`Frame`], slicing bulk payloads out of the frozen
//! `Bytes` (still zero-copy).
//!
//! Three properties fall out of that design:
//!
//! - Array elements are scanned against an index, so an N-element array
//!   costs O(N) work. The parser before that copied the entire remaining
//!   buffer into a fresh `BytesMut` per element.
//! - Scanning is **resumable** (KV stress report F-17). A [`Decoder`] keeps
//!   the elements it has already scanned, plus where to resume its `\r\n`
//!   search, across calls. A frame that arrives in many small reads is
//!   therefore scanned once in total, not once per read: restarting from
//!   byte 0 on every read cost ~100x the CPU for a 980 KB `MGET` trickled in
//!   100-byte writes.
//! - Nesting is capped at `MAX_ARRAY_DEPTH` (32), and the scan is a loop
//!   over an explicit stack rather than recursion, so ~40 KiB of repeated
//!   `*1\r\n` is a protocol error on one connection instead of a stack
//!   overflow that takes down the process.
//!
//! A request decoder ([`Decoder::request`]) also accepts Redis **inline
//! commands** (KV stress report F-16): a top-level frame that does not start
//! with `*` is one line of space-separated arguments, split with Redis's
//! quoting rules (`sdssplitargs`) — so `PING` typed into `telnet` or `nc`
//! works, as it does against Redis.

use std::ops::Range;

use bytes::{Buf, Bytes, BytesMut};

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
/// Mirrors Redis's hardcoded `PROTO_NESTED_MULTIBULK_DEPTH` (32). Nesting is
/// attacker-controlled; real RESP2 client traffic is one level deep.
const MAX_ARRAY_DEPTH: usize = 32;

/// Maximum length of an inline command line without its newline.
///
/// Mirrors Redis's `PROTO_INLINE_MAX_SIZE` (64 KiB): a client that sends a
/// line this long with no `\n` is refused instead of buffered.
const MAX_INLINE_LEN: usize = 64 * 1024;

/// Errors that can occur while parsing RESP frames.
///
/// An incomplete frame is not an error — [`Decoder::decode`] reports it as
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

/// An array whose header has been scanned but whose elements have not all
/// arrived yet.
#[derive(Debug)]
struct PartialArray {
    items: Vec<FrameSpec>,
    remaining: usize,
}

/// A scanned inline command: its arguments, and the index just past its `\n`.
type InlineCommand = (Vec<Vec<u8>>, usize);

/// What one scan step at `pos` produced.
enum Step {
    /// The buffer does not hold the whole element yet.
    Incomplete,
    /// A complete element ending just before the given index.
    Element(FrameSpec, usize),
    /// An array header for a non-empty array; elements start at the index.
    ArrayStart(usize, usize),
}

/// Resumable RESP frame decoder.
///
/// Keep one per connection and call [`decode`](Self::decode) after every
/// read. Between calls the caller may only **append** to the buffer: the
/// decoder remembers positions in it. It resets itself after each complete
/// frame and after an error, so a caller that discards the buffer on an
/// error (as the server does) needs nothing else.
#[derive(Debug)]
pub struct Decoder {
    /// Accept inline commands at the top level (request side).
    inline: bool,
    /// Start of the next element to scan.
    pos: usize,
    /// Where to resume the line-terminator search for the element at `pos`
    /// (always `>= pos`), so a long line arriving in pieces is searched once.
    resume: usize,
    /// Arrays still waiting for elements, outermost first.
    stack: Vec<PartialArray>,
    /// Bytes examined while scanning — lets tests assert that work is linear
    /// in the input rather than timing it.
    #[cfg(test)]
    work: usize,
}

impl Decoder {
    /// A decoder for client requests: RESP arrays plus inline commands.
    #[must_use]
    pub fn request() -> Self {
        Self::new(true)
    }

    /// A decoder for any RESP2 frame (e.g. server replies). Inline commands
    /// are not accepted: a frame must start with a RESP type byte.
    #[must_use]
    pub fn response() -> Self {
        Self::new(false)
    }

    fn new(inline: bool) -> Self {
        Self {
            inline,
            pos: 0,
            resume: 0,
            stack: Vec::new(),
            #[cfg(test)]
            work: 0,
        }
    }

    fn reset(&mut self) {
        self.pos = 0;
        self.resume = 0;
        self.stack.clear();
    }

    /// Try to decode one complete frame from `buf`.
    ///
    /// On success the frame's bytes are drained from `buf` and the frame is
    /// returned. Returns `Ok(None)` when `buf` does not hold a complete frame
    /// yet; the partial scan is kept, so the next call only looks at what
    /// was appended.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::Protocol`] for invalid RESP data, an array
    /// nested deeper than 32 levels, or (request decoders) an inline command
    /// longer than 64 KiB or with unbalanced quotes. The decoder is reset;
    /// the caller should discard `buf`, since framing cannot be recovered.
    pub fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
        let result = self.decode_inner(buf);
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn decode_inner(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
        loop {
            if self.inline && self.stack.is_empty() && self.pos == 0 {
                match buf.first() {
                    None => return Ok(None),
                    Some(b'*') => {}
                    Some(_) => match self.scan_inline(buf)? {
                        None => return Ok(None),
                        Some((args, end)) => {
                            buf.advance(end);
                            self.reset();
                            // An empty line is consumed silently, as Redis does.
                            if args.is_empty() {
                                continue;
                            }
                            return Ok(Some(Frame::Array(
                                args.into_iter().map(|a| Frame::Bulk(Bytes::from(a))).collect(),
                            )));
                        }
                    },
                }
            }

            let (mut spec, mut end) = match self.scan_element(buf)? {
                Step::Incomplete => return Ok(None),
                Step::ArrayStart(count, start) => {
                    self.stack.push(PartialArray {
                        items: Vec::with_capacity(count.min(MAX_ARRAY_PREALLOC)),
                        remaining: count,
                    });
                    self.pos = start;
                    self.resume = start;
                    continue;
                }
                Step::Element(spec, end) => (spec, end),
            };

            self.pos = end;
            self.resume = end;
            // Fold the finished element into its parent; a parent that is
            // now full is itself a finished element of *its* parent.
            loop {
                let Some(top) = self.stack.last_mut() else {
                    // Top level: the whole frame is present.
                    let data = buf.split_to(end).freeze();
                    self.reset();
                    return Ok(Some(materialize(spec, &data)));
                };
                top.items.push(spec);
                top.remaining -= 1;
                if top.remaining > 0 {
                    break;
                }
                let done = self.stack.pop().map(|a| a.items).unwrap_or_default();
                spec = FrameSpec::Array(done);
                end = self.pos;
            }
        }
    }

    /// Find the `\r\n` ending the line whose content starts at `start`,
    /// resuming where the previous call for this element stopped. Returns
    /// the index of the `\r`.
    fn find_crlf(&mut self, buf: &[u8], start: usize) -> Option<usize> {
        let from = self.resume.max(start);
        let hit = buf[from.min(buf.len())..].windows(2).position(|w| w == b"\r\n");
        #[cfg(test)]
        {
            self.work += hit.map_or(buf.len().saturating_sub(from), |i| i + 2);
        }
        match hit {
            Some(i) => Some(from + i),
            None => {
                // Keep the last byte: it may be the `\r` of a split `\r\n`.
                self.resume = buf.len().saturating_sub(1).max(start);
                None
            }
        }
    }

    /// Scan the element starting at `self.pos`.
    fn scan_element(&mut self, buf: &[u8]) -> Result<Step, ParseError> {
        let start = self.pos;
        let Some(&type_byte) = buf.get(start) else {
            return Ok(Step::Incomplete);
        };
        #[cfg(test)]
        {
            self.work += 1;
        }
        if !matches!(type_byte, b'+' | b'-' | b':' | b'$' | b'*') {
            return Err(ParseError::Protocol(format!("unexpected type byte: {type_byte:#04x}")));
        }
        // Reject before descending: an array header at the cap would be
        // level 33. Checked for every array (null and empty included).
        if type_byte == b'*' && self.stack.len() >= MAX_ARRAY_DEPTH {
            return Err(ParseError::Protocol(format!(
                "array nesting deeper than {MAX_ARRAY_DEPTH} levels"
            )));
        }
        let Some(crlf) = self.find_crlf(buf, start + 1) else {
            return Ok(Step::Incomplete);
        };
        let line = &buf[start + 1..crlf];
        let after = crlf + 2;

        match type_byte {
            b'+' => Ok(Step::Element(
                FrameSpec::Simple(String::from_utf8_lossy(line).into_owned()),
                after,
            )),
            b'-' => Ok(Step::Element(
                FrameSpec::Error(String::from_utf8_lossy(line).into_owned()),
                after,
            )),
            b':' => Ok(Step::Element(FrameSpec::Integer(parse_line_int(line)?), after)),
            b'$' => {
                let len = parse_line_int(line)?;
                // Null bulk string: $-1\r\n
                if len < 0 {
                    return Ok(Step::Element(FrameSpec::Null, after));
                }
                let len = usize::try_from(len)
                    .map_err(|_| ParseError::Protocol("bulk string length out of range".into()))?;
                if len > MAX_BULK_LEN {
                    return Err(ParseError::Protocol("invalid bulk length".into()));
                }
                let data_end = after + len;
                let end = data_end + 2; // data + \r\n
                if buf.len() < end {
                    // Resume at the header's `\r`: the next call re-reads
                    // only the short length line, never the payload.
                    self.resume = crlf;
                    return Ok(Step::Incomplete);
                }
                Ok(Step::Element(FrameSpec::Bulk(after..data_end), end))
            }
            _ => {
                // b'*'
                let count = parse_line_int(line)?;
                // Null array: *-1\r\n
                if count < 0 {
                    return Ok(Step::Element(FrameSpec::Null, after));
                }
                let count = usize::try_from(count)
                    .map_err(|_| ParseError::Protocol("array count out of range".into()))?;
                if count > MAX_ARRAY_LEN {
                    return Err(ParseError::Protocol("invalid multibulk length".into()));
                }
                if count == 0 {
                    return Ok(Step::Element(FrameSpec::Array(Vec::new()), after));
                }
                Ok(Step::ArrayStart(count, after))
            }
        }
    }

    /// Scan an inline command at the start of `buf`: one `\n`-terminated
    /// line (an optional `\r` before it is dropped) split into arguments.
    /// Returns the arguments and the index just past the `\n`.
    fn scan_inline(&mut self, buf: &[u8]) -> Result<Option<InlineCommand>, ParseError> {
        let from = self.resume.min(buf.len());
        let hit = buf[from..].iter().position(|&b| b == b'\n');
        #[cfg(test)]
        {
            self.work += hit.map_or(buf.len() - from, |i| i + 1);
        }
        let Some(i) = hit else {
            if buf.len() > MAX_INLINE_LEN {
                return Err(ParseError::Protocol("too big inline request".into()));
            }
            self.resume = buf.len();
            return Ok(None);
        };
        let nl = from + i;
        if nl > MAX_INLINE_LEN {
            return Err(ParseError::Protocol("too big inline request".into()));
        }
        let line = buf[..nl].strip_suffix(b"\r").unwrap_or(&buf[..nl]);
        let args = split_inline_args(line)
            .ok_or_else(|| ParseError::Protocol("unbalanced quotes in request".into()))?;
        Ok(Some((args, nl + 1)))
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::request()
    }
}

/// Try to parse one complete RESP frame (no inline commands) from `buf`.
///
/// A one-shot [`Decoder::response`]: fine for a reply that is read whole,
/// but it rescans from the start on every call — a connection that reads in
/// a loop should keep a [`Decoder`] instead.
///
/// # Errors
///
/// Returns [`ParseError::Protocol`] when the buffer contains invalid RESP
/// data, including an array nested deeper than `MAX_ARRAY_DEPTH` (32).
pub fn parse_frame(buf: &mut BytesMut) -> Result<Option<Frame>, ParseError> {
    Decoder::response().decode(buf)
}

/// Turn a scanned [`FrameSpec`] into a [`Frame`], slicing bulk payloads
/// out of `data` (the frozen bytes the spec was scanned against).
///
/// Recursion is bounded by [`MAX_ARRAY_DEPTH`], enforced during scanning.
/// Every range came from a bulk scan, which only emits ranges fully inside
/// the scanned region, so the slices are always in bounds.
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

/// Parse the integer in a RESP line (used for lengths and `:` frames).
fn parse_line_int(line: &[u8]) -> Result<i64, ParseError> {
    let s = std::str::from_utf8(line)
        .map_err(|_| ParseError::Protocol("non-UTF-8 integer line".into()))?;
    s.parse::<i64>().map_err(|_| ParseError::Protocol(format!("invalid integer: {s}")))
}

/// `isspace()` in the C locale, which is what Redis's `sdssplitargs` uses.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Split an inline command line into arguments with Redis's `sdssplitargs`
/// rules. Returns `None` for unbalanced quotes, or a closing quote that is
/// not followed by whitespace or the end of the line.
///
/// - Arguments are separated by whitespace.
/// - `"..."` supports `\n \r \t \b \a`, `\xHH`, and `\<any>` for `<any>`.
/// - `'...'` supports only `\'`.
/// - A quote may start mid-argument (`a"b c"` is the one argument `ab c`).
fn split_inline_args(line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut p = 0;
    loop {
        while p < line.len() && is_space(line[p]) {
            p += 1;
        }
        if p == line.len() {
            return Some(args);
        }
        let mut current = Vec::new();
        let mut in_double = false;
        let mut in_single = false;
        loop {
            let c = line.get(p).copied();
            if in_double {
                match c {
                    None => return None, // unterminated "
                    Some(b'\\') if line.get(p + 1) == Some(&b'x') => {
                        if let (Some(h), Some(l)) = (
                            line.get(p + 2).copied().and_then(hex_val),
                            line.get(p + 3).copied().and_then(hex_val),
                        ) {
                            current.push(h * 16 + l);
                            p += 3;
                        } else {
                            // `\x` not followed by two hex digits: a plain `x`.
                            current.push(b'x');
                            p += 1;
                        }
                    }
                    Some(b'\\') if p + 1 < line.len() => {
                        p += 1;
                        current.push(match line[p] {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'b' => 0x08,
                            b'a' => 0x07,
                            other => other,
                        });
                    }
                    Some(b'"') => {
                        // The closing quote must end the argument.
                        if line.get(p + 1).is_some_and(|&n| !is_space(n)) {
                            return None;
                        }
                        p += 1;
                        break;
                    }
                    Some(other) => current.push(other),
                }
            } else if in_single {
                match c {
                    None => return None, // unterminated '
                    Some(b'\\') if line.get(p + 1) == Some(&b'\'') => {
                        p += 1;
                        current.push(b'\'');
                    }
                    Some(b'\'') => {
                        if line.get(p + 1).is_some_and(|&n| !is_space(n)) {
                            return None;
                        }
                        p += 1;
                        break;
                    }
                    Some(other) => current.push(other),
                }
            } else {
                match c {
                    None => break,
                    Some(b) if is_space(b) => break,
                    Some(b'"') => in_double = true,
                    Some(b'\'') => in_single = true,
                    Some(other) => current.push(other),
                }
            }
            p += 1;
        }
        args.push(current);
    }
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

    // ── Inline commands (KV stress report F-16) ─────────────────────────

    fn bulks(items: &[&[u8]]) -> Frame {
        Frame::Array(items.iter().map(|b| Frame::Bulk(Bytes::copy_from_slice(b))).collect())
    }

    fn decode_all(input: &[u8]) -> Result<Vec<Frame>, ParseError> {
        let mut dec = Decoder::request();
        let mut buf = BytesMut::from(input);
        let mut out = Vec::new();
        while let Some(f) = dec.decode(&mut buf)? {
            out.push(f);
        }
        Ok(out)
    }

    /// `PING` typed into telnet used to be `-ERR protocol error: unexpected
    /// type byte: 0x50`.
    #[test]
    fn inline_ping_is_a_command() {
        assert_eq!(decode_all(b"PING\r\n").unwrap(), vec![bulks(&[b"PING"])]);
        // A bare `\n` (nc without -C) works too.
        assert_eq!(decode_all(b"PING\n").unwrap(), vec![bulks(&[b"PING"])]);
    }

    #[test]
    fn inline_splits_on_whitespace() {
        assert_eq!(
            decode_all(b"  SET\tkey   value \r\n").unwrap(),
            vec![bulks(&[b"SET", b"key", b"value"])]
        );
    }

    #[test]
    fn inline_quoting_follows_redis_sdssplitargs() {
        assert_eq!(
            decode_all(b"SET \"a b\" 'c d'\r\n").unwrap(),
            vec![bulks(&[b"SET", b"a b", b"c d"])]
        );
        // Double-quote escapes, including \xHH; single quotes only know \'.
        assert_eq!(
            decode_all(b"ECHO \"x\\n\\t\\x41\\\"\\\\\" 'it\\'s \\n'\r\n").unwrap(),
            vec![bulks(&[b"ECHO", b"x\n\tA\"\\", b"it's \\n"])]
        );
        // A quote may start mid-argument; empty quotes are an empty argument.
        assert_eq!(
            decode_all(b"SET k\"e y\" \"\"\r\n").unwrap(),
            vec![bulks(&[b"SET", b"ke y", b""])]
        );
    }

    #[test]
    fn inline_unbalanced_quotes_are_a_protocol_error() {
        for bad in [&b"SET \"abc\r\n"[..], b"SET 'abc\r\n", b"SET \"a\"b\r\n", b"SET 'a'b\r\n"] {
            let err = decode_all(bad).unwrap_err();
            assert!(err.to_string().contains("unbalanced quotes"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn inline_empty_lines_are_skipped() {
        assert_eq!(decode_all(b"\r\n\n   \r\nPING\r\n").unwrap(), vec![bulks(&[b"PING"])]);
    }

    /// Inline and multibulk requests can be pipelined on one connection.
    #[test]
    fn inline_and_multibulk_interleave() {
        assert_eq!(
            decode_all(b"PING\r\n*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\nECHO yo\r\n").unwrap(),
            vec![bulks(&[b"PING"]), bulks(&[b"ECHO", b"hi"]), bulks(&[b"ECHO", b"yo"])]
        );
    }

    #[test]
    fn inline_split_across_reads() {
        let mut dec = Decoder::request();
        let mut buf = BytesMut::from(&b"SET k "[..]);
        assert!(dec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"v\r");
        assert!(dec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"\n");
        assert_eq!(dec.decode(&mut buf).unwrap(), Some(bulks(&[b"SET", b"k", b"v"])));
        assert!(buf.is_empty());
    }

    /// Redis's `PROTO_INLINE_MAX_SIZE`: a newline-less line over 64 KiB is
    /// refused rather than buffered.
    #[test]
    fn inline_line_over_64k_is_rejected() {
        let mut dec = Decoder::request();
        let mut buf = BytesMut::from(&vec![b'a'; MAX_INLINE_LEN][..]);
        assert!(dec.decode(&mut buf).unwrap().is_none(), "at the cap: wait for more");
        buf.extend_from_slice(b"a");
        let err = dec.decode(&mut buf).unwrap_err();
        assert!(err.to_string().contains("too big inline request"), "{err}");
    }

    /// The response-side parser (used by the CLI client) still requires a
    /// RESP type byte.
    #[test]
    fn response_decoder_does_not_accept_inline() {
        assert!(parse(b"PING\r\n").is_err());
    }

    // ── Resumable scanning (KV stress report F-17) ──────────────────────

    /// Feed `input` in `chunk`-byte reads and return the decoded frames and
    /// the bytes the scanner examined.
    fn trickle(input: &[u8], chunk: usize) -> (Vec<Frame>, usize) {
        let mut dec = Decoder::request();
        let mut buf = BytesMut::new();
        let mut frames = Vec::new();
        for piece in input.chunks(chunk) {
            buf.extend_from_slice(piece);
            while let Some(f) = dec.decode(&mut buf).unwrap() {
                frames.push(f);
            }
        }
        assert!(buf.is_empty(), "every byte must have been consumed");
        (frames, dec.work)
    }

    fn mget_request(keys: usize) -> Vec<u8> {
        let mut out = format!("*{}\r\n$4\r\nMGET\r\n", keys + 1).into_bytes();
        for i in 0..keys {
            let key = format!("key:{i:08}");
            out.extend_from_slice(format!("${}\r\n{key}\r\n", key.len()).as_bytes());
        }
        out
    }

    /// The stress-run shape: a ~980 KB `MGET` arriving in 100-byte reads.
    /// Restarting the scan from byte 0 on every read examined ~4.8 GB here
    /// (quadratic); a resumable scan looks at each byte a bounded number of
    /// times, however the input is split.
    #[test]
    fn trickled_large_array_costs_linear_work() {
        let input = mget_request(50_000);
        assert!(input.len() > 900_000);

        let (whole, whole_work) = trickle(&input, input.len());
        let (frames, work) = trickle(&input, 100);
        assert_eq!(frames, whole, "chunking must not change the result");
        assert_eq!(frames.len(), 1);
        let Frame::Array(items) = &frames[0] else { panic!("expected an array") };
        assert_eq!(items.len(), 50_001);

        assert!(
            work <= 4 * input.len(),
            "100-byte reads examined {work} bytes for a {} byte frame (whole: {whole_work})",
            input.len()
        );
    }

    /// The worst case for a rescan: one byte per read. Each call re-reads
    /// the element's type byte and the last byte of the previous read (a
    /// possible `\r`), so the constant is ~3; a rescan from byte 0 would be
    /// ~input.len() / 2 (~19,000 here).
    #[test]
    fn byte_by_byte_costs_linear_work() {
        let input = mget_request(2_000);
        let (frames, work) = trickle(&input, 1);
        assert_eq!(frames.len(), 1);
        assert!(work <= 4 * input.len(), "examined {work} bytes for {}", input.len());
    }

    /// A large bulk payload arriving slowly re-reads only its length line,
    /// never the payload bytes already received.
    #[test]
    fn trickled_large_bulk_costs_linear_work() {
        let value = vec![b'v'; 500_000];
        let mut input = format!("*3\r\n$3\r\nSET\r\n$1\r\nk\r\n${}\r\n", value.len()).into_bytes();
        input.extend_from_slice(&value);
        input.extend_from_slice(b"\r\n");
        let (frames, work) = trickle(&input, 100);
        assert_eq!(frames.len(), 1);
        // The payload is located by length, not searched: work is tiny.
        assert!(work <= input.len() / 10, "examined {work} bytes for {}", input.len());
    }

    /// A long inline line arriving slowly is searched for its newline once.
    #[test]
    fn trickled_inline_line_costs_linear_work() {
        let mut input = b"ECHO ".to_vec();
        input.extend_from_slice(&vec![b'x'; 60_000]);
        input.extend_from_slice(b"\r\n");
        let (frames, work) = trickle(&input, 1);
        assert_eq!(frames.len(), 1);
        assert!(work <= 4 * input.len(), "examined {work} bytes for {}", input.len());
    }

    /// Pipelined frames split at every possible boundary decode identically.
    #[test]
    fn every_split_point_decodes_the_same() {
        let input: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\nPING\r\n*1\r\n*2\r\n:1\r\n$-1\r\n";
        let expected = decode_all(input).unwrap();
        assert_eq!(expected.len(), 3);
        for split in 0..=input.len() {
            let mut dec = Decoder::request();
            let mut buf = BytesMut::from(&input[..split]);
            let mut got = Vec::new();
            while let Some(f) = dec.decode(&mut buf).unwrap() {
                got.push(f);
            }
            buf.extend_from_slice(&input[split..]);
            while let Some(f) = dec.decode(&mut buf).unwrap() {
                got.push(f);
            }
            assert_eq!(got, expected, "split at {split}");
        }
    }

    /// After a protocol error the decoder starts clean on fresh input.
    #[test]
    fn decoder_resets_after_error() {
        let mut dec = Decoder::request();
        let mut buf = BytesMut::from(&b"*1\r\n!oops\r\n"[..]);
        assert!(dec.decode(&mut buf).is_err());
        buf.clear();
        buf.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");
        assert_eq!(dec.decode(&mut buf).unwrap(), Some(bulks(&[b"PING"])));
    }
}
