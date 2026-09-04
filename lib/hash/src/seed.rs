//! The 128-bit hash key and its one-shot publication seam.

use core::fmt;

use tairix_sync::once::OnceCell;

/// A 128-bit hash key.
///
/// Key material: a holder must not log it, render it, or hand it across a
/// process boundary — an attacker who learns it can pick colliding keys
/// again. [`fmt::Debug`] therefore redacts the words rather than printing
/// them, so a key cannot reach a log through a derived `Debug` on some
/// enclosing type.
///
/// `Copy`, because every hasher instance takes its own copy of the key; a
/// zeroising drop would be meaningless on a type that is duplicated by value.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct HashSeed {
    k0: u64,
    k1: u64,
}

impl HashSeed {
    /// Bytes in a key. The one definition every site that draws one sizes
    /// its buffer from.
    pub const LEN: usize = 16;

    /// The all-zero, **predictable** key.
    ///
    /// For a consumer whose hash is not a security decision and which must
    /// still work before the platform CSPRNG can supply a key — a bucket
    /// index that only affects contention, say. Naming it is the point: a
    /// container over attacker-chosen keys must refuse to run rather than
    /// reach for this.
    pub const UNKEYED: Self = Self { k0: 0, k1: 0 };

    /// Build a key from [`HashSeed::LEN`] raw bytes, read as two
    /// little-endian words.
    #[must_use]
    pub const fn from_bytes(key: [u8; Self::LEN]) -> Self {
        let k0 = u64::from_le_bytes([
            key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
        ]);
        let k1 = u64::from_le_bytes([
            key[8], key[9], key[10], key[11], key[12], key[13], key[14], key[15],
        ]);
        Self { k0, k1 }
    }

    /// Build a key from two words.
    #[must_use]
    pub const fn from_words(k0: u64, k1: u64) -> Self {
        Self { k0, k1 }
    }

    /// The key's two words, for a hasher's initialisation.
    #[must_use]
    pub const fn words(self) -> (u64, u64) {
        (self.k0, self.k1)
    }
}

impl fmt::Debug for HashSeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashSeed(<redacted>)")
    }
}

/// Returned by [`publish`] when a key was already published.
///
/// Carries the rejected key back so the caller can see nothing was consumed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AlreadyPublished(pub HashSeed);

/// The one key for this boot (in the kernel) or this process (in userland).
///
/// Only [`OnceCell::set`], [`OnceCell::get`], and `is_initialised` are used —
/// none of which spins — so a reader is safe from any context, unlike the
/// cell's lazy-initialiser path.
static PUBLISHED: OnceCell<HashSeed> = OnceCell::new();

/// Publish the key for this boot or process. The first call wins.
///
/// The boot path publishes as soon as the platform CSPRNG can supply a key
/// and before any untrusted input is parsed; a userland program publishes at
/// start-up. A second publication is refused rather than swapping the key
/// under a live container, whose entries would then be unfindable.
///
/// # Errors
///
/// [`AlreadyPublished`] if a key has already been published.
pub fn publish(seed: HashSeed) -> Result<(), AlreadyPublished> {
    PUBLISHED.set(seed).map_err(|rejected| {
        let tairix_sync::AlreadySetError(seed) = rejected;
        AlreadyPublished(seed)
    })
}

/// The published key, or `None` before publication.
#[must_use]
pub fn published() -> Option<HashSeed> {
    // A poisoned cell cannot arise: nothing here uses a fallible initialiser.
    PUBLISHED.get().ok().flatten().copied()
}

/// Whether a key has been published yet.
#[must_use]
pub fn is_published() -> bool {
    PUBLISHED.is_initialised()
}

#[cfg(test)]
mod tests {
    use super::{is_published, publish, published, HashSeed};

    #[test]
    fn bytes_and_words_agree() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        assert_eq!(
            HashSeed::from_bytes(key).words(),
            (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908)
        );
    }

    #[test]
    fn debug_redacts_the_key() {
        extern crate std;
        let rendered = std::format!("{:?}", HashSeed::from_words(0xdead, 0xbeef));
        assert!(!rendered.contains("dead"), "{rendered}");
        assert!(!rendered.contains("beef"), "{rendered}");
    }

    /// The publication seam is process-global, so one test owns the whole
    /// lifecycle: unpublished, published once, and a second publication
    /// refused. Splitting it across tests would race the shared cell.
    #[test]
    fn publication_is_one_shot() {
        assert!(!is_published());
        assert_eq!(published(), None);

        let first = HashSeed::from_words(0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00);
        assert_eq!(publish(first), Ok(()));
        assert!(is_published());
        assert_eq!(published(), Some(first));

        let second = HashSeed::from_words(1, 2);
        assert_eq!(publish(second).map_err(|e| e.0), Err(second));
        assert_eq!(published(), Some(first), "the first key still stands");
    }
}
