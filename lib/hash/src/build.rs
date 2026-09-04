//! [`BuildHasher`] shims — how a container obtains a hasher per key.
//!
//! A container stores one of these and asks it for a fresh
//! [`Hasher`](core::hash::Hasher) per hash. Which one it stores is the
//! security decision: [`BuildSipHash13`] under a published key for keys an
//! attacker can choose, [`BuildFastHash`] for keys the kernel assigns
//! itself.

use core::fmt;
use core::hash::BuildHasher;

use crate::fast::FastHash;
use crate::seed::{published, HashSeed};
use crate::siphash::SipHash13;

/// Builds keyed [`SipHash13`] hashers — the default for any container over
/// keys an attacker can choose or influence.
///
/// [`fmt::Debug`] redacts the key, as [`HashSeed`]'s does.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct BuildSipHash13 {
    seed: HashSeed,
}

/// Returned by [`BuildSipHash13::keyed`] when no key has been published yet.
///
/// Hashing attacker-chosen keys under a predictable key is the collision
/// flood this crate exists to prevent, so the construction is refused rather
/// than silently downgraded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Unseeded;

impl fmt::Display for Unseeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no hash key has been published for this boot or process")
    }
}

impl BuildSipHash13 {
    /// Keyed with the per-boot / per-process key, or [`Unseeded`] before one
    /// is published.
    ///
    /// # Errors
    ///
    /// [`Unseeded`] when [`published`] has nothing yet.
    pub fn keyed() -> Result<Self, Unseeded> {
        published().map(|seed| Self { seed }).ok_or(Unseeded)
    }

    /// Keyed with an explicit key — for a test, or for a holder that draws
    /// and owns its own key rather than sharing the published one.
    #[must_use]
    pub const fn with_seed(seed: HashSeed) -> Self {
        Self { seed }
    }

    /// The all-zero, **predictable** key.
    ///
    /// Only for a hash that is not a security decision and must work before
    /// the platform CSPRNG can supply one; naming it is what makes that
    /// choice visible in review.
    pub const UNKEYED: Self = Self {
        seed: HashSeed::UNKEYED,
    };
}

impl fmt::Debug for BuildSipHash13 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BuildSipHash13(<redacted>)")
    }
}

impl BuildHasher for BuildSipHash13 {
    type Hasher = SipHash13;

    fn build_hasher(&self) -> SipHash13 {
        SipHash13::new(self.seed)
    }
}

/// Builds [`FastHash`] hashers — **not** keyed, so never correct for a key an
/// attacker can choose or influence.
///
/// For kernel-assigned keys, content fingerprints, and revision counters,
/// where the only property wanted is spread.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildFastHash {
    seed: u64,
}

impl BuildFastHash {
    /// An unseeded builder.
    #[must_use]
    pub const fn new() -> Self {
        Self { seed: 0 }
    }

    /// A builder whose stream is distinguished by `seed`. The seed is a
    /// distinguisher, not a secret.
    #[must_use]
    pub const fn with_seed(seed: u64) -> Self {
        Self { seed }
    }
}

impl BuildHasher for BuildFastHash {
    type Hasher = FastHash;

    fn build_hasher(&self) -> FastHash {
        FastHash::with_seed(self.seed)
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildFastHash, BuildSipHash13};
    use crate::seed::HashSeed;
    use core::hash::BuildHasher;

    #[test]
    fn siphash_builder_reproduces_the_one_shot_hash() {
        let seed = HashSeed::from_words(0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);
        let build = BuildSipHash13::with_seed(seed);
        assert_eq!(
            build.hash_one(b"the quick brown fox"),
            BuildSipHash13::with_seed(seed).hash_one(b"the quick brown fox"),
        );
        assert_ne!(
            build.hash_one(b"the quick brown fox"),
            BuildSipHash13::UNKEYED.hash_one(b"the quick brown fox"),
            "a different key must give a different table layout",
        );
    }

    #[test]
    fn fast_builder_is_seed_distinguished() {
        assert_ne!(
            BuildFastHash::new().hash_one(42u64),
            BuildFastHash::with_seed(1).hash_one(42u64),
        );
        assert_eq!(
            BuildFastHash::default().hash_one(42u64),
            BuildFastHash::new().hash_one(42u64),
        );
    }

    #[test]
    fn siphash_builder_debug_redacts_the_key() {
        extern crate std;
        let rendered = std::format!(
            "{:?}",
            BuildSipHash13::with_seed(HashSeed::from_words(0xdead, 0xbeef))
        );
        assert!(!rendered.contains("dead"), "{rendered}");
        assert!(!rendered.contains("beef"), "{rendered}");
    }
}
