//! SQLite type affinity to PostgreSQL type OID mapping.

use pgwire::api::Type;

/// Map a SQLite declared type string to a PostgreSQL [`Type`].
///
/// Falls back to `TEXT` for unknown types, which is safe because SQLite
/// stores everything as text-compatible anyway.
pub fn sqlite_to_pg_type(decltype: Option<&str>) -> Type {
    let Some(dt) = decltype else {
        return Type::TEXT;
    };

    let upper = dt.to_ascii_uppercase();

    if upper.contains("INT") {
        return Type::INT8;
    }
    if upper.contains("REAL") || upper.contains("FLOAT") || upper.contains("DOUBLE") {
        return Type::FLOAT8;
    }
    if upper.contains("BLOB") || upper.contains("BYTEA") {
        return Type::BYTEA;
    }
    if upper.contains("BOOL") {
        return Type::BOOL;
    }

    // TEXT, VARCHAR, CHAR, CLOB, DATE, DATETIME, TIMESTAMP, etc.
    Type::TEXT
}
