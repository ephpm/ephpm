//! Blocking RESP client for the `ephpm php --site` KV wire (issue #700, 0.16.0).
//!
//! The DB CLI wire ([`crate::db_bridge::RemoteBackend`], driven by
//! `ephpm::site_db`) lets an out-of-band `ephpm php --site <key>` reach a
//! *running* server's per-vhost database. This is the symmetric path for the
//! KV store: it binds the `ephpm_kv_*` SAPI functions to the same running
//! server's live KV, so a cron job or cache warmer invoked as
//! `ephpm php --site <key> --config <path>` reads and writes the server's KV —
//! and its writes replicate across the cluster through the server's own
//! [`ephpm_kv::store::Store`], with no new replication code.
//!
//! # Why a blocking client
//!
//! The `ephpm_kv_*` functions are synchronous FFI callbacks invoked from PHP
//! threads. Unlike the DB wire (whose `mysql_async` backend is async and needs
//! a `Handle::block_on`), the KV wire speaks RESP, a trivial request/response
//! line protocol, over one connection. A plain blocking [`TcpStream`] with
//! read/write timeouts is the simplest thing that is correct here: no runtime,
//! no `block_on`, one round trip per call. The single connection is guarded by
//! a [`Mutex`] because RESP is strictly one-reply-per-request — two PHP threads
//! must not interleave frames on the wire — and because the CLI runs PHP on a
//! single thread anyway, there is no real contention.
//!
//! # Fail loud, fail bounded
//!
//! Every wait is bounded ([`CONNECT_TIMEOUT`], [`IO_TIMEOUT`]): a wedged or
//! disappeared server must make a KV call return its documented failure value
//! (`false` / `NULL` / `0` / `-2`), never hang the CLI thread. A transport
//! error drops the connection so the next call redials; it is surfaced to the
//! caller as a [`RemoteKvError`] for diagnostics, and the vtable maps it to the
//! op's failure value exactly as the local bridge maps a store miss.
//!
//! This module holds only the transport (no PHP types), so its frame
//! encode/decode, AUTH flow and timeout handling are unit-testable in stub mode
//! against an in-process mock listener. The vtable that calls into it lives in
//! [`crate::kv_bridge`], where it can reuse the staging buffers the local ops
//! already use.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

use bytes::BytesMut;
use ephpm_kv::resp::{Frame, parse_frame};

/// How long to wait for the TCP connection to the server to be established.
/// Loopback to a co-located server, so a live listener accepts well under
/// this; the ceiling only bounds a firewalled or wedged endpoint.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a single read or write on the connection may block. One RESP
/// round trip to a loopback server completes in microseconds; the ceiling
/// only fires on a server that accepted the connection but then stopped
/// responding, where failing the call beats hanging the PHP thread forever.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Where and how to reach the server's internal KV endpoint.
///
/// `user` / `password` carry the per-site HMAC credential
/// ([`ephpm_kv::auth::derive_site_password`]): in multi-tenant mode the
/// connection authenticates as `AUTH <user> <password>` and is scoped to that
/// vhost's store. Both are `None` only in a single-site, no-secret deployment,
/// where the endpoint demands no AUTH.
#[derive(Debug, Clone)]
pub struct RemoteKvEndpoint {
    /// Resolved loopback address of the server's internal KV listener.
    pub addr: SocketAddr,
    /// First `AUTH` argument — the site key, under per-site HMAC auth.
    pub user: Option<String>,
    /// The derived per-site RESP password; `None` sends no `AUTH`.
    pub password: Option<String>,
}

/// A transport-level failure reaching the server's KV endpoint. The vtable
/// maps every variant to the calling op's failure value; the variants exist
/// so the CLI can log *why* a call could not reach the store.
#[derive(Debug)]
pub enum RemoteKvError {
    /// Socket error (connect refused, timeout, reset, EOF).
    Io(std::io::Error),
    /// The reply could not be parsed as RESP, or was not the shape the op
    /// expected.
    Protocol(String),
    /// The server answered with a RESP `-ERR` (e.g. a failed `AUTH`).
    Server(String),
}

impl std::fmt::Display for RemoteKvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
            Self::Server(m) => write!(f, "server error: {m}"),
        }
    }
}

impl std::error::Error for RemoteKvError {}

impl From<std::io::Error> for RemoteKvError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// One live connection to the endpoint, plus its read buffer. The buffer keeps
/// any bytes read past a reply's end between calls (RESP replies are small and
/// a single read almost never over-reads, but the contract must hold).
#[derive(Debug)]
struct Conn {
    stream: TcpStream,
    buf: BytesMut,
}

/// A blocking RESP client bound to one server KV endpoint, holding a single
/// lazily-dialed connection.
#[derive(Debug)]
pub struct RemoteKvClient {
    endpoint: RemoteKvEndpoint,
    /// `None` until first use or after a transport error forces a redial.
    conn: Mutex<Option<Conn>>,
}

impl RemoteKvClient {
    /// Connect to `endpoint`, authenticate, and verify the endpoint answers
    /// (one `PING`), returning a ready client.
    ///
    /// Eager validation is deliberate, mirroring
    /// [`crate::db_bridge::RemoteBackend`]'s connect-at-bind contract: the CLI
    /// learns at bind time whether the credential works, rather than every
    /// `ephpm_kv_*` call silently failing later.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteKvError`] if the connection, authentication, or the
    /// `PING` round trip fails.
    pub fn connect(endpoint: RemoteKvEndpoint) -> Result<Self, RemoteKvError> {
        let client = Self { endpoint, conn: Mutex::new(None) };
        // Forces dial + AUTH; a bad credential surfaces here as Server(..).
        match client.command(&[b"PING"])? {
            Frame::Simple(_) | Frame::Bulk(_) => Ok(client),
            Frame::Error(e) => Err(RemoteKvError::Server(e)),
            other => Err(RemoteKvError::Protocol(format!("unexpected PING reply: {other:?}"))),
        }
    }

    /// The endpoint this client talks to (for diagnostics/logging).
    #[must_use]
    pub fn endpoint(&self) -> &RemoteKvEndpoint {
        &self.endpoint
    }

    /// Send one RESP command (its arguments as raw byte strings) and return the
    /// reply frame. Redials transparently when there is no live connection.
    ///
    /// On any transport error the connection is dropped so the next call
    /// redials against a possibly-restarted server.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteKvError`] on connect/AUTH/IO/parse failure.
    pub fn command(&self, args: &[&[u8]]) -> Result<Frame, RemoteKvError> {
        let mut guard = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(self.dial()?);
        }
        // `dial` just populated it when it was None; handle the impossible
        // empty case as a transport error rather than panicking in an
        // FFI-reachable path.
        let Some(conn) = guard.as_mut() else {
            return Err(RemoteKvError::Protocol("no connection after dial".into()));
        };
        let frame = encode_command(args);
        match send_recv(conn, &frame) {
            Ok(reply) => Ok(reply),
            Err(e) => {
                // Drop the (possibly half-broken) connection; next call redials.
                *guard = None;
                Err(e)
            }
        }
    }

    /// Open a fresh connection and authenticate it.
    fn dial(&self) -> Result<Conn, RemoteKvError> {
        let stream = TcpStream::connect_timeout(&self.endpoint.addr, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        // RESP is small request/response frames; Nagle + delayed ACK would
        // stall each round trip ~40 ms on Linux.
        let _ = stream.set_nodelay(true);
        let mut conn = Conn { stream, buf: BytesMut::with_capacity(4096) };

        if let Some(password) = &self.endpoint.password {
            let mut args: Vec<&[u8]> = vec![b"AUTH"];
            if let Some(user) = &self.endpoint.user {
                args.push(user.as_bytes());
            }
            args.push(password.as_bytes());
            let frame = encode_command(&args);
            match send_recv(&mut conn, &frame)? {
                Frame::Simple(_) => {}
                Frame::Error(e) => return Err(RemoteKvError::Server(e)),
                other => {
                    return Err(RemoteKvError::Protocol(format!(
                        "unexpected AUTH reply: {other:?}"
                    )));
                }
            }
        }
        Ok(conn)
    }
}

/// Encode a command as a RESP array of bulk strings.
fn encode_command(args: &[&[u8]]) -> Frame {
    Frame::Array(args.iter().map(|a| Frame::bulk(a.to_vec())).collect())
}

/// Write `frame` to `conn` and read exactly one reply frame back.
fn send_recv(conn: &mut Conn, frame: &Frame) -> Result<Frame, RemoteKvError> {
    let bytes = frame.to_bytes();
    conn.stream.write_all(&bytes)?;
    conn.stream.flush()?;

    let mut scratch = [0u8; 4096];
    loop {
        // Parse anything already buffered before issuing another read, so a
        // reply split across reads (or a pipelined over-read) is handled.
        match parse_frame(&mut conn.buf) {
            Ok(Some(reply)) => return Ok(reply),
            Ok(None) => {}
            Err(e) => return Err(RemoteKvError::Protocol(e.to_string())),
        }
        let n = conn.stream.read(&mut scratch)?;
        if n == 0 {
            return Err(RemoteKvError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "KV endpoint closed the connection",
            )));
        }
        conn.buf.extend_from_slice(&scratch[..n]);
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    /// A one-shot mock RESP server: for each accepted connection it reads a
    /// chunk, then replies with each canned response in order (one per request
    /// it is told to expect). Enough to exercise AUTH + a command round trip.
    fn mock_server(replies: Vec<Vec<u8>>) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                for reply in replies {
                    // Read one client request (best effort — one read is enough
                    // for the small frames these tests send).
                    let _ = stream.read(&mut scratch);
                    if stream.write_all(&reply).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
                // Hold the socket briefly so the client reads the last reply.
                thread::sleep(Duration::from_millis(50));
            }
        });
        (addr, handle)
    }

    #[test]
    fn encode_command_builds_a_resp_array_of_bulk_strings() {
        let frame = encode_command(&[b"SET", b"k", b"v"]);
        let bytes = frame.to_bytes();
        assert_eq!(&bytes[..], b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    }

    #[test]
    fn encode_command_preserves_binary_argument_bytes() {
        let frame = encode_command(&[b"SET", b"k", &[0x00, 0xff, 0x0d, 0x0a]]);
        let bytes = frame.to_bytes();
        // The 4-byte binary value is length-prefixed, never line-terminated.
        assert_eq!(&bytes[..], b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$4\r\n\x00\xff\r\n\r\n");
    }

    #[test]
    fn connect_sends_auth_then_ping_and_succeeds() {
        // Reply 1 → AUTH +OK, reply 2 → PING +PONG.
        let (addr, server) = mock_server(vec![b"+OK\r\n".to_vec(), b"+PONG\r\n".to_vec()]);
        let endpoint =
            RemoteKvEndpoint { addr, user: Some("shop".into()), password: Some("deadbeef".into()) };
        let client = RemoteKvClient::connect(endpoint).expect("connect");
        assert_eq!(client.endpoint().user.as_deref(), Some("shop"));
        server.join().unwrap();
    }

    #[test]
    fn connect_surfaces_a_failed_auth_as_server_error() {
        // AUTH is rejected before any PING can be sent.
        let (addr, server) = mock_server(vec![b"-ERR invalid password\r\n".to_vec()]);
        let endpoint =
            RemoteKvEndpoint { addr, user: Some("shop".into()), password: Some("wrong".into()) };
        let err = RemoteKvClient::connect(endpoint).expect_err("auth must fail");
        match err {
            RemoteKvError::Server(m) => assert!(m.contains("invalid password"), "{m}"),
            other => panic!("expected a server error, got {other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn command_round_trips_a_bulk_reply() {
        // AUTH ok, then GET → bulk "hello".
        let (addr, server) = mock_server(vec![b"+OK\r\n".to_vec(), b"$5\r\nhello\r\n".to_vec()]);
        let endpoint =
            RemoteKvEndpoint { addr, user: Some("shop".into()), password: Some("pw".into()) };
        // Dial lazily via `command` rather than the PING in `connect`.
        let client = RemoteKvClient { endpoint, conn: Mutex::new(None) };
        match client.command(&[b"GET", b"k"]).expect("get") {
            Frame::Bulk(b) => assert_eq!(&b[..], b"hello"),
            other => panic!("expected a bulk reply, got {other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn connect_refused_is_an_io_error_not_a_hang() {
        // Bind then drop, so the port is almost certainly closed.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = RemoteKvEndpoint { addr, user: None, password: None };
        let err = RemoteKvClient::connect(endpoint).expect_err("must fail fast");
        assert!(matches!(err, RemoteKvError::Io(_)), "expected an IO error, got {err:?}");
    }

    #[test]
    fn a_transport_error_drops_the_connection_for_redial() {
        // One connection: AUTH ok, one GET reply, then the server closes. The
        // second GET must see EOF, surface an error, and clear the cached conn.
        let (addr, server) = mock_server(vec![b"+OK\r\n".to_vec(), b"$1\r\nv\r\n".to_vec()]);
        let endpoint =
            RemoteKvEndpoint { addr, user: Some("s".into()), password: Some("pw".into()) };
        let client = RemoteKvClient { endpoint, conn: Mutex::new(None) };
        assert!(client.command(&[b"GET", b"k"]).is_ok());
        // Server has no more replies and will close; the next call errors.
        let _ = client.command(&[b"GET", b"k"]);
        assert!(
            client.conn.lock().unwrap().is_none(),
            "a transport error must clear the cached connection so the next call redials"
        );
        server.join().unwrap();
    }
}
