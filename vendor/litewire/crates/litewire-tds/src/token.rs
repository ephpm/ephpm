//! TDS response token encoding.
//!
//! Builds the binary token stream that goes inside TDS response packets.
//! Tokens are self-describing: a single byte identifies the token type,
//! followed by type-specific data.

use bytes::{BufMut, BytesMut};
use litewire_backend::{Column, Value};

// ── Token type constants ───────────────────────────────────────────────────

pub const TOKEN_ENVCHANGE: u8 = 0xE3;
pub const TOKEN_INFO: u8 = 0xAB;
pub const TOKEN_LOGINACK: u8 = 0xAD;
pub const TOKEN_COLMETADATA: u8 = 0x81;
pub const TOKEN_ROW: u8 = 0xD1;
pub const TOKEN_DONE: u8 = 0xFD;
pub const TOKEN_ERROR: u8 = 0xAA;

// ── DONE status flags ──────────────────────────────────────────────────────

pub const DONE_FINAL: u16 = 0x0000;
pub const DONE_MORE: u16 = 0x0001;
pub const DONE_COUNT: u16 = 0x0010;

// ── TDS type IDs for COLMETADATA ───────────────────────────────────────────

/// Variable-length NVARCHAR (UTF-16LE).
const TYPE_NVARCHAR: u8 = 0xE7;
/// Variable-length integer (1/2/4/8 bytes).
const TYPE_INTN: u8 = 0x26;
/// Variable-length float (4/8 bytes).
const TYPE_FLTN: u8 = 0x6D;
/// Variable-length binary.
const TYPE_BIGVARBINARY: u8 = 0xA5;

// ── Collation (used for string types) ──────────────────────────────────────

/// Default collation bytes (SQL_Latin1_General_CP1_CI_AS).
const DEFAULT_COLLATION: [u8; 5] = [0x09, 0x04, 0xD0, 0x00, 0x34];

/// Information about a column for the TDS wire format.
pub struct TdsColumn {
    pub name: String,
    pub tds_type: TdsType,
}

/// Simplified TDS type system for what SQLite can produce.
#[derive(Clone, Copy)]
pub enum TdsType {
    /// INTNTYPE with length 8 (BIGINT).
    BigInt,
    /// FLTNTYPE with length 8 (FLOAT).
    Float8,
    /// NVARCHAR(4000) — variable-length Unicode string.
    NVarChar,
    /// BIGVARBINARY(8000) — variable-length binary.
    VarBinary,
}

/// Map a SQLite declared type to a TDS wire type.
pub fn sqlite_to_tds_type(decltype: Option<&str>) -> TdsType {
    let Some(dt) = decltype else {
        return TdsType::NVarChar;
    };
    let upper = dt.to_ascii_uppercase();
    if upper.contains("INT") || upper.contains("BOOL") || upper.contains("BIT") {
        return TdsType::BigInt;
    }
    if upper.contains("REAL") || upper.contains("FLOAT") || upper.contains("DOUBLE") {
        return TdsType::Float8;
    }
    if upper.contains("BLOB") || upper.contains("BINARY") {
        return TdsType::VarBinary;
    }
    TdsType::NVarChar
}

/// Infer TDS type from an actual runtime value.
pub fn value_to_tds_type(val: &Value) -> TdsType {
    match val {
        Value::Null | Value::Text(_) => TdsType::NVarChar,
        Value::Integer(_) => TdsType::BigInt,
        Value::Float(_) => TdsType::Float8,
        Value::Blob(_) => TdsType::VarBinary,
    }
}

/// Build column metadata from backend columns + optional first row for type inference.
pub fn build_columns(columns: &[Column], first_row: Option<&[Value]>) -> Vec<TdsColumn> {
    columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let tds_type = if col.decltype.is_some() {
                sqlite_to_tds_type(col.decltype.as_deref())
            } else {
                first_row
                    .and_then(|row| row.get(idx))
                    .map(value_to_tds_type)
                    .unwrap_or(TdsType::NVarChar)
            };
            TdsColumn {
                name: col.name.clone(),
                tds_type,
            }
        })
        .collect()
}

// ── Token writers ──────────────────────────────────────────────────────────

/// Write a LOGINACK token.
///
/// Confirms successful login with server name and TDS version.
pub fn write_loginack(buf: &mut BytesMut, server_name: &str) {
    let name_utf16: Vec<u8> = server_name
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();

    // Token + length(u16) + interface(u8) + tds_version(u32) + name_len(u8) + name + version(u32)
    let body_len = 1 + 4 + 1 + name_utf16.len() + 4;

    buf.put_u8(TOKEN_LOGINACK);
    buf.put_u16_le(body_len as u16);
    buf.put_u8(1); // Interface: SQL (1)
    buf.put_u32(0x7400_0004); // TDS 7.4
    buf.put_u8((server_name.chars().count()) as u8); // Name length (in chars)
    buf.put_slice(&name_utf16);
    buf.put_u32_le(0x0F00_0000); // Server version (15.0.0.0)
}

/// Write an ENVCHANGE token for database change.
pub fn write_envchange_database(buf: &mut BytesMut, db_name: &str) {
    let name_utf16: Vec<u8> = db_name
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let char_len = db_name.chars().count() as u8;

    // type(1) + new_len(1) + new_value + old_len(1) + old_value
    let body_len = 1 + 1 + name_utf16.len() + 1 + name_utf16.len();

    buf.put_u8(TOKEN_ENVCHANGE);
    buf.put_u16_le(body_len as u16);
    buf.put_u8(1); // Type: Database
    buf.put_u8(char_len);
    buf.put_slice(&name_utf16);
    buf.put_u8(char_len);
    buf.put_slice(&name_utf16);
}

/// Write an ENVCHANGE token for packet size.
pub fn write_envchange_packet_size(buf: &mut BytesMut, size: u32) {
    let new_str = size.to_string();
    let new_utf16: Vec<u8> = new_str
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let old_str = "4096";
    let old_utf16: Vec<u8> = old_str
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();

    let body_len = 1 + 1 + new_utf16.len() + 1 + old_utf16.len();

    buf.put_u8(TOKEN_ENVCHANGE);
    buf.put_u16_le(body_len as u16);
    buf.put_u8(4); // Type: Packet size
    buf.put_u8(new_str.chars().count() as u8);
    buf.put_slice(&new_utf16);
    buf.put_u8(old_str.chars().count() as u8);
    buf.put_slice(&old_utf16);
}

/// Write an INFO token.
pub fn write_info(buf: &mut BytesMut, number: u32, message: &str) {
    write_info_or_error(buf, TOKEN_INFO, number, 1, message, "", "", 0);
}

/// Write an ERROR token.
pub fn write_error(buf: &mut BytesMut, number: u32, message: &str) {
    write_info_or_error(buf, TOKEN_ERROR, number, 14, message, "", "", 0);
}

/// Shared writer for INFO (0xAB) and ERROR (0xAA) tokens — same format.
fn write_info_or_error(
    buf: &mut BytesMut,
    token: u8,
    number: u32,
    state: u8,
    message: &str,
    server_name: &str,
    proc_name: &str,
    line: u32,
) {
    let msg_utf16: Vec<u8> = message
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let srv_utf16: Vec<u8> = server_name
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let proc_utf16: Vec<u8> = proc_name
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();

    // number(4) + state(1) + class(1) + msg_len(2) + msg + srv_len(1) + srv + proc_len(1) + proc + line(4)
    let body_len = 4 + 1 + 1 + 2 + msg_utf16.len() + 1 + srv_utf16.len() + 1 + proc_utf16.len() + 4;

    buf.put_u8(token);
    buf.put_u16_le(body_len as u16);
    buf.put_u32_le(number);
    buf.put_u8(state); // State
    buf.put_u8(if token == TOKEN_ERROR { 14 } else { 0 }); // Class (severity)
    buf.put_u16_le(message.chars().count() as u16);
    buf.put_slice(&msg_utf16);
    buf.put_u8(server_name.chars().count() as u8);
    buf.put_slice(&srv_utf16);
    buf.put_u8(proc_name.chars().count() as u8);
    buf.put_slice(&proc_utf16);
    buf.put_u32_le(line);
}

/// Write a COLMETADATA token describing result set columns.
pub fn write_colmetadata(buf: &mut BytesMut, columns: &[TdsColumn]) {
    buf.put_u8(TOKEN_COLMETADATA);
    buf.put_u16_le(columns.len() as u16); // Column count

    for col in columns {
        // UserType (u32) + Flags (u16) + TYPE_INFO + ColName
        buf.put_u32_le(0); // UserType
        buf.put_u16_le(0x08); // Flags: nullable

        match col.tds_type {
            TdsType::BigInt => {
                buf.put_u8(TYPE_INTN);
                buf.put_u8(8); // Max length
            }
            TdsType::Float8 => {
                buf.put_u8(TYPE_FLTN);
                buf.put_u8(8); // Max length
            }
            TdsType::NVarChar => {
                buf.put_u8(TYPE_NVARCHAR);
                buf.put_u16_le(8000); // Max length in bytes (4000 chars)
                buf.put_slice(&DEFAULT_COLLATION);
            }
            TdsType::VarBinary => {
                buf.put_u8(TYPE_BIGVARBINARY);
                buf.put_u16_le(8000); // Max length
            }
        }

        // Column name (B_VARCHAR: length in chars as u8, then UTF-16LE).
        let name_utf16: Vec<u8> = col
            .name
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        buf.put_u8(col.name.chars().count() as u8);
        buf.put_slice(&name_utf16);
    }
}

/// Write a ROW token for one result row.
pub fn write_row(buf: &mut BytesMut, columns: &[TdsColumn], row: &[Value]) {
    buf.put_u8(TOKEN_ROW);

    for (idx, val) in row.iter().enumerate() {
        let tds_type = columns.get(idx).map_or(TdsType::NVarChar, |c| c.tds_type);
        write_value(buf, val, tds_type);
    }
}

/// Encode a single value in TDS wire format.
fn write_value(buf: &mut BytesMut, val: &Value, tds_type: TdsType) {
    match val {
        Value::Null => {
            match tds_type {
                TdsType::BigInt | TdsType::Float8 => {
                    buf.put_u8(0); // 0-length = NULL for INTN/FLTN
                }
                TdsType::NVarChar | TdsType::VarBinary => {
                    buf.put_u16_le(0xFFFF); // PLP NULL marker for varchar/varbinary
                }
            }
        }
        Value::Integer(i) => match tds_type {
            TdsType::BigInt => {
                buf.put_u8(8); // Length
                buf.put_i64_le(*i);
            }
            TdsType::Float8 => {
                buf.put_u8(8);
                buf.put_f64_le(*i as f64);
            }
            _ => {
                // Encode as NVARCHAR string.
                let s = i.to_string();
                write_nvarchar(buf, &s);
            }
        },
        Value::Float(f) => match tds_type {
            TdsType::Float8 => {
                buf.put_u8(8);
                buf.put_f64_le(*f);
            }
            TdsType::BigInt => {
                buf.put_u8(8);
                buf.put_i64_le(*f as i64);
            }
            _ => {
                let s = f.to_string();
                write_nvarchar(buf, &s);
            }
        },
        Value::Text(s) => match tds_type {
            TdsType::BigInt => {
                if let Ok(i) = s.parse::<i64>() {
                    buf.put_u8(8);
                    buf.put_i64_le(i);
                } else {
                    buf.put_u8(0); // NULL
                }
            }
            TdsType::Float8 => {
                if let Ok(f) = s.parse::<f64>() {
                    buf.put_u8(8);
                    buf.put_f64_le(f);
                } else {
                    buf.put_u8(0); // NULL
                }
            }
            _ => write_nvarchar(buf, s),
        },
        Value::Blob(b) => match tds_type {
            TdsType::VarBinary => {
                buf.put_u16_le(b.len() as u16);
                buf.put_slice(b);
            }
            _ => {
                // Encode as hex string.
                let hex = hex_encode(b);
                write_nvarchar(buf, &hex);
            }
        },
    }
}

/// Write a UTF-16LE NVARCHAR value with u16 byte-length prefix.
fn write_nvarchar(buf: &mut BytesMut, s: &str) {
    let utf16: Vec<u8> = s
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    buf.put_u16_le(utf16.len() as u16);
    buf.put_slice(&utf16);
}

/// Simple hex encoder (avoids pulling in the hex crate).
fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write an ENVCHANGE token for BEGIN TRANSACTION.
///
/// Sends a transaction descriptor (8 bytes) as the new value.
pub fn write_envchange_begin_tran(buf: &mut BytesMut, tran_id: u64) {
    let new_val = tran_id.to_le_bytes();
    // type(1) + new_len(1) + new_value(8) + old_len(1)
    let body_len = 1 + 1 + 8 + 1;

    buf.put_u8(TOKEN_ENVCHANGE);
    buf.put_u16_le(body_len as u16);
    buf.put_u8(8); // Type: Begin Transaction
    buf.put_u8(8); // New value length
    buf.put_slice(&new_val);
    buf.put_u8(0); // Old value length (no previous transaction)
}

/// Write an ENVCHANGE token for COMMIT TRANSACTION.
pub fn write_envchange_commit_tran(buf: &mut BytesMut, old_tran_id: u64) {
    let old_val = old_tran_id.to_le_bytes();
    // type(1) + new_len(1) + old_len(1) + old_value(8)
    let body_len = 1 + 1 + 1 + 8;

    buf.put_u8(TOKEN_ENVCHANGE);
    buf.put_u16_le(body_len as u16);
    buf.put_u8(9); // Type: Commit Transaction
    buf.put_u8(0); // New value length (no active transaction)
    buf.put_u8(8); // Old value length
    buf.put_slice(&old_val);
}

/// Write an ENVCHANGE token for ROLLBACK TRANSACTION.
pub fn write_envchange_rollback_tran(buf: &mut BytesMut, old_tran_id: u64) {
    let old_val = old_tran_id.to_le_bytes();
    // type(1) + new_len(1) + old_len(1) + old_value(8)
    let body_len = 1 + 1 + 1 + 8;

    buf.put_u8(TOKEN_ENVCHANGE);
    buf.put_u16_le(body_len as u16);
    buf.put_u8(10); // Type: Rollback Transaction
    buf.put_u8(0); // New value length (no active transaction)
    buf.put_u8(8); // Old value length
    buf.put_slice(&old_val);
}

/// Write a DONE token (end of result set / batch).
pub fn write_done(buf: &mut BytesMut, status: u16, row_count: u64) {
    buf.put_u8(TOKEN_DONE);
    buf.put_u16_le(status);
    buf.put_u16_le(0); // CurCmd
    buf.put_u64_le(row_count); // DoneRowCount (8 bytes in TDS 7.2+)
}
