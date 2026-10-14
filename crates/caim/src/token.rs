//! The token: 32 random bytes per start, hex, checked in constant time on every request.

use crate::error::{Fail, Result};

/// Bytes of randomness in the token; it is written as twice as many lowercase hex characters.
pub const TOKEN_BYTES: usize = 32;

/// A fresh token for this start of the manager.
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| Fail::internal(format!("Craft AI Manager could not make a token: {error}")))?;
    Ok(hex(&bytes))
}

/// Lowercase hex, the shape the endpoint file and the header both carry.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Compare two tokens without stopping at the first difference.
#[must_use]
pub fn token_matches(expected: &str, given: &str) -> bool {
    let (expected, given) = (expected.as_bytes(), given.as_bytes());
    let mut diff = expected.len() ^ given.len();
    for (left, right) in expected.iter().zip(given.iter()) {
        diff |= usize::from(left != right);
    }
    diff == 0
}

/// Does this `Authorization` header carry our token? Amendment 11: `Bearer <token>`.
#[must_use]
pub fn bearer_matches(expected: &str, header: &str) -> bool {
    header.strip_prefix("Bearer ").is_some_and(|given| token_matches(expected, given))
}

#[cfg(test)]
mod tests {
    use super::{TOKEN_BYTES, bearer_matches, random_token, token_matches};

    #[test]
    fn a_token_is_hex_and_changes_between_starts() {
        let first = random_token().unwrap_or_default();
        let second = random_token().unwrap_or_default();
        assert_eq!(first.len(), TOKEN_BYTES * 2);
        assert!(first.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_ne!(first, second, "a new token per start");
    }

    #[test]
    fn the_bearer_header_is_the_only_way_in() {
        assert!(bearer_matches("abc", "Bearer abc"));
        assert!(!bearer_matches("abc", "abc"));
        assert!(!bearer_matches("abc", "Basic abc"));
        assert!(!bearer_matches("abc", "Bearer "));
        assert!(!bearer_matches("abc", "Bearer abcd"));
        assert!(!token_matches("abc", "abd"));
    }
}
