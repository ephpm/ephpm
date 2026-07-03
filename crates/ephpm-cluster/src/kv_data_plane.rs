//! TCP data plane for fetching and storing large KV values on remote nodes.
//!
//! When a key's value exceeds the gossip tier threshold, it is stored
//! locally on the owner node. Other nodes fetch or store it on demand
//! via a simple TCP protocol:
//!
//! ## Wire protocol (plaintext)
//!
//! **Request:** `[op: u8][key_len: u32 BE][key: bytes]`
//!
//! - `op = 0x00` (GET): fetch a value.
//! - `op = 0x01` (SET): store a value. Followed by `[value_len: u32 BE][value: bytes]`.
//!
//! **GET response (found):** `[value_len: u32 BE][value: bytes]`
//!
//! **GET response (not found):** `[0xFFFFFFFF: u32 BE]` (sentinel)
//!
//! **SET response:** `[status: u8]` where `0x00` = success, `0x01` = rejected (e.g., OOM).
//!
//! The sentinel `u32::MAX` indicates the key was not found. This is
//! unambiguous because real values are bounded by memory limits well
//! below 4 GiB.
//!
//! ## Wire protocol (encrypted)
//!
//! When `[cluster] secret` is set, both sides derive a
//! ChaCha20-Poly1305 key via [`ClusterCipher::for_kv_data_plane`] and
//! each logical message above (the whole request, the whole response)
//! becomes one sealed frame:
//!
//! ```text
//! [frame_len: u32 BE][nonce: 12 bytes][ciphertext + 16-byte tag]
//! ```
//!
//! The plaintext inside a frame is byte-identical to the plaintext
//! protocol message. A peer without the matching secret cannot read
//! values or inject writes: frames that fail authentication cause the
//! connection to be dropped without a response.

use std::net::SocketAddr;
use std::sync::Arc;

use ephpm_kv::store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::secure_transport::{ClusterCipher, SEAL_OVERHEAD};

/// Sentinel value indicating "key not found" in the wire protocol.
const NOT_FOUND_SENTINEL: u32 = u32::MAX;

/// Maximum key length accepted by the server (64 KiB).
const MAX_KEY_LEN: u32 = 64 * 1024;

/// Maximum value length accepted by the server (64 MiB).
const MAX_VALUE_LEN: u32 = 64 * 1024 * 1024;

/// Maximum sealed frame length accepted in encrypted mode: the largest
/// legal plaintext message (SET request) plus sealing overhead.
const MAX_FRAME_LEN: u32 = 1 + 4 + MAX_KEY_LEN + 4 + MAX_VALUE_LEN + SEAL_OVERHEAD as u32;

/// Op code for GET requests.
const OP_GET: u8 = 0x00;

/// Op code for SET requests.
const OP_SET: u8 = 0x01;

/// SET response: success.
const SET_OK: u8 = 0x00;

/// SET response: rejected (e.g., memory limit with `NoEviction`).
const SET_REJECTED: u8 = 0x01;

/// Start the TCP KV data plane listener on all interfaces at `port`.
///
/// Serves lookups against the local [`Store`] so remote cluster nodes
/// can fetch large values that exceed the gossip tier threshold.
///
/// When `cipher` is `Some`, every request and response is exchanged as
/// a sealed frame; connections that fail authentication are dropped.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve(
    store: Arc<Store>,
    port: u16,
    cipher: Option<Arc<ClusterCipher>>,
) -> anyhow::Result<()> {
    serve_on(store, ([0, 0, 0, 0], port).into(), cipher).await
}

/// Start the TCP KV data plane listener bound to a specific address.
///
/// Like [`serve`] but binds `addr` exactly instead of `0.0.0.0:port`.
/// Useful when several nodes share one host (e.g. in-process tests on
/// distinct `127.0.0.x` loopback addresses).
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve_on(
    store: Arc<Store>,
    addr: SocketAddr,
    cipher: Option<Arc<ClusterCipher>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind KV data plane to {addr}: {e}"))?;
    tracing::info!(%addr, encrypted = cipher.is_some(), "KV data plane listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!(%e, "KV data plane accept error");
                continue;
            }
        };
        let store = Arc::clone(&store);
        let cipher = cipher.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &store, cipher.as_deref()).await {
                tracing::debug!(%peer, %e, "KV data plane connection error");
            }
        });
    }
}

/// A parsed data plane request.
struct Request {
    /// Op code ([`OP_GET`] or [`OP_SET`]).
    op: u8,
    /// The key being fetched or stored.
    key: String,
    /// The value for SET requests.
    value: Option<Vec<u8>>,
}

/// Handle a single TCP connection: read one request, write one response.
async fn handle_connection(
    mut stream: TcpStream,
    store: &Store,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    let request = read_request(&mut stream, cipher).await?;

    let response = match request.op {
        OP_GET => {
            // Look up in local store and build the response.
            if let Some(value) = store.get(&request.key) {
                let len = u32::try_from(value.len()).unwrap_or(NOT_FOUND_SENTINEL - 1);
                let mut response = Vec::with_capacity(4 + value.len());
                response.extend_from_slice(&len.to_be_bytes());
                response.extend_from_slice(&value);
                response
            } else {
                NOT_FOUND_SENTINEL.to_be_bytes().to_vec()
            }
        }
        OP_SET => {
            let value = request.value.expect("SET request always carries a value");
            let ok = store.set(request.key, value, None);
            vec![if ok { SET_OK } else { SET_REJECTED }]
        }
        other => {
            anyhow::bail!("unknown op code: {other:#04x}");
        }
    };

    write_message(&mut stream, &response, cipher).await
}

/// Read and validate one request from the stream.
///
/// In plaintext mode the fields are read incrementally off the wire; in
/// encrypted mode one sealed frame is read and parsed in memory.
async fn read_request(
    stream: &mut TcpStream,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Request> {
    if let Some(cipher) = cipher {
        let frame = read_frame(stream, cipher).await?;
        return parse_request(&frame);
    }

    // Plaintext: incremental reads, byte-compatible with older nodes.
    let op = stream.read_u8().await?;

    let key_len = stream.read_u32().await?;
    if key_len > MAX_KEY_LEN {
        anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
    }
    let mut key_buf = vec![0u8; key_len as usize];
    stream.read_exact(&mut key_buf).await?;
    let key = String::from_utf8(key_buf).map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;

    let value = if op == OP_SET {
        let value_len = stream.read_u32().await?;
        if value_len > MAX_VALUE_LEN {
            anyhow::bail!("value length {value_len} exceeds maximum {MAX_VALUE_LEN}");
        }
        let mut value_buf = vec![0u8; value_len as usize];
        stream.read_exact(&mut value_buf).await?;
        Some(value_buf)
    } else {
        None
    };

    Ok(Request { op, key, value })
}

/// Parse a plaintext request message from a decrypted frame.
fn parse_request(mut buf: &[u8]) -> anyhow::Result<Request> {
    let op = take_u8(&mut buf)?;

    let key_len = take_u32(&mut buf)?;
    if key_len > MAX_KEY_LEN {
        anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
    }
    let key = String::from_utf8(take_bytes(&mut buf, key_len as usize)?.to_vec())
        .map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;

    let value = if op == OP_SET {
        let value_len = take_u32(&mut buf)?;
        if value_len > MAX_VALUE_LEN {
            anyhow::bail!("value length {value_len} exceeds maximum {MAX_VALUE_LEN}");
        }
        Some(take_bytes(&mut buf, value_len as usize)?.to_vec())
    } else {
        None
    };

    Ok(Request { op, key, value })
}

/// Write one protocol message, sealing it into a frame when encrypted.
async fn write_message(
    stream: &mut TcpStream,
    message: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    match cipher {
        Some(cipher) => {
            let sealed = cipher.seal(message)?;
            let len = u32::try_from(sealed.len())
                .map_err(|_| anyhow::anyhow!("sealed frame too large"))?;
            stream.write_u32(len).await?;
            stream.write_all(&sealed).await?;
        }
        None => {
            stream.write_all(message).await?;
        }
    }
    stream.flush().await?;
    Ok(())
}

/// Read one sealed frame and decrypt it.
async fn read_frame(stream: &mut TcpStream, cipher: &ClusterCipher) -> anyhow::Result<Vec<u8>> {
    let frame_len = stream.read_u32().await?;
    if frame_len > MAX_FRAME_LEN {
        anyhow::bail!(
            "frame length {frame_len} exceeds maximum {MAX_FRAME_LEN} \
             (peer sending plaintext to an encrypted data plane?)"
        );
    }
    let mut frame = vec![0u8; frame_len as usize];
    stream.read_exact(&mut frame).await?;
    cipher.open(&frame).ok_or_else(|| {
        anyhow::anyhow!("failed to authenticate KV data plane frame (wrong cluster secret?)")
    })
}

/// Fetch a value from a remote node's KV data plane.
///
/// Opens a TCP connection to `addr`, sends a GET request, and reads the
/// response. Returns `None` if the remote node does not have the key.
/// `cipher` must match the remote node's setting (both `Some` with the
/// same secret, or both `None`).
///
/// # Errors
///
/// Returns an error on connection or I/O failure, or when the peer's
/// encryption setting or secret does not match.
pub async fn fetch_remote(
    addr: SocketAddr,
    key: &str,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut stream = TcpStream::connect(addr).await?;

    // Send GET op + key.
    write_message(&mut stream, &encode_get_request(key)?, cipher).await?;

    // Read response.
    if let Some(cipher) = cipher {
        let frame = read_frame(&mut stream, cipher).await?;
        let mut buf = frame.as_slice();
        let value_len = take_u32(&mut buf)?;
        if value_len == NOT_FOUND_SENTINEL {
            return Ok(None);
        }
        return Ok(Some(take_bytes(&mut buf, value_len as usize)?.to_vec()));
    }

    let value_len = stream.read_u32().await?;
    if value_len == NOT_FOUND_SENTINEL {
        return Ok(None);
    }
    let mut buf = vec![0u8; value_len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Store a value on a remote node's KV data plane.
///
/// Opens a TCP connection to `addr`, sends a SET request with the key
/// and value, and reads the status response. `cipher` must match the
/// remote node's setting (both `Some` with the same secret, or both
/// `None`).
///
/// # Errors
///
/// Returns an error on connection or I/O failure, or when the peer's
/// encryption setting or secret does not match.
pub async fn store_remote(
    addr: SocketAddr,
    key: &str,
    value: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<bool> {
    let mut stream = TcpStream::connect(addr).await?;

    // Send SET op + key + value.
    write_message(&mut stream, &encode_set_request(key, value)?, cipher).await?;

    // Read status response.
    let status = if let Some(cipher) = cipher {
        let frame = read_frame(&mut stream, cipher).await?;
        take_u8(&mut frame.as_slice())?
    } else {
        stream.read_u8().await?
    };
    Ok(status == SET_OK)
}

/// Encode a plaintext GET request message.
fn encode_get_request(key: &str) -> anyhow::Result<Vec<u8>> {
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(|_| anyhow::anyhow!("key too long"))?;
    let mut message = Vec::with_capacity(1 + 4 + key_bytes.len());
    message.push(OP_GET);
    message.extend_from_slice(&key_len.to_be_bytes());
    message.extend_from_slice(key_bytes);
    Ok(message)
}

/// Encode a plaintext SET request message.
fn encode_set_request(key: &str, value: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(|_| anyhow::anyhow!("key too long"))?;
    let value_len = u32::try_from(value.len())
        .map_err(|_| anyhow::anyhow!("value too large for TCP data plane"))?;
    let mut message = Vec::with_capacity(1 + 4 + key_bytes.len() + 4 + value.len());
    message.push(OP_SET);
    message.extend_from_slice(&key_len.to_be_bytes());
    message.extend_from_slice(key_bytes);
    message.extend_from_slice(&value_len.to_be_bytes());
    message.extend_from_slice(value);
    Ok(message)
}

/// Take one byte from the front of `buf`.
fn take_u8(buf: &mut &[u8]) -> anyhow::Result<u8> {
    let (byte, rest) = buf.split_first().ok_or_else(|| anyhow::anyhow!("truncated message"))?;
    *buf = rest;
    Ok(*byte)
}

/// Take a big-endian u32 from the front of `buf`.
fn take_u32(buf: &mut &[u8]) -> anyhow::Result<u32> {
    let bytes: [u8; 4] =
        take_bytes(buf, 4)?.try_into().expect("take_bytes(4) returns exactly 4 bytes");
    Ok(u32::from_be_bytes(bytes))
}

/// Take `len` bytes from the front of `buf`.
fn take_bytes<'a>(buf: &mut &'a [u8], len: usize) -> anyhow::Result<&'a [u8]> {
    if buf.len() < len {
        anyhow::bail!("truncated message: expected {len} bytes, got {}", buf.len());
    }
    let (bytes, rest) = buf.split_at(len);
    *buf = rest;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a data plane listener that handles a single connection.
    async fn spawn_single_handler(
        store: Arc<Store>,
        cipher: Option<Arc<ClusterCipher>>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, &store, cipher.as_deref()).await.unwrap();
        });
        addr
    }

    /// Spawn a data plane listener that handles multiple connections.
    async fn spawn_multi_handler(
        store: Arc<Store>,
        cipher: Option<Arc<ClusterCipher>>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let store = Arc::clone(&store);
                let cipher = cipher.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, &store, cipher.as_deref()).await;
                });
            }
        });
        addr
    }

    fn test_cipher(secret: &str) -> Arc<ClusterCipher> {
        Arc::new(ClusterCipher::for_kv_data_plane(secret))
    }

    #[tokio::test]
    async fn get_roundtrip_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        store.set("hello".to_string(), b"world".to_vec(), None);

        let addr = spawn_single_handler(Arc::clone(&store), None).await;
        let result = fetch_remote(addr, "hello", None).await.unwrap();
        assert_eq!(result, Some(b"world".to_vec()));
    }

    #[tokio::test]
    async fn get_roundtrip_not_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_single_handler(Arc::clone(&store), None).await;
        let result = fetch_remote(addr, "missing", None).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn set_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Store a value remotely.
        let ok = store_remote(addr, "remote_key", b"remote_val", None).await.unwrap();
        assert!(ok);

        // Verify it landed in the local store.
        let value = store.get("remote_key");
        assert_eq!(value, Some(b"remote_val".to_vec()));
    }

    #[tokio::test]
    async fn set_then_get_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Store remotely via TCP, then fetch remotely via TCP.
        let ok = store_remote(addr, "k", b"v", None).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "k", None).await.unwrap();
        assert_eq!(result, Some(b"v".to_vec()));
    }

    #[tokio::test]
    async fn concurrent_fetches() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        for i in 0..10 {
            store.set(format!("k{i}"), format!("v{i}").into_bytes(), None);
        }

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Launch 10 concurrent fetches.
        let mut handles = Vec::new();
        for i in 0..10 {
            let key = format!("k{i}");
            handles.push(tokio::spawn(async move { fetch_remote(addr, &key, None).await }));
        }

        for (i, handle) in handles.into_iter().enumerate() {
            let result = handle.await.unwrap().unwrap();
            assert_eq!(result, Some(format!("v{i}").into_bytes()));
        }
    }

    #[tokio::test]
    async fn connection_refused_returns_error() {
        // Connect to a port that is not listening.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let result = fetch_remote(addr, "key", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn set_large_value() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Store a 100 KiB value.
        let big_value = vec![42u8; 100 * 1024];
        let ok = store_remote(addr, "big", &big_value, None).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "big", None).await.unwrap();
        assert_eq!(result.as_ref().map(Vec::len), Some(big_value.len()));
        assert_eq!(result, Some(big_value));
    }

    #[tokio::test]
    async fn encrypted_get_set_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let cipher = test_cipher("data-plane-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(Arc::clone(&cipher))).await;

        let ok = store_remote(addr, "enc_key", b"enc_val", Some(&cipher)).await.unwrap();
        assert!(ok);
        assert_eq!(store.get("enc_key"), Some(b"enc_val".to_vec()));

        let result = fetch_remote(addr, "enc_key", Some(&cipher)).await.unwrap();
        assert_eq!(result, Some(b"enc_val".to_vec()));

        let missing = fetch_remote(addr, "nope", Some(&cipher)).await.unwrap();
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn encrypted_large_value_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let cipher = test_cipher("data-plane-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(Arc::clone(&cipher))).await;

        let big_value = vec![7u8; 100 * 1024];
        let ok = store_remote(addr, "big", &big_value, Some(&cipher)).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "big", Some(&cipher)).await.unwrap();
        assert_eq!(result, Some(big_value));
    }

    #[tokio::test]
    async fn wrong_secret_is_rejected() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let server_cipher = test_cipher("right-secret");
        let client_cipher = test_cipher("wrong-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(server_cipher)).await;

        // Neither reads nor writes must succeed with the wrong secret.
        let fetch = fetch_remote(addr, "key", Some(&client_cipher)).await;
        assert!(fetch.is_err(), "wrong-secret fetch must fail");

        let set = store_remote(addr, "key", b"injected", Some(&client_cipher)).await;
        assert!(set.is_err(), "wrong-secret store must fail");
        assert_eq!(store.get("key"), None, "wrong-secret write must not land in the store");
    }

    #[tokio::test]
    async fn plaintext_client_rejected_by_encrypted_server() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        store.set("key".to_string(), b"secret-value".to_vec(), None);
        let server_cipher = test_cipher("server-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(server_cipher)).await;

        let result = fetch_remote(addr, "key", None).await;
        assert!(result.is_err(), "plaintext client must not read from an encrypted server");
    }

    #[tokio::test]
    async fn encrypted_client_rejected_by_plaintext_server() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let client_cipher = test_cipher("client-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // The plaintext server misparses the frame header and errors,
        // returns garbage that fails frame authentication client-side,
        // or (rarely) stalls waiting for bytes that never come — the
        // timeout covers that case. All outcomes are a rejection.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetch_remote(addr, "key", Some(&client_cipher)),
        )
        .await;
        assert!(
            !matches!(result, Ok(Ok(_))),
            "encrypted client must not accept a plaintext server"
        );
    }

    #[test]
    fn parse_request_rejects_truncated_input() {
        assert!(parse_request(&[]).is_err());
        assert!(parse_request(&[OP_GET]).is_err());
        assert!(parse_request(&[OP_GET, 0, 0, 0, 5, b'a']).is_err());
    }

    #[test]
    fn parse_request_roundtrips_encoders() {
        let get = encode_get_request("mykey").unwrap();
        let parsed = parse_request(&get).unwrap();
        assert_eq!(parsed.op, OP_GET);
        assert_eq!(parsed.key, "mykey");
        assert!(parsed.value.is_none());

        let set = encode_set_request("mykey", b"myvalue").unwrap();
        let parsed = parse_request(&set).unwrap();
        assert_eq!(parsed.op, OP_SET);
        assert_eq!(parsed.key, "mykey");
        assert_eq!(parsed.value.as_deref(), Some(b"myvalue".as_slice()));
    }
}
