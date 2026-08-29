//! The filesystem's dirty block set, ordered around the commit barrier.
//!
//! A copy-on-write filesystem rewrites the same metadata block many times
//! inside one transaction — every stored data block re-writes the extent leaf
//! that maps it, the spine above that leaf, and the transaction root — and only
//! the last version of each survives. Holding them here, keyed by physical
//! address, means the device is issued one write per block per transaction
//! rather than one per rewrite, and it gives the commit a point at which every
//! block the new root names has been sent and can be barriered ahead of the
//! superblock slot that publishes it.
//!
//! Holding them also lets the drain hand the device *runs* rather than blocks:
//! a transaction's data blocks are allocated consecutively and its mirrored
//! metadata blocks are adjacent pairs, so gathering the set's ascending order
//! into contiguous runs collapses a command and a completion wait per block
//! into one per run.
//!
//! A staged block is pinned memory, not reclaimable cache: it exists nowhere
//! else, so it can only be written, never dropped. The set performs no I/O —
//! the owning filesystem does, exactly as [`crate::pagecache`] — so it stays
//! free of the block device and unit-testable on the host.
//!
//! A transaction spans operations ([`CommitScheduler`]), so a failed operation
//! must undo itself without disturbing the operations that already joined the
//! transaction and were reported successful. The set therefore keeps a
//! **savepoint**: each block the running operation changes is remembered as it
//! stood before, once, and restored if that operation is undone.
//!
//! Because a staged block is pinned, the set must be *bounded*
//! ([`WritebackBound`]): a writer that outruns the device is made to wait for
//! real I/O — the transaction is published and the set emptied — rather than
//! being allowed to grow. The ceiling comes from the RAM the host discovered
//! and falls as memory tightens, so the response to pressure is always
//! *publish sooner*, never *hold more* and never *drop*.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::filesystem::WritebackHost;
use tairix_abi::driver::DriverHandle;
use tairix_abi::DriverError;
use tairix_reclaim::{
    CacheBudget, GrowthAllowance, PinnedAccounting, PressureBand, PressureGauge, MAP_ENTRY_OVERHEAD,
};
use zeroize::Zeroize;

use crate::{RunWindow, RUN_BYTES};

/// How long a transaction may stay open before the next operation publishes
/// it, by the class of device the volume sits on and how tight memory is.
///
/// The window buys device commands with recency: operations that arrive inside
/// it fold into one commit — one transaction root, one superblock slot, one
/// barrier, and one write of each metadata block they all rewrite — and a crash
/// costs the operations it folded. So the window is widest where a command is
/// dearest. Removable media pay the most per command and gain the most,
/// a rotational disk gets its metadata seeks back as sequential runs, and a
/// device already cheap per command gains little, so it keeps the smallest
/// exposure. The values are the one policy over the class; nothing tunes them
/// per volume.
#[must_use]
pub(crate) const fn writeback_window_ns(class: BlkDeviceClass, band: PressureBand) -> u64 {
    let base = match class {
        BlkDeviceClass::Removable => 30_000_000_000,
        BlkDeviceClass::Rotational => 15_000_000_000,
        BlkDeviceClass::SolidState | BlkDeviceClass::Virtual => 5_000_000_000,
    };
    // Tightening memory halves the window per band, so a volume that falls
    // quiet gives its pinned bytes back sooner. Nothing widens it: pressure
    // only ever buys recency back, never spends more of it.
    base >> band.depth()
}

/// The smallest ceiling worth having, in bytes: one coalesced device
/// transfer.
///
/// This is a floor, not a capacity, so it is a fixed figure rather than a
/// fraction of the machine — it says how little is too little. Below one
/// transfer window the drain can never form a full run, so the set stops
/// buying the device commands it exists to buy while still pinning memory.
/// A transaction is guaranteed to complete regardless (the write path always
/// stages at least one record before the bound can cut it short), so this
/// bounds the cache's *usefulness*, not its correctness.
pub(crate) const WRITEBACK_FLOOR_BYTES: usize = RUN_BYTES;

/// The RAM-derived byte ceiling, the pressure gauge, and the pinned ledger a
/// volume's dirty set is held to.
///
/// A staged block is pinned memory: it exists nowhere else, so it can only be
/// written out, never dropped. That makes the set the opposite of a
/// reclaimable cache — nothing may shrink it behind the driver's back — so it
/// is deliberately *not* admitted through the reclaim classification gate,
/// whose contract is droppability. What bounds it instead is a byte ceiling
/// derived from the RAM the host discovered, the machine-wide reserve floor
/// every consumer obeys, and the pressure band, which lowers the ceiling
/// toward [`WRITEBACK_FLOOR_BYTES`] and no further.
///
/// The ceiling is the **machine's**, not the volume's, and every mounted
/// volume takes a share of it: a figure derived per volume would let a
/// machine's volumes pin a multiple of what the machine has, which is the
/// one thing a bound over unreclaimable memory must not do.
///
/// A handle with no bound is not memory-governed, exactly as a handle with no
/// write-back host has no window: the host installs one on every writable
/// mount it opens, so only host tools and unit tests run without one.
pub(crate) struct WritebackBound {
    budget: CacheBudget,
    gauge: &'static dyn PressureGauge,
    pinned: Arc<PinnedAccounting>,
}

impl WritebackBound {
    /// The bound a volume gets from `budget` (derived from discovered RAM),
    /// the system pressure `gauge`, and the `pinned` ledger the host
    /// publishes its footprint through.
    ///
    /// # Errors
    ///
    /// [`DriverError::NoSpace`] when the machine's whole derived ceiling
    /// cannot hold even [`WRITEBACK_FLOOR_BYTES`]. The mount is refused
    /// rather than accepted and left to wedge later: a machine that cannot
    /// spare one device transfer has nothing to gain from mounting a volume
    /// that defers writes.
    pub(crate) fn new(
        budget: CacheBudget,
        gauge: &'static dyn PressureGauge,
        pinned: Arc<PinnedAccounting>,
    ) -> Result<Self, DriverError> {
        if budget.hard() < WRITEBACK_FLOOR_BYTES {
            return Err(DriverError::NoSpace);
        }
        Ok(Self {
            budget,
            gauge,
            pinned,
        })
    }

    /// One gauge reading, folded once, for the operation about to run.
    ///
    /// Read per operation rather than per staged block: in the kernel the
    /// reading is the physical frame allocator, so asking per block would
    /// take the global frame-allocator lock hundreds of times to answer a
    /// question that cannot change inside one operation.
    pub(crate) fn reading(&self, held_bytes: usize) -> WritebackReading {
        let allowance = self.gauge.growth_allowance();
        WritebackReading {
            band: allowance.band(),
            ceiling: self.ceiling_bytes(allowance, held_bytes),
        }
    }

    /// The ceiling for one operation: this volume's share of the machine-wide
    /// RAM-derived budget at the band last read, capped by what the volumes
    /// already holding leave and by what the machine-wide reserve leaves, and
    /// never below [`WRITEBACK_FLOOR_BYTES`].
    ///
    /// Three caps, because a machine's volumes have to share one total rather
    /// than take a slab each. An equal share of the ceiling bounds the total
    /// however many volumes write at once and whatever each of them holds —
    /// including the run bookkeeping a delete is almost entirely made of. What
    /// the others actually hold bounds it more tightly than an equal share
    /// wherever they are holding less than one, so a volume writing beside
    /// quiet ones is not throttled for company it does not have. The reserve
    /// is the machine's own floor, which no cache may draw into.
    ///
    /// The floor wins over all three: a volume must be able to complete a
    /// transaction, so a machine with more volumes than its ceiling divides
    /// into gives each one transfer window rather than refusing them all.
    fn ceiling_bytes(&self, allowance: GrowthAllowance, held_bytes: usize) -> usize {
        let banded = self.budget.hard() >> allowance.band().depth();
        let share = banded / self.pinned.drawing_pools();
        let left = banded.saturating_sub(self.pinned.other_bytes());
        let reserved = held_bytes.saturating_add(allowance.remaining_bytes());
        share.min(left).min(reserved).max(WRITEBACK_FLOOR_BYTES)
    }

    /// Publish what the volume's transaction pins, so the operator's ledger
    /// row and the machine-wide share its siblings decide against both carry
    /// it.
    ///
    /// Derived from what the transaction actually holds, so a missed call can
    /// leave the figure stale but never wrong.
    pub(crate) fn publish(&self, bytes: usize, entries: u64) {
        self.pinned.set(bytes, entries);
    }

    /// Note one pass that wrote the set out and returned its bytes.
    pub(crate) fn note_released(&self) {
        self.pinned.note_released();
    }

    /// Note one admission the bound cut short, so the caller had to publish
    /// before it could stage more.
    pub(crate) fn note_refusal(&self) {
        self.pinned.note_refusal();
    }
}

impl Drop for WritebackBound {
    /// The mount is going away, so it draws on the machine-wide share no
    /// longer. The ledger row outlives it — the host keeps a torn-down
    /// volume's final figures readable — so the footprint is zeroed here
    /// rather than left claiming an unmounted volume's bytes against the
    /// volumes that are still writing.
    fn drop(&mut self) {
        self.publish(0, 0);
    }
}

/// What one gauge reading told the operation about to run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct WritebackReading {
    /// The band the reading folded to, which sets the dirty-age window.
    pub(crate) band: PressureBand,
    /// Bytes the dirty set may hold before the transaction is published.
    pub(crate) ceiling: usize,
}

/// When the open transaction must be published.
///
/// A transaction stays open and the next operation joins it, so the commits
/// — and the barriers, transaction roots, and superblock slots that go with
/// them — cost one per burst of operations rather than one per operation. It
/// closes on the first of the dirty-age window expiring, an operation that
/// needs a barrier for its own correctness, an explicit sync, or the volume
/// being handed on.
///
/// Between operations nothing here runs, so the window is enforced by the
/// host's write-back timer: each transaction reports its deadline as it opens
/// and reports its absence as it closes, and the host publishes a volume that
/// falls quiet by calling `flush`. A handle with no host has neither a clock
/// to age against nor anything to wake it, so every operation publishes — a
/// host that cannot say how much time has passed, or cannot be told when to
/// come back, does not get to defer durability.
pub(crate) struct CommitScheduler {
    /// The host's timer and the handle this mount is registered under,
    /// installed when the volume is registered.
    host: Option<(DriverHandle, &'static dyn WritebackHost)>,
    /// The class of device the volume sits on, which sets the base window.
    class: BlkDeviceClass,
    /// The memory-pressure band the last operation read, which shortens the
    /// window. Normal until a bound with a gauge is installed, so a handle
    /// that is not memory-governed keeps its device class's full window.
    band: PressureBand,
    /// Whether a transaction is in flight. Separate from [`Self::opened_ns`],
    /// which is absent when a transaction is open but the host's clock
    /// declined to read.
    open: bool,
    /// Monotonic reading at which the open transaction started, when the
    /// host's clock answered.
    opened_ns: Option<u64>,
    /// Operations already reported successful into the open transaction. A
    /// commit failure loses them, which is what makes the failure more than
    /// the calling operation's own.
    acknowledged: u32,
}

impl CommitScheduler {
    /// A scheduler for a volume on a device of `class`, with no host yet.
    pub(crate) const fn new(class: BlkDeviceClass) -> Self {
        Self {
            host: None,
            class,
            band: PressureBand::Normal,
            open: false,
            opened_ns: None,
            acknowledged: 0,
        }
    }

    /// The window the open transaction is aged against, under the band last
    /// read.
    const fn window_ns(&self) -> u64 {
        writeback_window_ns(self.class, self.band)
    }

    /// Note the memory-pressure band the running operation read.
    ///
    /// A deepening band shortens the window, which can bring an open
    /// transaction's deadline forward, so the host is told the new instant:
    /// pressure must not have to wait out a window measured when memory was
    /// plentiful.
    pub(crate) fn set_band(&mut self, band: PressureBand) {
        if self.band == band {
            return;
        }
        self.band = band;
        self.report();
    }

    /// Install the host's write-back timer for the mount registered as
    /// `volume`, and report the current state to it.
    ///
    /// A transaction already open when the host arrives is adopted as
    /// starting now: it has been open for an unmeasured time, so dating it
    /// from here bounds it by one window rather than leaving it unreported.
    pub(crate) fn set_host(&mut self, volume: DriverHandle, host: &'static dyn WritebackHost) {
        self.host = Some((volume, host));
        if self.open && self.opened_ns.is_none() {
            self.opened_ns = host.now_ns();
        }
        self.report();
    }

    /// Note that a transaction is now open, if one was not already.
    pub(crate) fn opened(&mut self) {
        if self.open {
            return;
        }
        self.open = true;
        self.opened_ns = self.now_ns();
        self.report();
    }

    /// Whether a transaction is in flight.
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    /// Note one more operation reported successful into the open transaction.
    pub(crate) const fn joined(&mut self) {
        self.acknowledged = self.acknowledged.saturating_add(1);
    }

    /// Operations reported successful whose work the open transaction still
    /// holds.
    pub(crate) const fn acknowledged(&self) -> u32 {
        self.acknowledged
    }

    /// The transaction has been published or abandoned.
    pub(crate) fn closed(&mut self) {
        self.open = false;
        self.opened_ns = None;
        self.acknowledged = 0;
        self.report();
    }

    /// Whether the open transaction has reached its window and the operation
    /// that just finished must publish it.
    pub(crate) fn expired(&self) -> bool {
        let Some(opened_ns) = self.opened_ns else {
            return true;
        };
        match self.now_ns() {
            Some(now) => now.saturating_sub(opened_ns) >= self.window_ns(),
            // The clock stopped answering mid-transaction: publish rather
            // than hold work against an age nothing can measure.
            None => true,
        }
    }

    /// The absolute monotonic instant the open transaction is due at, or
    /// `None` when none is open or its age cannot be measured.
    fn deadline_ns(&self) -> Option<u64> {
        self.opened_ns.map(|at| at.saturating_add(self.window_ns()))
    }

    fn now_ns(&self) -> Option<u64> {
        self.host.and_then(|(_, host)| host.now_ns())
    }

    /// Tell the host when this volume next needs publishing.
    fn report(&self) {
        if let Some((volume, host)) = self.host {
            host.writeback_due(volume, self.deadline_ns());
        }
    }
}

/// The host half of the write-back timer, for tests: a settable monotonic
/// clock and a record of the last deadline the driver reported.
///
/// One definition for the whole crate — the scheduler's own tests and the
/// mounted-volume tests both drive a transaction across its window through
/// this, by moving the clock rather than waiting for one.
#[cfg(test)]
pub(crate) struct TestWritebackHost {
    now_ns: core::sync::atomic::AtomicU64,
    /// The last reported deadline, with `u64::MAX` for "nothing open".
    due_ns: core::sync::atomic::AtomicU64,
    reports: core::sync::atomic::AtomicU32,
}

#[cfg(test)]
impl TestWritebackHost {
    /// A leaked host reading `now_ns`, as the driver needs it: `&'static`.
    pub(crate) fn leaked(now_ns: u64) -> &'static Self {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Self {
            now_ns: core::sync::atomic::AtomicU64::new(now_ns),
            due_ns: core::sync::atomic::AtomicU64::new(u64::MAX),
            reports: core::sync::atomic::AtomicU32::new(0),
        }))
    }

    /// Move the clock to `now_ns`.
    pub(crate) fn set_now(&self, now_ns: u64) {
        self.now_ns
            .store(now_ns, core::sync::atomic::Ordering::Release);
    }

    /// The deadline the driver last reported, or `None` for "nothing open".
    pub(crate) fn reported(&self) -> Option<u64> {
        match self.due_ns.load(core::sync::atomic::Ordering::Acquire) {
            u64::MAX => None,
            deadline => Some(deadline),
        }
    }

    /// How many times the driver has reported.
    pub(crate) fn reports(&self) -> u32 {
        self.reports.load(core::sync::atomic::Ordering::Acquire)
    }

    /// The handle a test registers its one volume under.
    pub(crate) fn volume() -> DriverHandle {
        DriverHandle::from_raw(1).expect("a non-zero test handle")
    }
}

#[cfg(test)]
impl WritebackHost for TestWritebackHost {
    fn now_ns(&self) -> Option<u64> {
        Some(self.now_ns.load(core::sync::atomic::Ordering::Acquire))
    }

    fn writeback_due(&self, _volume: DriverHandle, deadline_ns: Option<u64>) {
        self.due_ns.store(
            deadline_ns.unwrap_or(u64::MAX),
            core::sync::atomic::Ordering::Release,
        );
        self.reports
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    }
}

/// One block's staged state before the running operation first changed it.
struct Prior {
    phase: WritePhase,
    bytes: Vec<u8>,
}

/// Which side of the commit barrier may issue a staged block.
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum WritePhase {
    /// Authoritative blocks that must be durable before publication.
    BeforeBarrier,
    /// Rebuildable blocks protected by a durable invalidation marker.
    AfterBarrier,
}

impl WritePhase {
    const ALL: [Self; 2] = [Self::BeforeBarrier, Self::AfterBarrier];

    const fn other(self) -> Self {
        match self {
            Self::BeforeBarrier => Self::AfterBarrier,
            Self::AfterBarrier => Self::BeforeBarrier,
        }
    }
}

/// Sealed blocks waiting for their ordered device drain.
pub(crate) struct DirtySet {
    entries: BTreeMap<(WritePhase, u64), Vec<u8>>,
    /// Each block the running operation has changed, as it stood before it
    /// did — `None` where the set held nothing. Absent between operations.
    savepoint: Option<BTreeMap<u64, Option<Prior>>>,
    block_size: usize,
    /// Block buffers the savepoint holds. Derived counts cannot drift from
    /// what is held, so the staged half is [`BTreeMap::len`] and only the
    /// savepoint — whose buffers are pinned for the running operation
    /// exactly as staged ones are — needs a counter.
    saved_priors: usize,
}

impl DirtySet {
    /// An empty set over blocks of `block_size`. Allocates nothing until the
    /// first block is staged.
    pub(crate) const fn new(block_size: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            savepoint: None,
            block_size,
            saved_priors: 0,
        }
    }

    /// Block buffers the set pins: the staged ones plus the savepoint's.
    pub(crate) fn held(&self) -> usize {
        self.entries.len().saturating_add(self.saved_priors)
    }

    /// Bytes the set pins: each held block buffer plus the bookkeeping the
    /// map node and its key cost on top of it.
    pub(crate) fn pinned_bytes(&self) -> usize {
        self.held()
            .saturating_mul(self.block_size.saturating_add(MAP_ENTRY_OVERHEAD))
    }

    /// Start recording an operation's changes so they can be undone alone.
    pub(crate) fn begin_operation(&mut self) {
        self.drop_savepoint();
        self.savepoint = Some(BTreeMap::new());
    }

    /// The operation succeeded: its changes are the transaction's now.
    pub(crate) fn end_operation(&mut self) {
        self.drop_savepoint();
    }

    /// Undo the running operation's changes, leaving every block the rest of
    /// the transaction staged exactly as it was.
    pub(crate) fn undo_operation(&mut self) {
        let Some(savepoint) = self.savepoint.take() else {
            return;
        };
        self.saved_priors = 0;
        for (phys, prior) in savepoint {
            for phase in WritePhase::ALL {
                if let Some(mut bytes) = self.entries.remove(&(phase, phys)) {
                    bytes.zeroize();
                }
            }
            if let Some(prior) = prior {
                self.entries.insert((prior.phase, phys), prior.bytes);
            }
        }
    }

    /// Forget the savepoint, wiping the versions it held.
    fn drop_savepoint(&mut self) {
        let Some(savepoint) = self.savepoint.take() else {
            return;
        };
        self.saved_priors = 0;
        for prior in savepoint.into_values().flatten() {
            let mut bytes = prior.bytes;
            bytes.zeroize();
        }
    }

    /// Move `phys` as it stands into the running operation's savepoint, the
    /// first time that operation changes it.
    ///
    /// A block the set does not hold is recorded as absent, so undoing the
    /// operation removes whatever it staged there.
    fn save_prior(&mut self, phys: u64) {
        if self
            .savepoint
            .as_ref()
            .is_none_or(|savepoint| savepoint.contains_key(&phys))
        {
            return;
        }
        let prior = WritePhase::ALL.into_iter().find_map(|phase| {
            self.entries
                .remove(&(phase, phys))
                .map(|bytes| Prior { phase, bytes })
        });
        if prior.is_some() {
            self.saved_priors = self.saved_priors.saturating_add(1);
        }
        if let Some(savepoint) = self.savepoint.as_mut() {
            savepoint.insert(phys, prior);
        }
    }

    /// Stage `bytes` at `phys` for `phase`, replacing any held version.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] when `bytes` is shorter than a block,
    /// [`DriverError::NoSpace`] when a first-time entry cannot be allocated.
    pub(crate) fn stage(
        &mut self,
        phase: WritePhase,
        phys: u64,
        bytes: &[u8],
    ) -> Result<(), DriverError> {
        let block = bytes
            .get(..self.block_size)
            .ok_or(DriverError::BufferTooSmall)?;
        // An allocation-map page is rebuildable and is written straight out,
        // so its staging is not undone: dropping its only copy would lose bits
        // the resident map no longer holds either. Everything that touches the
        // pre-barrier phase is.
        if phase == WritePhase::BeforeBarrier
            || self
                .entries
                .contains_key(&(WritePhase::BeforeBarrier, phys))
        {
            self.save_prior(phys);
        }
        if let Some(held) = self.entries.get_mut(&(phase, phys)) {
            held.copy_from_slice(block);
            return Ok(());
        }
        if let Some(mut held) = self.entries.remove(&(phase.other(), phys)) {
            held.copy_from_slice(block);
            self.entries.insert((phase, phys), held);
            return Ok(());
        }
        let mut held = Vec::new();
        held.try_reserve_exact(self.block_size)
            .map_err(|_| DriverError::NoSpace)?;
        held.extend_from_slice(block);
        self.entries.insert((phase, phys), held);
        Ok(())
    }

    /// How many of the `blocks` addresses from `phys` are staged.
    pub(crate) fn staged_in(&self, phys: u64, blocks: u64) -> u64 {
        let Some(last) = blocks.checked_sub(1).map(|last| phys.saturating_add(last)) else {
            return 0;
        };
        WritePhase::ALL
            .iter()
            .map(|&phase| self.entries.range((phase, phys)..=(phase, last)).count() as u64)
            .sum()
    }

    /// Overlay every staged block of the run starting at `phys` onto `run`,
    /// which holds that run's bytes as the device has them.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] when `run` is not a whole number of
    /// blocks.
    pub(crate) fn overlay(&self, phys: u64, run: &mut [u8]) -> Result<(), DriverError> {
        if !run.len().is_multiple_of(self.block_size) {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (run.len() / self.block_size) as u64;
        let Some(last) = blocks.checked_sub(1).map(|last| phys.saturating_add(last)) else {
            return Ok(());
        };
        for phase in WritePhase::ALL {
            for ((_, at), bytes) in self.entries.range((phase, phys)..=(phase, last)) {
                let index =
                    usize::try_from(*at - phys).map_err(|_| DriverError::LengthOutOfRange)?;
                let off = index
                    .checked_mul(self.block_size)
                    .ok_or(DriverError::LengthOutOfRange)?;
                let slot = run
                    .get_mut(off..off + self.block_size)
                    .ok_or(DriverError::BufferTooSmall)?;
                slot.copy_from_slice(bytes);
            }
        }
        Ok(())
    }

    /// Drain one ordering phase in ascending physical runs.
    ///
    /// `write` receives a run's start address and the whole of its payload: a
    /// positive whole number of blocks the set holds at consecutive addresses.
    /// A mirrored metadata block is two entries with identical bytes one apart,
    /// so it leaves as one two-block request. A run stops at the first address
    /// the set does not hold, so it never names a block outside the
    /// transaction, and at the gather window's end, so it never exceeds the
    /// device's transfer bound. With no window to gather into — a machine too
    /// short of memory to reserve one — each block is written straight from the
    /// set, so a commit costs more requests rather than failing.
    ///
    /// # Errors
    ///
    /// Whatever `write` reports. The failed run and everything after it stay
    /// staged so recovery never loses the only retained map bytes.
    pub(crate) fn drain<F>(
        &mut self,
        phase: WritePhase,
        run_bytes: usize,
        mut write: F,
    ) -> Result<(), DriverError>
    where
        F: FnMut(u64, &[u8]) -> Result<(), DriverError>,
    {
        let bound = run_bytes / self.block_size * self.block_size;
        let mut window = RunWindow::new(self.longest_run_bytes(phase, bound));
        while let Some((&(_, start), _)) = self.entries.range((phase, 0)..=(phase, u64::MAX)).next()
        {
            let (blocks, outcome) = match window.buf() {
                Some(buf) => {
                    let (blocks, run) = self.gather(phase, start, buf)?;
                    (blocks, write(start, run))
                }
                None => match self.entries.get(&(phase, start)) {
                    Some(block) => (1, write(start, block)),
                    None => (1, Err(DriverError::BufferTooSmall)),
                },
            };
            outcome?;
            self.release(phase, start, blocks);
        }
        Ok(())
    }

    /// Drain one named block without disturbing other work in the phase.
    pub(crate) fn drain_block<F>(
        &mut self,
        phase: WritePhase,
        phys: u64,
        write: F,
    ) -> Result<(), DriverError>
    where
        F: FnOnce(u64, &[u8]) -> Result<(), DriverError>,
    {
        let block = self
            .entries
            .get(&(phase, phys))
            .ok_or(DriverError::DeviceFault)?;
        write(phys, block)?;
        self.release(phase, phys, 1);
        Ok(())
    }

    /// Whether the running operation has changed anything the set holds.
    pub(crate) fn operation_changed(&self) -> bool {
        self.savepoint
            .as_ref()
            .is_some_and(|savepoint| !savepoint.is_empty())
    }

    /// Whether `phase` currently holds at least one block.
    pub(crate) fn has(&self, phase: WritePhase) -> bool {
        self.entries
            .range((phase, 0)..=(phase, u64::MAX))
            .next()
            .is_some()
    }

    /// Bytes the longest run of adjacent staged blocks occupies, capped at
    /// `bound`.
    ///
    /// The gather window is reserved for what the set actually holds rather
    /// than for the transfer bound, so a metadata-only transaction stages a
    /// handful of blocks instead of the whole window — the same sizing the read
    /// path applies to a small read.
    fn longest_run_bytes(&self, phase: WritePhase, bound: usize) -> usize {
        let mut longest = 0usize;
        let mut run = 0usize;
        let mut want = None;
        for &(_, at) in self
            .entries
            .range((phase, 0)..=(phase, u64::MAX))
            .map(|(key, _)| key)
        {
            run = if want == Some(at) { run + 1 } else { 1 };
            longest = longest.max(run);
            want = Some(at.saturating_add(1));
        }
        longest.saturating_mul(self.block_size).min(bound)
    }

    /// Copy the run of adjacent staged blocks starting at `start` into
    /// `window`, returning how many blocks it holds and their payload.
    ///
    /// One pass both decides the run's length and lays its bytes out, so the
    /// length the set then releases and the bytes the device is handed cannot
    /// disagree.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] when `window` cannot hold one block, the
    /// set holds none at `start`, or a staged entry is not a whole block.
    fn gather<'w>(
        &self,
        phase: WritePhase,
        start: u64,
        window: &'w mut [u8],
    ) -> Result<(usize, &'w [u8]), DriverError> {
        let mut want = start;
        let mut span = 0usize;
        for ((_, at), bytes) in self.entries.range((phase, start)..=(phase, u64::MAX)) {
            if *at != want {
                break;
            }
            let Some(end) = span.checked_add(self.block_size) else {
                break;
            };
            let Some(slot) = window.get_mut(span..end) else {
                break;
            };
            slot.copy_from_slice(
                bytes
                    .get(..self.block_size)
                    .ok_or(DriverError::BufferTooSmall)?,
            );
            span = end;
            want = want.saturating_add(1);
        }
        if span == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let run = window.get(..span).ok_or(DriverError::BufferTooSmall)?;
        Ok((span / self.block_size, run))
    }

    /// Drop and wipe the `blocks` staged blocks from `start`, whose bytes have
    /// left the set.
    fn release(&mut self, phase: WritePhase, start: u64, blocks: usize) {
        let mut at = start;
        for _ in 0..blocks {
            if let Some(mut bytes) = self.entries.remove(&(phase, at)) {
                bytes.zeroize();
            }
            at = at.saturating_add(1);
        }
    }

    /// Drop every block of `start..start + len` the set holds: their bytes name
    /// blocks nothing will reference, so writing them out would cost a device
    /// command for contents no reader can reach.
    ///
    /// Visits the blocks it *holds*, not the run's, so releasing a very large
    /// run costs what is staged inside it and nothing per block. A block the
    /// set does not hold needs no savepoint entry: it is unchanged, and a later
    /// staging in the same operation records its own.
    pub(crate) fn discard_run(&mut self, start: u64, len: u64) {
        let Some(last) = len.checked_sub(1).map(|last| start.saturating_add(last)) else {
            return;
        };
        for phase in WritePhase::ALL {
            while let Some(phys) = self
                .entries
                .range((phase, start)..=(phase, last))
                .next()
                .map(|(&(_, phys), _)| phys)
            {
                self.save_prior(phys);
                if let Some(mut bytes) = self.entries.remove(&(phase, phys)) {
                    bytes.zeroize();
                }
            }
        }
    }

    /// Drop and wipe every block in one ordering phase.
    pub(crate) fn clear_phase(&mut self, phase: WritePhase) {
        self.drop_savepoint();
        while let Some((&(_, phys), _)) = self.entries.range((phase, 0)..=(phase, u64::MAX)).next()
        {
            if let Some(mut bytes) = self.entries.remove(&(phase, phys)) {
                bytes.zeroize();
            }
        }
    }

    /// Drop and wipe every staged block.
    pub(crate) fn clear(&mut self) {
        self.drop_savepoint();
        while let Some((_, mut bytes)) = self.entries.pop_first() {
            bytes.zeroize();
        }
    }

    /// Blocks currently staged.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether an operation's savepoint is installed. Between operations it
    /// must not be: a stale one would record the next caller's changes against
    /// a snapshot nothing will unwind to.
    #[cfg(test)]
    pub(crate) const fn operation_running(&self) -> bool {
        self.savepoint.is_some()
    }
}

impl Drop for DirtySet {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 512;

    /// Gather bound wide enough that a test's runs are never window-bounded,
    /// so a test that means to measure adjacency does not measure the bound.
    const WIDE: usize = 64 * BS;

    fn block(fill: u8) -> [u8; BS] {
        [fill; BS]
    }

    /// A block whose every byte names the address it is staged at, so a
    /// gathered run's contents can be checked against the blocks it came from.
    fn tagged(phys: u64) -> [u8; BS] {
        block(u8::try_from(phys).expect("a test address fits a byte"))
    }

    /// Every request the drain issued: start address, block count, and the
    /// fill byte of each block it carried.
    fn requests(
        set: &mut DirtySet,
        phase: WritePhase,
        run_bytes: usize,
    ) -> Vec<(u64, usize, Vec<u8>)> {
        let mut seen = Vec::new();
        set.drain(phase, run_bytes, |phys, run| {
            seen.push((
                phys,
                run.len() / BS,
                run.chunks(BS).map(|block| block[0]).collect(),
            ));
            Ok(())
        })
        .expect("drain");
        seen
    }

    #[test]
    fn a_rewrite_replaces_the_staged_block_rather_than_adding_one() {
        let mut set = DirtySet::new(BS);
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xAA))
            .expect("stage");
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xBB))
            .expect("restage");
        assert_eq!(set.len(), 1);
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![(7, 1, alloc::vec![0xBB])]
        );
    }

    #[test]
    fn moving_a_block_between_phases_replaces_its_single_entry() {
        let mut set = DirtySet::new(BS);
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xAA))
            .expect("stage");
        set.stage(WritePhase::AfterBarrier, 7, &block(0xBB))
            .expect("move");
        assert!(!set.has(WritePhase::BeforeBarrier));
        assert_eq!(set.len(), 1);
        assert_eq!(
            requests(&mut set, WritePhase::AfterBarrier, WIDE),
            alloc::vec![(7, 1, alloc::vec![0xBB])]
        );
    }

    #[test]
    fn draining_one_block_leaves_the_rest_of_its_phase_staged() {
        let mut set = DirtySet::new(BS);
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xAA))
            .expect("stage marker");
        set.stage(WritePhase::BeforeBarrier, 9, &block(0xBB))
            .expect("stage transaction block");
        let mut seen = None;
        set.drain_block(WritePhase::BeforeBarrier, 7, |phys, bytes| {
            seen = Some((phys, bytes[0]));
            Ok(())
        })
        .expect("drain marker");
        assert_eq!(seen, Some((7, 0xAA)));
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![(9, 1, alloc::vec![0xBB])]
        );
    }

    #[test]
    fn clearing_one_phase_preserves_the_other() {
        let mut set = DirtySet::new(BS);
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xAA))
            .expect("stage before");
        set.stage(WritePhase::AfterBarrier, 9, &block(0xBB))
            .expect("stage after");
        set.clear_phase(WritePhase::BeforeBarrier);
        assert!(!set.has(WritePhase::BeforeBarrier));
        assert_eq!(
            requests(&mut set, WritePhase::AfterBarrier, WIDE),
            alloc::vec![(9, 1, alloc::vec![0xBB])]
        );
    }

    #[test]
    fn the_drain_runs_in_ascending_device_order() {
        let mut set = DirtySet::new(BS);
        for (fill, phys) in [(1_u8, 9_u64), (2, 2), (3, 40), (4, 3)] {
            set.stage(WritePhase::BeforeBarrier, phys, &block(fill))
                .expect("stage");
        }
        // 2 and 3 are adjacent and leave together; 9 and 40 stand alone.
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![
                (2, 2, alloc::vec![2, 4]),
                (9, 1, alloc::vec![1]),
                (40, 1, alloc::vec![3]),
            ]
        );
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn adjacent_blocks_leave_as_one_request_and_a_gap_ends_the_run() {
        let mut set = DirtySet::new(BS);
        for phys in [10_u64, 11, 12, 14, 15] {
            set.stage(WritePhase::BeforeBarrier, phys, &tagged(phys))
                .expect("stage");
        }
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![
                (10, 3, alloc::vec![10, 11, 12]),
                (14, 2, alloc::vec![14, 15]),
            ],
            "a run must cover only addresses the set holds"
        );
    }

    #[test]
    fn a_run_is_bounded_by_the_gather_window() {
        let mut set = DirtySet::new(BS);
        for phys in 0..5_u64 {
            set.stage(WritePhase::BeforeBarrier, phys, &tagged(phys))
                .expect("stage");
        }
        // Five adjacent blocks under a two-block bound: 2 + 2 + 1.
        let issued = requests(&mut set, WritePhase::BeforeBarrier, 2 * BS);
        assert_eq!(
            issued
                .iter()
                .map(|&(phys, blocks, _)| (phys, blocks))
                .collect::<Vec<_>>(),
            alloc::vec![(0, 2), (2, 2), (4, 1)]
        );
    }

    #[test]
    fn with_no_gather_window_each_block_is_written_from_the_set() {
        let mut set = DirtySet::new(BS);
        for phys in 0..3_u64 {
            set.stage(WritePhase::BeforeBarrier, phys, &tagged(phys))
                .expect("stage");
        }
        // A bound below one block leaves nothing to gather into, which is the
        // path a refused window allocation takes.
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, BS - 1),
            alloc::vec![
                (0, 1, alloc::vec![0]),
                (1, 1, alloc::vec![1]),
                (2, 1, alloc::vec![2]),
            ]
        );
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn a_failing_write_leaves_the_blocks_it_did_not_reach_staged() {
        let mut set = DirtySet::new(BS);
        for phys in [0_u64, 2, 4, 6] {
            set.stage(WritePhase::BeforeBarrier, phys, &block(0))
                .expect("stage");
        }
        let mut written = 0_usize;
        let outcome = set.drain(WritePhase::BeforeBarrier, WIDE, |_, _| {
            written += 1;
            if written == 2 {
                return Err(DriverError::DeviceFault);
            }
            Ok(())
        });
        assert_eq!(outcome, Err(DriverError::DeviceFault));
        assert_eq!(written, 2);
        assert_eq!(set.len(), 3, "the failed and untouched blocks stay staged");
    }

    #[test]
    fn the_overlay_replaces_only_the_staged_blocks_of_a_run() {
        let mut set = DirtySet::new(BS);
        set.stage(WritePhase::AfterBarrier, 11, &block(0xCC))
            .expect("stage");
        let mut run = alloc::vec![0x11_u8; 3 * BS];
        set.overlay(10, &mut run).expect("overlay");
        assert_eq!(run[0], 0x11);
        assert_eq!(run[BS], 0xCC);
        assert_eq!(run[2 * BS], 0x11);
        assert_eq!(set.staged_in(10, 3), 1);
        assert_eq!(set.staged_in(12, 3), 0);
    }

    #[test]
    fn a_short_or_ragged_buffer_is_refused_rather_than_truncated() {
        let mut set = DirtySet::new(BS);
        assert_eq!(
            set.stage(WritePhase::BeforeBarrier, 1, &[0u8; BS - 1]),
            Err(DriverError::BufferTooSmall)
        );
        assert_eq!(
            set.overlay(0, &mut [0u8; BS + 1]),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn each_device_class_is_served_its_own_window() {
        // Removable media pay the most per command, a rotational disk gets
        // its metadata seeks back, and a device already cheap per command
        // keeps the smallest exposure.
        let calm = PressureBand::Normal;
        assert_eq!(
            writeback_window_ns(BlkDeviceClass::Removable, calm),
            30_000_000_000
        );
        assert_eq!(
            writeback_window_ns(BlkDeviceClass::Rotational, calm),
            15_000_000_000
        );
        assert_eq!(
            writeback_window_ns(BlkDeviceClass::SolidState, calm),
            5_000_000_000
        );
        assert_eq!(
            writeback_window_ns(BlkDeviceClass::Virtual, calm),
            5_000_000_000
        );
        assert!(
            writeback_window_ns(BlkDeviceClass::Removable, calm)
                > writeback_window_ns(BlkDeviceClass::Rotational, calm)
                && writeback_window_ns(BlkDeviceClass::Rotational, calm)
                    > writeback_window_ns(BlkDeviceClass::SolidState, calm),
            "the window must widen with the cost of a command"
        );
    }

    #[test]
    fn tightening_memory_only_ever_shortens_the_window() {
        let mut previous = u64::MAX;
        for band in PressureBand::ALL {
            let window = writeback_window_ns(BlkDeviceClass::Removable, band);
            assert!(
                window < previous,
                "each deeper band must publish sooner than the one above it"
            );
            assert!(window > 0, "a window must never vanish into a busy commit");
            previous = window;
        }
    }

    /// A gauge parked at one band, as the driver sees it: the model's own
    /// receiving gauge, so the tests exercise the same policy a mount does
    /// rather than a second notion of pressure.
    fn gauge(band: PressureBand) -> &'static tairix_reclaim::ReportedPressure {
        let gauge = alloc::boxed::Box::leak(alloc::boxed::Box::new(
            tairix_reclaim::ReportedPressure::unknown(),
        ));
        gauge.report(band);
        gauge
    }

    /// A budget wide enough that the band ladder, not the floor, decides the
    /// first few bands.
    const AMPLE_BACKING: usize = 64 * WRITEBACK_FLOOR_BYTES * 16;

    fn bound(band: PressureBand) -> WritebackBound {
        WritebackBound::new(
            CacheBudget::from_backing(AMPLE_BACKING),
            gauge(band),
            Arc::new(PinnedAccounting::new()),
        )
        .expect("an ample machine bounds a volume")
    }

    #[test]
    fn the_ceiling_falls_with_the_band_and_stops_at_the_floor() {
        let mut previous = usize::MAX;
        for band in PressureBand::ALL {
            let ceiling = bound(band).reading(0).ceiling;
            assert!(
                ceiling <= previous,
                "a deeper band must never raise the ceiling"
            );
            assert!(
                ceiling >= WRITEBACK_FLOOR_BYTES,
                "pressure lowers the ceiling to the floor and no further"
            );
            previous = ceiling;
        }
        assert_eq!(
            bound(PressureBand::Normal).reading(0).ceiling,
            CacheBudget::from_backing(AMPLE_BACKING).hard(),
            "an unpressured machine gives the volume its whole share"
        );
        assert!(
            bound(PressureBand::Mild).reading(0).ceiling
                < bound(PressureBand::Normal).reading(0).ceiling,
            "the first tightening must actually bite"
        );
    }

    #[test]
    fn a_machine_too_small_for_one_transfer_is_refused_a_bound() {
        // A machine whose whole ceiling cannot hold one coalesced transfer
        // would commit after almost every record: the mount is refused rather
        // than accepted and left to wedge.
        let short = CacheBudget::from_backing(WRITEBACK_FLOOR_BYTES * 16 - 16);
        assert!(short.hard() < WRITEBACK_FLOOR_BYTES);
        assert_eq!(
            WritebackBound::new(
                short,
                gauge(PressureBand::Normal),
                Arc::new(PinnedAccounting::new()),
            )
            .err(),
            Some(DriverError::NoSpace)
        );
        let exact = CacheBudget::from_backing(WRITEBACK_FLOOR_BYTES * 16);
        assert_eq!(exact.hard(), WRITEBACK_FLOOR_BYTES);
        assert!(
            WritebackBound::new(
                exact,
                gauge(PressureBand::Normal),
                Arc::new(PinnedAccounting::new()),
            )
            .is_ok(),
            "exactly one transfer is enough"
        );
    }

    #[test]
    fn the_machine_wide_reserve_caps_the_ceiling_however_large_the_budget() {
        // The machine's own reserve is the last cap, below the share and below
        // what the other volumes leave: no cache may draw into it, so a volume
        // on a machine with no headroom left keeps only the floor it needs to
        // finish a transaction.
        struct Starved;
        impl tairix_reclaim::FreeMemorySource for Starved {
            fn free_bytes(&self) -> usize {
                0
            }
            fn total_bytes(&self) -> usize {
                AMPLE_BACKING
            }
        }
        static STARVED: Starved = Starved;
        let measured: &'static tairix_reclaim::MemoryPressure = alloc::boxed::Box::leak(
            alloc::boxed::Box::new(tairix_reclaim::MemoryPressure::over(&STARVED)),
        );
        let bound = WritebackBound::new(
            CacheBudget::from_backing(AMPLE_BACKING),
            measured,
            Arc::new(PinnedAccounting::new()),
        )
        .expect("the budget itself is ample");
        assert_eq!(
            bound.reading(0).ceiling,
            WRITEBACK_FLOOR_BYTES,
            "no headroom above the reserve leaves only the forward-progress floor"
        );
    }

    #[test]
    fn the_sets_pinned_figure_follows_what_it_holds() {
        let mut set = DirtySet::new(BS);
        assert_eq!((set.pinned_bytes(), set.held()), (0, 0));

        set.stage(WritePhase::BeforeBarrier, 4, &block(1))
            .expect("stage");
        set.stage(WritePhase::BeforeBarrier, 5, &block(2))
            .expect("stage");
        let per_block = BS + MAP_ENTRY_OVERHEAD;
        assert_eq!((set.pinned_bytes(), set.held()), (2 * per_block, 2));

        let _ = requests(&mut set, WritePhase::BeforeBarrier, WIDE);
        assert_eq!(
            (set.pinned_bytes(), set.held()),
            (0, 0),
            "a drained set pins nothing"
        );
    }

    #[test]
    fn a_saved_prior_version_is_pinned_like_a_staged_one() {
        // An operation's savepoint holds the version a failure unwinds to, so
        // its bytes are as unavailable to the machine as the staged ones and
        // are counted with them.
        let mut set = DirtySet::new(BS);
        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(1))
            .expect("stage");
        set.end_operation();

        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(2))
            .expect("rewrite");
        assert_eq!(
            set.held(),
            2,
            "the prior version and the new one are both held"
        );
        set.end_operation();
        assert_eq!(set.held(), 1, "the undo copy goes with the operation");
    }

    #[test]
    fn volumes_sharing_a_machine_divide_one_ceiling_rather_than_taking_one_each() {
        // A per-volume ceiling would let a machine's volumes pin a multiple of
        // what the machine has, and pinned bytes are exactly the bytes nothing
        // can reclaim. Each volume takes a share instead.
        let share: &'static tairix_reclaim::PinnedShare =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(tairix_reclaim::PinnedShare::new()));
        let bound = |slot: &Arc<PinnedAccounting>| {
            WritebackBound::new(
                CacheBudget::from_backing(AMPLE_BACKING),
                gauge(PressureBand::Normal),
                Arc::clone(slot),
            )
            .expect("the machine bounds a volume")
        };
        let whole = CacheBudget::from_backing(AMPLE_BACKING).hard();

        let alone = Arc::new(PinnedAccounting::within(share));
        let alone_bound = bound(&alone);
        assert_eq!(
            alone_bound.reading(0).ceiling,
            whole,
            "one volume drawing on the machine has the whole ceiling"
        );

        // Four volumes writing at once take a quarter each, so the machine
        // holds one ceiling however many of them there are.
        let slots: alloc::vec::Vec<Arc<PinnedAccounting>> = (0..4)
            .map(|_| Arc::new(PinnedAccounting::within(share)))
            .collect();
        let bounds: alloc::vec::Vec<WritebackBound> = slots.iter().map(bound).collect();
        for slot in &slots {
            slot.set(1, 1);
        }
        for held in &bounds {
            assert_eq!(held.reading(1).ceiling, whole / 4);
        }

        // The mount going away gives its share back.
        drop(bounds);
        assert_eq!(share.bytes(), 0, "a torn-down mount draws nothing");
        assert_eq!(alone_bound.reading(0).ceiling, whole);

        // A volume holding more than an equal share leaves its sibling only
        // what is left, which is tighter than the share itself — so a volume
        // whose neighbour is quiet is not throttled for company it does not
        // have, and one whose neighbour is full is.
        let pair: &'static tairix_reclaim::PinnedShare =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(tairix_reclaim::PinnedShare::new()));
        let hog = Arc::new(PinnedAccounting::within(pair));
        let beside = Arc::new(PinnedAccounting::within(pair));
        let _hog_bound = bound(&hog);
        let beside_bound = bound(&beside);
        beside.set(1, 1);
        hog.set(1, 1);
        assert_eq!(
            beside_bound.reading(1).ceiling,
            whole / 2,
            "two volumes drawing, half the machine each"
        );
        hog.set(3 * whole / 4, 1);
        assert_eq!(beside_bound.reading(1).ceiling, whole / 4);

        // A volume holding nothing counts for nothing, so a machine whose other
        // volumes are empty leaves the whole ceiling to the one that is writing.
        hog.set(0, 0);
        assert_eq!(beside_bound.reading(1).ceiling, whole);
    }

    #[test]
    fn a_deepening_band_brings_an_open_transactions_deadline_forward() {
        let host = TestWritebackHost::leaked(0);
        let mut schedule = CommitScheduler::new(BlkDeviceClass::Removable);
        schedule.set_host(TestWritebackHost::volume(), host);
        schedule.opened();
        let calm = host.reported().expect("an open transaction is due");
        schedule.set_band(PressureBand::Moderate);
        let tightened = host.reported().expect("still open, and due sooner");
        assert!(
            tightened < calm,
            "a tightening machine must not wait out a window measured when \
             memory was plentiful"
        );
        // A band that has not moved reports nothing: pressure is sampled every
        // operation, and a steady band must not cost a timer arm each time.
        let reports = host.reports();
        schedule.set_band(PressureBand::Moderate);
        assert_eq!(host.reports(), reports);
    }

    #[test]
    fn a_scheduler_with_no_host_publishes_every_operation() {
        let mut schedule = CommitScheduler::new(BlkDeviceClass::Removable);
        schedule.opened();
        assert!(
            schedule.expired(),
            "with no timer above it there is no window to measure, so nothing \
             is deferred"
        );
    }

    #[test]
    fn a_host_that_reads_no_clock_publishes_every_operation() {
        struct Clockless;
        impl WritebackHost for Clockless {
            fn now_ns(&self) -> Option<u64> {
                None
            }
            fn writeback_due(&self, _volume: DriverHandle, _deadline_ns: Option<u64>) {}
        }
        static CLOCKLESS: Clockless = Clockless;

        let mut schedule = CommitScheduler::new(BlkDeviceClass::Removable);
        schedule.set_host(TestWritebackHost::volume(), &CLOCKLESS);
        schedule.opened();
        assert!(
            schedule.expired(),
            "a host that will not say how much time has passed does not get \
             to defer durability"
        );
    }

    #[test]
    fn a_transaction_expires_when_its_window_elapses() {
        let window = writeback_window_ns(BlkDeviceClass::SolidState, PressureBand::Normal);
        let host = TestWritebackHost::leaked(0);
        let mut schedule = CommitScheduler::new(BlkDeviceClass::SolidState);
        schedule.set_host(TestWritebackHost::volume(), host);
        schedule.opened();
        assert!(schedule.is_open());
        assert!(!schedule.expired());
        host.set_now(window - 1);
        assert!(!schedule.expired());
        host.set_now(window);
        assert!(schedule.expired());
        schedule.closed();
        assert!(!schedule.is_open());
    }

    #[test]
    fn an_opening_transaction_reports_its_deadline_and_a_closing_one_reports_none() {
        let window = writeback_window_ns(BlkDeviceClass::Rotational, PressureBand::Normal);
        let host = TestWritebackHost::leaked(700);
        let mut schedule = CommitScheduler::new(BlkDeviceClass::Rotational);
        schedule.set_host(TestWritebackHost::volume(), host);
        assert_eq!(
            host.reported(),
            None,
            "installing the timer over an idle volume reports nothing to fire"
        );
        schedule.opened();
        assert_eq!(
            host.reported(),
            Some(700 + window),
            "the host is told exactly when this transaction must be published"
        );
        let reports = host.reports();
        schedule.opened();
        assert_eq!(
            host.reports(),
            reports,
            "an operation joining the open transaction moves no deadline"
        );
        schedule.closed();
        assert_eq!(
            host.reported(),
            None,
            "a published transaction leaves the timer nothing to fire"
        );
    }

    #[test]
    fn a_host_installed_over_an_open_transaction_dates_it_from_now() {
        let window = writeback_window_ns(BlkDeviceClass::Virtual, PressureBand::Normal);
        let mut schedule = CommitScheduler::new(BlkDeviceClass::Virtual);
        // Opened before any host existed, so its age was never measured.
        schedule.opened();
        assert!(schedule.expired());
        let host = TestWritebackHost::leaked(5_000);
        schedule.set_host(TestWritebackHost::volume(), host);
        assert_eq!(
            host.reported(),
            Some(5_000 + window),
            "an already-open transaction is bounded by one window from the \
             moment the timer arrives, never left unreported"
        );
        assert!(!schedule.expired());
    }

    #[test]
    fn a_closed_transaction_forgets_the_operations_it_carried() {
        let host = TestWritebackHost::leaked(0);
        let mut schedule = CommitScheduler::new(BlkDeviceClass::Virtual);
        schedule.set_host(TestWritebackHost::volume(), host);
        schedule.opened();
        schedule.joined();
        schedule.joined();
        assert_eq!(schedule.acknowledged(), 2);
        schedule.closed();
        assert_eq!(schedule.acknowledged(), 0);
    }

    #[test]
    fn undoing_an_operation_restores_what_an_earlier_one_staged() {
        let mut set = DirtySet::new(BS);
        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xAA))
            .expect("first operation stages");
        set.end_operation();

        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xBB))
            .expect("second operation rewrites it");
        set.stage(WritePhase::BeforeBarrier, 9, &block(0xCC))
            .expect("and stages one of its own");
        set.undo_operation();

        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![(7, 1, alloc::vec![0xAA])],
            "the earlier operation's block must survive with its own bytes"
        );
    }

    #[test]
    fn undoing_an_operation_restores_a_block_it_discarded_and_took_again() {
        // The hard case: a block an earlier operation staged, that this one
        // frees and immediately reuses for something else.
        let mut set = DirtySet::new(BS);
        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 4, &block(0xAA))
            .expect("first operation stages");
        set.end_operation();

        set.begin_operation();
        set.discard_run(4, 1);
        set.stage(WritePhase::BeforeBarrier, 4, &block(0xBB))
            .expect("reuse the freed block");
        set.undo_operation();

        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![(4, 1, alloc::vec![0xAA])]
        );
    }

    #[test]
    fn ending_an_operation_keeps_its_changes() {
        let mut set = DirtySet::new(BS);
        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xAA))
            .expect("stage");
        set.end_operation();
        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(0xBB))
            .expect("restage");
        set.end_operation();
        set.undo_operation();
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![(7, 1, alloc::vec![0xBB])],
            "an ended operation has nothing left to undo"
        );
    }

    #[test]
    fn undoing_an_operation_leaves_an_allocation_map_page_staged() {
        // A map page is written straight out and is rebuildable, so its only
        // copy must not be dropped by an unrelated operation's failure.
        let mut set = DirtySet::new(BS);
        set.begin_operation();
        set.stage(WritePhase::AfterBarrier, 12, &block(0xDD))
            .expect("stage a map page");
        set.undo_operation();
        assert_eq!(
            requests(&mut set, WritePhase::AfterBarrier, WIDE),
            alloc::vec![(12, 1, alloc::vec![0xDD])]
        );
    }

    #[test]
    fn a_savepoint_records_a_block_once_however_often_it_is_rewritten() {
        let mut set = DirtySet::new(BS);
        set.begin_operation();
        set.stage(WritePhase::BeforeBarrier, 7, &block(1))
            .expect("stage");
        assert!(set.operation_changed());
        for fill in 2..8_u8 {
            set.stage(WritePhase::BeforeBarrier, 7, &block(fill))
                .expect("rewrite");
        }
        set.undo_operation();
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn discarding_a_reclaimed_block_drops_it_from_the_drain() {
        let mut set = DirtySet::new(BS);
        set.stage(WritePhase::BeforeBarrier, 4, &block(1))
            .expect("stage");
        set.stage(WritePhase::BeforeBarrier, 5, &block(2))
            .expect("stage");
        set.discard_run(4, 1);
        set.discard_run(99, 1);
        assert_eq!(
            requests(&mut set, WritePhase::BeforeBarrier, WIDE),
            alloc::vec![(5, 1, alloc::vec![2])],
            "a discarded block must not rejoin its neighbour's run"
        );
    }
}
