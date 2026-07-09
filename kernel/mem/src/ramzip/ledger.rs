//! Checked accounting for the compressed anonymous-memory tier
//! (`plans/SWAPSWAPSWAP.md` section 10).
//!
//! Every byte the tier holds is attributable: the ledger tracks the
//! global footprint (stored ciphertext, bookkeeping metadata, the
//! logical page bytes represented, and the pre-encryption compressed
//! bytes) plus a per-task breakdown, so one process can never push
//! unlimited cold memory into the tier and externalise the cost.
//! Arithmetic is checked and fail-closed: an overflow or underflow is a
//! typed [`LedgerError`], never a wrap or a saturation that quietly
//! corrupts the books. A ledger whose books stop balancing poisons the
//! tier (admission stops; restores continue) — mirroring the
//! reclaimable-cache accounting discipline.
//!
//! The event counters ([`RamzipCounters`]) are monotonic diagnostics —
//! attempts, acceptances, typed rejections, fault-ins, authentication
//! and decode failures, warm-up and cluster activity, and thrash
//! detections — exposed only through these internal figures and the
//! audit log, never a public ABI.

use alloc::collections::BTreeMap;

/// Why a ledger operation was refused. Both variants are defects in
/// the caller's books (a charge that cannot fit, or a release of bytes
/// never charged); the tier reacts by poisoning admission.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LedgerError {
    /// A charge would overflow a byte or entry total.
    Overflow,
    /// A release exceeds the recorded total (global or per-task), or
    /// names a task with no recorded usage.
    Underflow,
}

/// One task's recorded contribution to the tier.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskUsage {
    /// Compressed entries held for the task.
    pub entries: u64,
    /// Logical (uncompressed) page bytes the entries represent.
    pub logical_bytes: usize,
    /// Stored bytes charged to the task: ciphertext plus per-entry
    /// bookkeeping metadata.
    pub stored_bytes: usize,
}

/// Monotonic event counters for the tier's observability
/// (`plans/SWAPSWAPSWAP.md` sections 10 and 16). Saturating: a counter
/// pinned at `u64::MAX` is obviously saturated, never wrapped to a
/// misleading small value.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct RamzipCounters {
    /// Compression attempts offered to the tier.
    pub attempts: u64,
    /// Attempts accepted and stored.
    pub accepted: u64,
    /// Refused by the pressure policy (handoff gate closed).
    pub rejected_policy: u64,
    /// Refused by the eligibility classifier.
    pub rejected_ineligible: u64,
    /// Refused because compression did not win.
    pub rejected_incompressible: u64,
    /// Refused by the band capacity cap.
    pub rejected_cap: u64,
    /// Refused by the decompression-floor reserve check.
    pub rejected_reserve: u64,
    /// Refused by the per-task fair-share bound.
    pub rejected_task_share: u64,
    /// Refused because the owning task is thrashing.
    pub rejected_thrash: u64,
    /// Compressed-entry pages restored on demand.
    pub fault_ins: u64,
    /// Entries lost to authentication failure (fail closed, audited).
    pub auth_failures: u64,
    /// Entries lost to metadata or decompression corruption (fail
    /// closed, audited).
    pub decode_failures: u64,
    /// Warm-up steps that considered candidates.
    pub warm_attempts: u64,
    /// Pages restored by the warm-up worker.
    pub warm_restored: u64,
    /// Warm-up steps stopped by a pressure or reserve gate.
    pub warm_stopped: u64,
    /// Pages restored by post-fault clustering.
    pub cluster_restored: u64,
    /// Tasks that crossed the thrash threshold.
    pub thrash_detected: u64,
}

/// The tier's checked ledger: global totals, per-task usage, counters.
#[derive(Debug, Default)]
pub struct RamzipLedger {
    entries: u64,
    logical_bytes: usize,
    compressed_bytes: usize,
    stored_bytes: usize,
    metadata_bytes: usize,
    per_task: BTreeMap<u64, TaskUsage>,
    counters: RamzipCounters,
}

impl RamzipLedger {
    /// An empty ledger: the tier's near-zero idle cost is exactly this
    /// struct and no payload.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compressed entries currently held.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// Logical (uncompressed) page bytes the tier represents.
    #[must_use]
    pub fn logical_bytes(&self) -> usize {
        self.logical_bytes
    }

    /// Compressed bytes before encryption overhead.
    #[must_use]
    pub fn compressed_bytes(&self) -> usize {
        self.compressed_bytes
    }

    /// Stored ciphertext bytes (after encryption and authentication
    /// overhead), excluding bookkeeping metadata.
    #[must_use]
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }

    /// Bookkeeping metadata bytes.
    #[must_use]
    pub fn metadata_bytes(&self) -> usize {
        self.metadata_bytes
    }

    /// The tier's whole accounted footprint: stored ciphertext plus
    /// metadata. This is the figure the caps bound.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.stored_bytes.saturating_add(self.metadata_bytes)
    }

    /// One task's recorded usage (zero if the task holds nothing).
    #[must_use]
    pub fn task_usage(&self, task: u64) -> TaskUsage {
        self.per_task.get(&task).copied().unwrap_or_default()
    }

    /// The live counters.
    #[must_use]
    pub fn counters(&self) -> &RamzipCounters {
        &self.counters
    }

    /// Mutable access to the counters for the owning tier.
    pub(crate) fn counters_mut(&mut self) -> &mut RamzipCounters {
        &mut self.counters
    }

    /// Charge one accepted entry to `task`.
    ///
    /// `logical` is the page size represented, `compressed` the
    /// pre-encryption compressed length, `stored` the ciphertext
    /// length, and `metadata` the per-entry bookkeeping bound.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Overflow`] if any total cannot fit; nothing is
    /// charged in that case (all-or-nothing).
    pub fn charge(
        &mut self,
        task: u64,
        logical: usize,
        compressed: usize,
        stored: usize,
        metadata: usize,
    ) -> Result<(), LedgerError> {
        let entries = self.entries.checked_add(1).ok_or(LedgerError::Overflow)?;
        let logical_total = self
            .logical_bytes
            .checked_add(logical)
            .ok_or(LedgerError::Overflow)?;
        let compressed_total = self
            .compressed_bytes
            .checked_add(compressed)
            .ok_or(LedgerError::Overflow)?;
        let stored_total = self
            .stored_bytes
            .checked_add(stored)
            .ok_or(LedgerError::Overflow)?;
        let metadata_total = self
            .metadata_bytes
            .checked_add(metadata)
            .ok_or(LedgerError::Overflow)?;
        let usage = self.per_task.entry(task).or_default();
        let task_entries = usage.entries.checked_add(1).ok_or(LedgerError::Overflow)?;
        let task_logical = usage
            .logical_bytes
            .checked_add(logical)
            .ok_or(LedgerError::Overflow)?;
        let task_stored = usage
            .stored_bytes
            .checked_add(stored.saturating_add(metadata))
            .ok_or(LedgerError::Overflow)?;

        usage.entries = task_entries;
        usage.logical_bytes = task_logical;
        usage.stored_bytes = task_stored;
        self.entries = entries;
        self.logical_bytes = logical_total;
        self.compressed_bytes = compressed_total;
        self.stored_bytes = stored_total;
        self.metadata_bytes = metadata_total;
        Ok(())
    }

    /// Release one entry previously charged to `task` with exactly the
    /// same figures.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Underflow`] if any total (global or per-task)
    /// does not cover the release, or the task holds nothing; nothing
    /// is released in that case (all-or-nothing).
    pub fn release(
        &mut self,
        task: u64,
        logical: usize,
        compressed: usize,
        stored: usize,
        metadata: usize,
    ) -> Result<(), LedgerError> {
        let entries = self.entries.checked_sub(1).ok_or(LedgerError::Underflow)?;
        let logical_total = self
            .logical_bytes
            .checked_sub(logical)
            .ok_or(LedgerError::Underflow)?;
        let compressed_total = self
            .compressed_bytes
            .checked_sub(compressed)
            .ok_or(LedgerError::Underflow)?;
        let stored_total = self
            .stored_bytes
            .checked_sub(stored)
            .ok_or(LedgerError::Underflow)?;
        let metadata_total = self
            .metadata_bytes
            .checked_sub(metadata)
            .ok_or(LedgerError::Underflow)?;
        let Some(usage) = self.per_task.get_mut(&task) else {
            return Err(LedgerError::Underflow);
        };
        let task_entries = usage.entries.checked_sub(1).ok_or(LedgerError::Underflow)?;
        let task_logical = usage
            .logical_bytes
            .checked_sub(logical)
            .ok_or(LedgerError::Underflow)?;
        let task_stored = usage
            .stored_bytes
            .checked_sub(stored.saturating_add(metadata))
            .ok_or(LedgerError::Underflow)?;

        if task_entries == 0 {
            // No per-task residue is left behind: an idle tier accounts
            // for exactly nothing (no metadata leak on free).
            self.per_task.remove(&task);
        } else {
            usage.entries = task_entries;
            usage.logical_bytes = task_logical;
            usage.stored_bytes = task_stored;
        }
        self.entries = entries;
        self.logical_bytes = logical_total;
        self.compressed_bytes = compressed_total;
        self.stored_bytes = stored_total;
        self.metadata_bytes = metadata_total;
        Ok(())
    }
}

/// Saturating increment for the monotonic counters.
pub(crate) fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn an_empty_ledger_accounts_for_nothing() {
        let ledger = RamzipLedger::new();
        assert_eq!(ledger.entries(), 0);
        assert_eq!(ledger.footprint(), 0);
        assert_eq!(ledger.task_usage(7), TaskUsage::default());
    }

    #[test]
    fn charge_and_release_balance_to_zero() {
        let mut ledger = RamzipLedger::new();
        ledger.charge(7, 4096, 1000, 1016, 128).expect("charge");
        assert_eq!(ledger.entries(), 1);
        assert_eq!(ledger.logical_bytes(), 4096);
        assert_eq!(ledger.compressed_bytes(), 1000);
        assert_eq!(ledger.stored_bytes(), 1016);
        assert_eq!(ledger.metadata_bytes(), 128);
        assert_eq!(ledger.footprint(), 1016 + 128);
        assert_eq!(ledger.task_usage(7).entries, 1);
        assert_eq!(ledger.task_usage(7).stored_bytes, 1016 + 128);

        ledger.release(7, 4096, 1000, 1016, 128).expect("release");
        assert_eq!(ledger.entries(), 0);
        assert_eq!(ledger.footprint(), 0);
        // No per-task residue survives the last release.
        assert_eq!(ledger.task_usage(7), TaskUsage::default());
    }

    #[test]
    fn per_task_contributions_are_separated() {
        let mut ledger = RamzipLedger::new();
        ledger.charge(1, 4096, 500, 516, 128).expect("task 1");
        ledger.charge(2, 4096, 700, 716, 128).expect("task 2");
        ledger.charge(2, 4096, 800, 816, 128).expect("task 2 again");
        assert_eq!(ledger.task_usage(1).entries, 1);
        assert_eq!(ledger.task_usage(2).entries, 2);
        assert_eq!(ledger.task_usage(2).stored_bytes, 716 + 816 + 2 * 128);
        assert_eq!(ledger.entries(), 3);
    }

    #[test]
    fn release_of_unknown_task_underflows_and_changes_nothing() {
        let mut ledger = RamzipLedger::new();
        ledger.charge(1, 4096, 500, 516, 128).expect("charge");
        assert_eq!(
            ledger.release(9, 4096, 500, 516, 128),
            Err(LedgerError::Underflow)
        );
        assert_eq!(ledger.entries(), 1);
        assert_eq!(ledger.task_usage(1).entries, 1);
    }

    #[test]
    fn release_larger_than_charged_underflows() {
        let mut ledger = RamzipLedger::new();
        ledger.charge(1, 4096, 500, 516, 128).expect("charge");
        assert_eq!(
            ledger.release(1, 8192, 500, 516, 128),
            Err(LedgerError::Underflow)
        );
        // Nothing was mutated by the failed release.
        assert_eq!(ledger.logical_bytes(), 4096);
        assert_eq!(ledger.entries(), 1);
    }

    #[test]
    fn charge_overflow_is_refused_all_or_nothing() {
        let mut ledger = RamzipLedger::new();
        ledger
            .charge(1, usize::MAX, usize::MAX, usize::MAX, 0)
            .expect("first charge fits");
        assert_eq!(ledger.charge(1, 1, 0, 0, 0), Err(LedgerError::Overflow));
        // The failed charge left every total untouched.
        assert_eq!(ledger.logical_bytes(), usize::MAX);
        assert_eq!(ledger.entries(), 1);
        assert_eq!(ledger.task_usage(1).entries, 1);
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let mut counter = u64::MAX - 1;
        bump(&mut counter);
        assert_eq!(counter, u64::MAX);
        bump(&mut counter);
        assert_eq!(counter, u64::MAX);
    }
}
