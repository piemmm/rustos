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
//! (`kernel/core`'s [`RandomReserve`]; the one sanctioned randomness source —
//! `plans`/§22). The draw is **non-blocking and fails closed**: if the reserve
//! is not yet seeded (e.g. a port whose platform entropy source is still
//! `Pending`) the mint yields [`BootId::UNSET`] rather than a predictable
//! substitute, and the `boot_id_get` syscall then reports
//! [`Errno::EntropyNotReady`](rustos_abi::Errno::EntropyNotReady) — the kernel
//! never hands out the all-zero sentinel as if it were a real id.

use alloc::boxed::Box;

use rustos_abi::{BootId, BOOT_ID_LEN};
use rustos_sync::RwLock;

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

#[cfg(test)]
mod tests {
    use super::mint_boot_id;
    use crate::random::{BootReserve, RandomReserve};
    use alloc::boxed::Box;
    use rustos_abi::BootId;
    use rustos_sync::RwLock;

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
}
