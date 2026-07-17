//! Kernel-side minting of process-instance identities ([`ProcId`]).
//!
//! Every process the kernel admits is assigned a 128-bit [`ProcId`] that is
//! **not** the reusable scheduler task id / PID: two process lifetimes that
//! reuse a numeric id never share a `ProcId`, so the security audit log (and a
//! future origin record) can attribute an action to the exact instance that
//! took it.
//!
//! # Construction
//!
//! A minted id is the concatenation of two halves:
//!
//! * **bytes 0..8** — a monotonic per-boot counter (big-endian). The counter
//!   starts at `1`, so a minted id is never the all-zero
//!   [`ProcId::KERNEL`] sentinel, and it advances once per admitted process,
//!   which **guarantees uniqueness within a boot** independently of the
//!   randomness source's state. Uniqueness — distinguishing reused PIDs — is
//!   the load-bearing property, so it never depends on entropy being ready.
//! * **bytes 8..16** — eight bytes drawn from the kernel's single CSPRNG
//!   output reserve (`kernel/core`'s [`RandomReserve`]). This adds
//!   unpredictability and cross-boot distinctness. There is no second
//!   randomness source: the bytes come from the one sanctioned reserve, and a
//!   draw that fails closed (the reserve is not yet seeded) simply leaves this
//!   half zero — the counter still makes the id unique.
//!
//! # Bootstrap principals
//!
//! [`mint_proc_id_bootstrap`] mints the counter half only, for the
//! kernel-trusted principals created during early boot before any untrusted
//! code runs — PID 1 and the storage bootstrap-floor drivers. Their
//! uniqueness is fully guaranteed by the shared counter; the random half adds
//! only cross-boot unpredictability against untrusted observers, which do not
//! yet exist when these are admitted (and the reserve is unseeded then in any
//! case). Both minters advance the **same** counter, so no two process
//! instances — bootstrap or ordinary — can ever collide.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::boxed::Box;

use tairix_abi::ProcId;
use tairix_sync::RwLock;

use crate::random::RandomReserve;

/// The shared per-boot process-instance counter.
///
/// Starts at `1` so the first minted id is distinct from the all-zero
/// [`ProcId::KERNEL`] sentinel. Both [`mint_proc_id`] and
/// [`mint_proc_id_bootstrap`] advance it, so uniqueness holds across every
/// admit path. A `u64` cannot realistically wrap within a single boot (it
/// would take 2^64 process admissions).
static PROC_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Reserve and return the next monotonic counter value.
fn next_counter() -> u64 {
    PROC_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Write the next counter value into the high half of a fresh id buffer.
fn counter_id() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&next_counter().to_be_bytes());
    bytes
}

/// Mint a process-instance identity, mixing the per-boot counter with eight
/// bytes from the kernel CSPRNG output reserve.
///
/// The draw is non-blocking and fails closed: if the reserve is not yet
/// seeded the random half is left zero and only the counter distinguishes the
/// id — never weakening to a predictable substitute and never blocking the
/// admit path on entropy. This is the only randomness path used; there is no
/// second PRNG.
#[must_use]
pub fn mint_proc_id(rng: &RwLock<Box<dyn RandomReserve + Send + Sync>>) -> ProcId {
    let mut bytes = counter_id();
    let mut random = [0u8; 8];
    if rng.write().draw(&mut random, true).is_ok() {
        bytes[8..16].copy_from_slice(&random);
    }
    ProcId::from_raw(bytes)
}

/// Mint a process-instance identity for a kernel-trusted bootstrap principal
/// (PID 1, the storage bootstrap-floor drivers).
///
/// Only the monotonic counter half is populated, which alone guarantees the
/// id is unique and distinct from [`ProcId::KERNEL`]. These principals are
/// admitted before any untrusted code runs and before the reserve is seeded,
/// so the random half would be zero regardless; populating only the counter
/// is honest about that rather than pretending an unavailable entropy source
/// contributed.
#[must_use]
pub fn mint_proc_id_bootstrap() -> ProcId {
    ProcId::from_raw(counter_id())
}

#[cfg(test)]
mod tests {
    use super::{mint_proc_id, mint_proc_id_bootstrap};
    use crate::random::{BootReserve, RandomReserve};
    use alloc::boxed::Box;
    use tairix_abi::ProcId;
    use tairix_sync::RwLock;

    fn unseeded_rng() -> RwLock<Box<dyn RandomReserve + Send + Sync>> {
        RwLock::new(Box::new(BootReserve::new()) as Box<dyn RandomReserve + Send + Sync>)
    }

    #[test]
    fn bootstrap_ids_are_unique_and_never_the_kernel_sentinel() {
        let a = mint_proc_id_bootstrap();
        let b = mint_proc_id_bootstrap();
        assert_ne!(a, b);
        assert!(!a.is_kernel());
        assert!(!b.is_kernel());
    }

    #[test]
    fn ordinary_and_bootstrap_ids_share_one_counter_so_never_collide() {
        let rng = unseeded_rng();
        let ordinary = mint_proc_id(&rng);
        let bootstrap = mint_proc_id_bootstrap();
        let ordinary2 = mint_proc_id(&rng);
        assert_ne!(ordinary, bootstrap);
        assert_ne!(ordinary, ordinary2);
        assert_ne!(bootstrap, ordinary2);
    }

    #[test]
    fn unseeded_reserve_leaves_the_random_half_zero_but_id_is_still_unique() {
        // The boot reserve is unseeded, so the fail-closed draw contributes
        // nothing; the counter alone must still distinguish the ids.
        let rng = unseeded_rng();
        let a = mint_proc_id(&rng);
        let b = mint_proc_id(&rng);
        assert_ne!(a, b);
        // The low (random) half is zero on an unseeded reserve.
        assert_eq!(&a.as_bytes()[8..16], &[0u8; 8]);
        assert!(!a.is_kernel());
    }

    #[test]
    fn minted_ids_are_never_kernel_even_in_bulk() {
        let rng = unseeded_rng();
        for _ in 0..64 {
            assert_ne!(mint_proc_id(&rng), ProcId::KERNEL);
            assert_ne!(mint_proc_id_bootstrap(), ProcId::KERNEL);
        }
    }
}
