//! RAID6 **double distributed-parity** composition over the public block seam.
//!
//! [`DualParityArray`] composes a caller-owned slice of [`DualParityMember`]s
//! into one logical [`Block`] device whose usable capacity is that of
//! `member_count - 2` members and which survives **any two** members being
//! lost. It is the double-fault-redundant sibling of the [`MirrorArray`],
//! [`StripeArray`], and single-parity [`ParityArray`] over the same block seam
//! (`AGENTS.md` §2.2 parallel implementations): it composes child `Block`
//! endpoints and consumes the shared block-health vocabulary
//! ([`tairix_abi::blkio`]) rather than re-inventing it.
//!
//! [`MirrorArray`]: crate::MirrorArray
//! [`StripeArray`]: crate::StripeArray
//! [`ParityArray`]: crate::ParityArray
//!
//! # Layout (left-symmetric double distributed parity)
//!
//! The array of `n` members is cut into **stripes**. One stripe is one chunk
//! ([`ArraySuperblock::chunk_blocks`](crate::ArraySuperblock::chunk_blocks)
//! logical blocks) on *every* member at the same member-local offset; `n - 2`
//! of those chunks hold data, one holds the **P** syndrome (bytewise XOR of
//! the data) and one holds the **Q** syndrome (the Reed-Solomon syndrome
//! `Q = Σ gᵏ·Dₖ` over GF(2^8)). Both syndrome slots
//! rotate one member per stripe so neither is a bottleneck.
//!
//! For stripe `s` the P member is `p = (n - 1) - (s mod n)` and the Q member is
//! `q = (p + 1) mod n`; the `n - 2` data chunks fill the remaining members in
//! ascending order starting just after `q`, so data position `k`
//! (`0 ≤ k < n - 2`) sits on member `(q + 1 + k) mod n` and carries Q
//! coefficient `gᵏ`.
//!
//! # Redundancy — survives two faults, fails closed on three
//!
//! Any single lost chunk is recovered from P (like RAID5); any *two* lost
//! chunks in a stripe are solved from the two independent syndromes. A *third*
//! lost member makes a stripe unsolvable, so the array is
//! [`ArrayHealth::Failed`] and every I/O fails closed (`AGENTS.md` §5.4,
//! §26.5) — it never fabricates data it cannot reconstruct.
//!
//! # Allocation-free
//!
//! Like its siblings, [`DualParityArray`] borrows a caller-owned member slice
//! (no fixed member ceiling, `AGENTS.md` §24.1). Syndrome computation and
//! two-erasure reconstruction need working space, so the array also borrows a
//! caller-owned **scratch** buffer of at least [`SCRATCH_BLOCKS`] logical
//! blocks; the growable tier and the scratch sizing live in the assembling
//! serve process.

use crate::gf256;
use crate::mirror::{member_faulting, ArrayHealth, MemberRole, MemberState};
use crate::superblock::{ArrayProgress, RaidLevel};
use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth};
use tairix_abi::driver::{BufferClass, DriverError};

#[cfg(test)]
mod tests;

/// The number of logical blocks of scratch a [`DualParityArray`] borrows.
///
/// Two-erasure reconstruction streams each surviving chunk through a read
/// buffer while accumulating the known-data P and Q sums and capturing the
/// stored P and Q syndrome blocks (five block slots), stages the solved block
/// in a sixth, and the write path keeps its running new-P and new-Q syndromes
/// in two more — eight block slots, enough to reconstruct any two lost chunks
/// and to recompute both syndromes on a degraded write. The caller sizes the
/// scratch to at least this many logical blocks.
pub const SCRATCH_BLOCKS: usize = 8;

/// One member slot of a [`DualParityArray`]: an optional child [`Block`]
/// device, the role it joined with, its membership state, and — while
/// [`MemberState::Resyncing`] — the rebuild cursor.
///
/// The slot model mirrors [`crate::ParityMember`]: a slot with no device is
/// [`MemberState::Absent`]; every other state has a backing device. Assembly
/// re-derives the real state from a geometry probe and the member's role, so a
/// stale copy is never trusted as a read source.
pub struct DualParityMember<B: Block> {
    device: Option<B>,
    role: MemberRole,
    state: MemberState,
    resync_next_lba: u64,
}

impl<B: Block> DualParityMember<B> {
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

/// A reason a double-parity array could not be assembled or reconfigured.
/// Distinct from [`DriverError`] (which flows on the I/O path) because these
/// are composition-policy failures, not device I/O outcomes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DualParityError {
    /// A RAID6 array needs at least four member slots (two data + P + Q);
    /// fewer cannot form a double-parity array.
    TooFewMembers,
    /// The caller-supplied scratch buffer was smaller than [`SCRATCH_BLOCKS`]
    /// logical blocks, too small for syndrome computation/reconstruction.
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
    /// Too many members are missing or unwell for the array to serve: a
    /// double-parity array tolerates at most two absent/faulted members. Fails
    /// closed.
    InsufficientRedundancy,
    /// A member index is outside the array.
    UnknownMember,
    /// A re-add/replace was asked of a member that is not currently faulted.
    NotFaulted,
    /// A re-add/replace member's device could not be probed (absent/unwell).
    ProbeFailed,
    /// [`DualParityArray::add_member`] was asked to populate a slot that
    /// already holds a device (it is not [`MemberState::Absent`]).
    SlotOccupied,
    /// A restored maintenance cursor named a block outside the array, so it
    /// cannot have come from this array in this shape. Refused rather than
    /// clamped: adopted as a rebuild position it would declare a member fully
    /// copied without its tail ever having been written.
    CursorOutOfRange,
    /// The array has more than 255 data members — more than the GF(2^8) Q
    /// syndrome can give distinct, non-zero coefficients — so a valid Q could
    /// not be encoded. Fails closed rather than build an unrecoverable array.
    TooManyMembers,
}

/// A RAID6 double distributed-parity array presenting `member_count - 2`
/// members' worth of capacity as one logical [`Block`] device, surviving any
/// two member faults.
///
/// See the [crate documentation](crate) for the left-symmetric layout and the
/// fail-closed two-fault redundancy model. The array borrows a caller-owned
/// member slice and a caller-owned scratch buffer (at least [`SCRATCH_BLOCKS`]
/// logical blocks), so it holds no allocation and imposes no fixed member
/// ceiling (`AGENTS.md` §24.1).
pub struct DualParityArray<'a, B: Block> {
    members: &'a mut [DualParityMember<B>],
    /// A caller-owned working buffer for syndrome computation and two-erasure
    /// reconstruction, at least [`SCRATCH_BLOCKS`] logical blocks (no
    /// allocation, no fixed ceiling; the caller sizes it, `AGENTS.md` §24.1).
    scratch: &'a mut [u8],
    /// The logical geometry the array presents (block size shared with the
    /// members; block count is `(member_count - 2) * per_member_blocks`).
    geometry: BlockGeometry,
    /// Per-member logical block count (every member is the same size).
    per_member_blocks: u64,
    /// The stripe unit in logical blocks.
    chunk_blocks: u64,
    /// The next member-local block a scrub pass will verify, or the per-member
    /// block count when no scrub is in progress.
    scrub_next_lba: u64,
}

impl<'a, B: Block> DualParityArray<'a, B> {
    /// Assemble a RAID6 array from `members` with stripe unit `chunk_blocks`
    /// logical blocks.
    ///
    /// `members` is the array's full member table in slot order; a missing
    /// member is passed as [`DualParityMember::absent`]. Every *present*
    /// member is probed for geometry: the first fixes the array geometry, and
    /// a member reporting a different (non-degenerate) geometry fails the
    /// assembly closed. A present member whose probe *errors* is admitted
    /// [`MemberState::Faulted`], and a [`MemberRole::Stale`] member that probes
    /// cleanly begins [`MemberState::Resyncing`] rather than serving stale
    /// reads. The array comes up as long as it retains enough redundancy — at
    /// most *two* members may be absent/faulted (a double-parity array
    /// reconstructs two losses but not three).
    ///
    /// # Errors
    ///
    /// * [`DualParityError::TooFewMembers`] if fewer than four slots.
    /// * [`DualParityError::TooManyMembers`] if the data-member count exceeds
    ///   the 255 the GF(2^8) Q syndrome keeps distinct coefficients for.
    /// * [`DualParityError::ZeroChunk`] if `chunk_blocks` is zero.
    /// * [`DualParityError::ScratchTooSmall`] if the scratch is under
    ///   [`SCRATCH_BLOCKS`] blocks.
    /// * [`DualParityError::GeometryMismatch`] if two members disagree on
    ///   geometry.
    /// * [`DualParityError::ZeroGeometry`] / [`DualParityError::UnalignedGeometry`]
    ///   for a degenerate or non-chunk-aligned member.
    /// * [`DualParityError::NoUsableMember`] if no member could report geometry.
    /// * [`DualParityError::TooLarge`] if the composed block count overflows.
    /// * [`DualParityError::InsufficientRedundancy`] if more than two members
    ///   are absent/faulted after probing.
    pub fn assemble(
        members: &'a mut [DualParityMember<B>],
        scratch: &'a mut [u8],
        chunk_blocks: u32,
    ) -> Result<Self, DualParityError> {
        if members.len() < RaidLevel::DualParity.min_members() as usize {
            return Err(DualParityError::TooFewMembers);
        }
        // Data members are member_count - 2; the Q syndrome keeps distinct
        // non-zero coefficients only up to `gf256::MAX_DATA_MEMBERS` positions,
        // which the level's shared ceiling expresses as its full slot count.
        if members.len() > RaidLevel::DualParity.max_members() as usize {
            return Err(DualParityError::TooManyMembers);
        }
        if chunk_blocks == 0 {
            return Err(DualParityError::ZeroChunk);
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
                return Err(DualParityError::ZeroGeometry);
            }
            if !g.block_count.is_multiple_of(chunk) {
                return Err(DualParityError::UnalignedGeometry);
            }
            match geometry {
                None => geometry = Some(g),
                Some(existing) if existing == g => {}
                Some(_) => return Err(DualParityError::GeometryMismatch),
            }
            member.state = match member.role {
                MemberRole::Current => MemberState::InSync,
                MemberRole::Stale => MemberState::Resyncing,
            };
            member.resync_next_lba = 0;
        }
        let Some(per_member) = geometry else {
            return Err(DualParityError::NoUsableMember);
        };
        if scratch.len() < SCRATCH_BLOCKS * per_member.block_size as usize {
            return Err(DualParityError::ScratchTooSmall);
        }
        // A double-parity array reconstructs at most two lost members; more
        // than two absent/faulted slots at assembly means it cannot serve.
        let unavailable = members
            .iter()
            .filter(|m| matches!(m.state, MemberState::Absent | MemberState::Faulted))
            .count();
        if unavailable > 2 {
            return Err(DualParityError::InsufficientRedundancy);
        }
        let member_count = members.len() as u64;
        let block_count = RaidLevel::DualParity
            .logical_block_count(per_member.block_count, member_count)
            .ok_or(DualParityError::TooLarge)?;
        Ok(Self {
            members,
            geometry: BlockGeometry {
                block_size: per_member.block_size,
                block_count,
            },
            per_member_blocks: per_member.block_count,
            chunk_blocks: chunk,
            scrub_next_lba: per_member.block_count,
            scratch,
        })
    }

    /// The logical geometry of the composed array (block size shared with the
    /// members; block count is `(member_count - 2)` members' worth).
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
        self.members.get(index).map(DualParityMember::state)
    }

    /// Borrow member `index` (for the serving process to inspect a member's
    /// device identity, health, or rebuild cursor when logging), or [`None`]
    /// if out of range.
    #[must_use]
    pub fn member(&self, index: usize) -> Option<&DualParityMember<B>> {
        self.members.get(index)
    }
}

impl<B: Block> DualParityArray<'_, B> {
    /// The current [`ArrayHealth`], derived from the members' states.
    ///
    /// [`Optimal`](ArrayHealth::Optimal) when every slot is in sync;
    /// [`Recovering`](ArrayHealth::Recovering) while a member is rebuilding;
    /// [`Degraded`](ArrayHealth::Degraded) with one or two members lost (the
    /// array still serves by reconstruction);
    /// [`Failed`](ArrayHealth::Failed) once three or more members are lost (no
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
        // A stripe needs member_count - 2 present chunks to reconstruct the two
        // it may be missing; three lost members leaves a stripe unrecoverable.
        if lost >= 3 || (lost > 0 && in_sync + resyncing < self.members.len() - 2) {
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

    /// Whether the array can still serve I/O: at most two members lost.
    fn can_serve(&self) -> bool {
        self.lost_count() <= 2
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

/// The role one member plays in a given stripe.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Role {
    /// A data chunk at data position `k` (Q coefficient `gᵏ`).
    Data(u64),
    /// The P (XOR) syndrome chunk.
    P,
    /// The Q (Reed-Solomon) syndrome chunk.
    Q,
}

/// The stripe layout math for a left-symmetric RAID6 array of `n` members with
/// stripe unit `chunk` logical blocks. Pure integer arithmetic, shared by
/// every I/O path so the mapping cannot diverge between them.
#[derive(Copy, Clone)]
struct Layout {
    n: u64,
    chunk: u64,
}

/// Where one logical block lives: the data member holding it, the P and Q
/// members of its stripe, that member's local LBA, and how many blocks of a
/// `remaining`-block run stay within the same chunk on that member.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Placement {
    data_member: u64,
    p_member: u64,
    q_member: u64,
    member_lba: u64,
    run: u64,
}

impl Layout {
    /// The P and Q members for stripe `s` (left-symmetric: P rotates down from
    /// the last member as the stripe advances, Q sits just after P).
    fn syndrome_members(self, stripe: u64) -> (u64, u64) {
        let p = (self.n - 1) - (stripe % self.n);
        let q = (p + 1) % self.n;
        (p, q)
    }

    /// The role member `member` plays in stripe `stripe`.
    fn role_of(self, member: u64, stripe: u64) -> Role {
        let (p, q) = self.syndrome_members(stripe);
        if member == p {
            Role::P
        } else if member == q {
            Role::Q
        } else {
            // Data chunks fill the non-syndrome members in ascending order
            // starting just after Q, so the data position is the member's
            // distance past Q+1 (modulo n).
            let q1 = (q + 1) % self.n;
            let k = (member + self.n - q1) % self.n;
            Role::Data(k)
        }
    }

    /// Place logical block `lba`, clamping a run to the chunk boundary.
    fn place(self, lba: u64, remaining: u64) -> Placement {
        let n = self.n;
        let chunk = self.chunk;
        let dchunk = lba / chunk;
        let offset = lba % chunk;
        let data_per_stripe = n - 2;
        let stripe = dchunk / data_per_stripe;
        let dpos = dchunk % data_per_stripe;
        let (p, q) = self.syndrome_members(stripe);
        let data_member = ((q + 1) + dpos) % n;
        let member_lba = stripe * chunk + offset;
        let run = (chunk - offset).min(remaining);
        Placement {
            data_member,
            p_member: p,
            q_member: q,
            member_lba,
            run,
        }
    }
}

/// XOR slot `src` into slot `dst` (`dst ^= src`) within one scratch buffer of
/// `bs`-byte blocks. `dst` and `src` must differ; the caller guarantees this.
fn xor_slot(scratch: &mut [u8], bs: usize, dst: usize, src: usize) {
    let (a, b) = split_two(scratch, bs, dst, src);
    for (d, s) in a.iter_mut().zip(b.iter()) {
        *d ^= *s;
    }
}

/// Accumulate `coeff · src` into slot `dst` (`dst ^= coeff·src` over GF(2^8)).
/// `dst` and `src` must differ; the caller guarantees this.
fn qacc_slot(scratch: &mut [u8], bs: usize, dst: usize, src: usize, coeff: u8) {
    let (a, b) = split_two(scratch, bs, dst, src);
    for (d, s) in a.iter_mut().zip(b.iter()) {
        *d ^= gf256::mul(coeff, *s);
    }
}

/// Copy slot `src` onto slot `dst`. `dst` and `src` must differ.
fn copy_slot(scratch: &mut [u8], bs: usize, dst: usize, src: usize) {
    let (a, b) = split_two(scratch, bs, dst, src);
    a.copy_from_slice(b);
}

/// Borrow two distinct `bs`-byte slots of `scratch` as `(&mut slot[dst],
/// &mut slot[src])`.
fn split_two(scratch: &mut [u8], bs: usize, dst: usize, src: usize) -> (&mut [u8], &mut [u8]) {
    debug_assert_ne!(dst, src);
    if dst < src {
        let (lo, hi) = scratch.split_at_mut(src * bs);
        (&mut lo[dst * bs..dst * bs + bs], &mut hi[..bs])
    } else {
        let (lo, hi) = scratch.split_at_mut(dst * bs);
        (&mut hi[..bs], &mut lo[src * bs..src * bs + bs])
    }
}

// Scratch slot roles. Slots 0..=4 are the reconstruction working set; the
// solved block lands in slot 5 so a caller can read slots 0..=4 while it is
// written. Slots 6..=7 are the write path's P and Q accumulators, disjoint
// from the reconstruction set so a degraded write may reconstruct a lost data
// member into slot 5 and fold it in without clobbering an accumulator.
const S_TMP: usize = 0; // per-member read buffer
const S_KP: usize = 1; // XOR of surviving data blocks (known-data P)
const S_KQ: usize = 2; // Σ gᵏ·(surviving data) (known-data Q)
const S_PV: usize = 3; // the stored P syndrome block, when P survived
const S_QV: usize = 4; // the stored Q syndrome block, when Q survived
const S_OUT: usize = 5; // the reconstructed target block
const S_PACC: usize = 6; // the write path's running new-P accumulator
const S_QACC: usize = 7; // the write path's running new-Q accumulator

impl<B: Block> DualParityArray<'_, B> {
    /// The layout math for this array.
    fn layout(&self) -> Layout {
        Layout {
            n: self.members.len() as u64,
            chunk: self.chunk_blocks,
        }
    }

    /// The byte size of one logical block.
    fn bs(&self) -> usize {
        self.geometry.block_size as usize
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

    /// Read one block at member-local `member_lba` of member `idx` into `dst`
    /// (exactly one block), classifying the outcome. Returns `true` on success;
    /// a whole-device fault marks the member faulted and returns `false`, and a
    /// per-block/transient error returns `false` keeping the device.
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

    /// Read one block at member-local `member_lba` of member `idx` into scratch
    /// slot `slot`, returning the read outcome (`Ok`, or the error). A
    /// whole-device fault marks the member faulted.
    fn read_member_into_slot(
        &mut self,
        idx: usize,
        member_lba: u64,
        slot: usize,
    ) -> Result<(), DriverError> {
        let bs = self.bs();
        let off = slot * bs;
        let outcome = if let Some(device) = self.members[idx].device.as_mut() {
            device.read_blocks_with_class(
                member_lba,
                &mut self.scratch[off..off + bs],
                BufferClass::Sensitive,
            )
        } else {
            self.fault(idx);
            return Err(DriverError::DeviceOffline);
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                if member_faulting(e) {
                    self.fault(idx);
                }
                Err(e)
            }
        }
    }

    /// Write `src` (one block) to member-local `member_lba` of member `idx`,
    /// faulting it on a whole-device error. Returns whether the write landed.
    /// `class` is the caller's own buffer-sensitivity marking, forwarded
    /// verbatim so a `Sensitive` write is zeroed on free and a `NonSensitive`
    /// bulk write is not needlessly slowed (matching the mirror and stripe).
    /// The P/Q syndrome and reconstruction staging use
    /// [`BufferClass::Sensitive`] regardless (they mix other stripes' opaque
    /// on-disk bytes); only a write of the caller's own block honours its
    /// class, and the read-repair write-back of reconstructed data stays
    /// `Sensitive` for the same opaque-bytes reason.
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

    /// Write the block staged in scratch slot `slot` to member-local
    /// `member_lba` of member `idx`, faulting it on a whole-device error.
    fn write_slot_block(&mut self, idx: usize, member_lba: u64, slot: usize) -> bool {
        let bs = self.bs();
        let off = slot * bs;
        let outcome = if let Some(device) = self.members[idx].device.as_mut() {
            device.write_blocks_with_class(
                member_lba,
                &self.scratch[off..off + bs],
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

impl<B: Block> DualParityArray<'_, B> {
    /// Reconstruct member `target`'s block at member-local `member_lba` into
    /// scratch slot [`S_OUT`] by solving the stripe's P and Q syndromes.
    ///
    /// `target` and every member the array currently treats as not in sync
    /// (faulted, absent, or still resyncing) are the *unknowns* of the stripe
    /// row. With at most two syndromes there can be at most two unknowns; a
    /// third makes the row unsolvable and the block is failed closed
    /// (`AGENTS.md` §5.4). Every surviving (in-sync) member is read to build
    /// the known-data P and Q sums and to capture the stored P/Q syndrome
    /// blocks, then the target is solved from the appropriate one or two
    /// syndrome equations. The result lands in slot [`S_OUT`] rather than an
    /// external buffer, so a caller (a degraded write) can reconstruct a lost
    /// data member without a second mutable borrow of the array.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceOffline`] if the row has more than two unknowns.
    /// * The read error of a surviving member if one could not be read (a
    ///   whole-device fault also drops that member); the block is unrecoverable
    ///   because a needed survivor is gone.
    fn reconstruct_into(&mut self, member_lba: u64, target: usize) -> Result<(), DriverError> {
        let bs = self.bs();
        let n = self.members.len();
        let layout = self.layout();
        let stripe = member_lba / self.chunk_blocks;

        // The unknowns: every not-in-sync member, plus the target itself.
        let mut unknown = [usize::MAX; 2];
        let mut ucount = 0usize;
        for i in 0..n {
            if i == target || self.members[i].state != MemberState::InSync {
                if ucount == 2 {
                    // A third unknown: the stripe cannot be solved. Fail closed.
                    return Err(DriverError::DeviceOffline);
                }
                unknown[ucount] = i;
                ucount += 1;
            }
        }

        // Stream every surviving member, accumulating the known-data syndromes
        // and capturing the stored P/Q blocks.
        for slot in [S_KP, S_KQ] {
            self.scratch[slot * bs..slot * bs + bs].fill(0);
        }
        for i in 0..n {
            if i == unknown[0] || (ucount == 2 && i == unknown[1]) {
                continue;
            }
            self.read_member_into_slot(i, member_lba, S_TMP)?;
            match layout.role_of(i as u64, stripe) {
                Role::Data(k) => {
                    xor_slot(self.scratch, bs, S_KP, S_TMP);
                    let coeff = gf256::gpow(k);
                    qacc_slot(self.scratch, bs, S_KQ, S_TMP, coeff);
                }
                Role::P => copy_slot(self.scratch, bs, S_PV, S_TMP),
                Role::Q => copy_slot(self.scratch, bs, S_QV, S_TMP),
            }
        }

        let target_role = layout.role_of(target as u64, stripe);
        if ucount == 1 {
            match target_role {
                // A lost data chunk with both syndromes present: D = P ⊕ Σdata.
                Role::Data(_) => self.solve_xor(bs, S_PV, S_KP),
                // A lost P is just the XOR of every (present) data chunk.
                Role::P => self.solve_copy(bs, S_KP),
                // A lost Q is Σ gᵏ·data over the (present) data chunks.
                Role::Q => self.solve_copy(bs, S_KQ),
            }
            return Ok(());
        }

        // Two unknowns: identify the other one and its role.
        let other = if unknown[0] == target {
            unknown[1]
        } else {
            unknown[0]
        };
        let other_role = layout.role_of(other as u64, stripe);
        match (target_role, other_role) {
            // Two lost data chunks, both syndromes present: solve the 2×2
            // system. Pxy = D_x ⊕ D_y, Qxy = gˣ·D_x ⊕ gʸ·D_y, so
            // D_x = (Qxy ⊕ gʸ·Pxy) / (gˣ ⊕ gʸ).
            (Role::Data(x), Role::Data(y)) => {
                let gy = gf256::gpow(y);
                let inv_denom = gf256::inv(gf256::gpow(x) ^ gy);
                for b in 0..bs {
                    let pxy = self.g(S_PV, b) ^ self.g(S_KP, b);
                    let qxy = self.g(S_QV, b) ^ self.g(S_KQ, b);
                    let v = gf256::mul(inv_denom, qxy ^ gf256::mul(gy, pxy));
                    self.put_out(b, v);
                }
            }
            // Lost data chunk x and the P syndrome: recover x from Q.
            (Role::Data(x), Role::P) => {
                let inv_gx = gf256::inv(gf256::gpow(x));
                for b in 0..bs {
                    let v = gf256::mul(inv_gx, self.g(S_QV, b) ^ self.g(S_KQ, b));
                    self.put_out(b, v);
                }
            }
            // Lost data chunk x and the Q syndrome: recover x from P.
            (Role::Data(_), Role::Q) => self.solve_xor(bs, S_PV, S_KP),
            // Lost P and a data chunk x: recover x from Q, then P = Σdata.
            (Role::P, Role::Data(x)) => {
                let inv_gx = gf256::inv(gf256::gpow(x));
                for b in 0..bs {
                    let dx = gf256::mul(inv_gx, self.g(S_QV, b) ^ self.g(S_KQ, b));
                    let v = self.g(S_KP, b) ^ dx;
                    self.put_out(b, v);
                }
            }
            // Lost P and Q: P is the XOR of every (present) data chunk.
            (Role::P, Role::Q) => self.solve_copy(bs, S_KP),
            // Lost Q and a data chunk x: recover x from P, then Q = Σ gᵏ·data.
            (Role::Q, Role::Data(x)) => {
                let gx = gf256::gpow(x);
                for b in 0..bs {
                    let dx = self.g(S_PV, b) ^ self.g(S_KP, b);
                    let v = self.g(S_KQ, b) ^ gf256::mul(gx, dx);
                    self.put_out(b, v);
                }
            }
            // Lost Q and P: Q is Σ gᵏ·data over every (present) data chunk.
            (Role::Q, Role::P) => self.solve_copy(bs, S_KQ),
            // Two P's or two Q's cannot both be unknowns of one stripe (each
            // syndrome occupies exactly one distinct member), so the remaining
            // combinations are unreachable.
            (Role::P, Role::P) | (Role::Q, Role::Q) => {
                return Err(DriverError::DeviceOffline);
            }
        }
        Ok(())
    }

    /// One byte of scratch slot `slot`.
    fn g(&self, slot: usize, b: usize) -> u8 {
        self.scratch[slot * self.bs() + b]
    }

    /// Store one byte into the reconstructed-output slot [`S_OUT`].
    fn put_out(&mut self, b: usize, v: u8) {
        let bs = self.bs();
        self.scratch[S_OUT * bs + b] = v;
    }

    /// Solve `S_OUT = slot_a ⊕ slot_b` byte-wise (the common XOR recovery).
    fn solve_xor(&mut self, bs: usize, slot_a: usize, slot_b: usize) {
        for b in 0..bs {
            let v = self.g(slot_a, b) ^ self.g(slot_b, b);
            self.put_out(b, v);
        }
    }

    /// Solve `S_OUT = slot` byte-wise (a syndrome that is exactly a known sum).
    fn solve_copy(&mut self, bs: usize, slot: usize) {
        for b in 0..bs {
            let v = self.g(slot, b);
            self.put_out(b, v);
        }
    }
}

impl<B: Block> DualParityArray<'_, B> {
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
        let bs = self.bs();
        // Fast path: the data member is an in-sync source and reads cleanly.
        if self.is_source(data_member) && self.try_member_read(data_member, member_lba, dst, class)
        {
            return Ok(());
        }
        // Reconstruct every block of the run from the survivors, copying each
        // solved block out of the scratch output slot.
        let run_usize = usize::try_from(run).map_err(|_| DriverError::LengthOutOfRange)?;
        for b in 0..run_usize {
            let off = b * bs;
            self.reconstruct_into(member_lba + b as u64, data_member)?;
            dst[off..off + bs].copy_from_slice(&self.scratch[S_OUT * bs..S_OUT * bs + bs]);
        }
        // If the data member is still an in-sync source, the failure was a
        // per-block media error, not a whole-device fault: repair it by writing
        // the reconstructed data back (forcing sector reallocation). A repair
        // that fails drops the member.
        if self.is_source(data_member)
            && !self.write_member_block(data_member, member_lba, dst, BufferClass::Sensitive)
        {
            self.fault(data_member);
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

/// The resolved stripe roles for a single-block write: the array indices of
/// the data member and its stripe's P and Q syndrome members, plus the data
/// member's Q coefficient exponent `x`. Grouping them keeps the per-block
/// write path a small, self-documenting call rather than a long positional
/// argument list.
#[derive(Copy, Clone)]
struct WriteRoles {
    data_member: usize,
    p_member: usize,
    q_member: usize,
    x: u64,
}

impl<B: Block> DualParityArray<'_, B> {
    /// Try the read-modify-write fast path for a single-block write: when the
    /// data member and both syndromes are in-sync and their old blocks read
    /// cleanly, the new P and Q are the old ones adjusted by the data delta.
    /// Leaves the new P in [`S_PACC`] and the new Q in [`S_QACC`] and returns
    /// `true` on success; `false` if any of the three could not be read (the
    /// caller falls back to a full recompute).
    fn try_rmw(
        &mut self,
        data_member: usize,
        p_member: usize,
        q_member: usize,
        x: u64,
        member_lba: u64,
        new_data: &[u8],
    ) -> bool {
        if !(self.is_source(data_member) && self.is_source(p_member) && self.is_source(q_member)) {
            return false;
        }
        if self
            .read_member_into_slot(data_member, member_lba, S_TMP)
            .is_err()
            || self
                .read_member_into_slot(p_member, member_lba, S_PACC)
                .is_err()
            || self
                .read_member_into_slot(q_member, member_lba, S_QACC)
                .is_err()
        {
            return false;
        }
        let bs = self.bs();
        let gx = gf256::gpow(x);
        for (b, &nd) in new_data.iter().enumerate() {
            // delta = old_data ⊕ new_data; new_P = old_P ⊕ delta;
            // new_Q = old_Q ⊕ gˣ·delta.
            let delta = self.scratch[S_TMP * bs + b] ^ nd;
            self.scratch[S_PACC * bs + b] ^= delta;
            self.scratch[S_QACC * bs + b] ^= gf256::mul(gx, delta);
        }
        true
    }

    /// Recompute the stripe's new P and Q syndromes from every data member's
    /// *current* content (reconstructing any lost/resyncing data member), with
    /// the data member at position `x` replaced by `new_data`. Leaves the new
    /// P in [`S_PACC`] and the new Q in [`S_QACC`]. Returns `false` if a needed
    /// data member could neither be read nor reconstructed (the stripe has more
    /// losses than the redundancy can cover), so the caller fails the write
    /// closed.
    fn recompute_syndromes(
        &mut self,
        data_member: usize,
        x: u64,
        member_lba: u64,
        new_data: &[u8],
    ) -> bool {
        let bs = self.bs();
        let n = self.members.len();
        let layout = self.layout();
        let stripe = member_lba / self.chunk_blocks;
        let gx = gf256::gpow(x);
        for (b, &nd) in new_data.iter().enumerate() {
            self.scratch[S_PACC * bs + b] = nd;
            self.scratch[S_QACC * bs + b] = gf256::mul(gx, nd);
        }
        for m in 0..n {
            if m == data_member {
                continue;
            }
            let Role::Data(k) = layout.role_of(m as u64, stripe) else {
                continue; // the P and Q members do not contribute to the sums
            };
            // Fetch member m's current content into S_TMP: read it if it is a
            // source, otherwise (lost/resyncing, or a source that just errored)
            // reconstruct it from the survivors.
            let have =
                self.is_source(m) && self.read_member_into_slot(m, member_lba, S_TMP).is_ok();
            if !have {
                if self.reconstruct_into(member_lba, m).is_err() {
                    return false;
                }
                self.scratch
                    .copy_within(S_OUT * bs..S_OUT * bs + bs, S_TMP * bs);
            }
            let gk = gf256::gpow(k);
            for b in 0..bs {
                let val = self.scratch[S_TMP * bs + b];
                self.scratch[S_PACC * bs + b] ^= val;
                self.scratch[S_QACC * bs + b] ^= gf256::mul(gk, val);
            }
        }
        true
    }

    /// Write one logical block (`new_data`, exactly one block) of member
    /// `data_member` (data position `x`) in the stripe whose syndromes live on
    /// `p_member`/`q_member`, at member-local `member_lba`. Returns whether the
    /// new data is durably represented — written to its member and/or encoded
    /// into the syndromes so it stays reconstructable. Faults are contained: a
    /// whole-device error drops that member.
    fn write_block(
        &mut self,
        roles: WriteRoles,
        member_lba: u64,
        new_data: &[u8],
        class: BufferClass,
    ) -> bool {
        let WriteRoles {
            data_member,
            p_member,
            q_member,
            x,
        } = roles;
        // Compute the stripe's new P and Q (fast RMW, else full recompute).
        if !self.try_rmw(data_member, p_member, q_member, x, member_lba, new_data)
            && !self.recompute_syndromes(data_member, x, member_lba, new_data)
        {
            return false;
        }
        // Write each stripe role that is a live source with its new value; a
        // failed write faults that member (its stale block is then excluded).
        if self.is_source(data_member) {
            self.write_member_block(data_member, member_lba, new_data, class);
        }
        if self.is_source(p_member) {
            self.write_slot_block(p_member, member_lba, S_PACC);
        }
        if self.is_source(q_member) {
            self.write_slot_block(q_member, member_lba, S_QACC);
        }
        // Keep a resyncing member's already-synced region current so it never
        // falls behind the array mid-rebuild.
        if self.in_synced_region(data_member, member_lba) {
            self.write_member_block(data_member, member_lba, new_data, class);
        }
        if self.in_synced_region(p_member, member_lba) {
            self.write_slot_block(p_member, member_lba, S_PACC);
        }
        if self.in_synced_region(q_member, member_lba) {
            self.write_slot_block(q_member, member_lba, S_QACC);
        }
        // The new data is durable while the array still has the redundancy to
        // reconstruct whatever roles were lost; a write that pushed it past two
        // losses fails closed.
        self.can_serve()
    }

    /// Whether member `idx` is resyncing and `member_lba` falls within its
    /// already-rebuilt region (so a new write must reach it to keep it current).
    fn in_synced_region(&self, idx: usize, member_lba: u64) -> bool {
        self.members[idx].state == MemberState::Resyncing
            && member_lba < self.members[idx].resync_next_lba
    }

    /// Write `buf.len() / block_size` blocks at logical `lba`, splitting at
    /// chunk boundaries and updating each affected stripe's P and Q syndromes.
    fn write_impl(&mut self, lba: u64, buf: &[u8], class: BufferClass) -> Result<(), DriverError> {
        let total = self.validate_io(lba, buf.len())?;
        if !self.can_serve() {
            return Err(DriverError::DeviceOffline);
        }
        let bs = u64::from(self.geometry.block_size);
        let bs_usize = self.geometry.block_size as usize;
        let layout = self.layout();
        let stripe_chunk = self.chunk_blocks;
        let mut cur_lba = lba;
        let mut done = 0u64;
        while done < total {
            let remaining = total - done;
            let place = layout.place(cur_lba, remaining);
            let data_member =
                usize::try_from(place.data_member).map_err(|_| DriverError::LengthOutOfRange)?;
            let p_member =
                usize::try_from(place.p_member).map_err(|_| DriverError::LengthOutOfRange)?;
            let q_member =
                usize::try_from(place.q_member).map_err(|_| DriverError::LengthOutOfRange)?;
            // The data position of this member in its stripe fixes its Q
            // coefficient; it is constant across the whole run (one chunk).
            let stripe = place.member_lba / stripe_chunk;
            let Role::Data(x) = layout.role_of(place.data_member, stripe) else {
                // `place` only ever names a data member; a syndrome member here
                // would be a layout bug. Fail closed rather than mis-encode.
                return Err(DriverError::DeviceFault);
            };
            let roles = WriteRoles {
                data_member,
                p_member,
                q_member,
                x,
            };
            for b in 0..place.run {
                let off =
                    usize::try_from((done + b) * bs).map_err(|_| DriverError::LengthOutOfRange)?;
                let block = &buf[off..off + bs_usize];
                if !self.write_block(roles, place.member_lba + b, block, class) {
                    // A block could not be stored on data nor encoded into the
                    // syndromes: a third member was lost mid-write. Fail closed.
                    return Err(DriverError::DeviceOffline);
                }
            }
            done += place.run;
            cur_lba += place.run;
        }
        Ok(())
    }
}

impl<B: Block> DualParityArray<'_, B> {
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
    ///   because a third member is not a source (the array has failed); the
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
                self.reconstruct_into(cursor, t)?;
                if !self.write_slot_block(t, cursor, S_OUT) {
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

impl<B: Block> DualParityArray<'_, B> {
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
    /// Like the single-parity array it heals latent *media* errors; it does not
    /// arbitrate a syndrome *content* disagreement between members that all
    /// read cleanly (a bare array cannot know which is wrong — that is the
    /// checksummed filesystem layer's job).
    ///
    /// Drive the pass with [`scrub_step`](Self::scrub_step) until
    /// [`scrubbing`](Self::scrubbing) is false. Calling `begin_scrub` again
    /// restarts from block 0.
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
    /// (`AGENTS.md` §26.6). Several members can rebuild at once with different
    /// cursors, and one record can only carry a single position, so the
    /// **least advanced** is reported: resuming from it re-copies blocks a
    /// further-ahead member already had (harmless — a rebuild write is
    /// idempotent) and can never skip a block that was still outstanding.
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
    /// un-sync a current member.
    ///
    /// # Errors
    ///
    /// [`DualParityError::CursorOutOfRange`] if a cursor names a block outside
    /// the array. The array is left exactly as it was, so the caller can
    /// proceed from the fresh-start position.
    pub fn restore_progress(&mut self, progress: ArrayProgress) -> Result<(), DualParityError> {
        if !progress.fits_span(self.per_member_blocks) {
            return Err(DualParityError::CursorOutOfRange);
        }
        if let Some(cursor) = progress.scrub_cursor {
            self.scrub_next_lba = cursor;
        }
        if let Some(cursor) = progress.resync_cursor {
            for member in &mut *self.members {
                if member.state == MemberState::Resyncing {
                    member.resync_next_lba = cursor;
                }
            }
        }
        Ok(())
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
    /// * [`DriverError::DeviceOffline`] if the array cannot serve (three
    ///   members lost); the cursor does not advance.
    /// * The media error seen if a block was bad on a member and could not be
    ///   reconstructed (a third member was also unavailable there). The cursor
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
        let bs = self.bs();
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
                // Verify by reading the block into the scratch temp slot.
                let Some(device) = self.members[i].device.as_mut() else {
                    self.fault(i);
                    continue;
                };
                let off = S_TMP * bs;
                let media = match device.read_blocks_with_class(
                    lba,
                    &mut self.scratch[off..off + bs],
                    BufferClass::Sensitive,
                ) {
                    Ok(()) => None,
                    Err(e) if member_faulting(e) => {
                        self.fault(i);
                        continue;
                    }
                    Err(e) => Some(e),
                };
                if let Some(media_err) = media {
                    // A per-block media error: repair by reconstructing from the
                    // others and writing the good block back. Only a
                    // reconstruction that also fails — a further member
                    // unavailable at this block — is a genuine loss.
                    if self.reconstruct_into(lba, i).is_ok() {
                        self.write_slot_block(i, lba, S_OUT);
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

impl<B: Block> DualParityArray<'_, B> {
    /// Begin rebuilding a currently-faulted member from its existing device
    /// (e.g. one returned through its own recovery grace window). The device is
    /// re-probed and, if its geometry matches, the member enters
    /// [`MemberState::Resyncing`] from block 0.
    ///
    /// # Errors
    ///
    /// * [`DualParityError::UnknownMember`] if `index` is out of range.
    /// * [`DualParityError::NotFaulted`] if the member is not currently faulted.
    /// * [`DualParityError::ProbeFailed`] if the device cannot be probed.
    /// * [`DualParityError::GeometryMismatch`] if the geometry no longer matches.
    pub fn readd_member(&mut self, index: usize) -> Result<(), DualParityError> {
        let per_member = self.per_member_blocks;
        let block_size = self.geometry.block_size;
        let member = self
            .members
            .get_mut(index)
            .ok_or(DualParityError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(DualParityError::NotFaulted);
        }
        let Some(device) = member.device.as_ref() else {
            return Err(DualParityError::ProbeFailed);
        };
        match device.geometry() {
            Ok(g) if g.block_size == block_size && g.block_count == per_member => {
                member.state = MemberState::Resyncing;
                member.resync_next_lba = 0;
                Ok(())
            }
            Ok(_) => Err(DualParityError::GeometryMismatch),
            Err(_) => Err(DualParityError::ProbeFailed),
        }
    }

    /// Replace a faulted member's device with a fresh one and begin rebuilding
    /// it (a physically-replaced disk hot-swapped into a still-occupied slot).
    ///
    /// # Errors
    ///
    /// * [`DualParityError::UnknownMember`] if `index` is out of range.
    /// * [`DualParityError::NotFaulted`] if the member is not currently faulted.
    /// * [`DualParityError::GeometryMismatch`] / [`DualParityError::ProbeFailed`]
    ///   on a mismatch or probe failure; the slot is left faulted holding the
    ///   new device.
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), DualParityError> {
        match self.members.get(index) {
            Some(member) if member.state == MemberState::Faulted => {}
            Some(_) => return Err(DualParityError::NotFaulted),
            None => return Err(DualParityError::UnknownMember),
        }
        self.install_rebuild_target(index, device)
    }

    /// Install a spare into a currently-[`MemberState::Absent`] slot and begin
    /// rebuilding it from the survivors — restoring a missing member's
    /// redundancy without a reboot (`AGENTS.md` §18.4).
    ///
    /// # Errors
    ///
    /// * [`DualParityError::UnknownMember`] if `index` is out of range.
    /// * [`DualParityError::SlotOccupied`] if the slot already holds a device.
    /// * [`DualParityError::GeometryMismatch`] / [`DualParityError::ProbeFailed`]
    ///   on a mismatch or probe failure; the slot is left faulted holding the
    ///   spare.
    pub fn add_member(&mut self, index: usize, device: B) -> Result<(), DualParityError> {
        match self.members.get(index) {
            Some(member) if member.state == MemberState::Absent => {}
            Some(_) => return Err(DualParityError::SlotOccupied),
            None => return Err(DualParityError::UnknownMember),
        }
        self.install_rebuild_target(index, device)
    }

    /// Remove a faulted member's device from its slot, leaving it
    /// [`MemberState::Absent`] and returning the removed device.
    ///
    /// # Errors
    ///
    /// * [`DualParityError::UnknownMember`] if `index` is out of range.
    /// * [`DualParityError::NotFaulted`] if the member is not currently faulted.
    pub fn remove_member(&mut self, index: usize) -> Result<B, DualParityError> {
        let member = self
            .members
            .get_mut(index)
            .ok_or(DualParityError::UnknownMember)?;
        if member.state != MemberState::Faulted {
            return Err(DualParityError::NotFaulted);
        }
        let Some(device) = member.device.take() else {
            member.state = MemberState::Absent;
            member.resync_next_lba = 0;
            return Err(DualParityError::NotFaulted);
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
    fn install_rebuild_target(&mut self, index: usize, device: B) -> Result<(), DualParityError> {
        let per_member = self.per_member_blocks;
        let block_size = self.geometry.block_size;
        let member = self
            .members
            .get_mut(index)
            .ok_or(DualParityError::UnknownMember)?;
        member.device = Some(device);
        member.resync_next_lba = 0;
        let Some(installed) = member.device.as_ref() else {
            member.state = MemberState::Absent;
            return Err(DualParityError::ProbeFailed);
        };
        match installed.geometry() {
            Ok(g) if g.block_size == block_size && g.block_count == per_member => {
                member.state = MemberState::Resyncing;
                Ok(())
            }
            Ok(_) => {
                member.state = MemberState::Faulted;
                Err(DualParityError::GeometryMismatch)
            }
            Err(_) => {
                member.state = MemberState::Faulted;
                Err(DualParityError::ProbeFailed)
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

impl<B: Block> Block for DualParityArray<'_, B> {
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
        // Durability requires the array to retain its two-fault redundancy: if
        // flush faults pushed it past two losses (it can no longer serve), fail
        // closed.
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
