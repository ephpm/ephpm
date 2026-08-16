//! Connection-ID minting.
//!
//! A connection ID is a **capability**, not a name. Anything that can produce a
//! valid `(site, id)` pair can push frames to that socket, so the ID has to be
//! unguessable by anyone who was not handed it: 128 bits from the OS CSPRNG,
//! rendered lowercase hex.
//!
//! 128 bits also removes any need to check for collisions — with a per-process
//! ceiling of a few million live connections the birthday probability is far
//! below the noise floor of every other failure in the system.

/// Length of a rendered connection ID, in ASCII characters (16 bytes as hex).
pub const CONNECTION_ID_LEN: usize = 32;

/// Raw entropy per connection ID.
const ID_BYTES: usize = 16;

/// Mint a fresh connection ID.
///
/// # Errors
///
/// Returns [`getrandom::Error`] when the OS entropy source is unavailable.
/// Callers must treat this as fatal for the connection and refuse the upgrade —
/// never fall back to a counter or a timestamp, which would make one tenant's
/// IDs predictable from another's.
pub fn new_connection_id() -> Result<String, getrandom::Error> {
    let mut raw = [0u8; ID_BYTES];
    getrandom::fill(&mut raw)?;

    let mut out = String::with_capacity(CONNECTION_ID_LEN);
    for byte in raw {
        // `write!` on a String is infallible but returns a Result; the manual
        // nibble push keeps this allocation-free and error-free.
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 0x0f));
    }
    Ok(out)
}

/// Map a 0..=15 nibble to its lowercase hex character.
const fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ids_are_lowercase_hex_of_the_documented_length() {
        let id = new_connection_id().expect("entropy");
        assert_eq!(id.len(), CONNECTION_ID_LEN);
        assert!(
            id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "id must be lowercase hex, got {id}"
        );
    }

    #[test]
    fn ids_do_not_repeat() {
        // Not a randomness test — a smoke test that we are not handing out a
        // constant or a counter. A duplicate here means the capability model is
        // broken outright.
        let mut seen = HashSet::new();
        for _ in 0..1_000 {
            assert!(seen.insert(new_connection_id().expect("entropy")), "duplicate connection id");
        }
    }

    #[test]
    fn ids_have_no_shared_prefix() {
        // A counter or a timestamp source would show up as a long common
        // prefix across successive IDs.
        let a = new_connection_id().expect("entropy");
        let b = new_connection_id().expect("entropy");
        let shared = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
        assert!(shared < 8, "successive ids share a {shared}-char prefix: {a} / {b}");
    }
}
