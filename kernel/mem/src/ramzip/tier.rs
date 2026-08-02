//! The `ramzip` tier: compress-out, fault-in, clustering, warm-up, and
//! escalation (`plans/SWAPSWAPSWAP.md` sections 4, 6, 7, 11, 12, 13).
//!
//! [`Ramzip`] owns the RAM-resident pool of sealed compressed pages and
//! every policy gate around it. A page enters through
//! [`Ramzip::compress_out`] only when the pressure handoff is open, the
//! eligibility classifier admits it, the owning task is not thrashing,
//! the band cap and per-task share have room, and the decompression
//! floor stays untouched — every refusal is a typed
//! [`CompressRefusal`], never a panic. A page leaves through
//! [`Ramzip::fault_in`] (move-only: restore, then delete the blob), or
//! through the strictly budgeted [`Ramzip::cluster_after_fault`] and
//! [`Ramzip::warm_step`] optimisations that run only when memory is
//! comfortably free and stop instantly when it is not.
//!
//! The compressed-entry marker is architecture-neutral by
//! construction: an entry is keyed by `(address-space id, page)` in the
//! tier while the page is simply absent from the owning
//! [`AddressSpace`]; no architecture PTE encoding leaks here.
//!
//! # Concurrency
//!
//! The tier is a plain `&mut self` state machine, like
//! [`crate::live::LiveSpace`]: the caller (the kernel's VM glue) holds
//! it behind its own lock and guarantees the owning task is not
//! concurrently running while its pages move — the same exclusivity
//! the live-space mutation path already relies on.

use alloc::collections::BTreeMap;

use tairix_log::Sink;
use tairix_reclaim::{FreeMemorySource, MemoryPressure, PressureBand};

use crate::anon::zero_frame;
use crate::frame::{FrameAllocator, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::pressure::{ramzip_handoff, EscalationStep, RamzipHandoff};
use crate::ptr::slice_within;
use crate::seal::{EntropySource, NonceSequence, SealError, SealKey};
use crate::vmm::{AddressSpace, MapFlags, Page, PageTable, PageTableError};

use super::audit::{log_ramzip_failure, RamzipAuditEvent};
use super::caps::{decompression_floor, RamzipCaps};
use super::eligibility::{eligibility, Ineligible, PageCandidate};
use super::ledger::{bump, RamzipLedger};
use super::store::{open_page, seal_page, OpenFailure, SealFailure, SealedBlob, SEAL_OVERHEAD};
use super::warm::ThrashDetector;

/// Accounted bookkeeping bytes per stored entry: the map key, the
/// entry struct, and headroom for the map node itself. A unit test
/// proves the real entry fits this bound, so metadata accounting can
/// never undercount (`plans/SWAPSWAPSWAP.md` section 10).
pub(crate) const ENTRY_METADATA_BYTES: usize = 160;

/// Acceptance bound for a compressed page: the stored blob plus its
/// metadata must be strictly smaller than the page it replaces, or the
/// entry is refused as incompressible.
pub(crate) const MAX_COMPRESSED_LEN: usize = PAGE_SIZE - SEAL_OVERHEAD - ENTRY_METADATA_BYTES;

/// Pages either side of a faulted page the cluster restore may touch.
const CLUSTER_RADIUS: u64 = 8;

/// Most pages one cluster event may restore (also its byte budget:
/// this many pages).
const CLUSTER_MAX_PAGES: usize = 8;

/// Entries sealed within this many events of the faulted entry count
/// as "compressed near the same time" for clustering.
const CLUSTER_EVENT_WINDOW: u64 = 32;

/// Most pages one warm-up step may restore.
const WARM_BATCH_PAGES: usize = 8;

/// Pages either side of a recent fault the warm-up worker considers.
const WARM_RADIUS: u64 = 64;

/// Recent demand faults remembered as warm-up locality hints.
const RECENT_FAULTS: usize = 8;

/// The mapping-flag bits that must not appear on a compressible page:
/// device (uncached) and DMA-coherent mappings are hardware-visible.
const FORBIDDEN_FLAG_BITS: u8 = MapFlags::NO_CACHE.bits() | MapFlags::DMA_COHERENT.bits();

/// The VM objects one tier operation works against: the owning address
/// space, the kernel direct map, the physical frame allocator, and the
/// audit-log sink security-relevant failures are recorded through.
pub struct VmContext<'a, P: PageTable> {
    /// Stable id of the owning address space (the task's space).
    pub space_id: u64,
    /// The owning task's live address space.
    pub space: &'a mut AddressSpace<P>,
    /// The kernel's direct physical map.
    pub physmap: &'a dyn PhysMap,
    /// The physical frame allocator.
    pub frames: &'a FrameAllocator,
    /// The audit-log sink for authentication/corruption events.
    pub sink: &'a dyn Sink,
}

/// One sealed entry: the owner, the restore flags, when it was sealed
/// (event clock), the figures charged to the ledger, and the sealed
/// blob. `charged_compressed` / `charged_stored` are recorded at
/// compression time and released verbatim: the books never depend on
/// the blob's own (corruptible) length.
struct Entry {
    task: u64,
    flags: MapFlags,
    sealed_at: u64,
    charged_compressed: usize,
    charged_stored: usize,
    blob: SealedBlob,
}

/// A remembered demand fault, used as a locality hint.
#[derive(Copy, Clone)]
struct RecentFault {
    space: u64,
    page_number: u64,
    sealed_at: u64,
}

/// Why [`Ramzip::compress_out`] refused a page. Every variant leaves
/// the page mapped and untouched.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompressRefusal {
    /// The pressure handoff gate is closed at the sampled band (or
    /// cheaper reclaimable cache remains).
    PressurePolicy,
    /// The eligibility classifier refused the page.
    Ineligible(Ineligible),
    /// The owning task is thrashing; compression would churn.
    TaskThrashing,
    /// The page is not mapped in the given space.
    NotMapped,
    /// The page's mapping flags forbid compression (device or
    /// DMA-coherent memory) — defence in depth behind the classifier.
    ForbiddenMapping,
    /// The tier's band capacity cap has no room for another entry.
    CapReached,
    /// The owning task's fair share has no room for another entry.
    TaskShareReached,
    /// Admitting the blob could push free memory to the decompression
    /// floor; compression must never cause reserve exhaustion.
    ReserveProtected,
    /// The page did not compress below the acceptance bound.
    Incompressible,
    /// The per-boot nonce sequence is exhausted.
    NonceExhausted,
    /// The blob allocation failed (deterministic OOM).
    OutOfMemory,
    /// The frame is outside the kernel direct map.
    PhysUnmapped,
    /// Unmapping the page failed; the entry was rolled back.
    PageTable(PageTableError),
    /// The freed frame could not be scrubbed or returned; the entry is
    /// kept (the data is safe in the tier) and the defect is surfaced.
    FrameRelease,
    /// The tier's ledger is poisoned; admission is disabled.
    Poisoned,
    /// An internal seal defect (never expected; surfaced, not hidden).
    Seal,
}

/// Why a restore failed. `NoEntry` and `AlreadyMapped` leave the tier
/// unchanged; `Authentication` and `Corrupt` discard the entry (its
/// bytes are unrecoverable), audit-log the loss, and return no
/// plaintext.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FaultError {
    /// No compressed entry exists for this page.
    NoEntry,
    /// The page is already mapped: nothing to restore (caller defect,
    /// surfaced rather than double-mapped).
    AlreadyMapped,
    /// No frame could be allocated to restore into.
    OutOfMemory,
    /// The restore frame is outside the kernel direct map.
    PhysUnmapped,
    /// Re-mapping the restored page failed; the entry is retained so
    /// the data is not lost.
    PageTable(PageTableError),
    /// The entry failed authentication: tampered, replayed, or
    /// damaged. Fail closed — the entry is gone and no plaintext was
    /// produced.
    Authentication,
    /// The entry failed metadata validation or decompression after
    /// authenticating. Fail closed as above.
    Corrupt,
}

/// The outcome of one [`Ramzip::warm_step`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WarmOutcome {
    /// This many pages were restored (may be fewer than the batch
    /// budget if candidates ran out).
    Restored(usize),
    /// No candidate had warm-up evidence; everything else stays
    /// compressed by design.
    NothingToDo,
    /// A pressure or reserve gate stopped the step immediately.
    Stopped,
}

/// The deterministic next step when the tier cannot help
/// (`plans/SWAPSWAPSWAP.md` sections 6 and 13): reclaimable cache is
/// always first; with caches drained, a refusal at moderate pressure
/// or deeper escalates to the VM policy (lower-tier swap where
/// approved, freeze, kill, or clean OOM); shallow bands hold.
#[must_use]
pub const fn escalate_refusal(band: PressureBand, reclaimable_bytes: usize) -> EscalationStep {
    if reclaimable_bytes > 0 {
        return EscalationStep::ReclaimCaches;
    }
    match band {
        PressureBand::Normal | PressureBand::Mild => EscalationStep::Hold,
        PressureBand::Moderate | PressureBand::Severe | PressureBand::Critical => {
            EscalationStep::VmPolicy
        }
    }
}

/// The encrypted compressed anonymous-memory tier. See the module
/// docs for the lifecycle and the concurrency contract.
pub struct Ramzip {
    caps: RamzipCaps,
    key: SealKey,
    nonces: NonceSequence,
    entries: BTreeMap<(u64, u64), Entry>,
    ledger: RamzipLedger,
    thrash: ThrashDetector,
    recent_faults: [Option<RecentFault>; RECENT_FAULTS],
    next_fault_slot: usize,
    event_clock: u64,
    poisoned: bool,
}

impl Ramzip {
    /// Build an empty tier: caps derived from discovered physical RAM,
    /// a fresh per-boot key, and no payload — near-zero idle cost.
    ///
    /// # Errors
    ///
    /// [`SealError::Entropy`] if the key or nonce salt cannot be
    /// drawn; no tier is constructed in that case.
    pub fn new(caps: RamzipCaps, entropy: &mut dyn EntropySource) -> Result<Self, SealError> {
        let key = SealKey::generate(entropy)?;
        let nonces = NonceSequence::new(entropy)?;
        Ok(Self {
            caps,
            key,
            nonces,
            entries: BTreeMap::new(),
            ledger: RamzipLedger::new(),
            thrash: ThrashDetector::new(),
            recent_faults: [None; RECENT_FAULTS],
            next_fault_slot: 0,
            event_clock: 0,
            poisoned: false,
        })
    }

    /// The tier's capacity policy.
    #[must_use]
    pub const fn caps(&self) -> &RamzipCaps {
        &self.caps
    }

    /// The tier's ledger: footprint, per-task usage, and counters.
    #[must_use]
    pub const fn ledger(&self) -> &RamzipLedger {
        &self.ledger
    }

    /// Whether a compressed entry exists for `page` in `space_id`.
    #[must_use]
    pub fn has_entry(&self, space_id: u64, page: Page) -> bool {
        self.entries.contains_key(&(space_id, page.number()))
    }

    /// Advance the event clock by one tick.
    fn tick(&mut self) -> u64 {
        self.event_clock = self.event_clock.saturating_add(1);
        self.event_clock
    }
}

/// The associated data binding an entry to its identity: space id,
/// page number, and mapping flags. A blob replayed against any other
/// identity fails authentication.
fn entry_aad(space_id: u64, page_number: u64, flags: MapFlags) -> [u8; 17] {
    let mut aad = [0u8; 17];
    aad[..8].copy_from_slice(&space_id.to_le_bytes());
    aad[8..16].copy_from_slice(&page_number.to_le_bytes());
    aad[16] = flags.bits();
    aad
}

/// Free bytes currently available from the context's frame allocator.
fn free_bytes<P: PageTable>(ctx: &VmContext<'_, P>) -> usize {
    FreeMemorySource::free_bytes(ctx.frames)
}

mod ops;

#[cfg(all(test, not(loom)))]
mod tests;
