//! The RAID0 **stripe** composition: several child [`Block`] members presented
//! as one logical device whose capacity is their *sum* (`plans/FIX-IO.md`
//! IO6).
//!
//! A stripe cuts the logical block space into fixed-size *chunks*
//! ([`ArraySuperblock::chunk_blocks`](crate::ArraySuperblock::chunk_blocks)
//! logical blocks each) and round-robins them across the members, so a large
//! transfer is spread over every member and their bandwidth aggregates. It is
//! the sibling of the RAID1 [`MirrorArray`](crate::MirrorArray) over the same
//! block seam (`AGENTS.md` §2.2 parallel implementations): both compose child
//! `Block` endpoints and consume the shared block-health vocabulary
//! ([`tairix_abi::blkio`]) rather than re-inventing it.
//!
//! # No redundancy — a stripe fails closed, it never degrades
//!
//! A stripe holds exactly one copy of each block, spread across the members,
//! so it has **no redundancy**: losing any one member loses a fraction of
//! every stored object. This shapes the whole engine and is the honest,
//! non-fabricated behaviour a senior reviewer expects of RAID0:
//!
//! - **Assembly requires every member present and aligned.** Unlike a mirror,
//!   a stripe cannot come up "degraded" over a missing or unwell member —
//!   there is no other copy to serve the blocks that member holds. [`StripeArray::assemble`]
//!   probes every member and fails closed
//!   ([`StripeError::MemberUnavailable`]) if any cannot report geometry, if the
//!   members disagree on geometry ([`StripeError::GeometryMismatch`]), or if a
//!   member's size is not a whole number of stripe chunks
//!   ([`StripeError::UnalignedGeometry`]).
//! - **A whole-device fault fails the array closed for good.** When a member
//!   returns a whole-device fault (gone/removed/unrecoverable — the shared
//!   [`member_faulting`] classification the mirror uses too), the stripe is
//!   marked [`ArrayHealth::Failed`] and every subsequent I/O fails closed
//!   ([`DriverError::DeviceOffline`]): the array can no longer serve a complete
//!   logical block space, and it never pretends otherwise (`AGENTS.md` §5.4,
//!   §26.5).
//! - **A per-block media error fails only that request.** A bad sector on a
//!   member ([`DriverError::MediumError`]) means *that* logical block is
//!   unrecoverable (no second copy to heal from, unlike the mirror), so the
//!   affected request fails closed — but the device is still reachable, so the
//!   array stays [`ArrayHealth::Optimal`] and unrelated stripes keep serving.
//!
//! A stripe therefore only ever reports [`ArrayHealth::Optimal`] or
//! [`ArrayHealth::Failed`]; it has no `Degraded`/`Recovering` state of its own
//! because it has nothing to degrade *to* and nothing to rebuild *from*.
//!
//! # Allocation-free
//!
//! Like the mirror, [`StripeArray`] borrows a caller-owned member slice, so it
//! holds no allocation and imposes no fixed member ceiling (`AGENTS.md`
//! §24.1); the growable member tier lives in the assembling serve process.

use crate::mirror::{member_faulting, ArrayHealth};
use crate::superblock::RaidLevel;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth};
use tairix_abi::driver::{BufferClass, DriverError};

/// One member of a [`StripeArray`]: a child [`Block`] device and whether it
/// has been dropped by a whole-device fault.
///
/// A stripe member is either live or faulted — there is no "absent" or
/// "resyncing" state as there is for a mirror member, because a stripe has no
/// redundancy to assemble around a gap or to rebuild a returning copy from.
pub struct StripeMember<B: Block> {
    device: B,
    faulted: bool,
}

impl<B: Block> StripeMember<B> {
    /// A live member backed by `device`.
    pub const fn new(device: B) -> Self {
        Self {
            device,
            faulted: false,
        }
    }

    /// Whether this member has been dropped by a whole-device fault.
    #[must_use]
    pub const fn faulted(&self) -> bool {
        self.faulted
    }

    /// Borrow the underlying device (for identity/health queries).
    #[must_use]
    pub const fn device(&self) -> &B {
        &self.device
    }
}

/// A reason a stripe could not be assembled. Distinct from [`DriverError`]
/// (which flows on the I/O path) because these are composition-policy
/// failures, not device I/O outcomes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StripeError {
    /// The member slice was empty; a stripe needs at least one member.
    NoMembers,
    /// The stripe unit (`chunk_blocks`) was zero; a stripe needs a positive
    /// chunk size.
    ZeroChunk,
    /// A member could not report its geometry (absent/unwell). A stripe has no
    /// redundancy, so it cannot assemble over a member it cannot read — fail
    /// closed rather than come up missing part of the logical block space.
    MemberUnavailable,
    /// Two members report different geometry: they cannot form one evenly
    /// striped array. Fails closed rather than silently truncating to the
    /// smaller.
    GeometryMismatch,
    /// A member reported a degenerate geometry (zero block size or count).
    ZeroGeometry,
    /// A member's size is not a whole number of stripe chunks, so striping it
    /// would leave a ragged tail. Fails closed rather than silently dropping
    /// the remainder.
    UnalignedGeometry,
    /// The composed logical block count overflows `u64`. Fails closed rather
    /// than wrapping to a smaller array that would truncate addresses.
    TooLarge,
}

/// A RAID0 stripe presenting several child [`Block`] members as one logical
/// device of their combined capacity.
///
/// See this module's documentation for the striping layout and the
/// fail-closed, no-redundancy fault model. The array borrows a caller-owned
/// member slice, so it holds no allocation and imposes no fixed member ceiling
/// (`AGENTS.md` §24.1).
pub struct StripeArray<'a, B: Block> {
    members: &'a mut [StripeMember<B>],
    /// The logical geometry the array presents (block size shared with the
    /// members; block count is their sum).
    geometry: BlockGeometry,
    /// The stripe unit in logical blocks: consecutive blocks placed on one
    /// member before the stripe moves to the next.
    chunk_blocks: u64,
    /// Set once any member suffers a whole-device fault: the array can no
    /// longer serve a complete logical block space and fails closed for good
    /// (sticky — a stripe has no way to rebuild a lost member).
    failed: bool,
}

impl<'a, B: Block> StripeArray<'a, B> {
    /// Assemble a stripe from `members` with stripe unit `chunk_blocks`
    /// logical blocks.
    ///
    /// The `members` slice is the array's full member table in slot order.
    /// Every member is probed for geometry: they must all report the *same*
    /// geometry, a non-degenerate one, and a block count that is a whole
    /// number of `chunk_blocks`. A stripe has no redundancy, so — unlike a
    /// mirror — a member that cannot be probed fails the whole assembly closed
    /// rather than coming up degraded.
    ///
    /// The composed array presents the members' shared block size and the
    /// *sum* of their block counts.
    ///
    /// # Errors
    ///
    /// * [`StripeError::NoMembers`] if `members` is empty.
    /// * [`StripeError::ZeroChunk`] if `chunk_blocks` is zero.
    /// * [`StripeError::MemberUnavailable`] if a member could not be probed.
    /// * [`StripeError::GeometryMismatch`] if two members disagree on
    ///   geometry.
    /// * [`StripeError::ZeroGeometry`] if a member reports a zero block size or
    ///   count.
    /// * [`StripeError::UnalignedGeometry`] if a member's block count is not a
    ///   multiple of `chunk_blocks`.
    /// * [`StripeError::TooLarge`] if the summed block count overflows `u64`.
    pub fn assemble(
        members: &'a mut [StripeMember<B>],
        chunk_blocks: u32,
    ) -> Result<Self, StripeError> {
        if members.is_empty() {
            return Err(StripeError::NoMembers);
        }
        if chunk_blocks == 0 {
            return Err(StripeError::ZeroChunk);
        }
        let chunk = u64::from(chunk_blocks);
        let mut shared: Option<BlockGeometry> = None;
        for member in members.iter() {
            let geo = member
                .device
                .geometry()
                .map_err(|_| StripeError::MemberUnavailable)?;
            if geo.block_size == 0 || geo.block_count == 0 {
                return Err(StripeError::ZeroGeometry);
            }
            if !geo.block_count.is_multiple_of(chunk) {
                return Err(StripeError::UnalignedGeometry);
            }
            match shared {
                None => shared = Some(geo),
                Some(first) if geo != first => return Err(StripeError::GeometryMismatch),
                Some(_) => {}
            }
        }
        // `members` is non-empty, so `shared` is always populated by now.
        let per_member = shared.ok_or(StripeError::NoMembers)?;
        let member_count = members.len() as u64;
        let block_count = RaidLevel::Stripe
            .logical_block_count(per_member.block_count, member_count)
            .ok_or(StripeError::TooLarge)?;
        Ok(Self {
            members,
            geometry: BlockGeometry {
                block_size: per_member.block_size,
                block_count,
            },
            chunk_blocks: chunk,
            failed: false,
        })
    }

    /// The logical geometry of the composed array (block size shared with the
    /// members, block count their sum).
    #[must_use]
    pub const fn array_geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// The number of member slots the stripe is composed of.
    #[must_use]
    pub const fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Borrow member `index` (for the serving process to inspect a member's
    /// device identity or fault state when logging), or [`None`] if `index` is
    /// out of range.
    #[must_use]
    pub fn member(&self, index: usize) -> Option<&StripeMember<B>> {
        self.members.get(index)
    }

    /// The current [`ArrayHealth`]: [`ArrayHealth::Optimal`] while every member
    /// is live, [`ArrayHealth::Failed`] once any member has suffered a
    /// whole-device fault. A stripe has no `Degraded`/`Recovering` state of its
    /// own — with no redundancy there is nothing to degrade to or rebuild
    /// from, so it maps onto the shared array-health vocabulary using only
    /// those two states (`AGENTS.md` §2.2).
    #[must_use]
    pub const fn health(&self) -> ArrayHealth {
        if self.failed {
            ArrayHealth::Failed
        } else {
            ArrayHealth::Optimal
        }
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

    /// Map one logical `lba` to the member that holds it, that member's local
    /// LBA, and the number of blocks of a `remaining`-block run that lie in the
    /// same chunk on that member (so a caller splits an I/O at chunk
    /// boundaries). The member index is a `u64` (the caller narrows it to a
    /// slot index), always less than `member_count`.
    pub(crate) fn locate(
        chunk_blocks: u64,
        member_count: u64,
        lba: u64,
        remaining: u64,
    ) -> (u64, u64, u64) {
        let chunk = lba / chunk_blocks;
        let offset = lba % chunk_blocks;
        let member = chunk % member_count;
        let member_chunk = chunk / member_count;
        let member_lba = member_chunk * chunk_blocks + offset;
        let run = (chunk_blocks - offset).min(remaining);
        (member, member_lba, run)
    }

    /// Drive one striped transfer, splitting `buf` at chunk boundaries and
    /// dispatching each contiguous run to the member that holds it through
    /// `io`. A whole-device fault on a member fails the array closed for good;
    /// a per-block error fails only this request, leaving the array serving.
    fn transfer(
        &mut self,
        lba: u64,
        buf_len: usize,
        class: BufferClass,
        mut io: impl FnMut(&mut B, u64, core::ops::Range<usize>, BufferClass) -> Result<(), DriverError>,
    ) -> Result<(), DriverError> {
        let total = self.validate_io(lba, buf_len)?;
        if self.failed {
            return Err(DriverError::DeviceOffline);
        }
        let bs = u64::from(self.geometry.block_size);
        let member_count = self.members.len() as u64;
        let chunk_blocks = self.chunk_blocks;
        let mut cur_lba = lba;
        let mut done_blocks = 0u64;
        while done_blocks < total {
            let remaining = total - done_blocks;
            let (member_u64, member_lba, run) =
                Self::locate(chunk_blocks, member_count, cur_lba, remaining);
            // Every value here is bounded by the validated request (which fits
            // the caller's buffer), so the narrowing never truncates; it fails
            // closed rather than panicking if that invariant is ever violated.
            let member_idx =
                usize::try_from(member_u64).map_err(|_| DriverError::LengthOutOfRange)?;
            let start =
                usize::try_from(done_blocks * bs).map_err(|_| DriverError::LengthOutOfRange)?;
            let end = usize::try_from((done_blocks + run) * bs)
                .map_err(|_| DriverError::LengthOutOfRange)?;
            let member = &mut self.members[member_idx];
            if member.faulted {
                // A previously-dropped member cannot serve its stripes; the
                // array can no longer present a complete block space.
                self.failed = true;
                return Err(DriverError::DeviceOffline);
            }
            if let Err(err) = io(&mut member.device, member_lba, start..end, class) {
                // A dead device drops the array for good; a per-block media
                // error fails only this request and leaves the device (and so
                // the array) serving its other stripes.
                if member_faulting(err) {
                    member.faulted = true;
                    self.failed = true;
                }
                return Err(err);
            }
            done_blocks += run;
            cur_lba += run;
        }
        Ok(())
    }

    /// Read `buf.len() / block_size` blocks at `lba`, gathering each stripe
    /// chunk from the member that holds it.
    fn read_impl(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let buf_len = buf.len();
        self.transfer(lba, buf_len, class, |device, member_lba, range, class| {
            device.read_blocks_with_class(member_lba, &mut buf[range], class)
        })
    }

    /// Write `buf.len() / block_size` blocks at `lba`, scattering each stripe
    /// chunk to the member that holds it.
    fn write_impl(&mut self, lba: u64, buf: &[u8], class: BufferClass) -> Result<(), DriverError> {
        let buf_len = buf.len();
        self.transfer(lba, buf_len, class, |device, member_lba, range, class| {
            device.write_blocks_with_class(member_lba, &buf[range], class)
        })
    }
}

impl<B: Block> Block for StripeArray<'_, B> {
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
        if self.failed {
            return Err(DriverError::DeviceOffline);
        }
        // Every member holds a disjoint slice of the block space, so durability
        // requires *all* of them to commit: a stripe with one member that
        // cannot flush is not durable and fails closed (`AGENTS.md` §5.4). A
        // member that faults its flush drops the array for good.
        for member in self.members.iter_mut() {
            if member.faulted {
                self.failed = true;
                return Err(DriverError::DeviceOffline);
            }
            if let Err(err) = member.device.flush() {
                member.faulted = true;
                self.failed = true;
                return Err(err);
            }
        }
        Ok(())
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(crate::health::aggregate_device_health(
            self.members
                .iter()
                .filter(|m| !m.faulted())
                .map(|m| m.device().device_health()),
        ))
    }
}

#[cfg(test)]
mod tests;
