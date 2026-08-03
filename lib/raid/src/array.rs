//! The unified **composed RAID device** ([`RaidArray`]) — one `Block` that
//! dispatches to whichever concrete RAID level composes it
//! (`plans/FIX-IO.md` IO6).
//!
//! The six RAID compositions ([`MirrorArray`], [`StripeArray`],
//! [`ParityArray`], [`DualParityArray`], [`TripleParityArray`],
//! [`Raid10Array`]) are siblings over the same block
//! seam (`AGENTS.md` §2.2). A serving process, once it has *discovered* an
//! array from its members' superblocks and resolved its [`RaidLevel`], must
//! present exactly one logical [`Block`] device to the filesystem layer and
//! drive its self-recovery, regardless of level. [`RaidArray`] is that single
//! composed-device abstraction (`AGENTS.md` §27, modelled on Linux md's
//! per-personality dispatch): it wraps the level-specific engine and forwards
//! both the [`Block`] I/O path and the level-agnostic health, maintenance, and
//! member-reconfiguration surface, so neither the autoloaded serve process nor
//! the ARXFS-native composition re-derives the level → engine mapping (§2.2).
//!
//! The wrapper is a thin, allocation-free dispatch layer: it changes none of
//! the engines' behaviour and adds no policy of its own. Where an operation is
//! only meaningful for a *redundant* array — a scrub, a resync, or a
//! hot-swap — the no-redundancy RAID0 stripe fails it closed with
//! [`RaidError::NotRedundant`] rather than pretending, exactly as the stripe
//! engine reports only [`ArrayHealth::Optimal`] / [`ArrayHealth::Failed`].

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use tairix_abi::driver::{BufferClass, DriverError};

use crate::dualparity::{DualParityArray, DualParityError};
use crate::mirror::{MirrorArray, MirrorError};
use crate::parity::{ParityArray, ParityError};
use crate::raid10::{Raid10Array, Raid10Error};
use crate::stripe::StripeArray;
use crate::superblock::ArrayProgress;
use crate::triple::{TripleParityArray, TripleParityError};
use tairix_abi::raid::{ArrayHealth, MemberState, RaidLevel};

/// A reason a level-agnostic [`RaidArray`] operation could not be carried out.
///
/// Distinct from [`DriverError`] (which flows on the block I/O path): these are
/// composition-policy outcomes of a maintenance or reconfiguration request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RaidError {
    /// The operation is only meaningful for a *redundant* array, and the
    /// composed level has no redundancy (a RAID0 stripe): there is nothing to
    /// scrub, resync, or hot-swap. Fails closed rather than pretending.
    NotRedundant,
    /// The maintenance scratch buffer was empty or not a whole multiple of the
    /// array block size, so no bounded chunk could be sized from it.
    BadScratch,
    /// A maintenance transfer (scrub/resync) failed on the underlying members;
    /// carries the block-layer error the engine reported.
    Io(DriverError),
    /// A member index is outside the array.
    UnknownMember,
    /// A re-add/replace was asked of a member that is not currently faulted
    /// (nothing to rebuild).
    NotFaulted,
    /// A re-add/replace member's device could not be probed (absent/unwell).
    ProbeFailed,
    /// A re-add/replace member's geometry no longer matches the array.
    GeometryMismatch,
    /// [`RaidArray::add_member`] was asked to populate a slot that already
    /// holds a device; vacate it with [`RaidArray::remove_member`] first, or
    /// hot-swap a faulted one with [`RaidArray::replace_member`].
    SlotOccupied,
    /// A restored maintenance cursor named a block outside the array, so it
    /// cannot have come from this array in this shape. Refused rather than
    /// clamped: adopted as a rebuild position it would declare a member fully
    /// copied without its tail ever having been written.
    CursorOutOfRange,
    /// A composition-policy rejection a reconfiguration path does not normally
    /// produce (an assembly-time failure surfaced from a member operation).
    /// Kept as a defensive, fail-closed catch so a future engine variant can
    /// never be silently discarded.
    Policy,
}

impl From<MirrorError> for RaidError {
    fn from(err: MirrorError) -> Self {
        match err {
            MirrorError::UnknownMember => Self::UnknownMember,
            MirrorError::NotFaulted => Self::NotFaulted,
            MirrorError::ProbeFailed => Self::ProbeFailed,
            MirrorError::GeometryMismatch => Self::GeometryMismatch,
            MirrorError::SlotOccupied => Self::SlotOccupied,
            MirrorError::CursorOutOfRange => Self::CursorOutOfRange,
            _ => Self::Policy,
        }
    }
}

impl From<ParityError> for RaidError {
    fn from(err: ParityError) -> Self {
        match err {
            ParityError::UnknownMember => Self::UnknownMember,
            ParityError::NotFaulted => Self::NotFaulted,
            ParityError::ProbeFailed => Self::ProbeFailed,
            ParityError::GeometryMismatch => Self::GeometryMismatch,
            ParityError::SlotOccupied => Self::SlotOccupied,
            ParityError::CursorOutOfRange => Self::CursorOutOfRange,
            _ => Self::Policy,
        }
    }
}

impl From<DualParityError> for RaidError {
    fn from(err: DualParityError) -> Self {
        match err {
            DualParityError::UnknownMember => Self::UnknownMember,
            DualParityError::NotFaulted => Self::NotFaulted,
            DualParityError::ProbeFailed => Self::ProbeFailed,
            DualParityError::GeometryMismatch => Self::GeometryMismatch,
            DualParityError::SlotOccupied => Self::SlotOccupied,
            DualParityError::CursorOutOfRange => Self::CursorOutOfRange,
            _ => Self::Policy,
        }
    }
}

impl From<Raid10Error> for RaidError {
    fn from(err: Raid10Error) -> Self {
        match err {
            Raid10Error::UnknownMember => Self::UnknownMember,
            Raid10Error::NotFaulted => Self::NotFaulted,
            Raid10Error::ProbeFailed => Self::ProbeFailed,
            Raid10Error::GeometryMismatch => Self::GeometryMismatch,
            Raid10Error::SlotOccupied => Self::SlotOccupied,
            Raid10Error::CursorOutOfRange => Self::CursorOutOfRange,
            // Assembly-shape failures (empty/odd/too-few members, zero chunk,
            // unaligned or missing geometry, overflow) are composition policy.
            _ => Self::Policy,
        }
    }
}

impl From<TripleParityError> for RaidError {
    fn from(err: TripleParityError) -> Self {
        match err {
            TripleParityError::UnknownMember => Self::UnknownMember,
            TripleParityError::NotFaulted => Self::NotFaulted,
            TripleParityError::ProbeFailed => Self::ProbeFailed,
            TripleParityError::GeometryMismatch => Self::GeometryMismatch,
            TripleParityError::SlotOccupied => Self::SlotOccupied,
            TripleParityError::CursorOutOfRange => Self::CursorOutOfRange,
            _ => Self::Policy,
        }
    }
}

/// A composed RAID device: one logical [`Block`] backed by whichever concrete
/// RAID level assembled it.
///
/// A serving process holds the [`RaidArray`] for the array it publishes and
/// drives every level through the one surface here: the [`Block`] I/O path
/// (forwarded to the inner engine), the health/geometry observations, the
/// bounded self-maintenance ([`begin_scrub`](Self::begin_scrub) /
/// [`scrub_step`](Self::scrub_step) / [`resync_step`](Self::resync_step)), and
/// the member-reconfiguration workflow
/// ([`readd_member`](Self::readd_member) / [`remove_member`](Self::remove_member)
/// / [`add_member`](Self::add_member) / [`replace_member`](Self::replace_member)).
///
/// The enum borrows the concrete engine, which in turn borrows its
/// caller-owned member slice, so the composed device holds no allocation and
/// imposes no fixed member ceiling (`AGENTS.md` §24.1).
pub enum RaidArray<'a, B: Block> {
    /// A RAID1 mirror ([`MirrorArray`]).
    Mirror(MirrorArray<'a, B>),
    /// A RAID0 stripe ([`StripeArray`]) — no redundancy.
    Stripe(StripeArray<'a, B>),
    /// A RAID5 distributed-parity array ([`ParityArray`]).
    Parity(ParityArray<'a, B>),
    /// A RAID6 double distributed-parity array ([`DualParityArray`]).
    DualParity(DualParityArray<'a, B>),
    /// A RAID-TP triple distributed-parity array ([`TripleParityArray`]).
    TripleParity(TripleParityArray<'a, B>),
    /// A RAID10 stripe of two-copy mirrors ([`Raid10Array`]).
    Raid10(Raid10Array<'a, B>),
}

impl<B: Block> RaidArray<'_, B> {
    /// The RAID level this composed device implements.
    #[must_use]
    pub fn level(&self) -> RaidLevel {
        match self {
            Self::Mirror(_) => RaidLevel::Mirror,
            Self::Stripe(_) => RaidLevel::Stripe,
            Self::Parity(_) => RaidLevel::Parity,
            Self::DualParity(_) => RaidLevel::DualParity,
            Self::TripleParity(_) => RaidLevel::TripleParity,
            Self::Raid10(_) => RaidLevel::Raid10,
        }
    }

    /// The current [`ArrayHealth`]. A stripe reports only
    /// [`ArrayHealth::Optimal`] / [`ArrayHealth::Failed`] (it has no
    /// redundancy to degrade to or rebuild from); the redundant levels report
    /// the full vocabulary.
    #[must_use]
    pub fn health(&self) -> ArrayHealth {
        match self {
            Self::Mirror(a) => a.health(),
            Self::Stripe(a) => a.health(),
            Self::Parity(a) => a.health(),
            Self::DualParity(a) => a.health(),
            Self::TripleParity(a) => a.health(),
            Self::Raid10(a) => a.health(),
        }
    }

    /// The number of member slots the array is defined to have (in sync,
    /// faulted, resyncing, or absent alike).
    #[must_use]
    pub fn member_count(&self) -> usize {
        match self {
            Self::Mirror(a) => a.member_count(),
            Self::Stripe(a) => a.member_count(),
            Self::Parity(a) => a.member_count(),
            Self::DualParity(a) => a.member_count(),
            Self::TripleParity(a) => a.member_count(),
            Self::Raid10(a) => a.member_count(),
        }
    }

    /// The composed device's logical geometry.
    #[must_use]
    pub fn array_geometry(&self) -> BlockGeometry {
        match self {
            Self::Mirror(a) => a.array_geometry(),
            Self::Stripe(a) => a.array_geometry(),
            Self::Parity(a) => a.array_geometry(),
            Self::DualParity(a) => a.array_geometry(),
            Self::TripleParity(a) => a.array_geometry(),
            Self::Raid10(a) => a.array_geometry(),
        }
    }

    /// The [`MemberState`] of member `index`, or [`None`] if out of range.
    ///
    /// A stripe member has no `Absent`/`Resyncing` state of its own (a stripe
    /// cannot assemble around a gap or rebuild a member), so it maps onto the
    /// shared vocabulary as [`MemberState::Faulted`] when dropped and
    /// [`MemberState::InSync`] otherwise.
    #[must_use]
    pub fn member_state(&self, index: usize) -> Option<MemberState> {
        match self {
            Self::Mirror(a) => a.member_state(index),
            Self::Stripe(a) => a.member(index).map(|m| {
                if m.faulted() {
                    MemberState::Faulted
                } else {
                    MemberState::InSync
                }
            }),
            Self::Parity(a) => a.member_state(index),
            Self::DualParity(a) => a.member_state(index),
            Self::TripleParity(a) => a.member_state(index),
            Self::Raid10(a) => a.member_state(index),
        }
    }

    /// Mutably borrow the device held by member `index` — the mutable
    /// companion of [`member_state`](Self::member_state), for a caller that
    /// must reach a member's own device (its reserved array-metadata blocks)
    /// rather than the array's data — or [`None`] if `index` is out of range
    /// or the slot holds no device.
    #[must_use]
    pub fn member_device_mut(&mut self, index: usize) -> Option<&mut B> {
        match self {
            Self::Mirror(a) => a.member_device_mut(index),
            Self::Stripe(a) => a.member_device_mut(index),
            Self::Parity(a) => a.member_device_mut(index),
            Self::DualParity(a) => a.member_device_mut(index),
            Self::TripleParity(a) => a.member_device_mut(index),
            Self::Raid10(a) => a.member_device_mut(index),
        }
    }

    /// Whether any member is still rebuilding (i.e.
    /// [`resync_step`](Self::resync_step) has work to do). Always `false` for a
    /// stripe, which never rebuilds.
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        match self {
            Self::Mirror(a) => a.needs_resync(),
            Self::Stripe(_) => false,
            Self::Parity(a) => a.needs_resync(),
            Self::DualParity(a) => a.needs_resync(),
            Self::TripleParity(a) => a.needs_resync(),
            Self::Raid10(a) => a.needs_resync(),
        }
    }

    /// Whether a proactive scrub pass is in progress. Always `false` for a
    /// stripe, which has no redundancy to scrub from.
    #[must_use]
    pub fn scrubbing(&self) -> bool {
        match self {
            Self::Mirror(a) => a.scrubbing(),
            Self::Stripe(_) => false,
            Self::Parity(a) => a.scrubbing(),
            Self::DualParity(a) => a.scrubbing(),
            Self::TripleParity(a) => a.scrubbing(),
            Self::Raid10(a) => a.scrubbing(),
        }
    }

    /// The scrub cursor (the next block a scrub will verify); equal to the
    /// array block count when idle. A stripe is always idle.
    #[must_use]
    pub fn scrub_cursor(&self) -> u64 {
        match self {
            Self::Mirror(a) => a.scrub_cursor(),
            Self::Stripe(a) => a.array_geometry().block_count,
            Self::Parity(a) => a.scrub_cursor(),
            Self::DualParity(a) => a.scrub_cursor(),
            Self::TripleParity(a) => a.scrub_cursor(),
            Self::Raid10(a) => a.scrub_cursor(),
        }
    }

    /// Begin a proactive scrub pass from block 0.
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe, which has no redundancy to
    ///   scrub from.
    pub fn begin_scrub(&mut self) -> Result<(), RaidError> {
        match self {
            Self::Mirror(a) => a.begin_scrub(),
            Self::Stripe(_) => return Err(RaidError::NotRedundant),
            Self::Parity(a) => a.begin_scrub(),
            Self::DualParity(a) => a.begin_scrub(),
            Self::TripleParity(a) => a.begin_scrub(),
            Self::Raid10(a) => a.begin_scrub(),
        }
        Ok(())
    }

    /// The array's resumable maintenance position: how far the current scrub
    /// pass and rebuild have got, or [`ArrayProgress::IDLE`] if neither is
    /// running.
    ///
    /// The serving process checkpoints this to the members' on-disk
    /// maintenance record ([`MaintenanceRecord`](crate::superblock::MaintenanceRecord))
    /// as the array works, so a pass measured in hours — which on a 100 TB+
    /// array is the normal case — is not silently discarded by a reboot
    /// (`AGENTS.md` §26.6).
    ///
    /// An observation, so it is answered for every level: a non-redundant
    /// stripe has no maintenance to record and reports
    /// [`ArrayProgress::IDLE`], exactly as it reports an idle scrub cursor.
    #[must_use]
    pub fn progress(&self) -> ArrayProgress {
        match self {
            Self::Mirror(a) => a.progress(),
            // A stripe has no redundancy, so it never scrubs or rebuilds.
            Self::Stripe(_) => ArrayProgress::IDLE,
            Self::Parity(a) => a.progress(),
            Self::DualParity(a) => a.progress(),
            Self::TripleParity(a) => a.progress(),
            Self::Raid10(a) => a.progress(),
        }
    }

    /// Resume maintenance at a previously checkpointed `progress`, read back
    /// from the members' on-disk maintenance record.
    ///
    /// Called once after assembly, before the first maintenance step. The
    /// record only yields a position it can vouch for — the same array at the
    /// same generation, decoding cleanly — and otherwise yields
    /// [`ArrayProgress::IDLE`], which leaves the array at its fresh-start
    /// position: a lost, foreign, or corrupt record costs time and never
    /// correctness (`AGENTS.md` §5.4, §26.5).
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe, which has no maintenance to
    ///   resume — the level check wins, as for every redundancy-only operation.
    /// * [`RaidError::CursorOutOfRange`] if a cursor names a block outside the
    ///   array; the array is left at its fresh-start position.
    pub fn restore_progress(&mut self, progress: ArrayProgress) -> Result<(), RaidError> {
        if !self.level().is_redundant() {
            return Err(RaidError::NotRedundant);
        }
        match self {
            Self::Mirror(a) => a.restore_progress(progress).map_err(RaidError::from),
            Self::Parity(a) => a.restore_progress(progress).map_err(RaidError::from),
            Self::DualParity(a) => a.restore_progress(progress).map_err(RaidError::from),
            Self::TripleParity(a) => a.restore_progress(progress).map_err(RaidError::from),
            Self::Raid10(a) => a.restore_progress(progress).map_err(RaidError::from),
            // The stripe arm returned above; maintenance is redundant-only.
            Self::Stripe(_) => Err(RaidError::NotRedundant),
        }
    }

    /// Verify and repair one bounded chunk of a scrub pass, advancing the
    /// scrub cursor. The chunk is sized from `scratch` (its length in whole
    /// array blocks): a larger buffer scrubs faster, a smaller one yields
    /// sooner, so a 100 TB+ array never scrubs in one sweep (`AGENTS.md`
    /// §26.6, §2.23).
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe.
    /// * [`RaidError::BadScratch`] if `scratch` is empty or not a block-size
    ///   multiple.
    /// * [`RaidError::Io`] if the underlying transfer fails.
    pub fn scrub_step(&mut self, scratch: &mut [u8]) -> Result<(), RaidError> {
        if !self.level().is_redundant() {
            return Err(RaidError::NotRedundant);
        }
        let blocks = self.scratch_blocks(scratch)?;
        match self {
            Self::Mirror(a) => a.scrub_step(scratch).map_err(RaidError::Io),
            Self::Parity(a) => a.scrub_step(blocks).map_err(RaidError::Io),
            Self::DualParity(a) => a.scrub_step(blocks).map_err(RaidError::Io),
            Self::TripleParity(a) => a.scrub_step(blocks).map_err(RaidError::Io),
            Self::Raid10(a) => a.scrub_step(scratch).map_err(RaidError::Io),
            // The stripe arm returned above; the budget is redundant-only.
            Self::Stripe(_) => Err(RaidError::NotRedundant),
        }
    }

    /// Rebuild one bounded chunk of every resyncing member from an in-sync
    /// source, sized from `scratch` exactly as [`scrub_step`](Self::scrub_step),
    /// so a 100 TB+ rebuild never blocks the system or busy-spins.
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe, which cannot rebuild a
    ///   member.
    /// * [`RaidError::BadScratch`] if `scratch` is empty or not a block-size
    ///   multiple.
    /// * [`RaidError::Io`] if the underlying transfer fails.
    pub fn resync_step(&mut self, scratch: &mut [u8]) -> Result<(), RaidError> {
        if !self.level().is_redundant() {
            return Err(RaidError::NotRedundant);
        }
        let blocks = self.scratch_blocks(scratch)?;
        match self {
            Self::Mirror(a) => a.resync_step(scratch).map_err(RaidError::Io),
            Self::Parity(a) => a.resync_step(blocks).map_err(RaidError::Io),
            Self::DualParity(a) => a.resync_step(blocks).map_err(RaidError::Io),
            Self::TripleParity(a) => a.resync_step(blocks).map_err(RaidError::Io),
            Self::Raid10(a) => a.resync_step(scratch).map_err(RaidError::Io),
            // The stripe arm returned above; the budget is redundant-only.
            Self::Stripe(_) => Err(RaidError::NotRedundant),
        }
    }

    /// Begin rebuilding a currently-faulted member from its existing device
    /// (e.g. one that has returned through its own recovery grace window,
    /// `plans/FIX-IO.md` IO3).
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe.
    /// * [`RaidError::UnknownMember`] / [`RaidError::NotFaulted`] /
    ///   [`RaidError::ProbeFailed`] / [`RaidError::GeometryMismatch`] as the
    ///   underlying engine reports.
    pub fn readd_member(&mut self, index: usize) -> Result<(), RaidError> {
        match self {
            Self::Mirror(a) => a.readd_member(index).map_err(RaidError::from),
            Self::Stripe(_) => Err(RaidError::NotRedundant),
            Self::Parity(a) => a.readd_member(index).map_err(RaidError::from),
            Self::DualParity(a) => a.readd_member(index).map_err(RaidError::from),
            Self::TripleParity(a) => a.readd_member(index).map_err(RaidError::from),
            Self::Raid10(a) => a.readd_member(index).map_err(RaidError::from),
        }
    }

    /// Remove a faulted member's device, leaving its slot absent and returning
    /// the device (the "pull a failed disk" step of a hot-swap).
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe.
    /// * [`RaidError::UnknownMember`] / [`RaidError::NotFaulted`] as the
    ///   underlying engine reports.
    pub fn remove_member(&mut self, index: usize) -> Result<B, RaidError> {
        match self {
            Self::Mirror(a) => a.remove_member(index).map_err(RaidError::from),
            Self::Stripe(_) => Err(RaidError::NotRedundant),
            Self::Parity(a) => a.remove_member(index).map_err(RaidError::from),
            Self::DualParity(a) => a.remove_member(index).map_err(RaidError::from),
            Self::TripleParity(a) => a.remove_member(index).map_err(RaidError::from),
            Self::Raid10(a) => a.remove_member(index).map_err(RaidError::from),
        }
    }

    /// Install a spare `device` into an absent slot and begin rebuilding it
    /// (the "add a spare" step of a hot-swap).
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe.
    /// * [`RaidError::UnknownMember`] / [`RaidError::SlotOccupied`] /
    ///   [`RaidError::ProbeFailed`] / [`RaidError::GeometryMismatch`] as the
    ///   underlying engine reports.
    pub fn add_member(&mut self, index: usize, device: B) -> Result<(), RaidError> {
        match self {
            Self::Mirror(a) => a.add_member(index, device).map_err(RaidError::from),
            Self::Stripe(_) => Err(RaidError::NotRedundant),
            Self::Parity(a) => a.add_member(index, device).map_err(RaidError::from),
            Self::DualParity(a) => a.add_member(index, device).map_err(RaidError::from),
            Self::TripleParity(a) => a.add_member(index, device).map_err(RaidError::from),
            Self::Raid10(a) => a.add_member(index, device).map_err(RaidError::from),
        }
    }

    /// Hot-swap a faulted member's device for a fresh one and begin rebuilding
    /// it in place.
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe.
    /// * [`RaidError::UnknownMember`] / [`RaidError::NotFaulted`] /
    ///   [`RaidError::ProbeFailed`] / [`RaidError::GeometryMismatch`] as the
    ///   underlying engine reports.
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), RaidError> {
        match self {
            Self::Mirror(a) => a.replace_member(index, device).map_err(RaidError::from),
            Self::Stripe(_) => Err(RaidError::NotRedundant),
            Self::Parity(a) => a.replace_member(index, device).map_err(RaidError::from),
            Self::DualParity(a) => a.replace_member(index, device).map_err(RaidError::from),
            Self::TripleParity(a) => a.replace_member(index, device).map_err(RaidError::from),
            Self::Raid10(a) => a.replace_member(index, device).map_err(RaidError::from),
        }
    }

    /// Validate `scratch` against the array geometry and return the whole
    /// number of array blocks it can hold (the bounded maintenance chunk).
    fn scratch_blocks(&self, scratch: &[u8]) -> Result<u64, RaidError> {
        let bs = self.array_geometry().block_size as usize;
        if scratch.is_empty() || bs == 0 || !scratch.len().is_multiple_of(bs) {
            return Err(RaidError::BadScratch);
        }
        Ok((scratch.len() / bs) as u64)
    }
}

impl<B: Block> Block for RaidArray<'_, B> {
    fn device_class(&self) -> BlkDeviceClass {
        match self {
            Self::Mirror(a) => a.device_class(),
            Self::Stripe(a) => a.device_class(),
            Self::Parity(a) => a.device_class(),
            Self::DualParity(a) => a.device_class(),
            Self::TripleParity(a) => a.device_class(),
            Self::Raid10(a) => a.device_class(),
        }
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        match self {
            Self::Mirror(a) => a.geometry(),
            Self::Stripe(a) => a.geometry(),
            Self::Parity(a) => a.geometry(),
            Self::DualParity(a) => a.geometry(),
            Self::TripleParity(a) => a.geometry(),
            Self::Raid10(a) => a.geometry(),
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        match self {
            Self::Mirror(a) => a.read_blocks(lba, buf),
            Self::Stripe(a) => a.read_blocks(lba, buf),
            Self::Parity(a) => a.read_blocks(lba, buf),
            Self::DualParity(a) => a.read_blocks(lba, buf),
            Self::TripleParity(a) => a.read_blocks(lba, buf),
            Self::Raid10(a) => a.read_blocks(lba, buf),
        }
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        match self {
            Self::Mirror(a) => a.write_blocks(lba, buf),
            Self::Stripe(a) => a.write_blocks(lba, buf),
            Self::Parity(a) => a.write_blocks(lba, buf),
            Self::DualParity(a) => a.write_blocks(lba, buf),
            Self::TripleParity(a) => a.write_blocks(lba, buf),
            Self::Raid10(a) => a.write_blocks(lba, buf),
        }
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        match self {
            Self::Mirror(a) => a.flush(),
            Self::Stripe(a) => a.flush(),
            Self::Parity(a) => a.flush(),
            Self::DualParity(a) => a.flush(),
            Self::TripleParity(a) => a.flush(),
            Self::Raid10(a) => a.flush(),
        }
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        match self {
            Self::Mirror(a) => a.read_blocks_with_class(lba, buf, class),
            Self::Stripe(a) => a.read_blocks_with_class(lba, buf, class),
            Self::Parity(a) => a.read_blocks_with_class(lba, buf, class),
            Self::DualParity(a) => a.read_blocks_with_class(lba, buf, class),
            Self::TripleParity(a) => a.read_blocks_with_class(lba, buf, class),
            Self::Raid10(a) => a.read_blocks_with_class(lba, buf, class),
        }
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        match self {
            Self::Mirror(a) => a.write_blocks_with_class(lba, buf, class),
            Self::Stripe(a) => a.write_blocks_with_class(lba, buf, class),
            Self::Parity(a) => a.write_blocks_with_class(lba, buf, class),
            Self::DualParity(a) => a.write_blocks_with_class(lba, buf, class),
            Self::TripleParity(a) => a.write_blocks_with_class(lba, buf, class),
            Self::Raid10(a) => a.write_blocks_with_class(lba, buf, class),
        }
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        match self {
            Self::Mirror(a) => a.discard_capability(),
            Self::Stripe(a) => a.discard_capability(),
            Self::Parity(a) => a.discard_capability(),
            Self::DualParity(a) => a.discard_capability(),
            Self::TripleParity(a) => a.discard_capability(),
            Self::Raid10(a) => a.discard_capability(),
        }
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        match self {
            Self::Mirror(a) => a.discard(lba, blocks),
            Self::Stripe(a) => a.discard(lba, blocks),
            Self::Parity(a) => a.discard(lba, blocks),
            Self::DualParity(a) => a.discard(lba, blocks),
            Self::TripleParity(a) => a.discard(lba, blocks),
            Self::Raid10(a) => a.discard(lba, blocks),
        }
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        match self {
            Self::Mirror(a) => a.device_health(),
            Self::Stripe(a) => a.device_health(),
            Self::Parity(a) => a.device_health(),
            Self::DualParity(a) => a.device_health(),
            Self::TripleParity(a) => a.device_health(),
            Self::Raid10(a) => a.device_health(),
        }
    }
}

#[cfg(test)]
mod tests;
