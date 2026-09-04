//! Kernel-side minting of the per-boot identifier ([`BootId`]).
//!
//! The kernel mints exactly **one** [`BootId`] per boot, immediately after the
//! CSPRNG output reserve is seeded from the platform entropy source. It is a
//! public 128-bit per-boot nonce — stable for the lifetime of the boot, fresh
//! across boots — that boot-scoped state binds itself to: the system log binds
//! each stream's hash-chain genesis to `machine-id-hash`, the stream, and the
//! `BootId` (`plans/SYSLOG.md` §7.1), so a segment cannot be silently replayed
//! from a different boot.
//!
//! # Construction
//!
//! Unlike [`crate::proc_id`] there is no monotonic-counter half: a boot
//! identity has no within-boot uniqueness obligation (there is one per boot),
//! so its entire value is drawn from the single kernel CSPRNG output reserve
//! (`kernel/core`'s [`RandomReserve`]; the one sanctioned randomness source).
//! The draw is **non-blocking and fails closed**: if the reserve is not yet
//! seeded (e.g. a port whose platform entropy source is still `Pending`) the
//! mint yields [`BootId::UNSET`] rather than a predictable substitute, and the
//! `boot_id_get` syscall then reports
//! [`Errno::EntropyNotReady`](tairix_abi::Errno::EntropyNotReady) — the kernel
//! never hands out the all-zero sentinel as if it were a real id.
//!
//! The per-boot **hash key** ([`publish_hash_key`]) is drawn from the same
//! reserve at the same point, because it has the same shape: one value per
//! boot, from the one sanctioned randomness source, before anything that
//! consumes it can run.

use alloc::boxed::Box;

use tairix_abi::{BootId, BOOT_ID_LEN};
use tairix_hash::HashSeed;
use tairix_sync::RwLock;
use tairix_util::secret::wipe;

use crate::random::RandomReserve;

/// Mint the per-boot [`BootId`] by drawing [`BOOT_ID_LEN`] bytes from the
/// kernel CSPRNG output reserve.
///
/// Non-blocking and fail-closed: a reserve that cannot serve the draw (it is
/// unseeded) yields [`BootId::UNSET`], never a predictable value and never a
/// block on the boot path. Call this exactly once per boot, after the reserve
/// has been seeded.
#[must_use]
pub fn mint_boot_id(rng: &RwLock<Box<dyn RandomReserve + Send + Sync>>) -> BootId {
    let mut bytes = [0u8; BOOT_ID_LEN];
    if rng.write().draw(&mut bytes, true).is_ok() {
        BootId::from_raw(bytes)
    } else {
        BootId::UNSET
    }
}

/// Publish the per-boot hash key from the same reserve, reporting whether a
/// key became available.
///
/// Every in-kernel hash over a key a caller can choose — a futex wait
/// address today — is taken under this key, so no caller can compute a set of
/// keys that all land in one bucket. Drawn once per boot, immediately after
/// the reserve is seeded and before userland can reach a syscall that hashes.
///
/// Non-blocking and honest rather than fail-closed: a bucket index is a
/// contention choice, not an authority one, so a port whose reserve never
/// seeded keeps working with an unkeyed hash — and the `false` this returns
/// is what puts that state in the audit log instead of leaving it silent.
#[must_use]
pub fn publish_hash_key(rng: &RwLock<Box<dyn RandomReserve + Send + Sync>>) -> bool {
    let mut key = [0u8; HashSeed::LEN];
    if rng.write().draw(&mut key, true).is_ok() {
        let _ = tairix_hash::publish(HashSeed::from_bytes(key));
    }
    wipe(&mut key);
    tairix_hash::is_published()
}

#[cfg(test)]
mod tests {
    use super::{mint_boot_id, publish_hash_key};
    use crate::random::{BootReserve, RandomReserve};
    use alloc::boxed::Box;
    use tairix_abi::BootId;
    use tairix_sync::RwLock;

    fn unseeded_rng() -> RwLock<Box<dyn RandomReserve + Send + Sync>> {
        RwLock::new(Box::new(BootReserve::new()) as Box<dyn RandomReserve + Send + Sync>)
    }

    #[test]
    fn unseeded_reserve_mints_the_unset_sentinel_fail_closed() {
        // With no seeded entropy the draw fails closed: the mint must yield
        // the UNSET sentinel, never a predictable (e.g. partially-zero) id.
        let rng = unseeded_rng();
        assert_eq!(mint_boot_id(&rng), BootId::UNSET);
    }

    /// The hash key is the same draw and fails the same way: no entropy, no
    /// key. Publishing a zero key here would hand every caller a predictable
    /// bucket index while reporting success.
    ///
    /// Nothing else in this test binary publishes, so the global cell is
    /// untouched when this runs.
    #[test]
    fn an_unseeded_reserve_publishes_no_hash_key() {
        let rng = unseeded_rng();
        assert!(!publish_hash_key(&rng));
        assert!(!tairix_hash::is_published());
    }
}
