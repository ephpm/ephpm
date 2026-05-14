//! Database URL parser.
//!
//! Parses `mysql://user:pass@host:port/dbname` and
//! `postgres://user:pass@host:port/dbname` URLs without external dependencies.

use crate::error::DbError;

/// Parsed components of a database connection URL.
#[derive(Debug, Clone)]
pub struct DbUrl {
    /// Database driver scheme (`"mysql"` or `"postgres"`).
    pub scheme: String,
    /// Authenticated username.
    pub username: String,
    /// Plaintext password (may be empty).
    pub password: String,
    /// Backend host or IP address.
    pub host: String,
    /// Backend TCP port.
    pub port: u16,
    /// Database / schema name.
    pub database: String,
}

impl DbUrl {
    /// Parse a database URL string.
    ///
    /// Supported schemes: `mysql`, `postgres`, `postgresql`.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::InvalidUrl`] if the URL is malformed.
    pub fn parse(url: &str) -> Result<Self, DbError> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| DbError::InvalidUrl(format!("missing '://' in '{url}'")))?;

        let scheme = match scheme {
            "mysql" => "mysql",
            "postgres" | "postgresql" => "postgres",
            other => {
                return Err(DbError::InvalidUrl(format!("unsupported scheme '{other}'")));
            }
        };

        let default_port: u16 = if scheme == "mysql" { 3306 } else { 5432 };

        // Split user:pass@host:port/db
        let (auth, host_path) =
            rest.split_once('@').ok_or_else(|| DbError::InvalidUrl("missing '@' in URL".into()))?;

        let (username, password) = auth.split_once(':').unwrap_or((auth, ""));

        let (host_port, database) = host_path.split_once('/').unwrap_or((host_path, ""));

        let (host, port_str) = host_port.rsplit_once(':').unwrap_or((host_port, ""));

        let port: u16 = if port_str.is_empty() {
            default_port
        } else {
            port_str
                .parse()
                .map_err(|_| DbError::InvalidUrl(format!("invalid port '{port_str}'")))?
        };

        let host = if host.is_empty() { "127.0.0.1" } else { host };

        Ok(Self {
            scheme: scheme.to_string(),
            username: percent_decode(username),
            password: percent_decode(password),
            host: host.to_string(),
            port,
            database: percent_decode(database),
        })
    }

    /// Returns the `host:port` string suitable for `TcpStream::connect`.
    #[must_use]
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Decode percent-encoded characters in a URL component.
fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_digit(bytes[i + 1]);
            let lo = hex_digit(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                // SAFETY: hi and lo are each 0..=15, so (hi << 4) | lo is 0..=255.
                result.push(char::from((hi << 4) | lo));
                i += 3;
                continue;
            }
        }
        result.push(char::from(bytes[i]));
        i += 1;
    }
    result
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mysql_url() {
        let u = DbUrl::parse("mysql://root:secret@db-host:3307/myapp").unwrap();
        assert_eq!(u.scheme, "mysql");
        assert_eq!(u.username, "root");
        assert_eq!(u.password, "secret");
        assert_eq!(u.host, "db-host");
        assert_eq!(u.port, 3307);
        assert_eq!(u.database, "myapp");
    }

    #[test]
    fn parse_postgres_url() {
        let u = DbUrl::parse("postgres://app:p%40ss@10.0.0.1:5432/prod").unwrap();
        assert_eq!(u.scheme, "postgres");
        assert_eq!(u.password, "p@ss"); // percent-decoded
        assert_eq!(u.port, 5432);
    }

    #[test]
    fn parse_default_ports() {
        let m = DbUrl::parse("mysql://u:p@host/db").unwrap();
        assert_eq!(m.port, 3306);
        let p = DbUrl::parse("postgresql://u:p@host/db").unwrap();
        assert_eq!(p.port, 5432);
    }
}
