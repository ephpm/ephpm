//! Streaming RESP2 protocol parser.
//!
//! Reads frames incrementally from a `BytesMut` buffer. Returns
//! `Ok(None)` when more data is needed (incomplete frame).

use bytes::BytesMut;

use super::frame::Frame;

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
        byte => Err(ParseError::Protocol(format!(
            "unexpected type byte: {byte:#04x}"
        ))),
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
    s.parse::<i64>()
        .map_err(|_| ParseError::Protocol(format!("invalid integer: {s}")))
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

    let len = len as usize;
    let total = after_len_line + len + 2; // data + \r\n

    if buf.len() < total {
        return Ok(None);
    }

    let data = buf[after_len_line..after_len_line + len].to_vec();
    let _ = buf.split_to(total);
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

    let count = count as usize;

    // We need to speculatively parse sub-frames without consuming `buf`
    // until we know the entire array is complete. Work on a snapshot of
    // the remaining bytes.
    let mut cursor = after_len_line;
    let mut items = Vec::with_capacity(count);

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
        assert_eq!(frame, Frame::Bulk(b"foobar".to_vec()));
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
                Frame::Bulk(b"GET".to_vec()),
                Frame::Bulk(b"key".to_vec()),
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
}
