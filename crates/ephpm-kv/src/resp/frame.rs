//! RESP2 protocol frame types.
//!
//! Implements the Redis Serialization Protocol as described in
//! <https://redis.io/docs/reference/protocol-spec/>.

use std::fmt;

use bytes::{BufMut, BytesMut};

/// A single RESP protocol frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `+OK\r\n` — status reply.
    Simple(String),
    /// `-ERR message\r\n` — error reply.
    Error(String),
    /// `:1000\r\n` — 64-bit signed integer.
    Integer(i64),
    /// `$6\r\nfoobar\r\n` — binary-safe string.
    Bulk(Vec<u8>),
    /// `*2\r\n...` — ordered collection of frames.
    Array(Vec<Frame>),
    /// `$-1\r\n` or `*-1\r\n` — null value.
    Null,
}

impl Frame {
    /// Convenience constructor for a simple `+OK` reply.
    #[must_use]
    pub fn ok() -> Self {
        Self::Simple("OK".into())
    }

    /// Convenience constructor for an error reply.
    #[must_use]
    pub fn error(msg: impl Into<String>) -> Self {
        Self::Error(msg.into())
    }

    /// Convenience constructor for a bulk string from `&str`.
    #[must_use]
    pub fn bulk(data: impl Into<Vec<u8>>) -> Self {
        Self::Bulk(data.into())
    }

    /// Convenience constructor for an integer reply.
    #[must_use]
    pub fn integer(n: i64) -> Self {
        Self::Integer(n)
    }

    /// Serialize this frame into the RESP wire format, appending to `buf`.
    pub fn write_to(&self, buf: &mut BytesMut) {
        match self {
            Self::Simple(s) => {
                buf.put_u8(b'+');
                buf.put_slice(s.as_bytes());
                buf.put_slice(b"\r\n");
            }
            Self::Error(s) => {
                buf.put_u8(b'-');
                buf.put_slice(s.as_bytes());
                buf.put_slice(b"\r\n");
            }
            Self::Integer(n) => {
                buf.put_u8(b':');
                // itoa is faster but we keep deps minimal for now.
                let s = n.to_string();
                buf.put_slice(s.as_bytes());
                buf.put_slice(b"\r\n");
            }
            Self::Bulk(data) => {
                buf.put_u8(b'$');
                let len = data.len().to_string();
                buf.put_slice(len.as_bytes());
                buf.put_slice(b"\r\n");
                buf.put_slice(data);
                buf.put_slice(b"\r\n");
            }
            Self::Array(items) => {
                buf.put_u8(b'*');
                let len = items.len().to_string();
                buf.put_slice(len.as_bytes());
                buf.put_slice(b"\r\n");
                for item in items {
                    item.write_to(buf);
                }
            }
            Self::Null => {
                buf.put_slice(b"$-1\r\n");
            }
        }
    }

    /// Returns the serialized RESP bytes for this frame.
    #[must_use]
    pub fn to_bytes(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(64);
        self.write_to(&mut buf);
        buf
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Simple(s) => write!(f, "+{s}"),
            Self::Error(s) => write!(f, "-{s}"),
            Self::Integer(n) => write!(f, ":{n}"),
            Self::Bulk(data) => {
                if let Ok(s) = std::str::from_utf8(data) {
                    write!(f, "\"{s}\"")
                } else {
                    write!(f, "<{} bytes>", data.len())
                }
            }
            Self::Array(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Self::Null => write!(f, "(nil)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_simple() {
        let f = Frame::ok();
        assert_eq!(&f.to_bytes()[..], b"+OK\r\n");
    }

    #[test]
    fn serialize_error() {
        let f = Frame::error("ERR unknown command");
        assert_eq!(&f.to_bytes()[..], b"-ERR unknown command\r\n");
    }

    #[test]
    fn serialize_integer() {
        let f = Frame::integer(42);
        assert_eq!(&f.to_bytes()[..], b":42\r\n");

        let f = Frame::integer(-1);
        assert_eq!(&f.to_bytes()[..], b":-1\r\n");
    }

    #[test]
    fn serialize_bulk() {
        let f = Frame::bulk("foobar");
        assert_eq!(&f.to_bytes()[..], b"$6\r\nfoobar\r\n");
    }

    #[test]
    fn serialize_null() {
        let f = Frame::Null;
        assert_eq!(&f.to_bytes()[..], b"$-1\r\n");
    }

    #[test]
    fn serialize_array() {
        let f = Frame::Array(vec![Frame::bulk("GET"), Frame::bulk("key")]);
        assert_eq!(&f.to_bytes()[..], b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n");
    }

    #[test]
    fn serialize_empty_bulk() {
        let f = Frame::bulk(vec![]);
        assert_eq!(&f.to_bytes()[..], b"$0\r\n\r\n");
    }
}
