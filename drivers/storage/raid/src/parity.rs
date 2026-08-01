//! RAID5 **distributed-parity** composition over the public block seam.
//!
//! [`ParityArray`] composes a caller-owned slice of [`ParityMember`]s into one
//! logical [`Block`] device whose usable capacity is that of `member_count - 1`
//! members. It is the redundant-with-capacity sibling of the [`MirrorArray`]
//! (full redundancy, one member's capacity) and the [`StripeArray`] (no
//! redundancy, all members' capacity) over the same block seam (`AGENTS.md`
//! §2.2 parallel implementations): it composes child `Block` endpoints and
//! consumes the shared block-health vocabulary ([`tairix_abi::blkio`]) rather
//! than re-inventing it.
//!
//! [`MirrorArray`]: crate::MirrorArray
//! [`StripeArray`]: crate::StripeArray
//!
//! # Layout (left-symmetric distributed parity)
//!
//! The array of `n` members is cut into **stripes**. One stripe is one chunk
//! ([`ArraySuperblock::chunk_blocks`](crate::ArraySuperblock::chunk_blocks)
//! logical blocks) on *every* member at the same member-local offset; `n - 1`
//! of those chunks hold data and one holds the parity (bytewise XOR) of the
//! others. The parity chunk rotates one slot per stripe so the parity write
//! load is spread across all members (unlike fixed-parity RAID4).
//!
//! For stripe `s` (`s = 0, 1, …`) the parity member is
//! `p = (n - 1) - (s mod n)`, and the `n - 1` data chunks of that stripe are
//! placed on the non-parity members in ascending order starting just after
//! `p`: data position `k` (`0 ≤ k < n - 1`) sits on member
//! `(p + 1 + k) mod n`. This is the classic left-symmetric placement, chosen
//! because it keeps sequential logical blocks moving across every member in
//! turn (good read throughput) while rotating parity.
//!
//! A logical block `lba` maps as: `dchunk = lba / chunk` is its data-chunk
//! index; `stripe = dchunk / (n - 1)` and `dpos = dchunk mod (n - 1)` place it
//! within a stripe; the member-local LBA of every chunk in stripe `s` is
//! `s * chunk + (lba mod chunk)`.
//!
//! # Redundancy — survives one fault, fails closed on two
//!
//! A parity array survives any single member being lost: a read of a chunk on
//! the missing member is **reconstructed** by XOR-ing the same offset from
//! every surviving member (data and parity), and a write recomputes the parity
//! so the missing data stays reconstructable. A *second* lost member makes the
//! stripe unrecoverable, so the array is [`ArrayHealth::Failed`] and every I/O
//! fails closed (`AGENTS.md` §5.4, §26.5) — it never fabricates data it cannot
//! reconstruct.
//!
//! # Allocation-free
//!
//! Like the mirror and stripe, [`ParityArray`] borrows a caller-owned member
//! slice (no fixed member ceiling, `AGENTS.md` §24.1). Parity computation and
//! reconstruction need a working buffer, so the array also borrows a
//! caller-owned **scratch** buffer (at least two logical blocks); the growable
//! tier and the scratch sizing live in the assembling serve process.

use crate::mirror::{member_faulting, ArrayHealth, MemberRole, MemberState};
use crate::superblock::RaidLevel;
use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth};
use tairix_abi::driver::{BufferClass, DriverError};

#[cfg(test)]
mod tests;

/// One member slot of a [`ParityArray`]: an optional child [`Block`] device,
/// the role it joined with, its membership state, and — while
/// [`MemberState::Resyncing`] — the rebuild cursor.
///
/// The slot model mirrors [`crate::MirrorMember`]: a slot with no device is
/// [`MemberState::Absent`]; every other state has a backing device. Assembly
/// re-derives the real state from a geometry probe and the member's role, so a
/// stale copy is never trusted as a read source.
pub struct ParityMember<B: Block> {
    device: Option<B>,
    role: MemberRole,
    state: MemberState,
    resync_next_lba: u64,
}

impl<B: Block> ParityMember<B> {
    /// Wrap `device` as a member presumed to hold a **current** copy
    /// ([`MemberRole::Current`]).
    #[must_use]
    pub const fn new(device: B) -> Self {
        Self::with_role(device, MemberRole::Current)
    }

    /// Wrap `device` as a member joining with `role`. Assembly re-derives the
    /// real state; the state recorded here is a placeholder.
    #[must_use]
    pub const fn with_role(device: B, role: MemberRole) -> Self {
        Self {
            device: Some(device),
            role,
            state: MemberState::InSync,
            resync_next_lba: 0,
        }
    }

    /// A slot the array is *defined* to have but which currently holds no
    /// device ([`MemberState::Absent`]) — the equivalent of a Linux md
    /// "removed" slot. Pass one per missing member when assembling.
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            device: None,
            role: MemberRole::Current,
            state: MemberState::Absent,
            resync_next_lba: 0,
        }
    }

    /// The role this member joined with.
    #[must_use]
    pub const fn role(&self) -> MemberRole {
        self.role
    }

    /// This member's current membership state.
    #[must_use]
    pub const fn state(&self) -> MemberState {
        self.state
    }

    /// The rebuild cursor (first not-yet-rebuilt logical block) while
    /// [`MemberState::Resyncing`]; `0` otherwise.
    #[must_use]
    pub const fn resync_cursor(&self) -> u64 {
        self.resync_next_lba
    }

    /// Borrow the underlying device, or [`None`] for an
    /// [`MemberState::Absent`] slot.
    #[must_use]
    pub const fn device(&self) -> Option<&B> {
        self.device.as_ref()
    }
}

/// A reason a parity array could not be assembled or reconfigured. Distinct
/// from [`DriverError`] (which flows on the I/O path) because these are
/// composition-policy failures, not device I/O outcomes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParityError {
    /// A RAID5 array needs at least three member slots (two data + one
    /// parity); fewer cannot form a distributed-parity array.
    TooFewMembers,
    /// The caller-supplied scratch buffer was smaller than two logical
    /// blocks, too small for parity computation/reconstruction.
    ScratchTooSmall,
    /// The stripe unit (`chunk_blocks`) was zero; a parity array needs a
    /// positive chunk size.
    ZeroChunk,
    /// No present member could report its geometry, so no array geometry could
    /// be established. Fails closed.
    NoUsableMember,
    /// Two members report different geometry: they are not members of the same
    /// array. Fails closed rather than truncating to the smaller.
    GeometryMismatch,
    /// A present member reported a degenerate geometry (zero block size or
    /// count).
    ZeroGeometry,
    /// A present member's block count is not a whole number of stripe chunks,
    /// so it cannot be striped evenly. Fails closed rather than leaving a
    /// ragged tail.
    UnalignedGeometry,
    /// The composed logical block count overflows `u64`. Fails closed rather
    /// than wrapping to a smaller array that would truncate addresses.
    TooLarge,
    /// Too many members are missing or unwell for the array to serve: a parity
    /// array tolerates at most one absent/faulted member. Fails closed.
    InsufficientRedundancy,
    /// A member index is outside the array.
    UnknownMember,
    /// A re-add/replace was asked of a member that is not currently faulted.
    NotFaulted,
    /// A re-add/replace member's device could not be probed (absent/unwell).
    ProbeFailed,
    /// [`ParityArray::add_member`] was asked to populate a slot that already
    /// holds a device (it is not [`MemberState::Absent`]).
    SlotOccupied,
}

/// A RAID5 distributed-parity array presenting `member_count - 1` members'
/// worth of capacity as one logical [`Block`] device, surviving any single
/// member fault.
///
/// See the [crate documentation](crate) for the left-symmetric layout and the
/// fail-closed one-fault redundancy model. The array borrows a caller-owned
/// member slice and a caller-owned scratch buffer, so it holds no allocation
/// and imposes no fixed member ceiling (`AGENTS.md` §24.1).
pub struct ParityArray<'a, B: Block> {
    members: &'a mut [ParityMember<B>],
    /// A caller-owned working buffer for parity computation and
    /// reconstruction, at least two logical blocks. Reconstruction and
    /// read-modify-write parity need scratch the `Block` read/write methods
    /// do not carry, so the array borrows it (no allocation, no fixed
    /// ceiling; the caller sizes it, `AGENTS.md` §24.1).
    scratch: &'a mut [u8],
    /// The logical geometry the array presents (block size shared with the
    /// members; block count is `(member_count - 1) * per_member_blocks`).
    geometry: BlockGeometry,
    /// Per-member logical block count (every member is the same size).
    per_member_blocks: u64,
    /// The stripe unit in logical blocks.
    chunk_blocks: u64,
    /// The next logical block a scrub pass will verify, or the array block
    /// count when no scrub is in progress.
    scrub_next_lba: u64,
}

impl<'a, B: Block> ParityArray<'a, B> {
    /// Assemble a RAID5 array from `members` with stripe unit `chunk_blocks`
    /// logical blocks.
    ///
    /// `members` is the array's full member table in slot order; a missing
    /// member is passed as [`ParityMember::absent`]. Every *present* member is
    /// probed for geometry: the first fixes the array geometry, and a member
    /// reporting a different (non-degenerate) geometry fails the assembly
    /// closed. A present member whose probe *errors* is admitted
    /// [`MemberState::Faulted`], and a [`MemberRole::Stale`] member that probes
    /// cleanly begins [`MemberState::Resyncing`] rather than serving stale
    /// reads. The array comes up as long as it retains enough redundancy — at
    /// most one member may be absent/faulted (a parity array reconstructs a
    /// single loss but not two).
    ///
    /// # Errors
    ///
    /// * [`ParityError::TooFewMembers`] if fewer than three slots.
    /// * [`ParityError::ZeroChunk`] if `chunk_blocks` is zero.
    /// * [`ParityError::GeometryMismatch`] if two members disagree on geometry.
    /// * [`ParityError::ZeroGeometry`] / [`ParityError::UnalignedGeometry`] for
    ///   a degenerate or non-chunk-aligned member.
    /// * [`ParityError::NoUsableMember`] if no member could report geometry.
    /// * [`ParityError::TooLarge`] if the composed block count overflows.
    /// * [`ParityError::InsufficientRedundancy`] if more than one member is
    ///   absent/faulted after probing.
    pub fn assemble(
        members: &'a mut [ParityMember<B>],
        scratch: &'a mut [u8],
        chunk_blocks: u32,
    ) -> Result<Self, ParityError> {
        if members.len() < RaidLevel::Parity.min_members() as usize {
            return Err(ParityError::TooFewMembers);
        }
        if chunk_blocks == 0 {
            return Err(ParityError::ZeroChunk);
        }
        let chunk = u64::from(chunk_blocks);
        let mut geometry: Option<BlockGeometry> = None;
        for member in &mut *members {
            let Some(device) = member.device.as_ref() else {
                member.state = MemberState::Absent;
                member.resync_next_lba = 0;
                continue;
            };
            let Ok(g) = device.geometry() else {
                member.state = MemberState::Faulted;
                member.resync_next_lba = 0;
                continue;
            };
            if g.block_size == 0 || g.block_count == 0 {
                return Err(ParityError::ZeroGeometry);
            }
            if !g.block_count.is_multiple_of(chunk) {
                return Err(ParityError::UnalignedGeometry);
            }
            match geometry {
                None => geometry = Some(g),
                Some(existing) if existing == g => {}
                Some(_) => return Err(ParityError::GeometryMismatch),
            }
            member.state = match member.role {
                MemberRole::Current => MemberState::InSync,
                MemberRole::Stale => MemberState::Resyncing,
            };
            member.resync_next_lba = 0;
        }
        let Some(per_member) = geometry else {
            return Err(ParityError::NoUsableMember);
        };
        // Parity computation and reconstruction need at least two logical
        // blocks of scratch (old data + old parity, or an accumulator + a
        // read block); fail closed rather than under-provisioned.
        if scratch.len() < 2 * per_member.block_size as usize {
            return Err(ParityError::ScratchTooSmall);
        }
        // A parity array reconstructs at most one lost member; more than one
        // absent/faulted slot at assembly means it cannot serve. Fail closed.
        let unavailable = members
            .iter()
            .filter(|m| matches!(m.state, MemberState::Absent | MemberState::Faulted))
            .count();
        if unavailable > 1 {
            return Err(ParityError::InsufficientRedundancy);
        }
        let member_count = members.len() as u64;
        let block_count = RaidLevel::Parity
            .logical_block_count(per_member.block_count, member_count)
            .ok_or(ParityError::TooLarge)?;
        Ok(Self {
            members,
            geometry: BlockGeometry {
                block_size: per_member.block_size,
                block_count,
            },
            per_member_blocks: per_member.block_count,
            chunk_blocks: chunk,
            // Scrub walks member-local LBA space (stripe rows); start "done".
            scrub_next_lba: per_member.block_count,
            scratch,
        })
    }

    /// The logical geometry of the composed array (block size shared with the
    /// members; block count is `(member_count - 1)` members' worth).
    #[must_use]
    pub const fn array_geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// The number of member slots the array is defined to have.
    #[must_use]
    pub const fn member_count(&self) -> usize {
        self.members.len()
    }

    /// The state of member `index`, or [`None`] if out of range.
    #[must_use]
    pub fn member_state(&self, index: usize) -> Option<MemberState> {
        self.members.get(index).map(ParityMember::state)
    }

    /// Borrow member `index` (for the serving process to inspect a member's
    /// device identity, health, or rebuild cursor when logging), or [`None`]
    /// if out of range.
    #[must_use]
    pub fn member(&self, index: usize) -> Option<&ParityMember<B>> {
        self.members.get(index)
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// The current [`ArrayHealth`], derived from the members' states.
    ///
    /// [`Optimal`](ArrayHealth::Optimal) when every slot is in sync;
    /// [`Recovering`](ArrayHealth::Recovering) while a member is rebuilding;
    /// [`Degraded`](ArrayHealth::Degraded) with exactly one member
    /// faulted/absent (the array still serves by reconstruction);
    /// [`Failed`](ArrayHealth::Failed) once two or more members are lost (no
    /// redundancy left to reconstruct a stripe).
    #[must_use]
    pub fn health(&self) -> ArrayHealth {
        let mut in_sync = 0usize;
        let mut resyncing = 0usize;
        let mut lost = 0usize;
        for member in &*self.members {
            match member.state {
                MemberState::InSync => in_sync += 1,
                MemberState::Resyncing => resyncing += 1,
                MemberState::Faulted | MemberState::Absent => lost += 1,
            }
        }
        // A stripe needs member_count - 1 present chunks to reconstruct the
        // one it is missing; two lost members leaves a stripe unrecoverable.
        if lost >= 2 || (lost == 1 && in_sync + resyncing < self.members.len() - 1) {
            ArrayHealth::Failed
        } else if resyncing > 0 {
            ArrayHealth::Recovering
        } else if lost > 0 {
            ArrayHealth::Degraded
        } else {
            ArrayHealth::Optimal
        }
    }

    /// Whether a member is still rebuilding.
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.members
            .iter()
            .any(|m| m.state == MemberState::Resyncing)
    }

    /// The number of members currently lost (faulted or absent).
    fn lost_count(&self) -> usize {
        self.members
            .iter()
            .filter(|m| matches!(m.state, MemberState::Faulted | MemberState::Absent))
            .count()
    }

    /// Whether the array can still serve I/O: at most one member lost.
    fn can_serve(&self) -> bool {
        self.lost_count() <= 1
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
}

/// The stripe layout math for a left-symmetric RAID5 array of `n` members with
/// stripe unit `chunk` logical blocks. Pure integer arithmetic, shared by
/// every I/O path so the mapping cannot diverge between them.
#[derive(Copy, Clone)]
struct Layout {
    n: u64,
    chunk: u64,
}

/// Where one logical block lives: the data member holding it, that member's
/// local LBA, the parity member of its stripe, and how many blocks of a
/// `remaining`-block run stay within the same chunk on that member.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Placement {
    data_member: u64,
    parity_member: u64,
    member_lba: u64,
    run: u64,
}

impl Layout {
    /// The parity member for stripe `s` (left-symmetric: parity rotates down
    /// from the last member as the stripe advances).
    fn parity_member(self, stripe: u64) -> u64 {
        (self.n - 1) - (stripe % self.n)
    }

    /// Place logical block `lba`, clamping a run to the chunk boundary.
    fn place(self, lba: u64, remaining: u64) -> Placement {
        let n = self.n;
        let chunk = self.chunk;
        let dchunk = lba / chunk;
        let offset = lba % chunk;
        let data_per_stripe = n - 1;
        let stripe = dchunk / data_per_stripe;
        let dpos = dchunk % data_per_stripe;
        let parity_member = self.parity_member(stripe);
        // The data chunks fill the non-parity members in ascending order
        // starting just after the parity member.
        let data_member = (parity_member + 1 + dpos) % n;
        let member_lba = stripe * chunk + offset;
        let run = (chunk - offset).min(remaining);
        Placement {
            data_member,
            parity_member,
            member_lba,
            run,
        }
    }
}

/// XOR `src` into `dst` bytewise (`dst ^= src`). The slices must be equal
/// length; the caller guarantees this from the shared block size.
fn xor_into(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d ^= *s;
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// The layout math for this array.
    fn layout(&self) -> Layout {
        Layout {
            n: self.members.len() as u64,
            chunk: self.chunk_blocks,
        }
    }

    /// Whether member `idx` is currently a usable read source (in sync).
    fn is_source(&self, idx: usize) -> bool {
        self.members
            .get(idx)
            .is_some_and(|m| m.state == MemberState::InSync)
    }

    /// Mark member `idx` faulted (dropped from the array pending re-add).
    fn fault(&mut self, idx: usize) {
        if let Some(m) = self.members.get_mut(idx) {
            m.state = MemberState::Faulted;
            m.resync_next_lba = 0;
        }
    }

    /// Read `bytes` bytes (one chunk-run of `bytes/bs` blocks) at member-local
    /// `member_lba` from member `idx` into `dst`, classifying the outcome.
    /// Returns `Ok(true)` on success, `Ok(false)` on a per-block/transient
    /// error the array can recover around (device kept), and `Err` only if the
    /// slot holds no device. A whole-device fault marks the member faulted and
    /// returns `Ok(false)`.
    fn try_member_read(
        &mut self,
        idx: usize,
        member_lba: u64,
        dst: &mut [u8],
        class: BufferClass,
    ) -> bool {
        let Some(device) = self.members[idx].device.as_mut() else {
            self.fault(idx);
            return false;
        };
        match device.read_blocks_with_class(member_lba, dst, class) {
            Ok(()) => true,
            Err(e) => {
                if member_faulting(e) {
                    self.fault(idx);
                }
                false
            }
        }
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Reconstruct one logical block at member-local `member_lba` of member
    /// `target` by XOR-ing the same offset from every *other* member, writing
    /// the result into `dst` (exactly one block). Every non-target member must
    /// be an in-sync, readable source; otherwise the stripe has lost a second
    /// member and the block is unrecoverable (fail closed).
    fn reconstruct_block(
        &mut self,
        target: usize,
        member_lba: u64,
        dst: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let bs = self.geometry.block_size as usize;
        let n = self.members.len();
        let mut first = true;
        for idx in 0..n {
            if idx == target {
                continue;
            }
            if self.members[idx].state != MemberState::InSync {
                // A second member is not a source: the stripe cannot be
                // reconstructed. Fail closed rather than fabricate data.
                return Err(DriverError::DeviceOffline);
            }
            if first {
                let Some(device) = self.members[idx].device.as_mut() else {
                    self.fault(idx);
                    return Err(DriverError::DeviceOffline);
                };
                match device.read_blocks_with_class(member_lba, dst, class) {
                    Ok(()) => first = false,
                    Err(e) => {
                        if member_faulting(e) {
                            self.fault(idx);
                        }
                        return Err(e);
                    }
                }
            } else {
                let outcome = if let Some(device) = self.members[idx].device.as_mut() {
                    device.read_blocks_with_class(member_lba, &mut self.scratch[..bs], class)
                } else {
                    self.fault(idx);
                    return Err(DriverError::DeviceOffline);
                };
                if let Err(e) = outcome {
                    if member_faulting(e) {
                        self.fault(idx);
                    }
                    return Err(e);
                }
                xor_into(dst, &self.scratch[..bs]);
            }
        }
        // With n >= 3 there is always at least one non-target member, so
        // `first` was cleared; a stripe with no source at all failed above.
        if first {
            return Err(DriverError::DeviceOffline);
        }
        Ok(())
    }

    /// Serve one chunk-run of `run` blocks of member `data_member` at
    /// member-local `member_lba` into `dst`, reconstructing and repairing as
    /// needed. `dst` is exactly `run` blocks.
    fn read_run(
        &mut self,
        data_member: usize,
        member_lba: u64,
        run: u64,
        dst: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let bs = self.geometry.block_size as usize;
        // Fast path: the data member is an in-sync source and reads cleanly.
        if self.is_source(data_member) && self.try_member_read(data_member, member_lba, dst, class)
        {
            return Ok(());
        }
        // Reconstruct every block of the run from the survivors.
        let run_usize = usize::try_from(run).map_err(|_| DriverError::LengthOutOfRange)?;
        for b in 0..run_usize {
            let off = b * bs;
            self.reconstruct_block(
                data_member,
                member_lba + b as u64,
                &mut dst[off..off + bs],
                class,
            )?;
        }
        // If the data member is still an in-sync source, the failure was a
        // per-block media error, not a whole-device fault: repair it by writing
        // the reconstructed data back (forcing sector reallocation). A repair
        // that fails drops the member. Reconstructed data is opaque on-disk
        // bytes, so the write-back is treated as sensitive.
        if self.is_source(data_member) {
            let failed = match self.members[data_member].device.as_mut() {
                Some(device) => device
                    .write_blocks_with_class(member_lba, dst, BufferClass::Sensitive)
                    .is_err(),
                None => true,
            };
            if failed {
                self.fault(data_member);
            }
        }
        Ok(())
    }

    /// Read `buf.len() / block_size` blocks at logical `lba`.
    fn read_impl(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let total = self.validate_io(lba, buf.len())?;
        if !self.can_serve() {
            return Err(DriverError::DeviceOffline);
        }
        let bs = u64::from(self.geometry.block_size);
        let layout = self.layout();
        let mut cur_lba = lba;
        let mut done = 0u64;
        while done < total {
            let remaining = total - done;
            let place = layout.place(cur_lba, remaining);
            let data_member =
                usize::try_from(place.data_member).map_err(|_| DriverError::LengthOutOfRange)?;
            let start = usize::try_from(done * bs).map_err(|_| DriverError::LengthOutOfRange)?;
            let end = usize::try_from((done + place.run) * bs)
                .map_err(|_| DriverError::LengthOutOfRange)?;
            self.read_run(
                data_member,
                place.member_lba,
                place.run,
                &mut buf[start..end],
                class,
            )?;
            done += place.run;
            cur_lba += place.run;
        }
        Ok(())
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Read one block at member-local `member_lba` of member `idx` into
    /// `self.scratch[dst_off..dst_off+bs]`. Returns whether the read
    /// succeeded; a whole-device fault marks the member faulted.
    fn read_into_scratch(&mut self, idx: usize, member_lba: u64, dst_off: usize) -> bool {
        let bs = self.geometry.block_size as usize;
        let outcome = if let Some(device) = self.members[idx].device.as_mut() {
            device.read_blocks_with_class(
                member_lba,
                &mut self.scratch[dst_off..dst_off + bs],
                BufferClass::Sensitive,
            )
        } else {
            self.fault(idx);
            return false;
        };
        match outcome {
            Ok(()) => true,
            Err(e) => {
                if member_faulting(e) {
                    self.fault(idx);
                }
                false
            }
        }
    }

    /// Write `src` (one block, the caller's own data) to member-local
    /// `member_lba` of member `idx`, faulting it on a whole-device error.
    /// Returns whether the write landed. `class` is the caller's own
    /// buffer-sensitivity marking, forwarded verbatim so a `Sensitive` write is
    /// zeroed on free and a `NonSensitive` bulk write is not needlessly slowed
    /// (matching the mirror and stripe). Parity and reconstruction staging use
    /// [`BufferClass::Sensitive`] regardless, because they mix other stripes'
    /// opaque on-disk bytes; only this write of the caller's own block honours
    /// the caller's class.
    fn write_member_block(
        &mut self,
        idx: usize,
        member_lba: u64,
        src: &[u8],
        class: BufferClass,
    ) -> bool {
        let outcome = if let Some(device) = self.members[idx].device.as_mut() {
            device.write_blocks_with_class(member_lba, src, class)
        } else {
            self.fault(idx);
            return false;
        };
        match outcome {
            Ok(()) => true,
            Err(e) => {
                if member_faulting(e) {
                    self.fault(idx);
                }
                false
            }
        }
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Compute the stripe's new parity for a single-block write into
    /// `self.scratch[bs..2*bs]` by the reconstruct-write rule
    /// `parity = new_data XOR (other data members)`. Reads every *other* data
    /// member of the stripe (all members but `data_member` and
    /// `parity_member`). Returns `true` only if every needed sibling was read;
    /// `false` means the parity cannot be recomputed (a second member is not a
    /// source), so the caller must not write a bogus parity.
    fn reconstruct_parity(
        &mut self,
        data_member: usize,
        parity_member: usize,
        member_lba: u64,
        new_data: &[u8],
    ) -> bool {
        let bs = self.geometry.block_size as usize;
        self.scratch[bs..2 * bs].copy_from_slice(new_data);
        let n = self.members.len();
        for idx in 0..n {
            if idx == data_member || idx == parity_member {
                continue;
            }
            if self.members[idx].state != MemberState::InSync {
                return false;
            }
            if !self.read_into_scratch(idx, member_lba, 0) {
                return false;
            }
            let (lo, hi) = self.scratch.split_at_mut(bs);
            xor_into(&mut hi[..bs], &lo[..bs]);
        }
        true
    }

    /// Write one logical block (`new_data`, exactly one block) of member
    /// `data_member` in the stripe whose parity lives on `parity_member`, at
    /// member-local `member_lba`, updating the parity. Returns whether the new
    /// data is durably represented (written to its member, or encoded into the
    /// parity so it is reconstructable). Faults are contained: a whole-device
    /// error drops that member.
    fn write_block(
        &mut self,
        data_member: usize,
        parity_member: usize,
        member_lba: u64,
        new_data: &[u8],
        class: BufferClass,
    ) -> bool {
        let bs = self.geometry.block_size as usize;
        let data_source = self.is_source(data_member);
        let parity_source = self.is_source(parity_member);
        // Update parity whenever it lives on a usable member.
        let mut parity_reflects_new = false;
        if parity_source {
            let computed = if data_source
                && self.read_into_scratch(data_member, member_lba, 0)
                && self.read_into_scratch(parity_member, member_lba, bs)
            {
                // Read-modify-write: new_parity = old_parity ^ old_data ^ new.
                let (lo, hi) = self.scratch.split_at_mut(bs);
                xor_into(&mut hi[..bs], &lo[..bs]);
                xor_into(&mut self.scratch[bs..2 * bs], new_data);
                true
            } else {
                // Old data/parity unavailable (data lost, or a media error):
                // recompute parity from the surviving data members.
                self.reconstruct_parity(data_member, parity_member, member_lba, new_data)
            };
            // Only write parity if it was computed and its member is still a
            // usable source (an RMW old-parity read may have just faulted it).
            if computed && self.is_source(parity_member) {
                parity_reflects_new = self.write_staged_block(parity_member, member_lba);
            }
        }
        // Write the data to its own member when it is a usable source.
        let data_ok = if data_source && self.is_source(data_member) {
            self.write_member_block(data_member, member_lba, new_data, class)
        } else {
            false
        };
        // Keep a resyncing member's already-synced region current so it never
        // falls behind the array mid-rebuild (the region at/above its cursor is
        // picked up by resync_step from the survivors we just updated).
        if self.members[data_member].state == MemberState::Resyncing
            && member_lba < self.members[data_member].resync_next_lba
        {
            self.write_member_block(data_member, member_lba, new_data, class);
        }
        if self.members[parity_member].state == MemberState::Resyncing
            && member_lba < self.members[parity_member].resync_next_lba
            && self.reconstruct_parity(data_member, parity_member, member_lba, new_data)
        {
            self.write_staged_block(parity_member, member_lba);
        }
        data_ok || parity_reflects_new
    }

    /// Write the block currently staged in `self.scratch[bs..2*bs]` to
    /// member-local `member_lba` of member `idx`, faulting it on a
    /// whole-device error. Returns whether the write landed. Used for both a
    /// freshly computed parity block and a reconstructed resync block.
    fn write_staged_block(&mut self, idx: usize, member_lba: u64) -> bool {
        let bs = self.geometry.block_size as usize;
        let outcome = if let Some(device) = self.members[idx].device.as_mut() {
            device.write_blocks_with_class(
                member_lba,
                &self.scratch[bs..2 * bs],
                BufferClass::Sensitive,
            )
        } else {
            self.fault(idx);
            return false;
        };
        match outcome {
            Ok(()) => true,
            Err(e) => {
                if member_faulting(e) {
                    self.fault(idx);
                }
                false
            }
        }
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Write `buf.len() / block_size` blocks at logical `lba`, splitting at
    /// chunk boundaries and updating each affected stripe's parity.
    fn write_impl(&mut self, lba: u64, buf: &[u8], class: BufferClass) -> Result<(), DriverError> {
        let total = self.validate_io(lba, buf.len())?;
        if !self.can_serve() {
            return Err(DriverError::DeviceOffline);
        }
        let bs = u64::from(self.geometry.block_size);
        let bs_usize = self.geometry.block_size as usize;
        let layout = self.layout();
        let mut cur_lba = lba;
        let mut done = 0u64;
        while done < total {
            let remaining = total - done;
            let place = layout.place(cur_lba, remaining);
            let data_member =
                usize::try_from(place.data_member).map_err(|_| DriverError::LengthOutOfRange)?;
            let parity_member =
                usize::try_from(place.parity_member).map_err(|_| DriverError::LengthOutOfRange)?;
            for b in 0..place.run {
                let off =
                    usize::try_from((done + b) * bs).map_err(|_| DriverError::LengthOutOfRange)?;
                let block = &buf[off..off + bs_usize];
                let accepted = self.write_block(
                    data_member,
                    parity_member,
                    place.member_lba + b,
                    block,
                    class,
                );
                if !accepted {
                    // A block could not be stored on data *or* in parity: a
                    // second member was lost mid-write. Fail closed.
                    return Err(DriverError::DeviceOffline);
                }
            }
            done += place.run;
            cur_lba += place.run;
        }
        Ok(())
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Reconstruct member `target`'s block at member-local `lba` into
    /// `self.scratch[bs..2*bs]` as the XOR of every other member's block at the
    /// same member-local LBA.
    ///
    /// Every member's chunk of a given stripe lives at the same member-local
    /// LBA, and the parity is the XOR of the data chunks, so the XOR of *all*
    /// members' blocks at any member-local LBA is zero. Member `target`'s block
    /// is therefore the XOR of all the others — the uniform reconstruction that
    /// rebuilds a data chunk and a parity chunk alike. Every other member must
    /// be an in-sync source; otherwise a second member is unavailable and the
    /// block is unrecoverable (fail closed).
    fn reconstruct_into_staged(&mut self, target: usize, lba: u64) -> Result<(), DriverError> {
        let bs = self.geometry.block_size as usize;
        let n = self.members.len();
        let mut first = true;
        for idx in 0..n {
            if idx == target {
                continue;
            }
            if self.members[idx].state != MemberState::InSync {
                return Err(DriverError::DeviceOffline);
            }
            if first {
                if !self.read_into_scratch(idx, lba, bs) {
                    return Err(DriverError::DeviceOffline);
                }
                first = false;
            } else {
                if !self.read_into_scratch(idx, lba, 0) {
                    return Err(DriverError::DeviceOffline);
                }
                let (lo, hi) = self.scratch.split_at_mut(bs);
                xor_into(&mut hi[..bs], &lo[..bs]);
            }
        }
        if first {
            return Err(DriverError::DeviceOffline);
        }
        Ok(())
    }

    /// Rebuild up to `blocks` member-local blocks of every resyncing member,
    /// reconstructing each from the survivors and writing it to the target.
    /// Call repeatedly until [`needs_resync`](Self::needs_resync) is false.
    ///
    /// `blocks` bounds the work per call (a larger value rebuilds faster, a
    /// smaller one yields to other work sooner — bounded and interruptible,
    /// never a busy-spin, `AGENTS.md` §26.6). A member whose cursor reaches the
    /// end of its blocks becomes [`MemberState::InSync`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `blocks` is zero.
    /// * [`DriverError::DeviceOffline`] if a stripe cannot be reconstructed
    ///   because a second member is not a source (the array has failed); the
    ///   caller sees the array health drop and stops.
    pub fn resync_step(&mut self, blocks: u64) -> Result<(), DriverError> {
        if blocks == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let per_member = self.per_member_blocks;
        for t in 0..self.members.len() {
            if self.members[t].state != MemberState::Resyncing {
                continue;
            }
            let mut cursor = self.members[t].resync_next_lba;
            if cursor >= per_member {
                self.members[t].state = MemberState::InSync;
                self.members[t].resync_next_lba = 0;
                continue;
            }
            let step_end = cursor.saturating_add(blocks).min(per_member);
            while cursor < step_end {
                self.reconstruct_into_staged(t, cursor)?;
                if !self.write_staged_block(t, cursor) {
                    // The rebuild target failed a write: drop it back to
                    // faulted rather than leaving a partial copy.
                    self.members[t].state = MemberState::Faulted;
                    self.members[t].resync_next_lba = 0;
                    break;
                }
                cursor += 1;
            }
            if self.members[t].state == MemberState::Resyncing {
                if cursor >= per_member {
                    self.members[t].state = MemberState::InSync;
                    self.members[t].resync_next_lba = 0;
                } else {
                    self.members[t].resync_next_lba = cursor;
                }
            }
        }
        Ok(())
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Whether a proactive scrub pass is in progress.
    #[must_use]
    pub const fn scrubbing(&self) -> bool {
        self.scrub_next_lba < self.per_member_blocks
    }

    /// The next member-local block a scrub pass will verify (the scrub cursor);
    /// equal to the per-member block count when no scrub is in progress.
    #[must_use]
    pub const fn scrub_cursor(&self) -> u64 {
        self.scrub_next_lba
    }

    /// Begin a proactive scrub pass from member-local block 0.
    ///
    /// A scrub complements the opportunistic read-path repair: it proactively
    /// reads *every* in-sync member's copy of *every* stripe row and repairs a
    /// latent media error on any member by reconstructing that block from the
    /// others and writing it back (forcing sector reallocation), so a bad
    /// sector is healed while the array still has the redundancy to reconstruct
    /// it (`AGENTS.md` §26.5).
    ///
    /// A parity scrub deliberately does **not** arbitrate a parity *content*
    /// disagreement between members that all read cleanly: a bare parity array
    /// cannot know *which* member is wrong (unlike the checksummed filesystem
    /// layer, ARXFS), and overwriting the wrong one would propagate corruption.
    /// Its remit is latent *media* errors, which it surfaces and heals here.
    ///
    /// Drive the pass with [`scrub_step`](Self::scrub_step) until
    /// [`scrubbing`](Self::scrubbing) is false. Calling `begin_scrub` again
    /// restarts from block 0.
    pub fn begin_scrub(&mut self) {
        self.scrub_next_lba = 0;
    }

    /// Verify and repair up to `blocks` member-local stripe rows of a scrub
    /// pass, advancing the scrub cursor. A no-op once the pass is complete.
    ///
    /// `blocks` bounds the work per call (bounded, interruptible, never a
    /// busy-spin, `AGENTS.md` §26.6). For each stripe row, every in-sync
    /// member's block is read; a whole-device fault drops that member, and a
    /// per-block media error is repaired by writing back the block
    /// reconstructed from the others.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `blocks` is zero.
    /// * [`DriverError::DeviceOffline`] if the array cannot serve (two members
    ///   lost); the cursor does not advance.
    /// * The media error seen if a block was bad on a member and could not be
    ///   reconstructed (a second member was also unavailable there). The cursor
    ///   **still advances** so a repeated call makes progress.
    pub fn scrub_step(&mut self, blocks: u64) -> Result<(), DriverError> {
        if blocks == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        if self.scrub_next_lba >= self.per_member_blocks {
            return Ok(());
        }
        if !self.can_serve() {
            return Err(DriverError::DeviceOffline);
        }
        let bs = self.geometry.block_size as usize;
        let n = self.members.len();
        let start = self.scrub_next_lba;
        let end = start.saturating_add(blocks).min(self.per_member_blocks);
        let mut unrepairable: Option<DriverError> = None;
        let mut lba = start;
        while lba < end {
            for i in 0..n {
                if self.members[i].state != MemberState::InSync {
                    continue;
                }
                // Verify by reading the block; discard into the scratch low half.
                let media = {
                    let outcome = if let Some(device) = self.members[i].device.as_mut() {
                        device.read_blocks_with_class(
                            lba,
                            &mut self.scratch[..bs],
                            BufferClass::Sensitive,
                        )
                    } else {
                        self.fault(i);
                        continue;
                    };
                    match outcome {
                        Ok(()) => None,
                        Err(e) if member_faulting(e) => {
                            self.fault(i);
                            continue;
                        }
                        Err(e) => Some(e),
                    }
                };
                if let Some(media_err) = media {
                    // A per-block media error: repair it by reconstructing from
                    // the others and writing the good block back (forcing sector
                    // reallocation). Only a reconstruction that also fails — a
                    // second member unavailable at this block — is a genuine loss
                    // and is surfaced.
                    if self.reconstruct_into_staged(i, lba).is_ok() {
                        self.write_staged_block(i, lba);
                    } else if unrepairable.is_none() {
                        unrepairable = Some(media_err);
                    }
                }
            }
            lba += 1;
        }
        self.scrub_next_lba = end;
        match unrepairable {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }
}

impl<B: Block> ParityArray<'_, B> {
    /// Begin rebuilding a currently-faulted member from its existing device
    /// (e.g. one returned through its own recovery grace window). The device is
    /// re-probed and, if its geometry matches, the member enters
    /// [`MemberState::Resyncing`] from block 0.
    ///
    /// # Errors
    ///
    /// * [`ParityError::UnknownMember`] if `index` is out of range.
    /// * [`ParityError::NotFaulted`] if the member is not currently faulted.
    /// * [`ParityError::ProbeFailed`] if the device cannot be probed.
    /// * [`ParityError::GeometryMismatch`] if the geometry no longer matches.
    pub fn readd_member(&mut self, index: usize) -> Result<(), ParityError> {
        let per_member = self.per_member_blocks;
        let block_size = self.geometry.block_size;
        let member = self
            .members
            .get_mut(index)
            .ok_or(ParityError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(ParityError::NotFaulted);
        }
        let Some(device) = member.device.as_ref() else {
            return Err(ParityError::ProbeFailed);
        };
        match device.geometry() {
            Ok(g) if g.block_size == block_size && g.block_count == per_member => {
                member.state = MemberState::Resyncing;
                member.resync_next_lba = 0;
                Ok(())
            }
            Ok(_) => Err(ParityError::GeometryMismatch),
            Err(_) => Err(ParityError::ProbeFailed),
        }
    }

    /// Replace a faulted member's device with a fresh one and begin rebuilding
    /// it (a physically-replaced disk hot-swapped into a still-occupied slot).
    ///
    /// # Errors
    ///
    /// * [`ParityError::UnknownMember`] if `index` is out of range.
    /// * [`ParityError::NotFaulted`] if the member is not currently faulted.
    /// * [`ParityError::GeometryMismatch`] / [`ParityError::ProbeFailed`] on a
    ///   mismatch or probe failure; the slot is left faulted holding the new
    ///   device.
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), ParityError> {
        match self.members.get(index) {
            Some(member) if member.state == MemberState::Faulted => {}
            Some(_) => return Err(ParityError::NotFaulted),
            None => return Err(ParityError::UnknownMember),
        }
        self.install_rebuild_target(index, device)
    }

    /// Install a spare into a currently-[`MemberState::Absent`] slot and begin
    /// rebuilding it from the survivors — restoring a missing member's
    /// redundancy without a reboot (`AGENTS.md` §18.4).
    ///
    /// # Errors
    ///
    /// * [`ParityError::UnknownMember`] if `index` is out of range.
    /// * [`ParityError::SlotOccupied`] if the slot already holds a device.
    /// * [`ParityError::GeometryMismatch`] / [`ParityError::ProbeFailed`] on a
    ///   mismatch or probe failure; the slot is left faulted holding the spare.
    pub fn add_member(&mut self, index: usize, device: B) -> Result<(), ParityError> {
        match self.members.get(index) {
            Some(member) if member.state == MemberState::Absent => {}
            Some(_) => return Err(ParityError::SlotOccupied),
            None => return Err(ParityError::UnknownMember),
        }
        self.install_rebuild_target(index, device)
    }

    /// Remove a faulted member's device from its slot, leaving it
    /// [`MemberState::Absent`] and returning the removed device.
    ///
    /// # Errors
    ///
    /// * [`ParityError::UnknownMember`] if `index` is out of range.
    /// * [`ParityError::NotFaulted`] if the member is not currently faulted.
    pub fn remove_member(&mut self, index: usize) -> Result<B, ParityError> {
        let member = self
            .members
            .get_mut(index)
            .ok_or(ParityError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(ParityError::NotFaulted);
        }
        let Some(device) = member.device.take() else {
            member.state = MemberState::Absent;
            member.resync_next_lba = 0;
            return Err(ParityError::NotFaulted);
        };
        member.state = MemberState::Absent;
        member.resync_next_lba = 0;
        Ok(device)
    }

    /// Install `device` into slot `index` and begin rebuilding it, discarding
    /// any device the slot previously held. On a geometry mismatch or probe
    /// failure the slot is left [`MemberState::Faulted`] holding the new
    /// device. The single definition shared by
    /// [`replace_member`](Self::replace_member) and
    /// [`add_member`](Self::add_member).
    fn install_rebuild_target(&mut self, index: usize, device: B) -> Result<(), ParityError> {
        let per_member = self.per_member_blocks;
        let block_size = self.geometry.block_size;
        let member = self
            .members
            .get_mut(index)
            .ok_or(ParityError::UnknownMember)?;
        member.device = Some(device);
        member.resync_next_lba = 0;
        let Some(installed) = member.device.as_ref() else {
            member.state = MemberState::Absent;
            return Err(ParityError::ProbeFailed);
        };
        match installed.geometry() {
            Ok(g) if g.block_size == block_size && g.block_count == per_member => {
                member.state = MemberState::Resyncing;
                Ok(())
            }
            Ok(_) => {
                member.state = MemberState::Faulted;
                Err(ParityError::GeometryMismatch)
            }
            Err(_) => {
                member.state = MemberState::Faulted;
                Err(ParityError::ProbeFailed)
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
}

impl<B: Block> Block for ParityArray<'_, B> {
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
        if !self.can_serve() {
            return Err(DriverError::DeviceOffline);
        }
        let mut worst: Option<DriverError> = None;
        for i in 0..self.members.len() {
            match self.members[i].state {
                MemberState::InSync | MemberState::Resyncing => {
                    let outcome = match self.members[i].device.as_mut() {
                        Some(device) => device.flush(),
                        None => Err(DriverError::DeviceOffline),
                    };
                    if let Err(e) = outcome {
                        if worst.is_none() {
                            worst = Some(e);
                        }
                        self.fault(i);
                    }
                }
                MemberState::Faulted | MemberState::Absent => {}
            }
        }
        // Durability requires every surviving member to have committed: a
        // parity array's redundancy tolerates one loss, so if a flush fault
        // pushed it past that (it can no longer serve), fail closed.
        if self.can_serve() {
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
