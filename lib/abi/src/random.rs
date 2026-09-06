//! Canonical random-number ABI.
//!
//! TAIRiX has exactly one kernel cryptographic random subsystem. Userland
//! never invents its own entropy collector, PRNG, or seeding path; it asks
//! the kernel for bytes through the single versioned random syscall whose
//! shape is pinned here. The implementation behind the syscall lives in the
//! kernel and draws from `lib/rng`'s CSPRNG-backed output reserve
//! (`tairix_rng::OutputReserve`); this module only fixes the *contract*.
//!
//! # The contract
//!
//! A caller hands the kernel a destination buffer, a length, and a set of
//! [`RandomFlags`]. The kernel fills the buffer with cryptographically
//! secure random bytes and returns the count written.
//!
//! * Before the kernel RNG is initialised, **every** request fails closed
//!   with [`crate::Errno::EntropyNotReady`] rather than returning weak
//!   randomness. It is not made to wait: the only way the RNG is still
//!   unseeded once userland exists is that the platform's entropy sources
//!   are all dead, and a wait on a dead source never ends. A caller that
//!   wants to retry decides that for itself, and one that set
//!   [`RandomFlags::NON_BLOCKING`] has said it will not.
//! * After initialisation a request never blocks waiting for fresh external
//!   entropy: the reserve serves from a cipher keyed by the CSPRNG, so an
//!   exhausted buffer is regenerated on the spot.
//!   [`RandomFlags::NON_BLOCKING`] then chooses only whether the reserve's
//!   periodic fold of fresh entropy into that key waits for a momentarily
//!   dry source or is deferred to a later request; the bytes served are the
//!   same either way.
//!
//! The numeric flag bits and the per-call length cap are part of the frozen
//! random ABI: new behaviour is added by allocating an unused bit, never by
//! repurposing an existing one.

use crate::Errno;

/// Default size, in bytes, of the kernel's per-CPU random output reserve.
///
/// the charter permits a default reserve of 2 KiB, preferably per-CPU to
/// avoid lock contention. The reserve holds CSPRNG *output* (not raw
/// entropy); it is refilled in the background and on demand.
pub const RANDOM_RESERVE_DEFAULT_BYTES: usize = 2048;

/// Maximum number of bytes a single random request may ask for.
///
/// Bounds the work the kernel performs for one syscall so a caller cannot
/// pin a CPU generating an unbounded stream in a single uninterruptible
/// call; a caller needing more issues further requests. A request larger
/// than [`RANDOM_RESERVE_DEFAULT_BYTES`] is still valid — the kernel simply
/// generates the overflow synchronously from the CSPRNG.
pub const RANDOM_REQUEST_MAX_BYTES: usize = 1 << 16;

// A single request may exceed one reserve's worth (the kernel tops up
// synchronously), so the per-call cap must be strictly larger than the
// reserve. Pinned at compile time.
const _: () = assert!(RANDOM_REQUEST_MAX_BYTES > RANDOM_RESERVE_DEFAULT_BYTES);

/// Flags accepted by the random request.
///
/// A `#[repr(transparent)]` newtype over the `u32` flags register so the
/// wire representation is exactly the integer the syscall trampoline passes.
/// Only the bits named here are defined; every other bit is reserved and
/// must be zero. [`RandomFlags::from_bits`] rejects a value with any
/// reserved bit set, so a future flag cannot be silently ignored by an older
/// kernel (validate every input, fail closed).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct RandomFlags(u32);

impl RandomFlags {
    /// Do not block waiting for the kernel RNG to initialise.
    ///
    /// When set and the RNG is not yet seeded, the request returns
    /// [`Errno::EntropyNotReady`] immediately instead of blocking. After the
    /// RNG is initialised this flag has no observable effect: a normal
    /// request never blocks waiting for fresh external entropy.
    pub const NON_BLOCKING: Self = Self(1 << 0);

    /// The set of all defined flag bits.
    ///
    /// Any bit outside this mask is reserved and rejected by
    /// [`RandomFlags::from_bits`].
    const DEFINED_BITS: u32 = Self::NON_BLOCKING.0;

    /// An empty flag set (blocking request, no options).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw flag bits, as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build a flag set from raw bits, rejecting any reserved bit.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `bits` sets any reserved
    /// (currently-undefined) bit.
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::DEFINED_BITS != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }

    /// Whether every bit set in `other` is also set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the caller asked for non-blocking behaviour.
    #[must_use]
    pub const fn is_non_blocking(self) -> bool {
        self.contains(Self::NON_BLOCKING)
    }
}

#[cfg(test)]
mod tests {
    use super::{RandomFlags, RANDOM_RESERVE_DEFAULT_BYTES};
    use crate::Errno;

    #[test]
    fn reserve_default_is_two_kib() {
        assert_eq!(RANDOM_RESERVE_DEFAULT_BYTES, 2048);
    }

    #[test]
    fn empty_is_blocking() {
        let f = RandomFlags::empty();
        assert_eq!(f.bits(), 0);
        assert!(!f.is_non_blocking());
    }

    #[test]
    fn non_blocking_round_trips() {
        let f = RandomFlags::NON_BLOCKING;
        assert!(f.is_non_blocking());
        let again = RandomFlags::from_bits(f.bits()).expect("defined bit");
        assert_eq!(again, f);
    }

    #[test]
    fn reserved_bits_are_rejected() {
        // Bit 1 is reserved today.
        assert_eq!(RandomFlags::from_bits(1 << 1), Err(Errno::OutOfRange));
        assert_eq!(RandomFlags::from_bits(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn contains_is_subset_relation() {
        assert!(RandomFlags::NON_BLOCKING.contains(RandomFlags::empty()));
        assert!(!RandomFlags::empty().contains(RandomFlags::NON_BLOCKING));
    }
}
