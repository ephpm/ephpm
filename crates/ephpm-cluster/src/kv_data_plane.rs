//! TCP data plane for fetching large KV values from remote nodes.
//!
//! When a key's value exceeds the gossip tier threshold, it is stored
//! locally on the owner node. Other nodes fetch it on demand via a
//! simple TCP protocol:
//!
//! ## Wire protocol
//!
//! **Request:** `[key_len: u32 BE][key: bytes]`
//!
//! **Response (found):** `[value_len: u32 BE][value: bytes]`
//!
//! **Response (not found):** `[0xFFFFFFFF: u32 BE]` (sentinel)
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

/// Handle a single TCP connection: read key, write value (or sentinel).
async fn handle_connection(mut stream: TcpStream, store: &Store) -> anyhow::Result<()> {
    // Read key length.
    let key_len = stream.read_u32().await?;
    if key_len > MAX_KEY_LEN {
        anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
    }

    // Read key bytes.
    let mut key_buf = vec![0u8; key_len as usize];
    stream.read_exact(&mut key_buf).await?;
    let key = String::from_utf8(key_buf)
        .map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;

    // Look up in local store and write response.
    if let Some(value) = store.get(&key) {
        let len = u32::try_from(value.len())
            .unwrap_or(NOT_FOUND_SENTINEL - 1);
        stream.write_u32(len).await?;
        stream.write_all(&value).await?;
    } else {
        stream.write_u32(NOT_FOUND_SENTINEL).await?;
    }

    stream.flush().await?;
    Ok(())
}

/// Fetch a value from a remote node's KV data plane.
///
/// Opens a TCP connection to `addr`, sends the key, and reads the
/// response. Returns `None` if the remote node does not have the key.
///
/// # Errors
///
/// Returns an error on connection or I/O failure.
pub async fn fetch_remote(addr: SocketAddr, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let mut stream = TcpStream::connect(addr).await?;

    // Send key.
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len())
        .map_err(|_| anyhow::anyhow!("key too long"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        store.set("hello".to_string(), b"world".to_vec(), None);

        // Start server on ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store_clone = Arc::clone(&store);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, &store_clone).await.unwrap();
        });

        let result = fetch_remote(addr, "hello").await.unwrap();
        assert_eq!(result, Some(b"world".to_vec()));
    }

    #[tokio::test]
    async fn roundtrip_not_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store_clone = Arc::clone(&store);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, &store_clone).await.unwrap();
        });

        let result = fetch_remote(addr, "missing").await.unwrap();
        assert_eq!(result, None);
    }
}
