//! RAID1 mirror composition over the public block seam.
//!
//! [`MirrorArray`] composes a caller-owned slice of [`MirrorMember`]s into one
//! logical [`Block`] device. It is `no_std` and allocation-free: the member set
//! is a slice the caller owns (the growable tier lives in the assembling serve
//! process), so the array imposes no fixed member ceiling and holds only a
//! borrow.

use crate::superblock::ArrayProgress;
use tairix_abi::blkio::{BlkDeviceClass, BlkStatus};
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth};
use tairix_abi::driver::{BufferClass, DriverError};
use tairix_abi::raid::{ArrayHealth, MemberState, SlotDisposition};
use tairix_abi::sysinfo::MountAvailability;

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
    /// sync" means: a slot the metadata marked a stale rebuild target
    /// (`in_sync == false`) becomes a [`Stale`](Self::Stale) member, never a
    /// trusted read source.
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

/// One mirror slot: an *optional* child [`Block`] device plus the role it
/// joined with, its membership state and, while [`MemberState::Resyncing`],
/// the rebuild cursor (the first not-yet-copied logical block).
///
/// A slot with no device is [`MemberState::Absent`] (a missing member the
/// array is defined to have); every other state has a device. That invariant
/// (`device.is_some()` iff the state is not `Absent`) is established by the
/// constructors and preserved by every reconfiguration operation.
pub struct MirrorMember<B: Block> {
    device: Option<B>,
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
            device: Some(device),
            role,
            state: MemberState::InSync,
            resync_next_lba: 0,
        }
    }

    /// A slot the array is *defined* to have but for which no device is
    /// present ([`MemberState::Absent`]) — a missing member, the equivalent
    /// of a Linux md "removed" slot.
    ///
    /// Pass one per missing member when assembling so the array knows its
    /// full width: the assembled array counts the absent slot toward its
    /// member count and reports [`ArrayHealth::Degraded`] for the reduced
    /// redundancy, and a spare can later be installed into it with
    /// [`MirrorArray::add_member`]. This is how a reassembler represents a
    /// [`SlotDisposition::Missing`] slot, for which [`MemberRole::for_slot`]
    /// yields [`None`].
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            device: None,
            role: MemberRole::Current,
            state: MemberState::Absent,
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

    /// Resume this member's rebuild at `cursor`, if it is rebuilding at all.
    ///
    /// Only a [`MemberState::Resyncing`] slot has a rebuild to resume: a
    /// cursor is never planted on an in-sync, faulted, or absent member, since
    /// that would describe a rebuild that is not happening. The caller has
    /// already checked the cursor against the array
    /// ([`ArrayProgress::fits_span`]), so it names a block the member has.
    ///
    /// Shared by the mirror and by the RAID10 stripe of mirrors, which is built
    /// from these same members.
    pub(crate) const fn resume_resync(&mut self, cursor: u64) {
        if matches!(self.state, MemberState::Resyncing) {
            self.resync_next_lba = cursor;
        }
    }

    /// Borrow the underlying device (for identity/health queries), or [`None`]
    /// for an [`MemberState::Absent`] slot that has no device.
    #[must_use]
    pub const fn device(&self) -> Option<&B> {
        self.device.as_ref()
    }

    /// Mutably borrow the underlying device (for a caller that must reach a
    /// member's own device — e.g. its reserved array-metadata blocks —
    /// rather than the array's data), or [`None`] for an
    /// [`MemberState::Absent`] slot that has no device.
    #[must_use]
    pub fn device_mut(&mut self) -> Option<&mut B> {
        self.device.as_mut()
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
    /// [`MirrorArray::add_member`] was asked to populate a slot that already
    /// holds a device (it is not [`MemberState::Absent`]). Vacate it first
    /// with [`MirrorArray::remove_member`], or hot-swap it with
    /// [`MirrorArray::replace_member`].
    SlotOccupied,
    /// A restored maintenance cursor named a block outside the array, so it
    /// cannot have come from this array in this shape. Refused rather than
    /// clamped: adopted as a rebuild position it would declare a member fully
    /// copied without its tail ever having been written.
    CursorOutOfRange,
}

/// A RAID1 mirror presenting several child [`Block`] copies as one logical
/// device.
///
/// See the [crate documentation](crate) for the mirror's fault-recovery
/// behaviour. The array borrows a caller-owned member slice, so it holds no
/// allocation and imposes no fixed member ceiling.
pub struct MirrorArray<'a, B: Block> {
    members: &'a mut [MirrorMember<B>],
    geometry: BlockGeometry,
    /// The next logical block a scrub pass will verify, or the array block
    /// count when no scrub is in progress (auto-scrub).
    scrub_next_lba: u64,
}

impl<'a, B: Block> MirrorArray<'a, B> {
    /// Assemble a mirror from `members`, establishing the array geometry from
    /// the members that can report it.
    ///
    /// The `members` slice is the array's full member table: one entry per
    /// slot the array is *defined* to have, in slot order. A missing member
    /// (a slot no device currently fills) is passed as
    /// [`MirrorMember::absent`] so the assembled array knows its true width
    /// and reports the reduced redundancy as [`ArrayHealth::Degraded`] rather
    /// than silently presenting as a smaller, optimal array.
    ///
    /// Every *present* member's device is probed for geometry. The first that
    /// reports one fixes the array geometry; a member reporting a *different*
    /// geometry fails the whole assembly closed
    /// ([`MirrorError::GeometryMismatch`]) — differently-sized copies are not
    /// a mirror. A present member whose probe *errors* (absent/unwell) is
    /// admitted [`MemberState::Faulted`] so the array assembles degraded
    /// rather than refusing to come up while one copy is down; it can be
    /// re-added later. An [`MirrorMember::absent`] slot contributes no device
    /// to probe and stays [`MemberState::Absent`].
    ///
    /// # Errors
    ///
    /// * [`MirrorError::NoMembers`] if `members` is empty.
    /// * [`MirrorError::GeometryMismatch`] if two members disagree on
    ///   geometry.
    /// * [`MirrorError::NoUsableMember`] if no member could report geometry
    ///   (every slot is absent, or every present copy is unwell).
    pub fn assemble(members: &'a mut [MirrorMember<B>]) -> Result<Self, MirrorError> {
        if members.is_empty() {
            return Err(MirrorError::NoMembers);
        }
        let mut geometry: Option<BlockGeometry> = None;
        for member in &mut *members {
            // An absent slot has no device to probe: it stays absent and
            // counts toward the array width so a missing member degrades the
            // array rather than shrinking it.
            let Some(device) = member.device.as_ref() else {
                member.state = MemberState::Absent;
                member.resync_next_lba = 0;
                continue;
            };
            let probe = device.geometry();
            let Ok(g) = probe else {
                // A present copy that cannot report its geometry is
                // absent/unwell: admit it faulted (device retained) so the
                // array still comes up on the rest.
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

    /// Wrap an already-prepared member sub-slice as a mirror view without
    /// re-probing geometry.
    ///
    /// The RAID10 stripe-of-mirrors composition
    /// ([`Raid10Array`](crate::Raid10Array)) drives each of its mirror pairs
    /// through this one mirror implementation rather than copying the
    /// recover/repair/rebuild logic: it probes the members once at its own
    /// `assemble` and then, per striped chunk, builds a transient pair view
    /// here (an allocation-free borrow) to serve the read/write/scrub/rebuild
    /// for that pair. `geometry` is the per-member geometry every pair shares
    /// and `scrub_next_lba` carries the pair's scrub cursor (the array block
    /// count when no scrub is in progress); the per-member rebuild cursor lives
    /// in each [`MirrorMember`], so it persists across transient views.
    ///
    /// [`OwnedRaidArray`](crate::owned::OwnedRaidArray) reuses this same
    /// idiom for a top-level mirror: it owns its members on the heap and
    /// builds a transient view here per operation instead of calling
    /// `assemble` again, which would re-derive every member's state from a
    /// fresh probe and silently re-admit one that faulted while serving.
    pub(crate) fn from_prepared(
        members: &'a mut [MirrorMember<B>],
        geometry: BlockGeometry,
        scrub_next_lba: u64,
    ) -> Self {
        Self {
            members,
            geometry,
            scrub_next_lba,
        }
    }

    /// The array's logical geometry (shared by every copy).
    #[must_use]
    pub const fn array_geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// The number of member slots the array is defined to have (in sync,
    /// faulted, resyncing, or absent alike).
    #[must_use]
    pub const fn member_count(&self) -> usize {
        self.members.len()
    }

    /// The state of member `index`, or [`None`] if `index` is out of range.
    #[must_use]
    pub fn member_state(&self, index: usize) -> Option<MemberState> {
        self.members.get(index).map(MirrorMember::state)
    }

    /// Mutably borrow the device held by member `index`, or [`None`] if
    /// `index` is out of range or the slot holds no device — the mutable
    /// companion of [`member_state`](Self::member_state), for a caller that
    /// must reach a member's own device (its reserved array-metadata blocks)
    /// rather than the array's data.
    #[must_use]
    pub fn member_device_mut(&mut self, index: usize) -> Option<&mut B> {
        self.members.get_mut(index)?.device_mut()
    }

    /// Borrow member `index` (for the serving process to inspect a copy's
    /// device identity, health, or rebuild cursor when logging), or [`None`]
    /// if `index` is out of range.
    #[must_use]
    pub fn member(&self, index: usize) -> Option<&MirrorMember<B>> {
        self.members.get(index)
    }

    /// The current [`ArrayHealth`], derived from the members' states.
    ///
    /// The array is [`Optimal`](ArrayHealth::Optimal) only when *every* slot
    /// holds an in-sync copy: a faulted **or absent** (missing) slot reduces
    /// redundancy and reports [`Degraded`](ArrayHealth::Degraded), so a mirror
    /// short a member never masquerades as fully redundant. A slot actively
    /// rebuilding reports [`Recovering`](ArrayHealth::Recovering); no in-sync
    /// copy at all is [`Failed`](ArrayHealth::Failed).
    #[must_use]
    pub fn health(&self) -> ArrayHealth {
        crate::health::mirror_health(self.members.iter().map(MirrorMember::state))
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
            let Some(device) = self.members[i].device.as_mut() else {
                // An in-sync slot must hold a device; a broken invariant
                // fails closed rather than reading from nothing.
                self.members[i].state = MemberState::Faulted;
                self.members[i].resync_next_lba = 0;
                continue;
            };
            let outcome = device.read_blocks_with_class(lba, buf, class);
            match outcome {
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
            if self.members[i].state != MemberState::InSync {
                continue;
            }
            let Some(device) = self.members[i].device.as_mut() else {
                self.members[i].state = MemberState::Faulted;
                self.members[i].resync_next_lba = 0;
                continue;
            };
            if device.write_blocks_with_class(lba, buf, class).is_err() {
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
                    let Some(device) = self.members[i].device.as_mut() else {
                        self.members[i].state = MemberState::Faulted;
                        self.members[i].resync_next_lba = 0;
                        continue;
                    };
                    let outcome = device.write_blocks_with_class(lba, buf, class);
                    match outcome {
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
                        let Some(device) = self.members[i].device.as_mut() else {
                            self.members[i].state = MemberState::Faulted;
                            self.members[i].resync_next_lba = 0;
                            continue;
                        };
                        if device
                            .write_blocks_with_class(lba, &buf[..bytes], class)
                            .is_err()
                        {
                            self.members[i].state = MemberState::Faulted;
                            self.members[i].resync_next_lba = 0;
                        }
                    }
                }
                // A faulted or absent slot receives no write: the former is
                // dropped pending re-add, the latter holds no device.
                MemberState::Faulted | MemberState::Absent => {}
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
    /// (bounded, interruptible, never a busy-spin). Each chunk is read from an
    /// in-sync source and written to the resyncing member; a member whose
    /// cursor reaches the end of the array becomes [`MemberState::InSync`].
    /// Resync data is treated as [`BufferClass::Sensitive`] because it copies
    /// opaque on-disk bytes that may include secrets, so member staging buffers
    /// are zeroed.
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
            let read_result = match self.members[src].device.as_mut() {
                Some(device) => device.read_blocks_with_class(
                    cursor,
                    &mut scratch[..bytes],
                    BufferClass::Sensitive,
                ),
                // An in-sync source must hold a device; fail closed if not.
                None => Err(DriverError::DeviceOffline),
            };
            if let Err(e) = read_result {
                if member_faulting(e) {
                    self.members[src].state = MemberState::Faulted;
                    self.members[src].resync_next_lba = 0;
                }
                return Err(e);
            }
            let write_failed = match self.members[t].device.as_mut() {
                Some(device) => device
                    .write_blocks_with_class(cursor, &scratch[..bytes], BufferClass::Sensitive)
                    .is_err(),
                // A resyncing target must hold a device; fail closed.
                None => true,
            };
            if write_failed {
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
    /// chosen as the read source stays invisible — until the copies ahead of it
    /// are gone, at which point that block is unrecoverable. A scrub
    /// proactively reads *every* in-sync copy of *every* block and repairs a
    /// copy that cannot read a block from one that can, so a bad sector is
    /// found and healed while a good copy still exists (the auto-scrub a mirror
    /// exists to provide).
    ///
    /// Drive the pass by calling [`scrub_step`](Self::scrub_step) until
    /// [`scrubbing`](Self::scrubbing) is false. Calling `begin_scrub` again
    /// restarts the pass from block 0.
    pub fn begin_scrub(&mut self) {
        self.scrub_next_lba = 0;
    }

    /// The array's resumable maintenance position: how far the current scrub
    /// pass and rebuild have got, or [`ArrayProgress::IDLE`] if neither is
    /// running.
    ///
    /// This is what the serving process checkpoints to the members' on-disk
    /// maintenance record, so a pass measured in hours survives a restart.
    /// Several members can rebuild at once with different cursors, and one
    /// record can only carry a single position, so the **least advanced** is
    /// reported: resuming from it re-copies blocks a further-ahead member
    /// already had (harmless — a rebuild write is idempotent) and can never
    /// skip a block that was still outstanding.
    #[must_use]
    pub fn progress(&self) -> ArrayProgress {
        ArrayProgress {
            scrub_cursor: self.scrubbing().then_some(self.scrub_next_lba),
            resync_cursor: self
                .members
                .iter()
                .filter(|m| m.state == MemberState::Resyncing)
                .map(|m| m.resync_next_lba)
                .min(),
        }
    }

    /// Resume maintenance at a previously checkpointed `progress`.
    ///
    /// Called once after [`assemble`](Self::assemble), before any maintenance
    /// step, with the position read back from the members' on-disk record. A
    /// position the record could not vouch for arrives as
    /// [`ArrayProgress::IDLE`] and simply leaves the array at its fresh-start
    /// position, so a lost or foreign record costs time and never correctness.
    ///
    /// A rebuild cursor is planted only on the members that are actually
    /// rebuilding, which the assembled member table decides: a member that
    /// rejoined as in sync is untouched, so a restored cursor can never
    /// un-sync a current copy.
    ///
    /// # Errors
    ///
    /// [`MirrorError::CursorOutOfRange`] if a cursor names a block outside the
    /// array. The array is left exactly as it was, so the caller can proceed
    /// from the fresh-start position.
    pub fn restore_progress(&mut self, progress: ArrayProgress) -> Result<(), MirrorError> {
        if !progress.fits_span(self.geometry.block_count) {
            return Err(MirrorError::CursorOutOfRange);
        }
        if let Some(cursor) = progress.scrub_cursor {
            self.scrub_next_lba = cursor;
        }
        if let Some(cursor) = progress.resync_cursor {
            for member in &mut *self.members {
                member.resume_resync(cursor);
            }
        }
        Ok(())
    }

    /// Verify and repair one bounded chunk of a scrub pass, advancing the
    /// scrub cursor. Call repeatedly while [`scrubbing`](Self::scrubbing) is
    /// true; a no-op returning `Ok(())` once the pass is complete.
    ///
    /// `scratch` sizes the chunk (a multiple of the block size); a larger
    /// scratch scrubs faster, a smaller one yields to other work sooner
    /// (bounded, interruptible, never a busy-spin). For the chunk, every
    /// in-sync copy is read:
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
            let outcome = if let Some(device) = self.members[i].device.as_mut() {
                device.read_blocks_with_class(cursor, &mut scratch[..bytes], BufferClass::Sensitive)
            } else {
                // An in-sync slot must hold a device; fail closed.
                self.members[i].state = MemberState::Faulted;
                self.members[i].resync_next_lba = 0;
                continue;
            };
            match outcome {
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
            let source_read = if let Some(device) = self.members[j].device.as_mut() {
                device.read_blocks_with_class(cursor, &mut scratch[..bytes], BufferClass::Sensitive)
            } else {
                // An in-sync source must hold a device; fail closed.
                self.members[j].state = MemberState::Faulted;
                self.members[j].resync_next_lba = 0;
                continue;
            };
            match source_read {
                Ok(()) => {
                    let write_failed = match self.members[target].device.as_mut() {
                        Some(device) => device
                            .write_blocks_with_class(
                                cursor,
                                &scratch[..bytes],
                                BufferClass::Sensitive,
                            )
                            .is_err(),
                        // The repair target holds no device: nothing to write
                        // back to, but the data is safe on the source, so this
                        // is still a successful recovery for the read/scrub.
                        None => true,
                    };
                    if write_failed {
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
        let geometry = self.geometry;
        let member = self
            .members
            .get_mut(index)
            .ok_or(MirrorError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(MirrorError::NotFaulted);
        }
        let Some(device) = member.device.as_ref() else {
            // A faulted slot must retain its device to be re-added; a broken
            // invariant leaves nothing to probe, so fail closed.
            return Err(MirrorError::ProbeFailed);
        };
        match device.geometry() {
            Ok(g) if g == geometry => {
                member.state = MemberState::Resyncing;
                member.resync_next_lba = 0;
                Ok(())
            }
            Ok(_) => Err(MirrorError::GeometryMismatch),
            Err(_) => Err(MirrorError::ProbeFailed),
        }
    }

    /// Replace a faulted member's device with a fresh one and begin rebuilding
    /// it (a physically-replaced disk hot-swapped into a still-occupied slot).
    /// The new device must match the array geometry.
    ///
    /// # Errors
    ///
    /// * [`MirrorError::UnknownMember`] if `index` is out of range.
    /// * [`MirrorError::NotFaulted`] if the member is not currently faulted
    ///   (only a dropped member is replaced; an absent slot uses
    ///   [`add_member`](Self::add_member)).
    /// * [`MirrorError::GeometryMismatch`] / [`MirrorError::ProbeFailed`] if
    ///   the new device's geometry does not match or it cannot be probed; on
    ///   either the slot is left [`MemberState::Faulted`] holding the new
    ///   device.
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), MirrorError> {
        match self.members.get(index) {
            Some(member) if member.state == MemberState::Faulted => {}
            Some(_) => return Err(MirrorError::NotFaulted),
            None => return Err(MirrorError::UnknownMember),
        }
        self.install_rebuild_target(index, device)
    }

    /// Install a spare `device` into a currently-[`MemberState::Absent`]
    /// (missing) slot and begin rebuilding it from a surviving copy — the Linux
    /// md "add a spare to a removed slot" operation that restores a missing
    /// member's redundancy without a reboot.
    ///
    /// The slot moves [`MemberState::Absent`] → [`MemberState::Resyncing`] on
    /// success; the rebuild is driven by [`resync_step`](Self::resync_step)
    /// exactly as for a returned copy, so a spare never serves reads until it
    /// is fully in sync.
    ///
    /// # Errors
    ///
    /// * [`MirrorError::UnknownMember`] if `index` is out of range.
    /// * [`MirrorError::SlotOccupied`] if the slot already holds a device
    ///   (it is not absent); vacate it first with
    ///   [`remove_member`](Self::remove_member), or use
    ///   [`replace_member`](Self::replace_member) to hot-swap a faulted one.
    /// * [`MirrorError::GeometryMismatch`] / [`MirrorError::ProbeFailed`] if
    ///   the spare's geometry does not match or it cannot be probed; on
    ///   either the slot is left [`MemberState::Faulted`] holding the spare.
    pub fn add_member(&mut self, index: usize, device: B) -> Result<(), MirrorError> {
        match self.members.get(index) {
            Some(member) if member.state == MemberState::Absent => {}
            Some(_) => return Err(MirrorError::SlotOccupied),
            None => return Err(MirrorError::UnknownMember),
        }
        self.install_rebuild_target(index, device)
    }

    /// Remove a faulted member's device from its slot, leaving the slot
    /// [`MemberState::Absent`] and returning the removed device — the Linux
    /// md "remove a failed disk" operation. The vacated slot keeps counting
    /// toward the array's width (so the array stays
    /// [`ArrayHealth::Degraded`]), and a spare can be installed into it with
    /// [`add_member`](Self::add_member).
    ///
    /// Only a [`MemberState::Faulted`] member is removed: an in-sync or
    /// resyncing member is still participating and must fault before it can be
    /// pulled, and an already-absent slot has nothing to remove.
    ///
    /// # Errors
    ///
    /// * [`MirrorError::UnknownMember`] if `index` is out of range.
    /// * [`MirrorError::NotFaulted`] if the member is not currently faulted.
    pub fn remove_member(&mut self, index: usize) -> Result<B, MirrorError> {
        let member = self
            .members
            .get_mut(index)
            .ok_or(MirrorError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(MirrorError::NotFaulted);
        }
        let Some(device) = member.device.take() else {
            // A faulted slot must retain its device (invariant); with none
            // present the slot is already effectively empty — fail closed.
            member.state = MemberState::Absent;
            member.resync_next_lba = 0;
            return Err(MirrorError::NotFaulted);
        };
        member.state = MemberState::Absent;
        member.resync_next_lba = 0;
        Ok(device)
    }

    /// Install `device` into slot `index` and begin rebuilding it from a
    /// surviving copy, discarding any device the slot previously held. On a
    /// geometry mismatch or a probe failure the slot is left
    /// [`MemberState::Faulted`] holding the new device (present but unusable),
    /// failing closed rather than admitting an unverified copy as a read
    /// source. The single definition shared by
    /// [`replace_member`](Self::replace_member) and
    /// [`add_member`](Self::add_member), so the install-then-rebuild policy
    /// cannot diverge between the two.
    fn install_rebuild_target(&mut self, index: usize, device: B) -> Result<(), MirrorError> {
        let geometry = self.geometry;
        let member = self
            .members
            .get_mut(index)
            .ok_or(MirrorError::UnknownMember)?;
        member.device = Some(device);
        member.resync_next_lba = 0;
        let Some(installed) = member.device.as_ref() else {
            // Unreachable: a device was just installed.
            member.state = MemberState::Absent;
            return Err(MirrorError::ProbeFailed);
        };
        match installed.geometry() {
            Ok(g) if g == geometry => {
                member.state = MemberState::Resyncing;
                Ok(())
            }
            Ok(_) => {
                member.state = MemberState::Faulted;
                Err(MirrorError::GeometryMismatch)
            }
            Err(_) => {
                member.state = MemberState::Faulted;
                Err(MirrorError::ProbeFailed)
            }
        }
    }

    /// The devices of the members that speak for the array in its
    /// device-level answers (health, class), selected by the one shared
    /// participation predicate.
    fn live_devices(&self) -> impl Iterator<Item = &B> {
        self.members
            .iter()
            .filter(|m| crate::health::member_participates(m.state))
            .filter_map(|m| m.device.as_ref())
    }

    /// The index of the first in-sync member, if any.
    fn first_in_sync(&self) -> Option<usize> {
        self.members
            .iter()
            .position(|m| m.state == MemberState::InSync)
    }
}

impl<B: Block> Block for MirrorArray<'_, B> {
    fn device_class(&self) -> BlkDeviceClass {
        crate::health::aggregate_device_class(self.live_devices().map(Block::device_class))
    }

    fn backing_availability(&self) -> MountAvailability {
        crate::health::aggregate_backing_availability(
            self.health(),
            self.live_devices().map(Block::backing_availability),
        )
    }

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
                MemberState::InSync => {
                    let outcome = match self.members[i].device.as_mut() {
                        Some(device) => device.flush(),
                        // An in-sync slot must hold a device; fail closed.
                        None => Err(DriverError::DeviceOffline),
                    };
                    match outcome {
                        Ok(()) => durable += 1,
                        Err(e) => {
                            worst = Some(most_severe(worst, e));
                            self.members[i].state = MemberState::Faulted;
                            self.members[i].resync_next_lba = 0;
                        }
                    }
                }
                MemberState::Resyncing => {
                    let failed = match self.members[i].device.as_mut() {
                        Some(device) => device.flush().is_err(),
                        None => true,
                    };
                    if failed {
                        self.members[i].state = MemberState::Faulted;
                        self.members[i].resync_next_lba = 0;
                    }
                }
                // A faulted slot is dropped pending re-add; an absent slot
                // holds no device: neither has anything to flush.
                MemberState::Faulted | MemberState::Absent => {}
            }
        }
        if durable > 0 {
            Ok(())
        } else {
            Err(worst.unwrap_or(DriverError::DeviceOffline))
        }
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(crate::health::aggregate_device_health(
            self.live_devices().map(Block::device_health),
        ))
    }
}

/// Whether a member that returned `err` on an I/O should be dropped from the
/// array (a whole-device fault or a misbehaving member), as opposed to a
/// per-block or transient error the array can recover around.
///
/// Shared with the striped composition ([`crate::StripeArray`]) so both RAID
/// levels classify "is this a dead device or a recoverable per-block error?"
/// through one definition.
pub(crate) fn member_faulting(err: DriverError) -> bool {
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
