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
    if let Some(key) = draw_hash_key(rng) {
        let _ = tairix_hash::publish(key);
    }
    tairix_hash::is_published()
}

/// Key the scheduler's process-wide task-id generator from the same reserve,
/// so the ids a boot hands out differ from the last boot's as well as from
/// each other.
///
/// The key is taken at the generator's full width, because a narrower draw
/// stretched to fill it would cap the effective entropy at the draw's width
/// however wide the generator's state is.
///
/// Unkeyed, the generator still runs from its compiled-in seed and still
/// hands out unpredictable, non-sequential ids within the boot; only
/// cross-boot distinctness is lost. That is why this neither fails closed nor
/// is audited, unlike the hash key.
pub fn seed_task_ids(rng: &RwLock<Box<dyn RandomReserve + Send + Sync>>) {
    let mut key = [0u8; tairix_rng::STREAM_KEY_LEN];
    if rng.write().draw(&mut key, true).is_ok() {
        tairix_kernel_sched_api::seed_task_ids(&key);
    }
    wipe(&mut key);
}

/// Draw a per-boot hash key from the reserve, or `None` when the reserve
/// cannot serve the draw.
///
/// Split from the publication so the fail-closed decision — no entropy, no
/// key — is observable on its own. The publication cell is process-global and
/// one-shot, so whether *a* key exists is not evidence about what this call
/// drew, and a test that reads the cell instead is asserting on state it does
/// not own.
fn draw_hash_key(rng: &RwLock<Box<dyn RandomReserve + Send + Sync>>) -> Option<HashSeed> {
    let mut key = [0u8; HashSeed::LEN];
    let drawn = rng.write().draw(&mut key, true).is_ok();
    let seed = drawn.then(|| HashSeed::from_bytes(key));
    wipe(&mut key);
    seed
}

#[cfg(test)]
mod tests {
    use super::{draw_hash_key, mint_boot_id};
    use crate::random::{BootReserve, RandomReserve};
    use alloc::boxed::Box;
    use tairix_abi::BootId;
    use tairix_rng::{EntropyError, EntropySource, OutputReserve};
    use tairix_sync::RwLock;

    fn unseeded_rng() -> RwLock<Box<dyn RandomReserve + Send + Sync>> {
        RwLock::new(Box::new(BootReserve::new()) as Box<dyn RandomReserve + Send + Sync>)
    }

    /// A deterministic entropy source, so the seeded half of a draw is
    /// exercised without a platform source.
    struct FixedEntropy;

    impl EntropySource for FixedEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            out.fill(0xA5);
            Ok(())
        }
    }

    fn seeded_rng() -> RwLock<Box<dyn RandomReserve + Send + Sync>> {
        let mut reserve = OutputReserve::<FixedEntropy>::new();
        reserve
            .seed(FixedEntropy)
            .expect("a deterministic source seeds");
        RwLock::new(Box::new(reserve) as Box<dyn RandomReserve + Send + Sync>)
    }

    #[test]
    fn unseeded_reserve_mints_the_unset_sentinel_fail_closed() {
        // With no seeded entropy the draw fails closed: the mint must yield
        // the UNSET sentinel, never a predictable (e.g. partially-zero) id.
        let rng = unseeded_rng();
        assert_eq!(mint_boot_id(&rng), BootId::UNSET);
    }

    /// The hash key is the same draw and fails the same way: no entropy, no
    /// key. Handing over a zero key would give every caller a predictable
    /// bucket index.
    ///
    /// Asserted on the *draw*, never on `tairix_hash::is_published`: that
    /// cell is process-global and one-shot, so another test in this binary
    /// publishing to it legitimately — the launch cache's index is keyed
    /// under it — would otherwise fail this one, in whichever order the
    /// harness happened to run them.
    #[test]
    fn the_hash_key_draw_fails_closed_without_entropy_and_serves_with_it() {
        assert!(draw_hash_key(&unseeded_rng()).is_none());
        let key = draw_hash_key(&seeded_rng()).expect("a seeded reserve serves");
        assert_ne!(key.words(), (0, 0), "a published key is never all-zero");
    }
}
