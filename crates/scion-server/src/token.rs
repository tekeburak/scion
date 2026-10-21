//! Bearer tokens. A token is stateless: a principal, an expiry and a keyed
//! BLAKE3 MAC over both under the server key, so verifying one needs no table.
//!
//! The signed bytes use the canonical primitives (never `serde` or `prost`):
//!
//! ```text
//! u8 version (1) | str principal | i64 expiry (unix seconds) | id mac
//! ```
//!
//! where `mac = blake3_keyed(key, everything before it)`. On the wire the
//! token is those bytes in lowercase hex.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scion_types::canonical::{Reader, Writer};

const TOKEN_VERSION: u8 = 1;

/// The server's MAC key, read from `SCION_TOKEN_KEY` as 64 hex digits.
#[derive(Clone)]
pub struct TokenKey([u8; 32]);

impl std::fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenKey(..)")
    }
}

impl TokenKey {
    /// Parse a key from 64 hex digits (32 bytes).
    pub fn from_hex(s: &str) -> Result<Self, TokenError> {
        let mut key = [0u8; 32];
        hex::decode_to_slice(s.trim(), &mut key).map_err(|_| TokenError::BadKey)?;
        Ok(Self(key))
    }

    /// Issue a token for `principal` that expires at `expiry`.
    pub fn issue(&self, principal: &str, expiry: SystemTime) -> Result<String, TokenError> {
        check_principal(principal)?;
        let mut w = Writer::new();
        w.u8(TOKEN_VERSION);
        w.str(principal);
        w.i64(unix_seconds(expiry));
        let mut bytes = w.into_vec();
        let mac = blake3::keyed_hash(&self.0, &bytes);
        bytes.extend_from_slice(mac.as_bytes());
        Ok(hex::encode(bytes))
    }

    /// Check `token` and return the principal it names and its expiry.
    /// Refuses a token that is not well formed, was signed with another key
    /// or was altered, and one whose expiry is at or before `now`.
    pub fn verify(&self, token: &str, now: SystemTime) -> Result<(String, SystemTime), TokenError> {
        let bytes = hex::decode(token).map_err(|_| TokenError::Malformed)?;
        let signed_len = bytes.len().checked_sub(32).ok_or(TokenError::Malformed)?;
        let mut r = Reader::new(&bytes);
        if r.u8().map_err(|_| TokenError::Malformed)? != TOKEN_VERSION {
            return Err(TokenError::Malformed);
        }
        let principal = r.str().map_err(|_| TokenError::Malformed)?;
        let expiry = r.i64().map_err(|_| TokenError::Malformed)?;
        let mac = r.id().map_err(|_| TokenError::Malformed)?;
        r.finish().map_err(|_| TokenError::Malformed)?;
        // `blake3::Hash` compares in constant time.
        if blake3::keyed_hash(&self.0, &bytes[..signed_len]) != blake3::Hash::from(mac) {
            return Err(TokenError::BadSignature);
        }
        if expiry <= unix_seconds(now) {
            return Err(TokenError::Expired);
        }
        check_principal(principal)?;
        // After `now`, so positive.
        let expiry = UNIX_EPOCH + Duration::from_secs(expiry.unsigned_abs());
        Ok((principal.to_owned(), expiry))
    }
}

/// Why a token or key was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("SCION_TOKEN_KEY must be 64 hex digits (32 bytes)")]
    BadKey,
    #[error("token is malformed")]
    Malformed,
    #[error("token signature does not match")]
    BadSignature,
    #[error("token has expired")]
    Expired,
    #[error("principal {0:?} is not github:<login> or sp:<name>")]
    BadPrincipal(String),
}

/// A principal is `github:<login>` or `sp:<name>`, the name made of ASCII
/// letters, digits, `-`, `_` and `.`.
pub(crate) fn check_principal(principal: &str) -> Result<(), TokenError> {
    let name = principal
        .strip_prefix("github:")
        .or_else(|| principal.strip_prefix("sp:"));
    match name {
        Some(n)
            if !n.is_empty()
                && n.len() <= 100
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) =>
        {
            Ok(())
        }
        _ => Err(TokenError::BadPrincipal(principal.to_owned())),
    }
}

fn unix_seconds(t: SystemTime) -> i64 {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const OTHER: &str = "ff0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn round_trips() {
        let key = TokenKey::from_hex(KEY).unwrap();
        for p in ["github:octo-cat", "sp:ci.bot_1"] {
            let t = key.issue(p, at(2_000)).unwrap();
            assert_eq!(
                key.verify(&t, at(1_999)).unwrap(),
                (p.to_owned(), at(2_000))
            );
        }
    }

    #[test]
    fn any_changed_byte_is_refused() {
        let key = TokenKey::from_hex(KEY).unwrap();
        let t = key.issue("github:octocat", at(2_000)).unwrap();
        let bytes = hex::decode(&t).unwrap();
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x01;
            let err = key.verify(&hex::encode(&b), at(1_000)).unwrap_err();
            assert!(
                matches!(err, TokenError::BadSignature | TokenError::Malformed),
                "byte {i}: {err:?}"
            );
        }
        assert_eq!(
            key.verify(&t[..t.len() - 2], at(1_000)),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            key.verify(&format!("{t}00"), at(1_000)),
            Err(TokenError::Malformed)
        );
        assert_eq!(key.verify("not hex", at(1_000)), Err(TokenError::Malformed));
        assert_eq!(key.verify("", at(1_000)), Err(TokenError::Malformed));
    }

    #[test]
    fn expired_is_refused() {
        let key = TokenKey::from_hex(KEY).unwrap();
        let t = key.issue("sp:ci", at(2_000)).unwrap();
        assert_eq!(key.verify(&t, at(2_000)), Err(TokenError::Expired));
        assert_eq!(key.verify(&t, at(3_000)), Err(TokenError::Expired));
    }

    #[test]
    fn other_key_is_refused() {
        let t = TokenKey::from_hex(KEY)
            .unwrap()
            .issue("sp:ci", at(2_000))
            .unwrap();
        let other = TokenKey::from_hex(OTHER).unwrap();
        assert_eq!(other.verify(&t, at(1_000)), Err(TokenError::BadSignature));
    }

    #[test]
    fn bad_keys_and_principals_are_refused() {
        assert_eq!(TokenKey::from_hex("abcd").unwrap_err(), TokenError::BadKey);
        assert_eq!(
            TokenKey::from_hex(&"zz".repeat(32)).unwrap_err(),
            TokenError::BadKey
        );
        let key = TokenKey::from_hex(KEY).unwrap();
        for p in ["octocat", "github:", "sp:a b", "user:x", "github:a/b"] {
            assert!(
                matches!(key.issue(p, at(2_000)), Err(TokenError::BadPrincipal(_))),
                "{p}"
            );
        }
    }
}
