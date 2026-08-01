//! An owning composed RAID device ([`OwnedRaidArray`]) for a long-running
//! serve process that discovers arrays at runtime (`plans/FIX-IO.md` IO6).
//!
//! The six composition engines ([`MirrorArray`], [`StripeArray`],
//! [`ParityArray`], [`DualParityArray`], [`TripleParityArray`],
//! [`Raid10Array`]) and the [`RaidArray`] dispatch over them all deliberately
//! *borrow* a caller-owned member slice: that is what lets them impose no
//! member ceiling and hold no allocation. A serve process that discovers a
//! variable number of arrays at runtime, though, must own its members on the
//! heap — and it must also hold the composed device somewhere, which in Rust
//! cannot be one self-referential struct (a type cannot own a `Vec` and also
//! store a borrow into that same `Vec` alongside it).
//!
//! Re-running [`RaidArray`]'s assembly on every operation is not a valid
//! workaround. Assembly deliberately re-derives every present member's
//! [`MemberState`] from a fresh geometry probe: a member that faulted while
//! serving would probe cleanly again the moment the transient fault clears,
//! and a re-assemble would silently re-admit it as [`MemberState::InSync`] —
//! trusting it as a read source again on the strength of one clean probe,
//! with no rebuild ever run over the writes it missed while it was down.
//! That is a data-integrity fault, not a cosmetic one: a copy the array
//! dropped must stay dropped until it is deliberately re-added or replaced,
//! never quietly resurrected by the next probe that happens to succeed.
//!
//! [`MirrorArray::from_prepared`] already solves exactly this problem
//! internally: it builds a transient view over already-prepared members,
//! preserving whatever membership state they already carry, and
//! [`Raid10Array`] drives each of its mirror pairs through one such view per
//! operation. This module generalises that idiom to every level and exposes
//! it once as a public owning wrapper, so neither of the two consumers named
//! in `plans/FIX-IO.md` — the autoloaded RAID composer and ARXFS's
//! multi-device volumes — has to hand-roll its own per-level member storage.
//!
//! [`OwnedRaidArray`] owns its members in a growable [`Vec`] per level (never
//! a fixed-size buffer) plus the handful of scalar values a transient view
//! needs beyond the members themselves (the composed geometry and the
//! resumable scrub position). The *first* construction goes through the
//! level's real `assemble`, so every fail-closed refusal it can produce still
//! refuses here. Every operation after that builds a transient
//! `from_prepared` view over the owned members for its duration, drives the
//! request through it, and writes the array-level scrub cursor back before
//! returning, so the next view resumes exactly where this one left off. The
//! members themselves are never rebuilt or re-probed between operations:
//! their membership state, and each resyncing member's own rebuild cursor,
//! simply persist in the owned `Vec` for free.

use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use tairix_abi::driver::{BufferClass, DriverError};

use crate::array::{RaidArray, RaidError};
use crate::dualparity::{DualParityArray, DualParityError, DualParityMember};
use crate::mirror::{ArrayHealth, MemberState, MirrorArray, MirrorError, MirrorMember};
use crate::parity::{ParityArray, ParityError, ParityMember};
use crate::raid10::{Raid10Array, Raid10Error};
use crate::stripe::{StripeArray, StripeError, StripeMember};
use crate::superblock::{ArrayProgress, RaidLevel};
use crate::triple::{TripleParityArray, TripleParityError, TripleParityMember};

#[cfg(test)]
mod tests;

/// A redundant member type whose live device speaks for the array in its
/// device-level health and class answers.
///
/// Each engine keeps a private `live_devices` helper over its own borrowed
/// slice, folding the shared participation predicate
/// ([`crate::health::member_participates`]) once. [`OwnedRaidArray`] owns
/// four different member types across its redundant levels and answers the
/// same two device-level questions directly from its own storage rather than
/// through a transient view (a `Block::geometry`-style query takes `&self`,
/// which cannot build the `&mut` view every engine's constructor needs), so
/// this trait lets it fold the one predicate across all four member types
/// instead of writing the same filter four times.
trait LiveMember<B: Block> {
    /// The backing device, or [`None`] if this member does not currently
    /// speak for the array.
    fn live_device(&self) -> Option<&B>;
}

impl<B: Block> LiveMember<B> for MirrorMember<B> {
    fn live_device(&self) -> Option<&B> {
        crate::health::member_participates(self.state())
            .then(|| self.device())
            .flatten()
    }
}

impl<B: Block> LiveMember<B> for ParityMember<B> {
    fn live_device(&self) -> Option<&B> {
        crate::health::member_participates(self.state())
            .then(|| self.device())
            .flatten()
    }
}

impl<B: Block> LiveMember<B> for DualParityMember<B> {
    fn live_device(&self) -> Option<&B> {
        crate::health::member_participates(self.state())
            .then(|| self.device())
            .flatten()
    }
}

impl<B: Block> LiveMember<B> for TripleParityMember<B> {
    fn live_device(&self) -> Option<&B> {
        crate::health::member_participates(self.state())
            .then(|| self.device())
            .flatten()
    }
}

/// The live devices of `members`, folding the shared participation predicate
/// once across every [`LiveMember`] type.
fn live_devices<'a, B: Block + 'a, M: LiveMember<B>>(
    members: &'a [M],
) -> impl Iterator<Item = &'a B> {
    members.iter().filter_map(LiveMember::live_device)
}

/// The live devices of a [`StripeArray`]'s members: a stripe has no
/// redundancy vocabulary of its own (no absent/resyncing state), so every
/// member not yet dropped by a whole-device fault speaks for the array.
fn stripe_live_devices<B: Block>(members: &[StripeMember<B>]) -> impl Iterator<Item = &B> {
    members
        .iter()
        .filter(|m| !m.faulted())
        .map(StripeMember::device)
}

/// An owning composed RAID device: the heap-owned counterpart of
/// [`RaidArray`] for a serve process holding many discovered arrays at once.
///
/// See the module documentation for why the borrowed engines cannot serve
/// this role directly and why the wrapper must not simply re-assemble on
/// every call. Every operation here builds a transient [`RaidArray`] view
/// over the owned state for its own duration; nothing outlives the call.
pub enum OwnedRaidArray<B: Block> {
    /// A RAID1 mirror ([`MirrorArray`]).
    Mirror {
        /// The owned member table, in slot order.
        members: Vec<MirrorMember<B>>,
        /// The array's logical geometry, fixed at assembly.
        geometry: BlockGeometry,
        /// The array's resumable scrub cursor.
        scrub_next_lba: u64,
    },
    /// A RAID0 stripe ([`StripeArray`]) — no redundancy.
    Stripe {
        /// The owned member table, in slot order.
        members: Vec<StripeMember<B>>,
        /// The array's logical geometry, fixed at assembly.
        geometry: BlockGeometry,
        /// The stripe unit in logical blocks, fixed at assembly.
        chunk_blocks: u64,
        /// Set once any member suffers a whole-device fault; sticky, since a
        /// stripe has no way to rebuild a lost member.
        failed: bool,
    },
    /// A RAID5 distributed-parity array ([`ParityArray`]).
    Parity {
        /// The owned member table, in slot order.
        members: Vec<ParityMember<B>>,
        /// The owned reconstruction scratch buffer (at least two logical
        /// blocks).
        scratch: Vec<u8>,
        /// The array's logical geometry, fixed at assembly.
        geometry: BlockGeometry,
        /// The per-member logical block count, fixed at assembly.
        per_member_blocks: u64,
        /// The stripe unit in logical blocks, fixed at assembly.
        chunk_blocks: u64,
        /// The array's resumable scrub cursor.
        scrub_next_lba: u64,
    },
    /// A RAID6 double distributed-parity array ([`DualParityArray`]).
    DualParity {
        /// The owned member table, in slot order.
        members: Vec<DualParityMember<B>>,
        /// The owned reconstruction scratch buffer (at least
        /// [`crate::SCRATCH_BLOCKS`] logical blocks).
        scratch: Vec<u8>,
        /// The array's logical geometry, fixed at assembly.
        geometry: BlockGeometry,
        /// The per-member logical block count, fixed at assembly.
        per_member_blocks: u64,
        /// The stripe unit in logical blocks, fixed at assembly.
        chunk_blocks: u64,
        /// The array's resumable scrub cursor.
        scrub_next_lba: u64,
    },
    /// A RAID-TP triple distributed-parity array ([`TripleParityArray`]).
    TripleParity {
        /// The owned member table, in slot order.
        members: Vec<TripleParityMember<B>>,
        /// The owned reconstruction scratch buffer (at least
        /// [`crate::TRIPLE_SCRATCH_BLOCKS`] logical blocks).
        scratch: Vec<u8>,
        /// The array's logical geometry, fixed at assembly.
        geometry: BlockGeometry,
        /// The per-member logical block count, fixed at assembly.
        per_member_blocks: u64,
        /// The stripe unit in logical blocks, fixed at assembly.
        chunk_blocks: u64,
        /// The array's resumable scrub cursor.
        scrub_next_lba: u64,
    },
    /// A RAID10 stripe of two-copy mirrors ([`Raid10Array`]).
    Raid10 {
        /// The owned member table, in slot order (members `2k`/`2k+1` form
        /// pair `k`).
        members: Vec<MirrorMember<B>>,
        /// The array's logical geometry, fixed at assembly.
        geometry: BlockGeometry,
        /// The per-member (and so per-pair) geometry, fixed at assembly.
        per_member: BlockGeometry,
        /// The stripe unit in logical blocks, fixed at assembly.
        chunk_blocks: u64,
        /// The array's resumable scrub cursor.
        scrub_next_lba: u64,
    },
}

impl<B: Block> OwnedRaidArray<B> {
    /// Assemble an owning RAID1 mirror from `members`.
    ///
    /// # Errors
    ///
    /// Whatever [`MirrorArray::assemble`] refuses.
    pub fn assemble_mirror(mut members: Vec<MirrorMember<B>>) -> Result<Self, MirrorError> {
        let assembled = MirrorArray::assemble(&mut members)?;
        let geometry = assembled.array_geometry();
        let scrub_next_lba = assembled.scrub_cursor();
        Ok(Self::Mirror {
            members,
            geometry,
            scrub_next_lba,
        })
    }

    /// Assemble an owning RAID0 stripe from `members` with stripe unit
    /// `chunk_blocks` logical blocks.
    ///
    /// # Errors
    ///
    /// Whatever [`StripeArray::assemble`] refuses.
    pub fn assemble_stripe(
        mut members: Vec<StripeMember<B>>,
        chunk_blocks: u32,
    ) -> Result<Self, StripeError> {
        let assembled = StripeArray::assemble(&mut members, chunk_blocks)?;
        let geometry = assembled.array_geometry();
        let failed = matches!(assembled.health(), ArrayHealth::Failed);
        Ok(Self::Stripe {
            members,
            geometry,
            chunk_blocks: u64::from(chunk_blocks),
            failed,
        })
    }

    /// Assemble an owning RAID5 distributed-parity array from `members` and
    /// `scratch` with stripe unit `chunk_blocks` logical blocks.
    ///
    /// # Errors
    ///
    /// Whatever [`ParityArray::assemble`] refuses.
    pub fn assemble_parity(
        mut members: Vec<ParityMember<B>>,
        mut scratch: Vec<u8>,
        chunk_blocks: u32,
    ) -> Result<Self, ParityError> {
        let assembled = ParityArray::assemble(&mut members, &mut scratch, chunk_blocks)?;
        let geometry = assembled.array_geometry();
        let per_member_blocks = assembled.per_member_blocks();
        let scrub_next_lba = assembled.scrub_cursor();
        Ok(Self::Parity {
            members,
            scratch,
            geometry,
            per_member_blocks,
            chunk_blocks: u64::from(chunk_blocks),
            scrub_next_lba,
        })
    }

    /// Assemble an owning RAID6 double distributed-parity array from
    /// `members` and `scratch` with stripe unit `chunk_blocks` logical
    /// blocks.
    ///
    /// # Errors
    ///
    /// Whatever [`DualParityArray::assemble`] refuses.
    pub fn assemble_dual_parity(
        mut members: Vec<DualParityMember<B>>,
        mut scratch: Vec<u8>,
        chunk_blocks: u32,
    ) -> Result<Self, DualParityError> {
        let assembled = DualParityArray::assemble(&mut members, &mut scratch, chunk_blocks)?;
        let geometry = assembled.array_geometry();
        let per_member_blocks = assembled.per_member_blocks();
        let scrub_next_lba = assembled.scrub_cursor();
        Ok(Self::DualParity {
            members,
            scratch,
            geometry,
            per_member_blocks,
            chunk_blocks: u64::from(chunk_blocks),
            scrub_next_lba,
        })
    }

    /// Assemble an owning RAID-TP triple distributed-parity array from
    /// `members` and `scratch` with stripe unit `chunk_blocks` logical
    /// blocks.
    ///
    /// # Errors
    ///
    /// Whatever [`TripleParityArray::assemble`] refuses.
    pub fn assemble_triple_parity(
        mut members: Vec<TripleParityMember<B>>,
        mut scratch: Vec<u8>,
        chunk_blocks: u32,
    ) -> Result<Self, TripleParityError> {
        let assembled = TripleParityArray::assemble(&mut members, &mut scratch, chunk_blocks)?;
        let geometry = assembled.array_geometry();
        let per_member_blocks = assembled.per_member_blocks();
        let scrub_next_lba = assembled.scrub_cursor();
        Ok(Self::TripleParity {
            members,
            scratch,
            geometry,
            per_member_blocks,
            chunk_blocks: u64::from(chunk_blocks),
            scrub_next_lba,
        })
    }

    /// Assemble an owning RAID10 stripe of mirrors from `members` with
    /// stripe unit `chunk_blocks` logical blocks.
    ///
    /// # Errors
    ///
    /// Whatever [`Raid10Array::assemble`] refuses.
    pub fn assemble_raid10(
        mut members: Vec<MirrorMember<B>>,
        chunk_blocks: u32,
    ) -> Result<Self, Raid10Error> {
        let assembled = Raid10Array::assemble(&mut members, chunk_blocks)?;
        let geometry = assembled.array_geometry();
        let per_member = assembled.per_member_geometry();
        let scrub_next_lba = assembled.scrub_cursor();
        Ok(Self::Raid10 {
            members,
            geometry,
            per_member,
            chunk_blocks: u64::from(chunk_blocks),
            scrub_next_lba,
        })
    }

    /// Run `op` over `view` and write the array-level scrub cursor it left
    /// back into `scrub_next_lba` before returning.
    ///
    /// The shared tail of every [`with_view`](Self::with_view) arm but the
    /// stripe's (which has no scrub state, only its sticky fault flag), kept
    /// in one place so it is written once rather than once per redundant
    /// level.
    fn run_and_sync_cursor<R>(
        mut view: RaidArray<'_, B>,
        op: impl FnOnce(&mut RaidArray<'_, B>) -> R,
        scrub_next_lba: &mut u64,
    ) -> R {
        let result = op(&mut view);
        *scrub_next_lba = view.scrub_cursor();
        result
    }

    /// Build a transient [`RaidArray`] view over the owned state, run `op`
    /// over it, and write the array-level scrub cursor (or, for a stripe,
    /// the sticky fault flag) back before returning.
    ///
    /// The per-member rebuild cursor already lives in each member, so it
    /// persists in the owned [`Vec`] without any write-back here; only the
    /// state a transient view does not otherwise own — the scrub position, or
    /// the stripe's own fault latch — needs to be threaded through
    /// explicitly. Every call goes through [`from_prepared`](MirrorArray::from_prepared)
    /// and its siblings rather than the engines' `assemble`, so a member's
    /// recorded state is carried forward untouched instead of being
    /// re-derived from a fresh probe.
    fn with_view<R>(&mut self, op: impl FnOnce(&mut RaidArray<'_, B>) -> R) -> R {
        match self {
            Self::Mirror {
                members,
                geometry,
                scrub_next_lba,
            } => {
                let view = RaidArray::Mirror(MirrorArray::from_prepared(
                    members.as_mut_slice(),
                    *geometry,
                    *scrub_next_lba,
                ));
                Self::run_and_sync_cursor(view, op, scrub_next_lba)
            }
            Self::Stripe {
                members,
                geometry,
                chunk_blocks,
                failed,
            } => {
                let mut view = RaidArray::Stripe(StripeArray::from_prepared(
                    members.as_mut_slice(),
                    *geometry,
                    *chunk_blocks,
                    *failed,
                ));
                let result = op(&mut view);
                *failed = matches!(view.health(), ArrayHealth::Failed);
                result
            }
            Self::Parity {
                members,
                scratch,
                geometry,
                per_member_blocks,
                chunk_blocks,
                scrub_next_lba,
            } => {
                let view = RaidArray::Parity(ParityArray::from_prepared(
                    members.as_mut_slice(),
                    scratch.as_mut_slice(),
                    *geometry,
                    *per_member_blocks,
                    *chunk_blocks,
                    *scrub_next_lba,
                ));
                Self::run_and_sync_cursor(view, op, scrub_next_lba)
            }
            Self::DualParity {
                members,
                scratch,
                geometry,
                per_member_blocks,
                chunk_blocks,
                scrub_next_lba,
            } => {
                let view = RaidArray::DualParity(DualParityArray::from_prepared(
                    members.as_mut_slice(),
                    scratch.as_mut_slice(),
                    *geometry,
                    *per_member_blocks,
                    *chunk_blocks,
                    *scrub_next_lba,
                ));
                Self::run_and_sync_cursor(view, op, scrub_next_lba)
            }
            Self::TripleParity {
                members,
                scratch,
                geometry,
                per_member_blocks,
                chunk_blocks,
                scrub_next_lba,
            } => {
                let view = RaidArray::TripleParity(TripleParityArray::from_prepared(
                    members.as_mut_slice(),
                    scratch.as_mut_slice(),
                    *geometry,
                    *per_member_blocks,
                    *chunk_blocks,
                    *scrub_next_lba,
                ));
                Self::run_and_sync_cursor(view, op, scrub_next_lba)
            }
            Self::Raid10 {
                members,
                geometry,
                per_member,
                chunk_blocks,
                scrub_next_lba,
            } => {
                let view = RaidArray::Raid10(Raid10Array::from_prepared(
                    members.as_mut_slice(),
                    *geometry,
                    *per_member,
                    *chunk_blocks,
                    *scrub_next_lba,
                ));
                Self::run_and_sync_cursor(view, op, scrub_next_lba)
            }
        }
    }

    /// The RAID level this composed device implements.
    #[must_use]
    pub fn level(&mut self) -> RaidLevel {
        self.with_view(|v| v.level())
    }

    /// The current [`ArrayHealth`].
    #[must_use]
    pub fn health(&mut self) -> ArrayHealth {
        self.with_view(|v| v.health())
    }

    /// The number of member slots the array is defined to have.
    #[must_use]
    pub fn member_count(&mut self) -> usize {
        self.with_view(|v| v.member_count())
    }

    /// The composed device's logical geometry.
    #[must_use]
    pub fn array_geometry(&mut self) -> BlockGeometry {
        self.with_view(|v| v.array_geometry())
    }

    /// The [`MemberState`] of member `index`, or [`None`] if out of range.
    #[must_use]
    pub fn member_state(&mut self, index: usize) -> Option<MemberState> {
        self.with_view(|v| v.member_state(index))
    }

    /// Whether any member is still rebuilding.
    #[must_use]
    pub fn needs_resync(&mut self) -> bool {
        self.with_view(|v| v.needs_resync())
    }

    /// Whether a proactive scrub pass is in progress.
    #[must_use]
    pub fn scrubbing(&mut self) -> bool {
        self.with_view(|v| v.scrubbing())
    }

    /// The scrub cursor (the next block a scrub will verify); equal to the
    /// array block count when idle.
    #[must_use]
    pub fn scrub_cursor(&mut self) -> u64 {
        self.with_view(|v| v.scrub_cursor())
    }

    /// Begin a proactive scrub pass from block 0.
    ///
    /// # Errors
    ///
    /// * [`RaidError::NotRedundant`] for a stripe.
    // A bare `RaidArray::begin_scrub` function item fails to satisfy the
    // higher-ranked lifetime `with_view` needs for a generic `B`, so the
    // closure is not actually redundant here despite the identical body.
    #[allow(clippy::redundant_closure_for_method_calls)]
    pub fn begin_scrub(&mut self) -> Result<(), RaidError> {
        self.with_view(|v| v.begin_scrub())
    }

    /// Verify and repair one bounded chunk of a scrub pass.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::scrub_step`].
    pub fn scrub_step(&mut self, scratch: &mut [u8]) -> Result<(), RaidError> {
        self.with_view(|v| v.scrub_step(scratch))
    }

    /// Rebuild one bounded chunk of every resyncing member.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::resync_step`].
    pub fn resync_step(&mut self, scratch: &mut [u8]) -> Result<(), RaidError> {
        self.with_view(|v| v.resync_step(scratch))
    }

    /// Begin rebuilding a currently-faulted member from its existing device.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::readd_member`].
    pub fn readd_member(&mut self, index: usize) -> Result<(), RaidError> {
        self.with_view(|v| v.readd_member(index))
    }

    /// Remove a faulted member's device, leaving its slot absent and
    /// returning the device.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::remove_member`].
    pub fn remove_member(&mut self, index: usize) -> Result<B, RaidError> {
        self.with_view(|v| v.remove_member(index))
    }

    /// Install a spare `device` into an absent slot and begin rebuilding it.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::add_member`].
    pub fn add_member(&mut self, index: usize, device: B) -> Result<(), RaidError> {
        self.with_view(|v| v.add_member(index, device))
    }

    /// Hot-swap a faulted member's device for a fresh one.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::replace_member`].
    pub fn replace_member(&mut self, index: usize, device: B) -> Result<(), RaidError> {
        self.with_view(|v| v.replace_member(index, device))
    }

    /// The array's resumable maintenance position.
    #[must_use]
    pub fn progress(&mut self) -> ArrayProgress {
        self.with_view(|v| v.progress())
    }

    /// Resume maintenance at a previously checkpointed `progress`.
    ///
    /// # Errors
    ///
    /// As [`RaidArray::restore_progress`].
    pub fn restore_progress(&mut self, progress: ArrayProgress) -> Result<(), RaidError> {
        self.with_view(|v| v.restore_progress(progress))
    }
}

impl<B: Block> Block for OwnedRaidArray<B> {
    fn device_class(&self) -> BlkDeviceClass {
        match self {
            Self::Mirror { members, .. } | Self::Raid10 { members, .. } => {
                crate::health::aggregate_device_class(
                    live_devices(members).map(Block::device_class),
                )
            }
            Self::Stripe { members, .. } => crate::health::aggregate_device_class(
                stripe_live_devices(members).map(Block::device_class),
            ),
            Self::Parity { members, .. } => crate::health::aggregate_device_class(
                live_devices(members).map(Block::device_class),
            ),
            Self::DualParity { members, .. } => crate::health::aggregate_device_class(
                live_devices(members).map(Block::device_class),
            ),
            Self::TripleParity { members, .. } => crate::health::aggregate_device_class(
                live_devices(members).map(Block::device_class),
            ),
        }
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(match self {
            Self::Mirror { geometry, .. }
            | Self::Stripe { geometry, .. }
            | Self::Parity { geometry, .. }
            | Self::DualParity { geometry, .. }
            | Self::TripleParity { geometry, .. }
            | Self::Raid10 { geometry, .. } => *geometry,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.with_view(|v| v.read_blocks(lba, buf))
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.with_view(|v| v.write_blocks(lba, buf))
    }

    // See the comment on `begin_scrub` above: the function item does not
    // satisfy the higher-ranked lifetime bound here either.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn flush(&mut self) -> Result<(), DriverError> {
        self.with_view(|v| v.flush())
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.with_view(|v| v.read_blocks_with_class(lba, buf, class))
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.with_view(|v| v.write_blocks_with_class(lba, buf, class))
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        // No level overrides the trait's default: a composed array declares
        // no discard support of its own, so the wrapper's answer is the same
        // constant regardless of level, and there is nothing to forward.
        Ok(DiscardCapability::unsupported())
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        let _ = (lba, blocks);
        Err(DriverError::Unsupported)
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(match self {
            Self::Mirror { members, .. } | Self::Raid10 { members, .. } => {
                crate::health::aggregate_device_health(
                    live_devices(members).map(Block::device_health),
                )
            }
            Self::Stripe { members, .. } => crate::health::aggregate_device_health(
                stripe_live_devices(members).map(Block::device_health),
            ),
            Self::Parity { members, .. } => crate::health::aggregate_device_health(
                live_devices(members).map(Block::device_health),
            ),
            Self::DualParity { members, .. } => crate::health::aggregate_device_health(
                live_devices(members).map(Block::device_health),
            ),
            Self::TripleParity { members, .. } => crate::health::aggregate_device_health(
                live_devices(members).map(Block::device_health),
            ),
        })
    }
}
