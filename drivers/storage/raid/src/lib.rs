//! TAIRiX **RAID composition** driver — fault-aware virtual block devices
//! that compose child block endpoints through the public block seam
//! (`plans/FIX-IO.md` IO6).
//!
//! A RAID volume is itself a [`Block`](tairix_abi::driver::block::Block): it
//! composes several child `Block` endpoints and presents one logical device
//! to the filesystem layer, so a
//! composed array nests naturally over the same seam every leaf device uses
//! (`AGENTS.md` §2.2 one seam, §27 complete abstraction). It **consumes**
//! the block-layer health vocabulary (`tairix_abi::blkio`); it does not
//! re-invent it.
//!
//! Six compositions are provided as siblings over that one seam (`AGENTS.md`
//! §2.2 parallel implementations): the redundant RAID1 mirror
//! ([`MirrorArray`]), the capacity-aggregating RAID0 stripe
//! ([`StripeArray`]), the RAID5 distributed-parity array ([`ParityArray`])
//! that combines capacity aggregation with single-fault redundancy, the
//! RAID6 double distributed-parity array ([`DualParityArray`]) that survives
//! *two* member losses, the RAID-TP triple distributed-parity array
//! ([`TripleParityArray`]) that survives *three*, and the RAID10 stripe of
//! mirrors ([`Raid10Array`]) that combines mirror redundancy with stripe
//! capacity and bandwidth.
//!
//! # RAID10 stripe of mirrors ([`Raid10Array`])
//!
//! The sixth composition is a RAID10 stripe of two-copy mirrors: an even
//! number of members are paired into mirrors and the logical block space is
//! striped in fixed-size chunks across the pairs. It is a *composition* of the
//! two engines above rather than a re-implementation (`AGENTS.md` §2.2): the
//! RAID0 striping map places each chunk on its pair (column), and each pair is
//! driven through the one [`MirrorArray`] implementation via an
//! allocation-free transient view, so RAID10 inherits the mirror's
//! recover/read-repair/write-fan-out/scrub/rebuild behaviour and adds only the
//! pairing and the aggregation of per-pair health into array health. The array
//! has the capacity of half its members, survives any member fault — and
//! several at once — as long as no pair loses *both* copies
//! ([`ArrayHealth::Degraded`]/[`ArrayHealth::Recovering`] meanwhile), and fails
//! closed ([`ArrayHealth::Failed`]) only when a pair loses both copies and can
//! no longer serve its stripes.
//!
//! # RAID1 mirror ([`MirrorArray`])
//!
//! The first composition is a RAID1 mirror. Every member holds a full copy
//! of the same logical-block array, so the array survives any subset of
//! member faults as long as one copy remains:
//!
//! - **Reads** are served from any in-sync member, trying members in a
//!   deterministic order. A member that returns a *per-block* error
//!   ([`DriverError::MediumError`](tairix_abi::DriverError::MediumError)) does
//!   not kill the array: the data is
//!   recovered from a good copy and the bad copy is **repaired** in place by
//!   writing the good data back, forcing the device to reallocate the sector.
//!   This read-path repair is opportunistic (it only touches copies read
//!   before the serving one), complemented by the proactive scrub below.
//! - **Scrub** ([`MirrorArray::begin_scrub`]/[`MirrorArray::scrub_step`]) is a
//!   bounded, interruptible pass that proactively reads *every* in-sync copy
//!   of *every* block and repairs a latent media error on any copy from a good
//!   one, so a bad sector on a copy the read path never consults is found and
//!   healed while a good copy still exists (`AGENTS.md` §26.5) — the auto-scrub
//!   a mirror exists to provide, chunked so a 100 TB+ array never scrubs in one
//!   sweep (`AGENTS.md` §26.6).
//! - **Writes** fan out to every copy. A member that fails a write is
//!   dropped from the array (a write error is a member fault, not an array
//!   fault); the write still succeeds as long as one copy accepted it.
//! - A member going faulted **degrades the array, never the system**: the
//!   survivors keep serving and the array reports [`ArrayHealth::Degraded`].
//! - A **missing member slot** ([`MemberState::Absent`]) is first-class, like
//!   a Linux md "removed" slot: the array is assembled to its full defined
//!   width (one [`MirrorMember::absent`] per missing copy), counts the empty
//!   slot toward its member count, and reports [`ArrayHealth::Degraded`] for
//!   the reduced redundancy rather than masquerading as a smaller, optimal
//!   array. A faulted disk is pulled with [`MirrorArray::remove_member`]
//!   (vacating its slot to [`MemberState::Absent`] and returning the device),
//!   and a fresh spare is inserted into an empty slot with
//!   [`MirrorArray::add_member`], which rebuilds it from a surviving copy — the
//!   full remove-failed / add-spare replacement workflow, without a reboot
//!   (`AGENTS.md` §18.4).
//! - A returning member is **rebuilt** by a bounded, interruptible resync
//!   ([`MirrorArray::resync_step`]) that copies the array contents from an
//!   in-sync member a caller-sized chunk at a time (`AGENTS.md` §26.6), so a
//!   100 TB+ member rebuild never blocks the system or busy-spins. While a
//!   member is resyncing it receives new writes to its already-synced region
//!   so it never falls behind, and it becomes a read source only once fully
//!   in sync. The array reports [`ArrayHealth::Recovering`] meanwhile.
//!
//! # RAID0 stripe ([`StripeArray`])
//!
//! The second composition is a RAID0 stripe: the logical block space is cut
//! into fixed-size chunks round-robined across the members, so the array's
//! capacity is the *sum* of the members' and a large transfer is spread over
//! all of them. A stripe has **no redundancy**, and the engine is honest
//! about it (`AGENTS.md` §5.4, §26.5): [`StripeArray::assemble`] requires
//! every member present and evenly striped (no coming up "degraded" over a
//! gap it cannot serve), a whole-device fault fails the array closed for good
//! ([`ArrayHealth::Failed`]), and a per-block media error fails only that one
//! request while the still-reachable device keeps serving its other stripes.
//! It shares the mirror's whole-device-fault classification and the
//! [`ArrayHealth`] vocabulary rather than re-inventing them.
//!
//! # RAID5 distributed parity ([`ParityArray`])
//!
//! The third composition is a RAID5 distributed-parity array. The logical
//! block space is striped across the members like RAID0, but each stripe
//! reserves one member's chunk for the parity (XOR) of the others, and the
//! parity slot rotates across stripes so no member is a parity bottleneck. The
//! array has the capacity of `member_count - 1` members and survives any
//! single member being lost:
//!
//! - **Reads** in the healthy case go straight to the data member that holds
//!   the block; a read of a lost member's chunk is **reconstructed** by
//!   XOR-ing the surviving members (data and parity), and a per-block media
//!   error on an otherwise-healthy member is reconstructed and repaired in
//!   place, complemented by the proactive scrub below.
//! - **Writes** update the affected stripe's parity, by read-modify-write when
//!   the old data and parity are readable and by recomputing the parity from
//!   the surviving data members when they are not (a degraded write), so a
//!   lost member's data stays reconstructable.
//! - **Scrub** ([`ParityArray::begin_scrub`]/[`ParityArray::scrub_step`]) is a
//!   bounded, interruptible pass that reads every member's copy of every stripe
//!   row and repairs a latent media error from the survivors (`AGENTS.md`
//!   §26.5); like the mirror it heals *media* errors and leaves *content*
//!   arbitration to the checksummed filesystem layer.
//! - A member going faulted, or a missing slot ([`MemberState::Absent`]),
//!   **degrades the array, never the system** ([`ArrayHealth::Degraded`]); a
//!   *second* loss makes a stripe unrecoverable and the array fails closed
//!   ([`ArrayHealth::Failed`]) rather than fabricate data it cannot
//!   reconstruct.
//! - A returning or replaced member is **rebuilt** by a bounded, interruptible
//!   resync ([`ParityArray::resync_step`]) that reconstructs its blocks from
//!   the survivors a caller-sized budget at a time (`AGENTS.md` §26.6); the
//!   same [`MirrorArray::remove_member`]/[`MirrorArray::add_member`]-style
//!   disk-replacement workflow ([`ParityArray::remove_member`] /
//!   [`ParityArray::add_member`] / [`ParityArray::replace_member`]) restores
//!   redundancy without a reboot (`AGENTS.md` §18.4). Parity computation and
//!   reconstruction borrow a caller-owned **scratch** buffer (at least two
//!   logical blocks), so the engine stays allocation-free.
//!
//! # RAID6 double distributed parity ([`DualParityArray`])
//!
//! The fourth composition is a RAID6 double distributed-parity array. It
//! stripes like RAID5 but reserves *two* chunks per stripe — a P (XOR)
//! syndrome and a Q (Reed-Solomon, GF(2^8)) syndrome —
//! both rotating across the members, so the array has the capacity of
//! `member_count - 2` members and survives **any two** members being lost:
//!
//! - **Reads** in the healthy case go straight to the data member; a lost
//!   chunk is reconstructed from P (one loss) or by solving the P and Q
//!   syndromes together (two losses), and a per-block media error is
//!   reconstructed and repaired in place, complemented by the proactive scrub.
//! - **Writes** update the affected stripe's P and Q, by read-modify-write
//!   when the old data and syndromes are readable and by recomputing both from
//!   the surviving data members otherwise (a degraded write), so a lost
//!   member's data stays reconstructable.
//! - **Scrub** ([`DualParityArray::begin_scrub`]/[`DualParityArray::scrub_step`])
//!   heals latent media errors from the survivors like the single-parity
//!   array (`AGENTS.md` §26.5).
//! - A first or second lost member (or an [`MemberState::Absent`] slot)
//!   **degrades the array, never the system** ([`ArrayHealth::Degraded`]); a
//!   *third* loss makes a stripe unreconstructable and the array fails closed
//!   ([`ArrayHealth::Failed`]).
//! - A returning or replaced member is **rebuilt** by a bounded, interruptible
//!   resync ([`DualParityArray::resync_step`]); the same
//!   [`remove_member`](DualParityArray::remove_member) /
//!   [`add_member`](DualParityArray::add_member) /
//!   [`replace_member`](DualParityArray::replace_member) disk-replacement
//!   workflow restores redundancy without a reboot (`AGENTS.md` §18.4). Both
//!   syndromes and the two-erasure solver borrow a caller-owned **scratch**
//!   buffer of at least [`SCRATCH_BLOCKS`] logical blocks, so the engine stays
//!   allocation-free.
//!
//! # RAID-TP triple distributed parity ([`TripleParityArray`])
//!
//! The fifth composition is a RAID-TP triple distributed-parity array. It
//! stripes like RAID6 but reserves *three* chunks per stripe — a P (XOR)
//! syndrome, a Q (`Σ gᵏ·Dₖ`) syndrome, and an R (`Σ g²ᵏ·Dₖ`) syndrome over
//! GF(2^8) — all three rotating across the members, so the array has the
//! capacity of `member_count - 3` members and survives **any three** members
//! being lost:
//!
//! - **Reads** in the healthy case go straight to the data member; up to three
//!   lost chunks are reconstructed by solving the P, Q, and R syndromes'
//!   Vandermonde system, and a per-block media error is reconstructed and
//!   repaired in place, complemented by the proactive scrub.
//! - **Writes** update the affected stripe's P, Q, and R, by read-modify-write
//!   when the old data and syndromes are readable and by recomputing all three
//!   from the surviving data members otherwise (a degraded write), so a lost
//!   member's data stays reconstructable.
//! - **Scrub** ([`TripleParityArray::begin_scrub`]/[`TripleParityArray::scrub_step`])
//!   heals latent media errors from the survivors like the other parity
//!   levels (`AGENTS.md` §26.5).
//! - A first, second, or third lost member (or an [`MemberState::Absent`]
//!   slot) **degrades the array, never the system** ([`ArrayHealth::Degraded`]);
//!   a *fourth* loss makes a stripe unreconstructable and the array fails
//!   closed ([`ArrayHealth::Failed`]).
//! - A returning or replaced member is **rebuilt** by a bounded, interruptible
//!   resync ([`TripleParityArray::resync_step`]); the same
//!   [`remove_member`](TripleParityArray::remove_member) /
//!   [`add_member`](TripleParityArray::add_member) /
//!   [`replace_member`](TripleParityArray::replace_member) disk-replacement
//!   workflow restores redundancy without a reboot (`AGENTS.md` §18.4). The
//!   three syndromes and the three-erasure solver borrow a caller-owned
//!   **scratch** buffer of at least [`TRIPLE_SCRATCH_BLOCKS`] logical blocks,
//!   so the engine stays allocation-free.
//!
//! # Fail closed (`AGENTS.md` §5.4)
//!
//! At the boundary of what the array can vouch for it returns a typed error
//! and never serves data it cannot trust: a read with no surviving copy, a
//! write no copy accepted, and a flush no copy could commit each fail closed
//! rather than fabricating success. The *operation* fails; the *system*
//! keeps running.
//!
//! # Device health (`AGENTS.md` §26.5)
//!
//! Because every composition is itself a
//! [`Block`](tairix_abi::driver::block::Block), a consumer that schedules a
//! scrub from a device's `SMART` / `NVMe` telemetry queries the *array*, so all
//! the compositions override
//! [`device_health`](tairix_abi::driver::block::Block::device_health) to
//! aggregate their live members' telemetry through one shared definition
//! (`health::aggregate_device_health`) rather than inherit the trait default
//! and hide every member's health. Independent integrity counters sum
//! (saturating), shared conditions take the worst member, a faulted/absent slot
//! or a member with no telemetry contributes nothing, and the array reports
//! `Unavailable` only when no live member exposes telemetry.
//!
//! # On-disk metadata and reassembly ([`ArraySuperblock`], [`ArrayIdentity`])
//!
//! An array is discovered, not configured: each member carries a checksummed
//! [`ArraySuperblock`] naming the array, this member's slot, the geometry, and
//! a monotonic generation counter. [`distinct_arrays`] partitions a
//! heterogeneous set of discovered [`Candidate`] members into the distinct
//! arrays present among them, then [`ArrayIdentity::resolve`] reconstructs each
//! array from its members — the freshest member fixes the authoritative
//! shape — and [`ArrayIdentity::fill_slots`] places
//! each member into its slot, marking one that is behind as a stale rebuild
//! target and refusing a foreign, mis-shaped, or duplicate claimant. The
//! decoder is fail-closed on any malformed on-disk byte (`AGENTS.md` §5.4,
//! §26.5) and fuzzed for panic-freedom.
//!
//! The reassembly verdict is carried into composition through one mapping,
//! [`MemberRole::for_slot`]: a slot the metadata proved is behind
//! (`in_sync == false`) becomes a [`MemberRole::Stale`] member, which
//! [`MirrorArray::assemble`] admits [`MemberState::Resyncing`] — a rebuild
//! target, never an immediate read source — so the array can never serve a
//! reader data from a copy known to be out of date (`AGENTS.md` §5.4, §26.5).
//!
//! # Composed-device dispatch ([`RaidArray`])
//!
//! Once a serve process has *discovered* an array and resolved its
//! [`RaidLevel`], it presents exactly one logical
//! [`Block`](tairix_abi::driver::block::Block) device regardless
//! of level. [`RaidArray`] is that single composed-device abstraction
//! (`AGENTS.md` §27, modelled on Linux md's per-personality dispatch): it
//! wraps the level-specific engine and forwards the `Block` I/O path
//! together with the level-agnostic health, self-maintenance
//! (scrub/resync), and member-reconfiguration surface, so neither the
//! autoloaded serve process nor the ARXFS-native composition re-derives the
//! level → engine mapping (`AGENTS.md` §2.2). Operations that are only
//! meaningful for a *redundant* array fail closed on a RAID0 stripe with
//! [`RaidError::NotRedundant`].
//!
//! # Scope
//!
//! This crate is the host-testable composition **engine** plus the on-disk
//! metadata layer above. The autoloaded serve process that reads each
//! discovered device's superblock, assembles the members through
//! [`ArrayIdentity`], drives resync off the members' recovery signals, and
//! publishes the composed device as its own block-service node rides with the
//! multi-device volume-assembly work (`plans/FIX-IO.md` IO6 remaining); the
//! engine and its metadata are proven host-side first, exactly as the other
//! FIX-IO primitives landed their shared logic before their live wiring.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod array;
mod dualparity;
mod gf256;
mod health;
mod mirror;
mod parity;
mod raid10;
mod stripe;
mod superblock;
mod triple;

pub use array::{RaidArray, RaidError};
pub use dualparity::{DualParityArray, DualParityError, DualParityMember, SCRATCH_BLOCKS};
pub use mirror::{ArrayHealth, MemberRole, MemberState, MirrorArray, MirrorError, MirrorMember};
pub use parity::{ParityArray, ParityError, ParityMember};
pub use raid10::{Raid10Array, Raid10Error};
pub use stripe::{StripeArray, StripeError, StripeMember};
pub use superblock::{
    distinct_arrays, ArrayIdentity, ArraySuperblock, ArrayUuid, AssemblyError, Candidate,
    CandidateVerdict, DistinctArrays, RaidLevel, RejectReason, SlotDisposition, SuperblockError,
    FORMAT_VERSION, MAGIC, WIRE_LEN,
};
pub use triple::{
    TripleParityArray, TripleParityError, TripleParityMember,
    SCRATCH_BLOCKS as TRIPLE_SCRATCH_BLOCKS,
};
