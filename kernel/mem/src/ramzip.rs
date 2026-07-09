//! `ramzip` — the encrypted, compressed, RAM-resident tier for cold
//! anonymous pages (`plans/SWAPSWAPSWAP.md`).
//!
//! Under memory pressure, after cheaper reclaim has run
//! ([`crate::pressure::ramzip_handoff`]), cold anonymous user pages are
//! compressed, sealed with authenticated encryption under an ephemeral
//! per-boot key, and parked in a bounded RAM pool; their frames return
//! to the allocator. A parked page is restored on demand — decrypted,
//! authenticated, decompressed, and remapped, move-only — and small,
//! strictly budgeted cluster/warm-up restores may bring neighbours back
//! early when memory is comfortably free. This is a compressed memory
//! tier, not magic extra RAM and not persistent swap; the optional
//! block-swap layer ([`crate::swap`]) is independent and shares neither
//! key nor metadata format with it.
//!
//! The module split follows the plan's stages:
//!
//! - [`eligibility`](self::eligibility()) / [`PageCandidate`] — which
//!   pages may ever be considered (fail-closed classifier).
//! - [`RamzipCaps`] — the derived min/soft/hard capacity policy and the
//!   decompression floor.
//! - [`RamzipLedger`] / [`RamzipCounters`] — checked, per-task
//!   accounting and internal diagnostics.
//! - [`Ramzip`] — the tier itself: `compress_out`, `fault_in`,
//!   `cluster_after_fault`, `warm_step`, and the deterministic
//!   [`escalate_refusal`] policy for when the tier cannot help.
//!
//! # Enablement
//!
//! The tier is complete as the arch-neutral VM mechanism (host-proven
//! over the same [`AddressSpace`](crate::vmm::AddressSpace) /
//! [`PhysMap`](crate::phys::PhysMap) surfaces production uses).
//! Switching it on for arbitrary *running* tasks additionally needs a
//! restartable user page-fault path in the architecture ports — a task
//! touching a compressed page must trap, restore, and resume — which no
//! port provides yet; that prerequisite is staged in `PLAN.md`, and
//! nothing here may be weakened to work around its absence.

mod audit;
mod caps;
mod eligibility;
mod ledger;
mod store;
mod tier;
mod warm;

pub use audit::{log_ramzip_failure, RamzipAuditEvent};
pub use caps::{decompression_floor, RamzipCaps};
pub use eligibility::{eligibility, Ineligible, PageCandidate, PageKind};
pub use ledger::{LedgerError, RamzipCounters, RamzipLedger, TaskUsage};
pub use tier::{escalate_refusal, CompressRefusal, FaultError, Ramzip, VmContext, WarmOutcome};
