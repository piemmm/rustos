//! The process-global compressed-memory tier singleton and the typed
//! outcomes its live entry points return (`plans/SWAPSWAPSWAP.md`).
//!
//! [`Ramzip`] is designed as one pool shared by every address space:
//! its entries are keyed by `(space id, page)`, its ledger tracks a
//! per-task breakdown, and its caps and fair-share bound are derived
//! from *total* physical RAM. That is a single global pool with
//! per-task fairness, not one tier per process — so the kernel holds
//! exactly one instance here, behind a lock, installed once at boot
//! from the platform entropy path and the discovered RAM size.
//!
//! # Why a lock, not a `static mut`
//!
//! The tier is a `&mut self` state machine (its concurrency contract is
//! in [`super::tier`]). The one instance is shared across CPUs, so it
//! lives behind a [`SpinLock`]: every live operation takes the lock,
//! mutates the pool, and drops it. The lock is only ever taken from
//! task/syscall context (a demand fault or the pressure sweep), never
//! from interrupt context, so a plain [`SpinLock`] is the right
//! primitive (no ISR sharing to guard against).
//!
//! # Testability
//!
//! The live operations that need the tier take a `&`[`SpinLock`]`<`[`Ramzip`]`>`
//! explicitly (see [`crate::live::LiveUserSpace`]), so a host test
//! constructs its own tier and drives the exact production path without
//! touching this global. Production wiring reaches the one installed
//! instance through [`global`]; before boot installs it (and on a port
//! with no entropy) [`global`] is `None` and every caller falls closed.

use tairix_abi::sysinfo::RamzipStats;
use tairix_sync::once::OnceCell;
use tairix_sync::SpinLock;

use super::tier::{FaultError, Ramzip};

/// The one installed tier. `None` until boot installs it; a port whose
/// entropy path cannot supply a key never installs one and every live
/// caller falls closed (fault-in reports [`RamzipFaultOutcome::NoEntry`],
/// the reclaim sweep compresses nothing).
static TIER: OnceCell<SpinLock<Ramzip>> = OnceCell::new();

/// Install the boot-constructed tier. First install wins.
///
/// Returns `true` if this call installed the tier, `false` if one was
/// already installed (the passed tier is then dropped, discarding its
/// freshly generated key — a second install is a boot-path defect, not
/// a runtime condition). The boot path calls this exactly once after
/// the frame allocator and the kernel CSPRNG exist.
pub fn install(tier: Ramzip) -> bool {
    TIER.set(SpinLock::new(tier)).is_ok()
}

/// The one installed tier, or `None` before boot installs it.
///
/// The live fault-in and reclaim paths pass the returned lock to the
/// owning [`crate::live::LiveSpace`]; a `None` result means the tier is
/// not active and the caller falls closed.
#[must_use]
pub fn global() -> Option<&'static SpinLock<Ramzip>> {
    // A poisoned cell (an initialiser that panicked — impossible here,
    // `set` takes a ready value) reads as absent: fail closed.
    TIER.get().ok().flatten()
}

/// Snapshot the installed tier's exported counters for the System
/// Information feed, or an all-zero idle snapshot before one is
/// installed. Counters only — never page contents or key material.
#[must_use]
pub fn global_stats() -> RamzipStats {
    match global() {
        Some(lock) => stats_of(&lock.lock()),
        None => RamzipStats::default(),
    }
}

/// Project a tier's caps, ledger totals, and event counters onto the
/// ABI [`RamzipStats`] record. `pinned_bytes` is left zero: the pinned
/// footprint is a task-registry figure the introspection source folds
/// in separately, not a tier counter.
#[must_use]
pub fn stats_of(tier: &Ramzip) -> RamzipStats {
    let caps = tier.caps();
    let ledger = tier.ledger();
    let counters = ledger.counters();
    RamzipStats {
        entries: ledger.entries(),
        logical_bytes: ledger.logical_bytes() as u64,
        compressed_bytes: ledger.compressed_bytes() as u64,
        stored_bytes: ledger.stored_bytes() as u64,
        metadata_bytes: ledger.metadata_bytes() as u64,
        min_cap_bytes: caps.min() as u64,
        soft_cap_bytes: caps.soft() as u64,
        hard_cap_bytes: caps.hard() as u64,
        attempts: counters.attempts,
        accepted: counters.accepted,
        rejected_policy: counters.rejected_policy,
        rejected_ineligible: counters.rejected_ineligible,
        rejected_incompressible: counters.rejected_incompressible,
        rejected_cap: counters.rejected_cap,
        rejected_reserve: counters.rejected_reserve,
        rejected_task_share: counters.rejected_task_share,
        rejected_thrash: counters.rejected_thrash,
        fault_ins: counters.fault_ins,
        auth_failures: counters.auth_failures,
        decode_failures: counters.decode_failures,
        warm_attempts: counters.warm_attempts,
        warm_restored: counters.warm_restored,
        warm_stopped: counters.warm_stopped,
        cluster_restored: counters.cluster_restored,
        thrash_detected: counters.thrash_detected,
        pinned_bytes: 0,
    }
}

/// The outcome of a live compressed-page fault-in attempt
/// ([`crate::live::LiveUserSpace::ramzip_fault_in`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RamzipFaultOutcome {
    /// A compressed entry existed for the faulting page and was
    /// restored: the page is now resident and the faulting instruction
    /// may be retried.
    Handled,
    /// No compressed entry existed (or the tier is not installed): the
    /// fault resolver falls through to the next handler.
    NoEntry,
    /// The entry was unrecoverable (authentication or decode failure,
    /// or a restore-time allocation/page-table failure). Fail closed:
    /// no plaintext was produced. The caller escalates through the VM
    /// fault policy — the task cannot continue without the page.
    Fatal(FaultError),
}

/// The result of one live reclaim sweep over a space's cold anonymous
/// pages ([`crate::live::LiveUserSpace::ramzip_reclaim`]).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct RamzipReclaimSummary {
    /// Resident anonymous candidate pages the cold-page scanner
    /// examined this sweep.
    pub scanned: usize,
    /// Cold pages compressed out into the tier (frames returned to the
    /// allocator).
    pub compressed: usize,
    /// Cold pages the tier refused (a typed [`super::CompressRefusal`]);
    /// the page stays resident.
    pub refused: usize,
    /// `true` when the backend exposes no referenced bit, so no page
    /// could be shown cold and the sweep reclaimed nothing (fail
    /// closed). Distinguishes "nothing was cold" from "cannot tell".
    pub access_tracking_unsupported: bool,
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::ramzip::RamzipCaps;
    use crate::seal::{EntropySource, SealError};

    /// Deterministic counting entropy for a test key/salt.
    struct CountingEntropy {
        next: u8,
    }

    impl EntropySource for CountingEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
            for byte in out.iter_mut() {
                *byte = self.next;
                self.next = self.next.wrapping_add(1);
            }
            Ok(())
        }
    }

    #[test]
    fn idle_stats_are_all_zero() {
        let mut entropy = CountingEntropy { next: 1 };
        let tier = Ramzip::new(RamzipCaps::from_physical(8 << 30), &mut entropy).expect("tier");
        let stats = stats_of(&tier);
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.attempts, 0);
        assert_eq!(stats.pinned_bytes, 0);
        // Caps are reported from the derived policy even when idle.
        assert_eq!(stats.hard_cap_bytes, (8u64 << 30) / 4);
        assert_eq!(stats.soft_cap_bytes, (8u64 << 30) / 10);
    }

    #[test]
    fn global_stats_default_before_install() {
        // The production `TIER` is never installed in host tests, so the
        // global snapshot is the idle default.
        assert_eq!(global_stats(), RamzipStats::default());
        assert!(global().is_none());
    }
}
