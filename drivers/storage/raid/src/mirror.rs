//! RAID1 mirror composition over the public block seam.
//!
//! [`MirrorArray`] composes a caller-owned slice of [`MirrorMember`]s into one
//! logical [`Block`] device. It is `no_std` and allocation-free: the member
//! set is a slice the caller owns (the growable tier lives in the assembling
//! serve process, `AGENTS.md` §24), so the array imposes no fixed member
//! ceiling and holds only a borrow.

use crate::superblock::SlotDisposition;
use tairix_abi::blkio::BlkStatus;
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::{BufferClass, DriverError};
use tairix_abi::sysinfo::MountAvailability;

/// The membership state of one mirror copy.
///
/// A member is only ever a read source while [`InSync`](Self::InSync). A
/// [`Faulted`](Self::Faulted) member has been dropped from the array (a
/// whole-device fault, or a failed write/repair) and no longer serves or
/// receives I/O until it is re-added. A [`Resyncing`](Self::Resyncing) member
/// is being rebuilt from an in-sync copy: it receives writes to its
/// already-synced region so it never falls behind, but is not yet a read
/// source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemberState {
    /// A full, current copy. A read source and a write target.
    InSync,
    /// Dropped from the array after a whole-device fault or a failed write.
    /// Neither serves reads nor receives writes until re-added.
    Faulted,
    /// Being rebuilt from an in-sync copy. Receives writes to its
    /// already-synced region; becomes [`InSync`](Self::InSync) when the
    /// rebuild cursor reaches the end of the array.
    Resyncing,
}

/// Whether a member joining the array holds a copy believed **current** or
/// one the reassembly proved is **stale** and must be rebuilt before it can
/// serve a read.
///
/// The on-disk generation counter ([`ArraySuperblock::generation`], resolved
/// by [`ArrayIdentity`]) is the only authority on which copies are current: a
/// member below the authoritative generation is behind and its bytes must not
/// be trusted as a read source. Assembly ([`MirrorArray::assemble`]) turns a
/// member's role into its initial [`MemberState`]: a [`Current`](Self::Current)
/// copy that probes cleanly becomes [`MemberState::InSync`] (a read source at
/// once); a [`Stale`](Self::Stale) copy that probes cleanly becomes
/// [`MemberState::Resyncing`] so it is rebuilt from a current copy before it
/// ever answers a read. The array can therefore never serve a reader data from
/// a copy known to be out of date (the charter's fail-closed rule; a disk that
/// missed writes is a disk that can lie).
///
/// [`ArraySuperblock::generation`]: crate::ArraySuperblock::generation
/// [`ArrayIdentity`]: crate::ArrayIdentity
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemberRole {
    /// A copy believed current: at the array's authoritative generation.
    Current,
    /// A copy known to be behind the array: a rebuild target, admitted
    /// [`MemberState::Resyncing`] and never a read source until resynced.
    Stale,
}

impl MemberRole {
    /// The role a reassembled array slot contributes, or [`None`] for a
    /// [`SlotDisposition::Missing`] slot that offers no device to admit.
    ///
    /// This is the single mapping from the on-disk reassembly verdict
    /// ([`ArrayIdentity::fill_slots`]) to the composed member's role, so the
    /// metadata layer and the composition layer cannot disagree on what "in
    /// sync" means (`AGENTS.md` §2.2): a slot the metadata marked a stale
    /// rebuild target (`in_sync == false`) becomes a [`Stale`](Self::Stale)
    /// member, never a trusted read source.
    ///
    /// [`ArrayIdentity::fill_slots`]: crate::ArrayIdentity::fill_slots
    #[must_use]
    pub const fn for_slot(slot: SlotDisposition) -> Option<Self> {
        match slot {
            SlotDisposition::Missing => None,
            SlotDisposition::Present { in_sync: true, .. } => Some(Self::Current),
            SlotDisposition::Present { in_sync: false, .. } => Some(Self::Stale),
        }
    }
}

/// One mirror copy: a child [`Block`] device plus the role it joined with, its
/// membership state and, while [`MemberState::Resyncing`], the rebuild cursor
/// (the first not-yet-copied logical block).
pub struct MirrorMember<B: Block> {
    device: B,
    role: MemberRole,
    state: MemberState,
    resync_next_lba: u64,
}

impl<B: Block> MirrorMember<B> {
    /// Wrap `device` as a member presumed to hold a **current** copy
    /// ([`MemberRole::Current`]). Equivalent to
    /// [`with_role(device, MemberRole::Current)`](Self::with_role).
    #[must_use]
    pub const fn new(device: B) -> Self {
        Self::with_role(device, MemberRole::Current)
    }

    /// Wrap `device` as a member joining the array with `role`.
    ///
    /// Assembly ([`MirrorArray::assemble`]) re-derives the real state from the
    /// device's geometry probe and this role: a member whose device is absent
    /// or unwell at assembly is set [`MemberState::Faulted`] rather than
    /// trusted; a [`MemberRole::Stale`] member that probes cleanly begins
    /// [`MemberState::Resyncing`] rather than serving stale reads. The state
    /// recorded here is a placeholder overwritten by `assemble`.
    #[must_use]
    pub const fn with_role(device: B, role: MemberRole) -> Self {
        Self {
            device,
            role,
            state: MemberState::InSync,
            resync_next_lba: 0,
        }
    }

    /// The role this member joined the array with.
    #[must_use]
    pub const fn role(&self) -> MemberRole {
        self.role
    }

    /// This member's current membership state.
    #[must_use]
    pub const fn state(&self) -> MemberState {
        self.state
    }

    /// The rebuild cursor (first not-yet-copied logical block) while
    /// [`MemberState::Resyncing`]; `0` otherwise.
    #[must_use]
    pub const fn resync_cursor(&self) -> u64 {
        self.resync_next_lba
    }

    /// Borrow the underlying device (for identity/health queries).
    #[must_use]
    pub const fn device(&self) -> &B {
        &self.device
    }
}

/// The health of a composed array, ordered best → worst.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ArrayHealth {
    /// Every member is in sync: full redundancy.
    Optimal,
    /// At least one copy is serving, but redundancy is reduced (a member is
    /// faulted and none is currently rebuilding). Data is safe on the
    /// survivors; a tool shows the array as at-risk.
    Degraded,
    /// At least one copy is serving and a member is being rebuilt.
    Recovering,
    /// No in-sync copy remains: the array cannot serve and fails closed.
    Failed,
}

impl ArrayHealth {
    /// Whether the array can still serve I/O (any state but
    /// [`Failed`](Self::Failed)).
    #[must_use]
    pub const fn is_serving(self) -> bool {
        !matches!(self, Self::Failed)
    }

    /// The volume-availability this array health maps to, so a serving
    /// process can surface array health through the same `sysinfo` mount
    /// surface a leaf volume uses (`AGENTS.md` §2.2; `plans/FIX-IO.md`
    /// IO2/IO5) rather than a second vocabulary.
    #[must_use]
    pub const fn to_mount_availability(self) -> MountAvailability {
        match self {
            Self::Optimal => MountAvailability::Available,
            Self::Degraded => MountAvailability::Degraded,
            Self::Recovering => MountAvailability::Recovering,
            Self::Failed => MountAvailability::UnavailableLost,
        }
    }
}

/// A reason a mirror could not be assembled or reconfigured. Distinct from
/// [`DriverError`] (which flows on the I/O path) because these are
/// composition-policy failures, not device I/O outcomes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MirrorError {
    /// The member slice was empty; a mirror needs at least one copy.
    NoMembers,
    /// No member could report its geometry, so no array geometry could be
    /// established (every copy is absent or unwell). Fails closed.
    NoUsableMember,
    /// Two members report different geometry: they are not copies of the
    /// same array. Fails closed rather than silently truncating to the
    /// smaller.
    GeometryMismatch,
    /// A member index is outside the array.
    UnknownMember,
    /// A re-add/replace was asked of a member that is not currently faulted
    /// (nothing to rebuild).
    NotFaulted,
    /// A re-add/replace member's device could not be probed (absent/unwell),
    /// so it cannot begin rebuilding yet.
    ProbeFailed,
}

/// A RAID1 mirror presenting several child [`Block`] copies as one logical
/// device.
///
/// See the [crate documentation](crate) for the mirror's fault-recovery
/// behaviour. The array borrows a caller-owned member slice, so it holds no
/// allocation and imposes no fixed member ceiling (`AGENTS.md` §24.1).
pub struct MirrorArray<'a, B: Block> {
    members: &'a mut [MirrorMember<B>],
    geometry: BlockGeometry,
    /// The next logical block a scrub pass will verify, or the array block
    /// count when no scrub is in progress (`AGENTS.md` §26.5 auto-scrub).
    scrub_next_lba: u64,
}

impl<'a, B: Block> MirrorArray<'a, B> {
    /// Assemble a mirror from `members`, establishing the array geometry from
    /// the members that can report it.
    ///
    /// Every member's device is probed for geometry. The first that reports
    /// one fixes the array geometry; a member reporting a *different*
    /// geometry fails the whole assembly closed
    /// ([`MirrorError::GeometryMismatch`]) — differently-sized copies are not
    /// a mirror. A member whose probe *errors* (absent/unwell) is admitted as
    /// [`MemberState::Faulted`] so the array assembles degraded rather than
    /// refusing to come up while one copy is down; it can be re-added later.
    ///
    /// # Errors
    ///
    /// * [`MirrorError::NoMembers`] if `members` is empty.
    /// * [`MirrorError::GeometryMismatch`] if two members disagree on
    ///   geometry.
    /// * [`MirrorError::NoUsableMember`] if no member could report geometry.
    pub fn assemble(members: &'a mut [MirrorMember<B>]) -> Result<Self, MirrorError> {
        if members.is_empty() {
            return Err(MirrorError::NoMembers);
        }
        let mut geometry: Option<BlockGeometry> = None;
        for member in &mut *members {
            let Ok(g) = member.device.geometry() else {
                // A copy that cannot report its geometry is absent/unwell:
                // admit it faulted so the array still comes up on the rest.
                member.state = MemberState::Faulted;
                member.resync_next_lba = 0;
                continue;
            };
            match geometry {
                None => geometry = Some(g),
                Some(existing) if existing == g => {}
                Some(_) => return Err(MirrorError::GeometryMismatch),
            }
            // A copy the reassembly proved is behind must be rebuilt from a
            // current copy before it serves a read; only a copy believed
            // current is admitted as an immediate read source.
            member.state = match member.role {
                MemberRole::Current => MemberState::InSync,
                MemberRole::Stale => MemberState::Resyncing,
            };
            member.resync_next_lba = 0;
        }
        let Some(geometry) = geometry else {
            return Err(MirrorError::NoUsableMember);
        };
        Ok(Self {
            members,
            geometry,
            scrub_next_lba: geometry.block_count,
        })
    }

    /// The array's logical geometry (shared by every copy).
    #[must_use]
    pub const fn array_geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// The number of member slots (in sync, faulted, or resyncing alike).
    #[must_use]
    pub const fn member_count(&self) -> usize {
        self.members.len()
    }

    /// The state of member `index`, or [`None`] if `index` is out of range.
    #[must_use]
    pub fn member_state(&self, index: usize) -> Option<MemberState> {
        self.members.get(index).map(MirrorMember::state)
    }

    /// Borrow member `index` (for the serving process to inspect a copy's
    /// device identity, health, or rebuild cursor when logging), or [`None`]
    /// if `index` is out of range.
    #[must_use]
    pub fn member(&self, index: usize) -> Option<&MirrorMember<B>> {
        self.members.get(index)
    }

    /// The current [`ArrayHealth`], derived from the members' states.
    #[must_use]
    pub fn health(&self) -> ArrayHealth {
        let mut in_sync = 0usize;
        let mut resyncing = 0usize;
        let mut faulted = 0usize;
        for member in &*self.members {
            match member.state {
                MemberState::InSync => in_sync += 1,
                MemberState::Resyncing => resyncing += 1,
                MemberState::Faulted => faulted += 1,
            }
        }
        if in_sync == 0 {
            ArrayHealth::Failed
        } else if resyncing > 0 {
            ArrayHealth::Recovering
        } else if faulted > 0 {
            ArrayHealth::Degraded
        } else {
            ArrayHealth::Optimal
        }
    }

    /// Whether any member is still rebuilding (i.e. [`resync_step`] has more
    /// work to do).
    ///
    /// [`resync_step`]: Self::resync_step
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.members
            .iter()
            .any(|m| m.state == MemberState::Resyncing)
    }

    /// Number of currently in-sync members (read sources / full copies).
    fn in_sync_count(&self) -> usize {
        self.members
            .iter()
            .filter(|m| m.state == MemberState::InSync)
            .count()
    }

    /// Validate an I/O request against the array geometry, returning the
    /// block count.
    fn validate_io(&self, lba: u64, buf_len: usize) -> Result<u64, DriverError> {
        let bs = self.geometry.block_size as usize;
        if buf_len == 0 || bs == 0 || !buf_len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (buf_len / bs) as u64;
        let end = lba
            .checked_add(blocks)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.geometry.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(blocks)
    }

    /// Read `buf.len() / block_size` blocks at `lba` from a surviving copy,
    /// recovering from a good copy and repairing a proven-bad one.
    fn read_impl(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.validate_io(lba, buf.len())?;
        if self.in_sync_count() == 0 {
            return Err(DriverError::DeviceOffline);
        }
        let mut source: Option<usize> = None;
        let mut worst: Option<DriverError> = None;
        for i in 0..self.members.len() {
            if self.members[i].state != MemberState::InSync {
                continue;
            }
            match self.members[i]
                .device
                .read_blocks_with_class(lba, buf, class)
            {
                Ok(()) => {
                    source = Some(i);
                    break;
                }
                Err(e) => {
                    worst = Some(most_severe(worst, e));
                    if member_faulting(e) {
                        self.members[i].state = MemberState::Faulted;
                        self.members[i].resync_next_lba = 0;
                    }
                }
            }
        }
        let Some(src) = source else {
            // No copy could serve; the data is genuinely unrecoverable.
            return Err(worst.unwrap_or(DriverError::DeviceOffline));
        };
        // Repair the copies before `src` that failed this read but were not
        // whole-device faults (a per-block or transient error): write the
        // good data back so the device reallocates the bad sector. A repair
        // that fails drops that copy.
        for i in 0..src {
            if self.members[i].state == MemberState::InSync
                && self.members[i]
                    .device
                    .write_blocks_with_class(lba, buf, class)
                    .is_err()
            {
                self.members[i].state = MemberState::Faulted;
                self.members[i].resync_next_lba = 0;
            }
        }
        Ok(())
    }

    /// Write `buf.len() / block_size` blocks at `lba` to every copy.
    fn write_impl(&mut self, lba: u64, buf: &[u8], class: BufferClass) -> Result<(), DriverError> {
        let blocks = self.validate_io(lba, buf.len())?;
        if self.in_sync_count() == 0 {
            return Err(DriverError::DeviceOffline);
        }
        let bs = self.geometry.block_size as usize;
        let end = lba + blocks;
        let mut accepted = 0usize;
        let mut worst: Option<DriverError> = None;
        for i in 0..self.members.len() {
            match self.members[i].state {
                MemberState::InSync => {
                    match self.members[i]
                        .device
                        .write_blocks_with_class(lba, buf, class)
                    {
                        Ok(()) => accepted += 1,
                        Err(e) => {
                            worst = Some(most_severe(worst, e));
                            self.members[i].state = MemberState::Faulted;
                            self.members[i].resync_next_lba = 0;
                        }
                    }
                }
                MemberState::Resyncing => {
                    // The rebuild copies [cursor, end-of-array) from an
                    // in-sync source, so only the already-synced region
                    // [0, cursor) must be kept current here. The part at or
                    // above the cursor is picked up by the resync from the
                    // source we just wrote.
                    let cursor = self.members[i].resync_next_lba;
                    if lba < cursor {
                        let overlap = usize::try_from(end.min(cursor) - lba)
                            .map_err(|_| DriverError::LengthOutOfRange)?;
                        let bytes = overlap * bs;
                        if self.members[i]
                            .device
                            .write_blocks_with_class(lba, &buf[..bytes], class)
                            .is_err()
                        {
                            self.members[i].state = MemberState::Faulted;
                            self.members[i].resync_next_lba = 0;
                        }
                    }
                }
                MemberState::Faulted => {}
            }
        }
        if accepted > 0 {
            Ok(())
        } else {
            Err(worst.unwrap_or(DriverError::DeviceOffline))
        }
    }

    /// Copy one bounded chunk of the rebuild for every resyncing member,
    /// advancing each member's cursor. Call repeatedly until
    /// [`needs_resync`](Self::needs_resync) is false.
    ///
    /// `scratch` sizes the chunk (a multiple of the block size); a larger
    /// scratch rebuilds faster, a smaller one yields to other work sooner
    /// (`AGENTS.md` §26.6 — bounded, interruptible, never a busy-spin). Each
    /// chunk is read from an in-sync source and written to the resyncing
    /// member; a member whose cursor reaches the end of the array becomes
    /// [`MemberState::InSync`]. Resync data is treated as
    /// [`BufferClass::Sensitive`] because it copies opaque on-disk bytes that
    /// may include secrets, so member staging buffers are zeroed.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `scratch` is empty or not a
    ///   block-size multiple.
    /// * [`DriverError::DeviceOffline`] if no in-sync source exists to copy
    ///   from (the array has failed).
    /// * The source device's error if reading the chunk fails; the caller
    ///   retries (a source that whole-device-faults is dropped, so the next
    ///   call picks another source).
    pub fn resync_step(&mut self, scratch: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.geometry.block_size as usize;
        if scratch.is_empty() || bs == 0 || !scratch.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let chunk_blocks = (scratch.len() / bs) as u64;
        for t in 0..self.members.len() {
            if self.members[t].state != MemberState::Resyncing {
                continue;
            }
            let cursor = self.members[t].resync_next_lba;
            if cursor >= self.geometry.block_count {
                self.members[t].state = MemberState::InSync;
                self.members[t].resync_next_lba = 0;
                continue;
            }
            let Some(src) = self.first_in_sync() else {
                return Err(DriverError::DeviceOffline);
            };
            let remaining = self.geometry.block_count - cursor;
            let this = chunk_blocks.min(remaining);
            let this_usize = usize::try_from(this).map_err(|_| DriverError::LengthOutOfRange)?;
            let bytes = this_usize * bs;
            if let Err(e) = self.members[src].device.read_blocks_with_class(
                cursor,
                &mut scratch[..bytes],
                BufferClass::Sensitive,
            ) {
                if member_faulting(e) {
                    self.members[src].state = MemberState::Faulted;
                    self.members[src].resync_next_lba = 0;
                }
                return Err(e);
            }
            if self.members[t]
                .device
                .write_blocks_with_class(cursor, &scratch[..bytes], BufferClass::Sensitive)
                .is_err()
            {
                // The rebuild target failed a write: drop it back to faulted
                // rather than leaving a partial copy pretending to be in sync.
                self.members[t].state = MemberState::Faulted;
                self.members[t].resync_next_lba = 0;
                continue;
            }
            let next = cursor + this;
            if next >= self.geometry.block_count {
                self.members[t].state = MemberState::InSync;
                self.members[t].resync_next_lba = 0;
            } else {
                self.members[t].resync_next_lba = next;
            }
        }
        Ok(())
    }

    /// Whether a proactive scrub pass is in progress (i.e.
    /// [`scrub_step`](Self::scrub_step) has more of the array left to verify).
    #[must_use]
    pub const fn scrubbing(&self) -> bool {
        self.scrub_next_lba < self.geometry.block_count
    }

    /// The next logical block a scrub pass will verify (the scrub cursor);
    /// equal to the array block count when no scrub is in progress. Exposed so
    /// the serving process can report scrub progress when logging.
    #[must_use]
    pub const fn scrub_cursor(&self) -> u64 {
        self.scrub_next_lba
    }

    /// Begin a proactive scrub pass from block 0.
    ///
    /// A scrub complements the opportunistic read-repair on the read path. The
    /// read path only ever verifies the copies it consults *before* the first
    /// that serves a block, so a latent media error on a copy that is never
    /// chosen as the read source stays invisible — until the copies ahead of
    /// it are gone, at which point that block is unrecoverable. A scrub
    /// proactively reads *every* in-sync copy of *every* block and repairs a
    /// copy that cannot read a block from one that can, so a bad sector is
    /// found and healed while a good copy still exists (the auto-scrub a
    /// mirror exists to provide, `AGENTS.md` §26.5).
    ///
    /// Drive the pass by calling [`scrub_step`](Self::scrub_step) until
    /// [`scrubbing`](Self::scrubbing) is false. Calling `begin_scrub` again
    /// restarts the pass from block 0.
    pub fn begin_scrub(&mut self) {
        self.scrub_next_lba = 0;
    }

    /// Verify and repair one bounded chunk of a scrub pass, advancing the
    /// scrub cursor. Call repeatedly while [`scrubbing`](Self::scrubbing) is
    /// true; a no-op returning `Ok(())` once the pass is complete.
    ///
    /// `scratch` sizes the chunk (a multiple of the block size); a larger
    /// scratch scrubs faster, a smaller one yields to other work sooner
    /// (`AGENTS.md` §26.6 — bounded, interruptible, never a busy-spin). For the
    /// chunk, every in-sync copy is read:
    ///
    /// * a copy that reads the chunk cleanly is verified good;
    /// * a copy that returns a *whole-device* fault is dropped from the array
    ///   ([`MemberState::Faulted`]), exactly as on the read path;
    /// * a copy that returns a *per-block* media error is **repaired** by
    ///   writing back data read from a good copy, forcing the device to
    ///   reallocate the sector; a repair whose write-back fails drops that
    ///   copy, but the data is safe on the source so it is not a loss.
    ///
    /// A scrub deliberately does **not** arbitrate a *content* disagreement
    /// between two copies that both read cleanly: a bare mirror has no
    /// authority to decide which differing copy is correct, and overwriting
    /// one from another could propagate corruption. Detecting silent
    /// divergence is the checksummed filesystem layer's job (ARXFS), not the
    /// block mirror's; the scrub's remit is latent *media* errors, which it
    /// surfaces and heals here.
    ///
    /// Scrub reads opaque on-disk bytes that may include secrets, so the
    /// staging buffer is treated as [`BufferClass::Sensitive`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `scratch` is empty or not a
    ///   block-size multiple.
    /// * [`DriverError::DeviceOffline`] if no in-sync copy exists to scrub
    ///   from (the array has failed); the cursor does not advance.
    /// * The most fail-closed media error seen if a block in this chunk was bad
    ///   on *every* copy and could not be repaired (a genuine data loss). The
    ///   cursor **still advances** past the chunk in this case — the bad block
    ///   is left for the read path to surface — so a repeated call makes
    ///   progress rather than looping on the loss.
    pub fn scrub_step(&mut self, scratch: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.geometry.block_size as usize;
        if scratch.is_empty() || bs == 0 || !scratch.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        if self.scrub_next_lba >= self.geometry.block_count {
            return Ok(());
        }
        if self.in_sync_count() == 0 {
            return Err(DriverError::DeviceOffline);
        }
        let chunk_blocks = (scratch.len() / bs) as u64;
        let cursor = self.scrub_next_lba;
        let this = chunk_blocks.min(self.geometry.block_count - cursor);
        let this_usize = usize::try_from(this).map_err(|_| DriverError::LengthOutOfRange)?;
        let bytes = this_usize * bs;
        let mut unrepairable: Option<DriverError> = None;
        for i in 0..self.members.len() {
            if self.members[i].state != MemberState::InSync {
                continue;
            }
            match self.members[i].device.read_blocks_with_class(
                cursor,
                &mut scratch[..bytes],
                BufferClass::Sensitive,
            ) {
                Ok(()) => {}
                Err(e) if member_faulting(e) => {
                    self.members[i].state = MemberState::Faulted;
                    self.members[i].resync_next_lba = 0;
                }
                Err(e) => {
                    if !self.repair_chunk(i, cursor, bytes, scratch) {
                        unrepairable = Some(most_severe(unrepairable, e));
                    }
                }
            }
        }
        self.scrub_next_lba = cursor + this;
        match unrepairable {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Repair member `target`'s copy of the chunk at `[cursor, cursor+bytes)`
    /// by writing back data read from another in-sync copy. Returns `true`
    /// once the data was recovered from some copy (whether or not the write
    /// back to `target` succeeded — the data is safe on the source either
    /// way), or `false` if no in-sync copy could read the chunk (a genuine
    /// loss). A whole-device fault in a source drops that source; a write-back
    /// failure drops `target`. `scratch` is the staging buffer.
    fn repair_chunk(
        &mut self,
        target: usize,
        cursor: u64,
        bytes: usize,
        scratch: &mut [u8],
    ) -> bool {
        for j in 0..self.members.len() {
            if j == target || self.members[j].state != MemberState::InSync {
                continue;
            }
            match self.members[j].device.read_blocks_with_class(
                cursor,
                &mut scratch[..bytes],
                BufferClass::Sensitive,
            ) {
                Ok(()) => {
                    if self.members[target]
                        .device
                        .write_blocks_with_class(cursor, &scratch[..bytes], BufferClass::Sensitive)
                        .is_err()
                    {
                        self.members[target].state = MemberState::Faulted;
                        self.members[target].resync_next_lba = 0;
                    }
                    return true;
                }
                Err(e) if member_faulting(e) => {
                    self.members[j].state = MemberState::Faulted;
                    self.members[j].resync_next_lba = 0;
                }
                Err(_) => {}
            }
        }
        false
    }

    /// Begin rebuilding a currently-faulted member from its existing device
    /// (e.g. a device that has returned through its own recovery grace
    /// window, `plans/FIX-IO.md` IO3). The device is re-probed and, if its
    /// geometry matches the array, the member enters
    /// [`MemberState::Resyncing`] from block 0.
    ///
    /// # Errors
    ///
    /// * [`MirrorError::UnknownMember`] if `index` is out of range.
    /// * [`MirrorError::NotFaulted`] if the member is not currently faulted.
    /// * [`MirrorError::ProbeFailed`] if the device cannot be probed.
    /// * [`MirrorError::GeometryMismatch`] if the device's geometry no longer
    ///   matches the array.
    pub fn readd_member(&mut self, index: usize) -> Result<(), MirrorError> {
        let member = self
            .members
            .get_mut(index)
            .ok_or(MirrorError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(MirrorError::NotFaulted);
        }
        match member.device.geometry() {
            Ok(g) if g == self.geometry => {
                member.state = MemberState::Resyncing;
                member.resync_next_lba = 0;
                Ok(())
            }
            Ok(_) => Err(MirrorError::GeometryMismatch),
            Err(_) => Err(MirrorError::ProbeFailed),
        }
    }

    /// Replace a faulted member's device with a fresh one and begin rebuilding
    /// it (a physically-replaced disk). The new device must match the array
    /// geometry.
    ///
    /// # Errors
    ///
    /// As for [`readd_member`](Self::readd_member); on any error the member is
    /// left [`MemberState::Faulted`] holding the new device.
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), MirrorError> {
        let geometry = self.geometry;
        let member = self
            .members
            .get_mut(index)
            .ok_or(MirrorError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(MirrorError::NotFaulted);
        }
        member.device = device;
        member.resync_next_lba = 0;
        match member.device.geometry() {
            Ok(g) if g == geometry => {
                member.state = MemberState::Resyncing;
                Ok(())
            }
            Ok(_) => Err(MirrorError::GeometryMismatch),
            Err(_) => Err(MirrorError::ProbeFailed),
        }
    }

    /// The index of the first in-sync member, if any.
    fn first_in_sync(&self) -> Option<usize> {
        self.members
            .iter()
            .position(|m| m.state == MemberState::InSync)
    }
}

impl<B: Block> Block for MirrorArray<'_, B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.read_impl(lba, buf, BufferClass::NonSensitive)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.write_impl(lba, buf, BufferClass::NonSensitive)
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.read_impl(lba, buf, class)
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.write_impl(lba, buf, class)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        if self.in_sync_count() == 0 {
            return Err(DriverError::DeviceOffline);
        }
        let mut durable = 0usize;
        let mut worst: Option<DriverError> = None;
        for i in 0..self.members.len() {
            match self.members[i].state {
                MemberState::InSync => match self.members[i].device.flush() {
                    Ok(()) => durable += 1,
                    Err(e) => {
                        worst = Some(most_severe(worst, e));
                        self.members[i].state = MemberState::Faulted;
                        self.members[i].resync_next_lba = 0;
                    }
                },
                MemberState::Resyncing => {
                    if self.members[i].device.flush().is_err() {
                        self.members[i].state = MemberState::Faulted;
                        self.members[i].resync_next_lba = 0;
                    }
                }
                MemberState::Faulted => {}
            }
        }
        if durable > 0 {
            Ok(())
        } else {
            Err(worst.unwrap_or(DriverError::DeviceOffline))
        }
    }
}

/// Whether a member that returned `err` on an I/O should be dropped from the
/// array (a whole-device fault or a misbehaving member), as opposed to a
/// per-block or transient error the array can recover around.
fn member_faulting(err: DriverError) -> bool {
    match BlkStatus::for_driver_health(err) {
        // A per-block bad sector, or a transient/reset the child already
        // exhausted its own reissue for: recover around it and keep the copy
        // (a completion status never accompanies an `Err`, but the arm keeps
        // the match total and health-neutral).
        Some(
            BlkStatus::MediumError
            | BlkStatus::TransientError
            | BlkStatus::Reset
            | BlkStatus::Timeout
            | BlkStatus::Ok
            | BlkStatus::Degraded,
        ) => false,
        // The device is gone or unrecoverably faulted, or a member returned a
        // request-level error for a request the array already validated (its
        // geometry has drifted or it is misbehaving): drop it either way.
        Some(BlkStatus::Offline | BlkStatus::Removed | BlkStatus::Fatal) | None => true,
    }
}

/// The more fail-closed of `a` (if any) and `b`, ranked by the shared block
/// health severity so the array reports the worst outcome it saw.
fn most_severe(a: Option<DriverError>, b: DriverError) -> DriverError {
    match a {
        None => b,
        Some(x) => {
            if severity_of(b) >= severity_of(x) {
                b
            } else {
                x
            }
        }
    }
}

/// The block-health severity of a driver error (an unclassifiable error is
/// treated as maximally fail-closed).
fn severity_of(err: DriverError) -> u8 {
    BlkStatus::for_driver_health(err)
        .unwrap_or(BlkStatus::Fatal)
        .severity()
}

#[cfg(test)]
mod tests;
