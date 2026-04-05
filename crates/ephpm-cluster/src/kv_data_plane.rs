//! TCP data plane for fetching and storing large KV values on remote nodes.
//!
//! When a key's value exceeds the gossip tier threshold, it is stored
//! locally on the owner node. Other nodes fetch or store it on demand
//! via a simple TCP protocol:
//!
//! ## Wire protocol
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

use std::net::SocketAddr;
use std::sync::Arc;

use ephpm_kv::store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Sentinel value indicating "key not found" in the wire protocol.
const NOT_FOUND_SENTINEL: u32 = u32::MAX;

/// Maximum key length accepted by the server (64 KiB).
const MAX_KEY_LEN: u32 = 64 * 1024;

/// Maximum value length accepted by the server (64 MiB).
const MAX_VALUE_LEN: u32 = 64 * 1024 * 1024;

/// Op code for GET requests.
const OP_GET: u8 = 0x00;

/// Op code for SET requests.
const OP_SET: u8 = 0x01;

/// SET response: success.
const SET_OK: u8 = 0x00;

/// SET response: rejected (e.g., memory limit with `NoEviction`).
const SET_REJECTED: u8 = 0x01;

/// Start the TCP KV data plane listener.
///
/// Serves lookups against the local [`Store`] so remote cluster nodes
/// can fetch large values that exceed the gossip tier threshold.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve(store: Arc<Store>, port: u16) -> anyhow::Result<()> {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind KV data plane to {addr}: {e}"))?;
    tracing::info!(%addr, "KV data plane listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!(%e, "KV data plane accept error");
                continue;
            }
        };
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &store).await {
                tracing::debug!(%peer, %e, "KV data plane connection error");
            }
        });
    }
}

/// Handle a single TCP connection: read op + key, dispatch GET or SET.
async fn handle_connection(mut stream: TcpStream, store: &Store) -> anyhow::Result<()> {
    // Read op code.
    let op = stream.read_u8().await?;

    // Read key length.
    let key_len = stream.read_u32().await?;
    if key_len > MAX_KEY_LEN {
        anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
    }

    // Read key bytes.
    let mut key_buf = vec![0u8; key_len as usize];
    stream.read_exact(&mut key_buf).await?;
    let key = String::from_utf8(key_buf).map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;

    match op {
        OP_GET => {
            // Look up in local store and write response.
            if let Some(value) = store.get(&key) {
                let len = u32::try_from(value.len()).unwrap_or(NOT_FOUND_SENTINEL - 1);
                stream.write_u32(len).await?;
                stream.write_all(&value).await?;
            } else {
                stream.write_u32(NOT_FOUND_SENTINEL).await?;
            }
        }
        OP_SET => {
            // Read value length + value bytes.
            let value_len = stream.read_u32().await?;
            if value_len > MAX_VALUE_LEN {
                anyhow::bail!("value length {value_len} exceeds maximum {MAX_VALUE_LEN}");
            }
            let mut value_buf = vec![0u8; value_len as usize];
            stream.read_exact(&mut value_buf).await?;

            let ok = store.set(key, value_buf, None);
            stream.write_u8(if ok { SET_OK } else { SET_REJECTED }).await?;
        }
        other => {
            anyhow::bail!("unknown op code: {other:#04x}");
        }
    }

    stream.flush().await?;
    Ok(())
}

/// Fetch a value from a remote node's KV data plane.
///
/// Opens a TCP connection to `addr`, sends a GET request, and reads the
/// response. Returns `None` if the remote node does not have the key.
///
/// # Errors
///
/// Returns an error on connection or I/O failure.
pub async fn fetch_remote(addr: SocketAddr, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let mut stream = TcpStream::connect(addr).await?;

    // Send GET op + key.
    stream.write_u8(OP_GET).await?;
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(|_| anyhow::anyhow!("key too long"))?;
    stream.write_u32(key_len).await?;
    stream.write_all(key_bytes).await?;
    stream.flush().await?;

    // Read response.
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
/// and value, and reads the status response.
///
/// # Errors
///
/// Returns an error on connection or I/O failure, or if the remote
/// store rejected the write (e.g., memory limit with `NoEviction`).
pub async fn store_remote(addr: SocketAddr, key: &str, value: &[u8]) -> anyhow::Result<bool> {
    let mut stream = TcpStream::connect(addr).await?;

    // Send SET op + key + value.
    stream.write_u8(OP_SET).await?;
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(|_| anyhow::anyhow!("key too long"))?;
    stream.write_u32(key_len).await?;
    stream.write_all(key_bytes).await?;
    let value_len = u32::try_from(value.len())
        .map_err(|_| anyhow::anyhow!("value too large for TCP data plane"))?;
    stream.write_u32(value_len).await?;
    stream.write_all(value).await?;
    stream.flush().await?;

    // Read status response.
    let status = stream.read_u8().await?;
    Ok(status == SET_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a data plane listener that handles a single connection.
    async fn spawn_single_handler(store: Arc<Store>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, &store).await.unwrap();
        });
        addr
    }

    /// Spawn a data plane listener that handles multiple connections.
    async fn spawn_multi_handler(store: Arc<Store>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, &store).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn get_roundtrip_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        store.set("hello".to_string(), b"world".to_vec(), None);

        let addr = spawn_single_handler(Arc::clone(&store)).await;
        let result = fetch_remote(addr, "hello").await.unwrap();
        assert_eq!(result, Some(b"world".to_vec()));
    }

    #[tokio::test]
    async fn get_roundtrip_not_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_single_handler(Arc::clone(&store)).await;
        let result = fetch_remote(addr, "missing").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn set_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store)).await;

        // Store a value remotely.
        let ok = store_remote(addr, "remote_key", b"remote_val").await.unwrap();
        assert!(ok);

        // Verify it landed in the local store.
        let value = store.get("remote_key");
        assert_eq!(value, Some(b"remote_val".to_vec()));
    }

    #[tokio::test]
    async fn set_then_get_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store)).await;

        // Store remotely via TCP, then fetch remotely via TCP.
        let ok = store_remote(addr, "k", b"v").await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "k").await.unwrap();
        assert_eq!(result, Some(b"v".to_vec()));
    }

    #[tokio::test]
    async fn concurrent_fetches() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        for i in 0..10 {
            store.set(format!("k{i}"), format!("v{i}").into_bytes(), None);
        }

        let addr = spawn_multi_handler(Arc::clone(&store)).await;

        // Launch 10 concurrent fetches.
        let mut handles = Vec::new();
        for i in 0..10 {
            let key = format!("k{i}");
            handles.push(tokio::spawn(async move { fetch_remote(addr, &key).await }));
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
        let result = fetch_remote(addr, "key").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn set_large_value() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store)).await;

        // Store a 100 KiB value.
        let big_value = vec![42u8; 100 * 1024];
        let ok = store_remote(addr, "big", &big_value).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "big").await.unwrap();
        assert_eq!(result.as_ref().map(Vec::len), Some(big_value.len()));
        assert_eq!(result, Some(big_value));
    }
}
