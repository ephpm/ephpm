//! SQL proxy and connection pooling for ePHPm.

pub mod duration;
pub mod error;
pub mod mysql;
pub mod pool;
pub mod postgres;
pub mod url;

/// Strategy for resetting backend connections when returning to the pool.
#[derive(Clone, Copy, Debug, Default)]
pub enum ResetStrategy {
    /// Always send `COM_RESET_CONNECTION` on return.
    Always,
    /// Never reset (fastest, use only in trusted dev environments).
    Never,
    /// Reset only when session may be dirty (tracks dirty bit per connection).
    #[default]
    Smart,
}

impl ResetStrategy {
    /// Parse a reset strategy from a string (case-insensitive).
    ///
    /// Returns [`ResetStrategy::Smart`] for unrecognised values.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "never" => Self::Never,
            "always" => Self::Always,
            _ => Self::Smart,
        }
    }
}

impl std::str::FromStr for ResetStrategy {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_str_lossy(s))
    }
}
