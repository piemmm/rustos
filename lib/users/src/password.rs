//! The stored password record: `pbkdf2-sha256$<iterations>$<salt>$<hash>`.
//!
//! A [`PasswordRecord`] is the only secret-bearing field of a user record.
//! It never stores the password itself: only the PBKDF2-HMAC-SHA256 hash
//! (`lib/crypto`), the per-record random salt, and the
//! iteration cost. Decoding is fail-closed: a record with
//! the wrong scheme tag, an out-of-range cost, or a salt/hash of the wrong
//! width yields no record. Verification is constant-time with respect to the
//! stored hash.

use core::fmt;
use core::num::NonZeroU32;

use alloc::string::String;

use rustos_crypto::{pbkdf2_sha256, pbkdf2_sha256_verify, PasswordHash, PASSWORD_HASH_LEN};

use crate::ParseError;

/// Scheme tag every `users-v1` password record begins with. Exactly one
/// scheme exists; an unknown tag is rejected, never guessed at.
pub const PASSWORD_SCHEME: &str = "pbkdf2-sha256";

/// Length, in bytes, of the per-record random salt.
pub const SALT_LEN: usize = 16;

/// A per-record random salt as raw bytes.
pub type Salt = [u8; SALT_LEN];

/// Inclusive PBKDF2 iteration bounds a record may carry. A cost below the
/// floor would make offline guessing cheap; one above the ceiling is a
/// denial-of-service on every login attempt. Both are validation
/// bounds, fixed by policy, not capacities.
pub const MIN_ITERATIONS: u32 = 1_000;
/// See [`MIN_ITERATIONS`].
pub const MAX_ITERATIONS: u32 = 10_000_000;

/// Default PBKDF2 cost for a newly set password (OWASP's 2023 floor for
/// PBKDF2-HMAC-SHA256).
pub const DEFAULT_ITERATIONS: u32 = 600_000;

/// Longest password, in bytes, the verifier will derive a hash from. A
/// longer offering is rejected outright — an unbounded input would let an
/// attacker buy arbitrarily long derivations (validation bound).
pub const MAX_PASSWORD_LEN: usize = 256;

/// A decoded, validated stored-password record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasswordRecord {
    iterations: NonZeroU32,
    salt: Salt,
    hash: PasswordHash,
}

impl PasswordRecord {
    /// Build a record by hashing `password` under a caller-supplied random
    /// `salt` and `iterations` cost.
    ///
    /// The caller draws `salt` from its platform entropy source; this crate
    /// stays deterministic and `no_std`. The password bytes are read once,
    /// hashed, and never stored.
    ///
    /// # Errors
    ///
    /// [`ParseError::PasswordRecord`] if `iterations` is outside
    /// [`MIN_ITERATIONS`]`..=`[`MAX_ITERATIONS`] or `password` exceeds
    /// [`MAX_PASSWORD_LEN`].
    pub fn new(password: &[u8], salt: Salt, iterations: u32) -> Result<Self, ParseError> {
        if password.len() > MAX_PASSWORD_LEN {
            return Err(ParseError::PasswordRecord);
        }
        let iterations = checked_iterations(iterations)?;
        Ok(Self {
            iterations,
            salt,
            hash: pbkdf2_sha256(password, &salt, iterations),
        })
    }

    /// Decode a record from its stored `pbkdf2-sha256$…` text form.
    ///
    /// # Errors
    ///
    /// [`ParseError::PasswordRecord`] on an unknown scheme tag, a malformed
    /// or out-of-range iteration count, or a salt/hash that is not exactly
    /// the expected width of lowercase hex.
    pub fn decode(text: &str) -> Result<Self, ParseError> {
        let mut fields = text.split('$');
        let scheme = fields.next().ok_or(ParseError::PasswordRecord)?;
        let iterations = fields.next().ok_or(ParseError::PasswordRecord)?;
        let salt = fields.next().ok_or(ParseError::PasswordRecord)?;
        let hash = fields.next().ok_or(ParseError::PasswordRecord)?;
        if fields.next().is_some() || scheme != PASSWORD_SCHEME {
            return Err(ParseError::PasswordRecord);
        }

        let iterations = iterations
            .parse::<u32>()
            .ok()
            .ok_or(ParseError::PasswordRecord)
            .and_then(checked_iterations)?;
        Ok(Self {
            iterations,
            salt: unhex::<SALT_LEN>(salt)?,
            hash: unhex::<PASSWORD_HASH_LEN>(hash)?,
        })
    }

    /// Encode the record into the stored text form [`Self::decode`] accepts.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // Writing into a `String` cannot fail; `fmt::Write` is total here.
        let _ = fmt::Write::write_fmt(
            &mut out,
            format_args!("{}${}$", PASSWORD_SCHEME, self.iterations),
        );
        push_hex(&mut out, &self.salt);
        out.push('$');
        push_hex(&mut out, &self.hash);
        out
    }

    /// Verify an offered `password` against this record, in constant time
    /// with respect to the stored hash.
    ///
    /// An over-long password is rejected without deriving anything.
    #[must_use]
    pub fn verify(&self, password: &[u8]) -> bool {
        if password.len() > MAX_PASSWORD_LEN {
            return false;
        }
        pbkdf2_sha256_verify(password, &self.salt, self.iterations, &self.hash)
    }

    /// The record's PBKDF2 iteration cost.
    #[must_use]
    pub fn iterations(&self) -> u32 {
        self.iterations.get()
    }
}

/// Validate an iteration count into the accepted [`NonZeroU32`] range.
fn checked_iterations(iterations: u32) -> Result<NonZeroU32, ParseError> {
    if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
        return Err(ParseError::PasswordRecord);
    }
    NonZeroU32::new(iterations).ok_or(ParseError::PasswordRecord)
}

/// Decode exactly `N` bytes of lowercase hex, rejecting anything else.
fn unhex<const N: usize>(text: &str) -> Result<[u8; N], ParseError> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 * N {
        return Err(ParseError::PasswordRecord);
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_value(bytes[2 * i])?;
        let lo = hex_value(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

/// One lowercase hex digit's value; uppercase is rejected so each record has
/// exactly one valid spelling.
fn hex_value(byte: u8) -> Result<u8, ParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ParseError::PasswordRecord),
    }
}

/// Append `bytes` to `out` as lowercase hex.
fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::{PasswordRecord, Salt, MAX_PASSWORD_LEN, MIN_ITERATIONS};
    use crate::ParseError;

    use alloc::string::String;
    use alloc::vec;

    const SALT: Salt = [0xA5; 16];

    fn record() -> PasswordRecord {
        PasswordRecord::new(b"byron", SALT, MIN_ITERATIONS).expect("valid record")
    }

    #[test]
    fn encode_decode_round_trips() {
        let original = record();
        let text = original.encode();
        assert!(text.starts_with("pbkdf2-sha256$1000$a5a5"));
        assert_eq!(PasswordRecord::decode(&text), Ok(original));
    }

    #[test]
    fn verify_accepts_the_password_and_rejects_others() {
        let record = record();
        assert!(record.verify(b"byron"));
        assert!(!record.verify(b"Byron"));
        assert!(!record.verify(b""));
    }

    #[test]
    fn an_oversized_password_is_rejected_everywhere() {
        let long = vec![b'x'; MAX_PASSWORD_LEN + 1];
        assert_eq!(
            PasswordRecord::new(&long, SALT, MIN_ITERATIONS),
            Err(ParseError::PasswordRecord)
        );
        assert!(!record().verify(&long));
    }

    #[test]
    fn iteration_bounds_are_enforced() {
        assert_eq!(
            PasswordRecord::new(b"x", SALT, MIN_ITERATIONS - 1),
            Err(ParseError::PasswordRecord)
        );
        assert_eq!(
            PasswordRecord::new(b"x", SALT, super::MAX_ITERATIONS + 1),
            Err(ParseError::PasswordRecord)
        );
        assert_eq!(
            PasswordRecord::new(b"x", SALT, 0),
            Err(ParseError::PasswordRecord)
        );
    }

    #[test]
    fn malformed_stored_records_are_rejected() {
        let good = record().encode();
        for bad in [
            String::new(),
            String::from("pbkdf2-sha256"),
            String::from("pbkdf2-sha256$1000$aa"),
            good.replace("pbkdf2-sha256", "scrypt"),
            good.replace("1000", "0"),
            good.replace("1000", "999"),
            good.replace("1000", "10000001"),
            good.replace("1000", "ten"),
            good.replacen("a5", "A5", 1),
            good.clone() + "$extra",
            good.clone() + "ff",
        ] {
            assert_eq!(
                PasswordRecord::decode(&bad),
                Err(ParseError::PasswordRecord),
                "accepted: {bad}"
            );
        }
    }

    #[test]
    fn distinct_salts_yield_distinct_hashes() {
        let a = PasswordRecord::new(b"byron", [0x01; 16], MIN_ITERATIONS).expect("valid");
        let b = PasswordRecord::new(b"byron", [0x02; 16], MIN_ITERATIONS).expect("valid");
        assert_ne!(a.encode(), b.encode());
    }
}
