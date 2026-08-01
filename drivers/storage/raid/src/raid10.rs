//! The RAID10 **stripe of mirrors** composition: an even number of child
//! [`Block`] members paired into two-copy mirrors, with the logical block
//! space striped in fixed-size chunks across the pairs (`plans/FIX-IO.md`
//! IO6).
//!
//! RAID10 is the capacity-*and*-redundancy layout most commonly deployed in
//! the field: it survives any member fault — and several at once — as long as
//! no mirror pair loses *both* copies, while still spreading a large transfer
//! across every pair for bandwidth. It is a sibling of the RAID0
//! [`StripeArray`] and RAID1 [`MirrorArray`]
//! over the same block seam (`AGENTS.md` §2.2 parallel implementations).
//!
//! # It is a composition, not a re-implementation
//!
//! A stripe of mirrors *is* a stripe over mirrors, so this engine composes the
//! two it is built from rather than copying their logic (`AGENTS.md` §2.2):
//!
//! * the **striping map** ([`StripeArray::locate`](crate::stripe::StripeArray::locate))
//!   places each logical chunk on the pair (column) that holds it, exactly as
//!   RAID0 does across members;
//! * each **mirror pair** is driven through the one
//!   [`MirrorArray`] implementation — recover-from-a-good-copy,
//!   opportunistic read-repair, write fan-out, bounded rebuild, and scrub — by
//!   building a transient [`MirrorArray::from_prepared`] view over the pair's
//!   two members per operation (an allocation-free borrow, `AGENTS.md` §24.1).
//!
//! So RAID10 adds only the *pairing* and the *aggregation of pair health into
//! array health*; the fault-recovery behaviour is the mirror's, verified once.
//!
//! # Fault model
//!
//! * A pair with one copy down is [`ArrayHealth::Degraded`] (or
//!   [`ArrayHealth::Recovering`] while that copy rebuilds) but keeps serving
//!   from the survivor — the read path recovers and repairs through the mirror.
//! * A pair that loses *both* copies can no longer serve its stripes, so the
//!   whole array is [`ArrayHealth::Failed`] and every I/O fails closed
//!   ([`DriverError::DeviceOffline`], `AGENTS.md` §5.4): a stripe cannot present
//!   a partial logical block space.
//! * The array is [`ArrayHealth::Optimal`] only when every pair holds two
//!   in-sync copies.
//!
//! # Allocation-free
//!
//! Like its siblings, [`Raid10Array`] borrows a caller-owned member slice, so
//! it holds no allocation and imposes no fixed member ceiling (`AGENTS.md`
//! §24.1); the growable member tier lives in the assembling serve process.

use crate::mirror::{ArrayHealth, MemberState, MirrorArray, MirrorError, MirrorMember};
use crate::stripe::StripeArray;
use crate::superblock::{ArrayProgress, RaidLevel};
use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth};
use tairix_abi::driver::{BufferClass, DriverError};

/// A reason a RAID10 array could not be assembled. Distinct from
/// [`DriverError`] (which flows on the I/O path) because these are
/// composition-policy failures, not device I/O outcomes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Raid10Error {
    /// The member slice was empty.
    NoMembers,
    /// The stripe unit (`chunk_blocks`) was zero; a striped level needs a
    /// positive chunk size.
    ZeroChunk,
    /// The member count is odd: RAID10 pairs its copies two per mirror, so an
    /// odd count cannot be composed. Fails closed rather than dropping a
    /// member.
    OddMembers,
    /// Fewer than four members (two two-copy pairs): the layout is a plain
    /// mirror, not a stripe of mirrors.
    TooFewMembers,
    /// Two members report different geometry: they cannot form one evenly
    /// striped, evenly mirrored array.
    GeometryMismatch,
    /// No member could report geometry (every pair is fully down), so the
    /// array shape cannot be established.
    NoUsableMember,
    /// A member's size is not a whole number of stripe chunks, so striping it
    /// would leave a ragged tail.
    UnalignedGeometry,
    /// The composed logical block count overflows `u64`.
    TooLarge,
    /// A member-management request named a slot outside the array.
    UnknownMember,
    /// A re-add/replace target is not currently faulted (nothing to rebuild).
    NotFaulted,
    /// A re-add/replace/add target device could not be probed (absent/unwell),
    /// so it cannot begin rebuilding yet.
    ProbeFailed,
    /// [`Raid10Array::add_member`] was asked to populate a slot that already
    /// holds a device; vacate it with
    /// [`Raid10Array::remove_member`] or hot-swap it with
    /// [`Raid10Array::replace_member`] first.
    SlotOccupied,
    /// A restored maintenance cursor named a block outside a member, so it
    /// cannot have come from this array in this shape. Refused rather than
    /// clamped: adopted as a rebuild position it would declare a copy fully
    /// rebuilt without its tail ever having been written.
    CursorOutOfRange,
}

impl From<MirrorError> for Raid10Error {
    fn from(err: MirrorError) -> Self {
        match err {
            MirrorError::NoMembers => Self::NoMembers,
            MirrorError::NoUsableMember => Self::NoUsableMember,
            MirrorError::GeometryMismatch => Self::GeometryMismatch,
            MirrorError::UnknownMember => Self::UnknownMember,
            MirrorError::NotFaulted => Self::NotFaulted,
            MirrorError::ProbeFailed => Self::ProbeFailed,
            MirrorError::SlotOccupied => Self::SlotOccupied,
            MirrorError::CursorOutOfRange => Self::CursorOutOfRange,
        }
    }
}

/// A RAID10 stripe of two-copy mirrors presenting an even number of child
/// [`Block`] members as one logical device of half their combined capacity.
///
/// See this module's documentation for the layout, the composed fault model,
/// and how each mirror pair is driven through the one
/// [`MirrorArray`] implementation. The array borrows a
/// caller-owned member slice, so it holds no allocation and imposes no fixed
/// member ceiling (`AGENTS.md` §24.1).
pub struct Raid10Array<'a, B: Block> {
    /// The full member table in slot order; members `2k` and `2k+1` form
    /// mirror pair (column) `k`.
    members: &'a mut [MirrorMember<B>],
    /// The logical geometry the array presents (block size shared with the
    /// members; block count is `pairs × per_member`).
    geometry: BlockGeometry,
    /// The geometry each individual member (and so each mirror pair) presents.
    per_member: BlockGeometry,
    /// The stripe unit in logical blocks: consecutive blocks placed on one
    /// pair before the stripe moves to the next.
    chunk_blocks: u64,
    /// The next *member-local* block a scrub pass will verify, or the
    /// per-member block count when no scrub is in progress. Every pair shares
    /// the same per-member geometry, so one member-local cursor drives the
    /// scrub of every pair uniformly.
    scrub_next_lba: u64,
}

impl<'a, B: Block> Raid10Array<'a, B> {
    /// Assemble a RAID10 array from `members` with stripe unit `chunk_blocks`
    /// logical blocks.
    ///
    /// `members` is the array's full member table in slot order; members `2k`
    /// and `2k+1` form mirror pair `k`. A missing copy is passed as
    /// [`MirrorMember::absent`] so the array knows its true width. Each pair is
    /// probed through [`MirrorArray::assemble`] (so the mirror's probing and
    /// state rules are reused, `AGENTS.md` §2.2); every present member must
    /// report the same geometry, a non-degenerate one, and a block count that
    /// is a whole number of `chunk_blocks`.
    ///
    /// # Errors
    ///
    /// * [`Raid10Error::NoMembers`] / [`Raid10Error::ZeroChunk`] /
    ///   [`Raid10Error::OddMembers`] / [`Raid10Error::TooFewMembers`] for a
    ///   malformed member table or chunk.
    /// * [`Raid10Error::GeometryMismatch`] if two members disagree on geometry.
    /// * [`Raid10Error::NoUsableMember`] if no member could report geometry.
    /// * [`Raid10Error::UnalignedGeometry`] if a member's block count is not a
    ///   multiple of `chunk_blocks`.
    /// * [`Raid10Error::TooLarge`] if the composed block count overflows `u64`.
    pub fn assemble(
        members: &'a mut [MirrorMember<B>],
        chunk_blocks: u32,
    ) -> Result<Self, Raid10Error> {
        if members.is_empty() {
            return Err(Raid10Error::NoMembers);
        }
        if chunk_blocks == 0 {
            return Err(Raid10Error::ZeroChunk);
        }
        if !members.len().is_multiple_of(2) {
            return Err(Raid10Error::OddMembers);
        }
        if members.len() < RaidLevel::Raid10.min_members() as usize {
            return Err(Raid10Error::TooFewMembers);
        }
        let chunk = u64::from(chunk_blocks);
        let member_count = members.len() as u64;
        let mut per_member: Option<BlockGeometry> = None;
        for pair in members.chunks_mut(2) {
            match MirrorArray::assemble(pair) {
                Ok(mirror) => {
                    let g = mirror.array_geometry();
                    match per_member {
                        None => per_member = Some(g),
                        Some(existing) if existing == g => {}
                        Some(_) => return Err(Raid10Error::GeometryMismatch),
                    }
                }
                // A fully-down pair set its members' states while probing; the
                // array can still assemble its geometry from another pair. It
                // will report `Failed` (that pair cannot serve its stripes).
                Err(MirrorError::NoUsableMember) => {}
                Err(other) => return Err(Raid10Error::from(other)),
            }
        }
        let Some(per_member) = per_member else {
            return Err(Raid10Error::NoUsableMember);
        };
        if !per_member.block_count.is_multiple_of(chunk) {
            return Err(Raid10Error::UnalignedGeometry);
        }
        let block_count = RaidLevel::Raid10
            .logical_block_count(per_member.block_count, member_count)
            .ok_or(Raid10Error::TooLarge)?;
        Ok(Self {
            members,
            geometry: BlockGeometry {
                block_size: per_member.block_size,
                block_count,
            },
            per_member,
            chunk_blocks: chunk,
            scrub_next_lba: per_member.block_count,
        })
    }

    /// The number of mirror pairs (stripe columns) the array is composed of.
    const fn pairs(&self) -> u64 {
        self.members.len() as u64 / 2
    }

    /// The logical geometry of the composed array (block size shared with the
    /// members; block count is `pairs × per_member`).
    #[must_use]
    pub const fn array_geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// The number of member slots the array is defined to have.
    #[must_use]
    pub const fn member_count(&self) -> usize {
        self.members.len()
    }

    /// The [`MemberState`] of member `index`, or [`None`] if out of range.
    #[must_use]
    pub fn member_state(&self, index: usize) -> Option<MemberState> {
        self.members.get(index).map(MirrorMember::state)
    }

    /// Borrow member `index` (for the serving process to inspect a copy's
    /// device identity, health, or rebuild cursor when logging), or [`None`]
    /// if out of range.
    #[must_use]
    pub fn member(&self, index: usize) -> Option<&MirrorMember<B>> {
        self.members.get(index)
    }

    /// Build the transient mirror view over pair `column` for one operation.
    /// `scrub_next_lba` seeds the pair's scrub cursor (the per-member block
    /// count when no scrub is in progress).
    fn pair(&mut self, column: usize, scrub_next_lba: u64) -> MirrorArray<'_, B> {
        let base = column * 2;
        MirrorArray::from_prepared(
            &mut self.members[base..base + 2],
            self.per_member,
            scrub_next_lba,
        )
    }

    /// The current [`ArrayHealth`], aggregated across the mirror pairs.
    ///
    /// [`Failed`](ArrayHealth::Failed) if any pair has lost both copies (it
    /// cannot serve its stripes); otherwise [`Recovering`](ArrayHealth::Recovering)
    /// if any pair is rebuilding, [`Degraded`](ArrayHealth::Degraded) if any
    /// pair is short a copy, and [`Optimal`](ArrayHealth::Optimal) only when
    /// every pair holds two in-sync copies.
    #[must_use]
    pub fn health(&self) -> ArrayHealth {
        let mut recovering = false;
        let mut degraded = false;
        for column in 0..self.members.len() / 2 {
            let base = column * 2;
            let (mut in_sync, mut resyncing) = (0usize, 0usize);
            for member in &self.members[base..base + 2] {
                match member.state() {
                    MemberState::InSync => in_sync += 1,
                    MemberState::Resyncing => resyncing += 1,
                    MemberState::Faulted | MemberState::Absent => {}
                }
            }
            if in_sync == 0 && resyncing == 0 {
                return ArrayHealth::Failed;
            }
            if resyncing > 0 || in_sync == 0 {
                recovering = true;
            } else if in_sync < 2 {
                degraded = true;
            }
        }
        if recovering {
            ArrayHealth::Recovering
        } else if degraded {
            ArrayHealth::Degraded
        } else {
            ArrayHealth::Optimal
        }
    }

    /// Whether any member is still rebuilding.
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.members
            .iter()
            .any(|m| m.state() == MemberState::Resyncing)
    }

    /// Whether a proactive scrub pass is in progress.
    #[must_use]
    pub const fn scrubbing(&self) -> bool {
        self.scrub_next_lba < self.per_member.block_count
    }

    /// The scrub cursor in *member-local* block space (equal to the per-member
    /// block count when idle), exposed so the serving process can report scrub
    /// progress when logging.
    #[must_use]
    pub const fn scrub_cursor(&self) -> u64 {
        self.scrub_next_lba
    }

    /// Validate an I/O request against the array geometry, returning the block
    /// count it covers.
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

    fn read_impl(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let total = self.validate_io(lba, buf.len())?;
        let bs = u64::from(self.geometry.block_size);
        let pairs = self.pairs();
        let chunk = self.chunk_blocks;
        let idle = self.per_member.block_count;
        let mut cur = lba;
        let mut done = 0u64;
        while done < total {
            let (col, local, run) = StripeArray::<'_, B>::locate(chunk, pairs, cur, total - done);
            let column = usize::try_from(col).map_err(|_| DriverError::LengthOutOfRange)?;
            let start = usize::try_from(done * bs).map_err(|_| DriverError::LengthOutOfRange)?;
            let end =
                usize::try_from((done + run) * bs).map_err(|_| DriverError::LengthOutOfRange)?;
            self.pair(column, idle)
                .read_blocks_with_class(local, &mut buf[start..end], class)?;
            done += run;
            cur += run;
        }
        Ok(())
    }

    fn write_impl(&mut self, lba: u64, buf: &[u8], class: BufferClass) -> Result<(), DriverError> {
        let total = self.validate_io(lba, buf.len())?;
        let bs = u64::from(self.geometry.block_size);
        let pairs = self.pairs();
        let chunk = self.chunk_blocks;
        let idle = self.per_member.block_count;
        let mut cur = lba;
        let mut done = 0u64;
        while done < total {
            let (col, local, run) = StripeArray::<'_, B>::locate(chunk, pairs, cur, total - done);
            let column = usize::try_from(col).map_err(|_| DriverError::LengthOutOfRange)?;
            let start = usize::try_from(done * bs).map_err(|_| DriverError::LengthOutOfRange)?;
            let end =
                usize::try_from((done + run) * bs).map_err(|_| DriverError::LengthOutOfRange)?;
            self.pair(column, idle)
                .write_blocks_with_class(local, &buf[start..end], class)?;
            done += run;
            cur += run;
        }
        Ok(())
    }

    /// Rebuild one bounded chunk of every resyncing member from its pair's
    /// in-sync copy, sized from `scratch`. Call repeatedly until
    /// [`needs_resync`](Self::needs_resync) is false.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `scratch` is empty or not a
    ///   block-size multiple.
    /// * The mirror pair's error if a rebuild transfer fails.
    pub fn resync_step(&mut self, scratch: &mut [u8]) -> Result<(), DriverError> {
        for column in 0..self.members.len() / 2 {
            let idle = self.per_member.block_count;
            if !self.members[column * 2..column * 2 + 2]
                .iter()
                .any(|m| m.state() == MemberState::Resyncing)
            {
                continue;
            }
            self.pair(column, idle).resync_step(scratch)?;
        }
        Ok(())
    }

    /// Begin a proactive scrub pass from member-local block 0.
    pub fn begin_scrub(&mut self) {
        self.scrub_next_lba = 0;
    }

    /// The array's resumable maintenance position: how far the current scrub
    /// pass and rebuild have got (in member-local blocks, as
    /// [`scrub_cursor`](Self::scrub_cursor) reports), or
    /// [`ArrayProgress::IDLE`] if neither is running.
    ///
    /// This is what the serving process checkpoints to the members' on-disk
    /// maintenance record, so a pass measured in hours survives a restart
    /// (`AGENTS.md` §26.6). Copies in several pairs can rebuild at once with
    /// different cursors, and one record can only carry a single position, so
    /// the **least advanced** is reported: resuming from it re-copies blocks a
    /// further-ahead copy already had (harmless — a rebuild write is
    /// idempotent) and can never skip a block that was still outstanding.
    #[must_use]
    pub fn progress(&self) -> ArrayProgress {
        ArrayProgress {
            scrub_cursor: self.scrubbing().then_some(self.scrub_next_lba),
            resync_cursor: self
                .members
                .iter()
                .filter(|m| m.state() == MemberState::Resyncing)
                .map(MirrorMember::resync_cursor)
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
    /// A rebuild cursor is planted only on the copies that are actually
    /// rebuilding, through the same guarded member seam the mirror uses
    /// (`AGENTS.md` §2.2), so a restored cursor can never un-sync a current
    /// copy.
    ///
    /// # Errors
    ///
    /// [`Raid10Error::CursorOutOfRange`] if a cursor names a block outside a
    /// member. The array is left exactly as it was, so the caller can proceed
    /// from the fresh-start position.
    pub fn restore_progress(&mut self, progress: ArrayProgress) -> Result<(), Raid10Error> {
        if !progress.fits_span(self.per_member.block_count) {
            return Err(Raid10Error::CursorOutOfRange);
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

    /// Verify and repair one bounded chunk of a scrub pass across every pair,
    /// advancing the shared member-local scrub cursor.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `scratch` is empty or not a
    ///   block-size multiple.
    /// * [`DriverError::DeviceOffline`] if the array has a failed pair (it
    ///   cannot be scrubbed as a whole); the cursor does not advance.
    /// * The most fail-closed media error if a block was bad on every copy of
    ///   some pair; the cursor still advances past the chunk.
    pub fn scrub_step(&mut self, scratch: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.geometry.block_size as usize;
        if scratch.is_empty() || bs == 0 || !scratch.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        if self.scrub_next_lba >= self.per_member.block_count {
            return Ok(());
        }
        if matches!(self.health(), ArrayHealth::Failed) {
            return Err(DriverError::DeviceOffline);
        }
        let cursor = self.scrub_next_lba;
        let mut next = cursor;
        let mut worst: Option<DriverError> = None;
        for column in 0..self.members.len() / 2 {
            let mut pair = self.pair(column, cursor);
            let outcome = pair.scrub_step(scratch);
            // Every pair shares the per-member geometry and starts from the
            // same cursor, so each advances by the same chunk; record it once.
            next = pair.scrub_cursor();
            if let Err(e) = outcome {
                worst = Some(e);
            }
        }
        self.scrub_next_lba = next;
        match worst {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Locate the mirror pair (and pair-local index 0/1) member `index` belongs
    /// to, or [`Raid10Error::UnknownMember`] if out of range.
    fn locate_member(&self, index: usize) -> Result<(usize, usize), Raid10Error> {
        if index >= self.members.len() {
            return Err(Raid10Error::UnknownMember);
        }
        Ok((index / 2, index % 2))
    }

    /// Begin rebuilding a currently-faulted member from its pair's surviving
    /// copy (e.g. one that returned through its own recovery grace window).
    ///
    /// # Errors
    ///
    /// [`Raid10Error::UnknownMember`] for an out-of-range slot, or the mirror
    /// pair's policy error mapped through [`Raid10Error`].
    pub fn readd_member(&mut self, index: usize) -> Result<(), Raid10Error> {
        let (column, local) = self.locate_member(index)?;
        let idle = self.per_member.block_count;
        self.pair(column, idle)
            .readd_member(local)
            .map_err(Raid10Error::from)
    }

    /// Vacate a faulted member's slot, returning its device (an absent slot is
    /// left in its place).
    ///
    /// # Errors
    ///
    /// [`Raid10Error::UnknownMember`] for an out-of-range slot, or the mirror
    /// pair's policy error mapped through [`Raid10Error`].
    pub fn remove_member(&mut self, index: usize) -> Result<B, Raid10Error> {
        let (column, local) = self.locate_member(index)?;
        let idle = self.per_member.block_count;
        self.pair(column, idle)
            .remove_member(local)
            .map_err(Raid10Error::from)
    }

    /// Install a spare into an absent slot and begin rebuilding it from its
    /// pair's surviving copy.
    ///
    /// # Errors
    ///
    /// [`Raid10Error::UnknownMember`] for an out-of-range slot, or the mirror
    /// pair's policy error mapped through [`Raid10Error`].
    pub fn add_member(&mut self, index: usize, device: B) -> Result<(), Raid10Error> {
        let (column, local) = self.locate_member(index)?;
        let idle = self.per_member.block_count;
        self.pair(column, idle)
            .add_member(local, device)
            .map_err(Raid10Error::from)
    }

    /// Hot-swap a faulted member for a replacement device and begin rebuilding
    /// it.
    ///
    /// # Errors
    ///
    /// [`Raid10Error::UnknownMember`] for an out-of-range slot, or the mirror
    /// pair's policy error mapped through [`Raid10Error`].
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), Raid10Error> {
        let (column, local) = self.locate_member(index)?;
        let idle = self.per_member.block_count;
        self.pair(column, idle)
            .replace_member(local, device)
            .map_err(Raid10Error::from)
    }

    /// The devices of the members that speak for the array in its
    /// device-level answers (health, class), selected by the one shared
    /// participation predicate — the same set the mirrored pairs beneath
    /// this array answer from, so a RAID10 and its component mirrors can
    /// never disagree about which copies count.
    fn live_devices(&self) -> impl Iterator<Item = &B> {
        self.members
            .iter()
            .filter(|m| crate::health::member_participates(m.state()))
            .filter_map(MirrorMember::device)
    }
}

impl<B: Block> Block for Raid10Array<'_, B> {
    fn device_class(&self) -> BlkDeviceClass {
        crate::health::aggregate_device_class(self.live_devices().map(Block::device_class))
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
        let idle = self.per_member.block_count;
        for column in 0..self.members.len() / 2 {
            self.pair(column, idle).flush()?;
        }
        Ok(())
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(crate::health::aggregate_device_health(
            self.live_devices().map(Block::device_health),
        ))
    }
}

#[cfg(test)]
mod tests;
