//! Duration string parser for config values like `"300s"`, `"5m"`, `"500ms"`.

use std::time::Duration;

use crate::error::DbError;

/// Parse a duration string into a [`Duration`].
///
/// Supported suffixes:
/// - `ms` — milliseconds
/// - `s`  — seconds
/// - `m`  — minutes
/// - `h`  — hours
///
/// # Errors
///
/// Returns [`DbError::InvalidDuration`] if the string is malformed.
pub fn parse_duration(s: &str) -> Result<Duration, DbError> {
    let err = |reason: &str| DbError::InvalidDuration {
        value: s.to_string(),
        reason: reason.to_string(),
    };

    let (num_str, suffix) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, "ms")
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, "s")
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, "m")
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, "h")
    } else {
        return Err(err("missing unit suffix (ms, s, m, h)"));
    };

    let n: u64 = num_str.parse().map_err(|_| err("not a valid integer"))?;

    Ok(match suffix {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_various() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn parse_invalid() {
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("xms").is_err());
    }
}
