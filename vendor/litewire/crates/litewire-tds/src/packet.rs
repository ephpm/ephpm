//! TDS packet framing.
//!
//! Each TDS message is split into one or more 8-byte-header packets.
//! This module handles reading complete messages from the wire and
//! writing response packets.

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// TDS packet header size in bytes.
pub const HEADER_SIZE: usize = 8;

/// Default maximum packet size (negotiated during login, but we use 4096 as default).
pub const DEFAULT_PACKET_SIZE: usize = 4096;

/// TDS packet types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    SqlBatch = 0x01,
    RpcRequest = 0x03,
    Response = 0x04,
    Login7 = 0x10,
    PreLogin = 0x12,
}

impl PacketType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::SqlBatch),
            0x03 => Some(Self::RpcRequest),
            0x04 => Some(Self::Response),
            0x10 => Some(Self::Login7),
            0x12 => Some(Self::PreLogin),
            _ => None,
        }
    }
}

/// Status byte flags.
pub const STATUS_EOM: u8 = 0x01;

/// A complete TDS message (reassembled from one or more packets).
pub struct TdsMessage {
    pub packet_type: PacketType,
    pub payload: BytesMut,
}

/// Read a complete TDS message from the stream.
///
/// Reassembles multi-packet messages by reading until the EOM status bit is set.
pub async fn read_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<TdsMessage>> {
    let mut payload = BytesMut::new();
    let mut msg_type = None;

    loop {
        // Read 8-byte header.
        let mut header = [0u8; HEADER_SIZE];
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Client disconnected cleanly.
                if payload.is_empty() {
                    return Ok(None);
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }

        let pkt_type = header[0];
        let status = header[1];
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;

        if msg_type.is_none() {
            msg_type = PacketType::from_u8(pkt_type);
        }

        // Read payload (length includes the 8-byte header).
        let payload_len = length.saturating_sub(HEADER_SIZE);
        if payload_len > 0 {
            let old_len = payload.len();
            payload.resize(old_len + payload_len, 0);
            reader.read_exact(&mut payload[old_len..]).await?;
        }

        // If EOM bit is set, we have the complete message.
        if status & STATUS_EOM != 0 {
            break;
        }
    }

    match msg_type {
        Some(pt) => Ok(Some(TdsMessage {
            packet_type: pt,
            payload,
        })),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unknown TDS packet type",
        )),
    }
}

/// Write a TDS response message, splitting into packets if needed.
pub async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    packet_type: PacketType,
    data: &[u8],
    max_packet_size: usize,
) -> std::io::Result<()> {
    let max_payload = max_packet_size - HEADER_SIZE;
    let chunks: Vec<&[u8]> = if data.is_empty() {
        // Send at least one empty packet for EOM.
        vec![&[]]
    } else {
        data.chunks(max_payload).collect()
    };

    let last_idx = chunks.len() - 1;

    for (i, chunk) in chunks.iter().enumerate() {
        let mut header = BytesMut::with_capacity(HEADER_SIZE + chunk.len());

        let status = if i == last_idx { STATUS_EOM } else { 0x00 };
        let total_len = (HEADER_SIZE + chunk.len()) as u16;

        header.put_u8(packet_type as u8); // Type
        header.put_u8(status); // Status
        header.put_u16(total_len); // Length (big-endian)
        header.put_u16(0); // SPID
        header.put_u8((i + 1) as u8); // PacketID
        header.put_u8(0); // Window

        writer.write_all(&header).await?;
        if !chunk.is_empty() {
            writer.write_all(chunk).await?;
        }
    }

    writer.flush().await
}
