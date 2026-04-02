//! TDS connection handler.
//!
//! Manages the lifecycle of a single TDS client connection:
//! Pre-Login → Login7 → SQL Batch / RPC loop.

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use litewire_backend::{SharedBackend, Value};
use litewire_translate::{self, Dialect, StatementKind, TranslateResult, classify};

use crate::packet::{self, PacketType, DEFAULT_PACKET_SIZE};
use crate::token;

/// Per-connection transaction state for TDS.
struct TdsSession {
    /// Whether the connection is inside an explicit transaction.
    in_transaction: bool,
    /// Monotonic transaction ID counter.
    next_tran_id: u64,
    /// Current transaction descriptor (non-zero when in a transaction).
    current_tran_id: u64,
}

impl TdsSession {
    fn new() -> Self {
        Self {
            in_transaction: false,
            next_tran_id: 1,
            current_tran_id: 0,
        }
    }

    fn begin(&mut self) -> u64 {
        let id = self.next_tran_id;
        self.next_tran_id += 1;
        self.current_tran_id = id;
        self.in_transaction = true;
        id
    }

    fn end(&mut self) -> u64 {
        let old = self.current_tran_id;
        self.current_tran_id = 0;
        self.in_transaction = false;
        old
    }
}

/// Handle a single TDS client connection from start to finish.
pub async fn handle_connection<S>(mut stream: S, backend: SharedBackend) -> std::io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Phase 1: Pre-Login
    handle_prelogin(&mut stream).await?;

    // Phase 2: Login7
    let db_name = handle_login7(&mut stream).await?;
    debug!(database = %db_name, "TDS login complete");

    let mut session = TdsSession::new();

    // Phase 3: Query loop
    loop {
        let msg = match packet::read_message(&mut stream).await? {
            Some(m) => m,
            None => {
                debug!("TDS client disconnected");
                return Ok(());
            }
        };

        match msg.packet_type {
            PacketType::SqlBatch => {
                handle_sql_batch(&mut stream, &backend, &msg.payload, &mut session).await?;
            }
            PacketType::RpcRequest => {
                // Basic RPC support: try to extract SQL from sp_executesql.
                handle_rpc_request(&mut stream, &backend, &msg.payload, &mut session).await?;
            }
            other => {
                debug!(?other, "ignoring unexpected TDS packet type");
            }
        }
    }
}

/// Handle the Pre-Login exchange.
///
/// Reads the client's PRELOGIN packet and responds with our server version
/// and encryption=OFF.
async fn handle_prelogin<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
) -> std::io::Result<()> {
    let msg = packet::read_message(stream)
        .await?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no prelogin"))?;

    if msg.packet_type != PacketType::PreLogin {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected PRELOGIN, got {:?}", msg.packet_type),
        ));
    }

    // Build PRELOGIN response.
    // Minimal response: VERSION + ENCRYPTION + terminator.
    let mut resp = BytesMut::new();

    // Option tokens (type, offset, length):
    // VERSION: type=0x00, offset=?, length=6
    // ENCRYPTION: type=0x01, offset=?, length=1
    // TERMINATOR: 0xFF
    let header_size = 5 + 5 + 1; // Two 5-byte option entries + terminator

    // VERSION option
    resp.put_u8(0x00); // Token: VERSION
    resp.put_u16(header_size as u16); // Offset to data
    resp.put_u16(6); // Length

    // ENCRYPTION option
    resp.put_u8(0x01); // Token: ENCRYPTION
    resp.put_u16((header_size + 6) as u16); // Offset
    resp.put_u16(1); // Length

    // Terminator
    resp.put_u8(0xFF);

    // VERSION data: 15.0.0.0 (6 bytes: major.minor as u8s + build as u16)
    resp.put_u8(15); // Major
    resp.put_u8(0); // Minor
    resp.put_u16(0); // Build
    resp.put_u16(0); // Sub-build

    // ENCRYPTION data: NOT_SUP (0x02 = encryption not supported)
    resp.put_u8(0x02);

    packet::write_message(stream, PacketType::Response, &resp, DEFAULT_PACKET_SIZE).await
}

/// Handle the Login7 packet.
///
/// Parses the login packet to extract the database name, then sends
/// LOGINACK + ENVCHANGE + DONE.
async fn handle_login7<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
) -> std::io::Result<String> {
    let msg = packet::read_message(stream)
        .await?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no login7"))?;

    if msg.packet_type != PacketType::Login7 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected LOGIN7, got {:?}", msg.packet_type),
        ));
    }

    // Parse the Login7 packet to extract the database name.
    let db_name = parse_login7_database(&msg.payload).unwrap_or_else(|| "master".to_string());

    // Build login response.
    let mut resp = BytesMut::new();

    token::write_loginack(&mut resp, "litewire");
    token::write_envchange_database(&mut resp, &db_name);
    token::write_envchange_packet_size(&mut resp, DEFAULT_PACKET_SIZE as u32);
    token::write_info(&mut resp, 5701, &format!("Changed database context to '{db_name}'."));
    token::write_done(&mut resp, token::DONE_FINAL, 0);

    packet::write_message(stream, PacketType::Response, &resp, DEFAULT_PACKET_SIZE).await?;

    Ok(db_name)
}

/// Extract the database name from a Login7 payload.
///
/// Login7 has a fixed header followed by offset/length pairs for variable data.
/// Database name offset/length is at bytes 60-63.
fn parse_login7_database(payload: &[u8]) -> Option<String> {
    if payload.len() < 94 {
        return None;
    }

    // Database offset is at byte 60 (u16 LE), length at byte 62 (u16 LE).
    let db_offset = u16::from_le_bytes([payload[60], payload[61]]) as usize;
    let db_len = u16::from_le_bytes([payload[62], payload[63]]) as usize;

    if db_len == 0 {
        return None;
    }

    // Database name is UTF-16LE encoded.
    let start = db_offset;
    let end = start + db_len * 2; // Length is in characters, 2 bytes each.
    if end > payload.len() {
        return None;
    }

    decode_utf16le(&payload[start..end])
}

/// Decode a UTF-16LE byte slice to a Rust String.
fn decode_utf16le(data: &[u8]) -> Option<String> {
    if data.len() % 2 != 0 {
        return None;
    }
    let chars: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&chars).ok()
}

/// Handle a SQL Batch message.
async fn handle_sql_batch<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    backend: &SharedBackend,
    payload: &[u8],
    session: &mut TdsSession,
) -> std::io::Result<()> {
    // SQL Batch payload: ALL_HEADERS + UTF-16LE SQL text.
    // Skip the ALL_HEADERS section (total_length as u32 LE at start).
    let sql_start = skip_all_headers(payload);
    let sql_bytes = &payload[sql_start..];

    let sql = match decode_utf16le(sql_bytes) {
        Some(s) => s,
        None => {
            return send_error(stream, "Failed to decode SQL batch as UTF-16LE").await;
        }
    };

    debug!(sql = %sql, "TDS SQL batch");
    execute_sql(stream, backend, &sql, &[], session).await
}

/// Skip the ALL_HEADERS section in a TDS request payload.
///
/// ALL_HEADERS format: TotalLength (u32 LE) followed by header entries.
/// Returns the byte offset where the actual data begins.
fn skip_all_headers(payload: &[u8]) -> usize {
    if payload.len() < 4 {
        return 0;
    }
    let total_len =
        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if total_len >= 4 && total_len <= payload.len() {
        total_len
    } else {
        0
    }
}

/// Handle an RPC request.
///
/// Minimal implementation: extracts SQL from sp_executesql calls.
async fn handle_rpc_request<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    backend: &SharedBackend,
    payload: &[u8],
    session: &mut TdsSession,
) -> std::io::Result<()> {
    // RPC format: ALL_HEADERS + NameLenOrProcID(u16) + ...
    // If NameLenOrProcID == 0xFFFF, it's a well-known proc ID (u16).
    // sp_executesql = proc ID 10.
    let rpc_start = skip_all_headers(payload);
    let payload = &payload[rpc_start..];

    if payload.len() < 4 {
        return send_error(stream, "RPC request too short").await;
    }

    let name_len = u16::from_le_bytes([payload[0], payload[1]]);

    if name_len == 0xFFFF {
        // Well-known procedure.
        let proc_id = u16::from_le_bytes([payload[2], payload[3]]);
        if proc_id == 10 {
            // sp_executesql — extract the SQL parameter.
            return handle_sp_executesql(stream, backend, &payload[4..], session).await;
        }
    }

    // For other RPC calls, try to extract the procedure name and treat as SQL.
    debug!("unsupported RPC request, sending empty response");
    let mut resp = BytesMut::new();
    token::write_done(&mut resp, token::DONE_FINAL, 0);
    packet::write_message(stream, PacketType::Response, &resp, DEFAULT_PACKET_SIZE).await
}

/// Handle sp_executesql RPC.
///
/// Extracts the SQL text from the first NVARCHAR parameter.
async fn handle_sp_executesql<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    backend: &SharedBackend,
    data: &[u8],
    session: &mut TdsSession,
) -> std::io::Result<()> {
    // After the proc ID, we have option flags (u16), then parameters.
    // Skip option flags.
    if data.len() < 2 {
        return send_error(stream, "sp_executesql: missing option flags").await;
    }
    let rest = &data[2..];

    // First parameter is the SQL text (NVARCHAR).
    // Parameter format: name_len(u8) + name(utf16) + status(u8) + TYPE_INFO + value
    // For sp_executesql, the first param has no name (len=0).
    match extract_nvarchar_param(rest) {
        Some(sql) => {
            debug!(sql = %sql, "sp_executesql");
            execute_sql(stream, backend, &sql, &[], session).await
        }
        None => send_error(stream, "sp_executesql: failed to extract SQL parameter").await,
    }
}

/// Try to extract the first NVARCHAR parameter value from RPC parameter data.
fn extract_nvarchar_param(data: &[u8]) -> Option<String> {
    let mut pos = 0;

    // Parameter name length (u8).
    if pos >= data.len() {
        return None;
    }
    let name_len = data[pos] as usize;
    pos += 1;
    pos += name_len * 2; // Skip name (UTF-16LE).

    // Status flags (u8).
    if pos >= data.len() {
        return None;
    }
    pos += 1; // Skip status byte.

    // TYPE_INFO for NVARCHAR: type_id(0xE7) + max_len(u16) + collation(5)
    if pos >= data.len() {
        return None;
    }
    let type_id = data[pos];
    pos += 1;

    if type_id == 0xE7 {
        // NVARCHAR
        if pos + 2 + 5 > data.len() {
            return None;
        }
        pos += 2; // Skip max_length.
        pos += 5; // Skip collation.
    } else {
        // Unknown type, try to skip.
        return None;
    }

    // Value: length(u16) + data
    if pos + 2 > data.len() {
        return None;
    }
    let val_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;

    if val_len == 0xFFFF || pos + val_len > data.len() {
        return None; // NULL or insufficient data.
    }

    decode_utf16le(&data[pos..pos + val_len])
}

/// Execute translated SQL and send the result as a TDS response.
async fn execute_sql<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    backend: &SharedBackend,
    sql: &str,
    params: &[Value],
    session: &mut TdsSession,
) -> std::io::Result<()> {
    // Translate from T-SQL to SQLite.
    let translated = match litewire_translate::translate(sql, Dialect::TDS) {
        Ok(r) => r,
        Err(e) => {
            warn!("SQL translation error: {e}");
            return send_error(stream, &e.to_string()).await;
        }
    };

    let mut resp = BytesMut::new();

    for result in translated {
        match result {
            TranslateResult::Noop => {
                token::write_done(&mut resp, token::DONE_FINAL, 0);
            }
            TranslateResult::Metadata(meta) => {
                let sqlite_sql = meta.to_sqlite_sql();
                write_query_result(&mut resp, backend, &sqlite_sql, params).await;
            }
            TranslateResult::Sql(sqlite_sql) => {
                if sqlite_sql.is_empty() {
                    token::write_done(&mut resp, token::DONE_FINAL, 0);
                } else {
                    let kind = classify(&sqlite_sql);
                    match kind {
                        StatementKind::Query => {
                            write_query_result(&mut resp, backend, &sqlite_sql, params).await;
                        }
                        StatementKind::Transaction => {
                            write_transaction_result(
                                &mut resp, backend, &sqlite_sql, session,
                            )
                            .await;
                        }
                        _ => {
                            write_exec_result(&mut resp, backend, &sqlite_sql, params).await;
                        }
                    }
                }
            }
        }
    }

    // Ensure we always send at least a DONE.
    if resp.is_empty() {
        token::write_done(&mut resp, token::DONE_FINAL, 0);
    }

    packet::write_message(stream, PacketType::Response, &resp, DEFAULT_PACKET_SIZE).await
}

/// Execute a transaction command and write ENVCHANGE + DONE tokens.
async fn write_transaction_result(
    resp: &mut BytesMut,
    backend: &SharedBackend,
    sql: &str,
    session: &mut TdsSession,
) {
    match backend.execute(sql, &[]).await {
        Ok(_) => {
            let upper = sql.trim().to_ascii_uppercase();
            if upper.starts_with("BEGIN") || upper.starts_with("START") {
                let tran_id = session.begin();
                token::write_envchange_begin_tran(resp, tran_id);
            } else if upper.starts_with("COMMIT") {
                let old_id = session.end();
                token::write_envchange_commit_tran(resp, old_id);
            } else if upper.starts_with("ROLLBACK") {
                let old_id = session.end();
                token::write_envchange_rollback_tran(resp, old_id);
            }
            token::write_done(resp, token::DONE_FINAL, 0);
        }
        Err(e) => {
            token::write_error(resp, 50000, &e.to_string());
            token::write_done(resp, token::DONE_FINAL, 0);
        }
    }
}

/// Execute a SELECT and write COLMETADATA + ROW + DONE tokens.
async fn write_query_result(
    resp: &mut BytesMut,
    backend: &SharedBackend,
    sql: &str,
    params: &[Value],
) {
    match backend.query(sql, params).await {
        Ok(rs) => {
            let first_row = rs.rows.first().map(|r| r.as_slice());
            let columns = token::build_columns(&rs.columns, first_row);

            token::write_colmetadata(resp, &columns);
            for row in &rs.rows {
                token::write_row(resp, &columns, row);
            }
            token::write_done(resp, token::DONE_COUNT, rs.rows.len() as u64);
        }
        Err(e) => {
            token::write_error(resp, 50000, &e.to_string());
            token::write_done(resp, token::DONE_FINAL, 0);
        }
    }
}

/// Execute a mutation and write a DONE token with row count.
async fn write_exec_result(
    resp: &mut BytesMut,
    backend: &SharedBackend,
    sql: &str,
    params: &[Value],
) {
    match backend.execute(sql, params).await {
        Ok(result) => {
            token::write_done(resp, token::DONE_COUNT, result.affected_rows);
        }
        Err(e) => {
            token::write_error(resp, 50000, &e.to_string());
            token::write_done(resp, token::DONE_FINAL, 0);
        }
    }
}

/// Send an error response.
async fn send_error<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    message: &str,
) -> std::io::Result<()> {
    let mut resp = BytesMut::new();
    token::write_error(&mut resp, 50000, message);
    token::write_done(&mut resp, token::DONE_FINAL, 0);
    packet::write_message(stream, PacketType::Response, &resp, DEFAULT_PACKET_SIZE).await
}
