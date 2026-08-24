//! TAIRiX **RAID composition** engines — fault-aware virtual block devices
//! that compose child block endpoints through the public block seam
//! (`plans/FIX-IO.md` IO6).
//!
//! A RAID volume is itself a [`Block`](tairix_abi::driver::block::Block): it
//! composes several child `Block` endpoints and presents one logical device to
//! the filesystem layer, so a composed array nests naturally over the same seam
//! every leaf device uses (one seam, complete abstraction). It **consumes** the
//! block-layer health vocabulary (`tairix_abi::blkio`); it does not re-invent
//! it.
//!
//! Six compositions are provided as siblings over that one seam (parallel
//! implementations): the redundant RAID1 mirror ([`MirrorArray`]), the
//! capacity-aggregating RAID0 stripe ([`StripeArray`]), the RAID5
//! distributed-parity array ([`ParityArray`]) that combines capacity
//! aggregation with single-fault redundancy, the RAID6 double
//! distributed-parity array ([`DualParityArray`]) that survives *two* member
//! losses, the RAID-TP triple distributed-parity array ([`TripleParityArray`])
//! that survives *three*, and the RAID10 stripe of mirrors ([`Raid10Array`])
//! that combines mirror redundancy with stripe capacity and bandwidth.
//!
//! # RAID10 stripe of mirrors ([`Raid10Array`])
//!
//! The sixth composition is a RAID10 stripe of two-copy mirrors: an even number
//! of members are paired into mirrors and the logical block space is striped in
//! fixed-size chunks across the pairs. It is a *composition* of the two engines
//! above rather than a re-implementation: the RAID0 striping map places each
//! chunk on its pair (column), and each pair is driven through the one
//! [`MirrorArray`] implementation via an allocation-free transient view, so
//! RAID10 inherits the mirror's recover/read-repair/write-fan-out/scrub/rebuild
//! behaviour and adds only the pairing and the aggregation of per-pair health
//! into array health. The array has the capacity of half its members, survives
//! any member fault — and several at once — as long as no pair loses *both*
//! copies ([`ArrayHealth::Degraded`]/[`ArrayHealth::Recovering`] meanwhile),
//! and fails closed ([`ArrayHealth::Failed`]) only when a pair loses both
//! copies and can no longer serve its stripes.
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
//!   bounded, interruptible pass that proactively reads *every* in-sync copy of
//!   *every* block and repairs a latent media error on any copy from a good
//!   one, so a bad sector on a copy the read path never consults is found and
//!   healed while a good copy still exists — the auto-scrub a mirror exists to
//!   provide, chunked so a 100 TB+ array never scrubs in one sweep.
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
//!   full remove-failed / add-spare replacement workflow, without a reboot.
//! - A returning member is **rebuilt** by a bounded, interruptible resync
//!   ([`MirrorArray::resync_step`]) that copies the array contents from an
//!   in-sync member a caller-sized chunk at a time, so a 100 TB+ member rebuild
//!   never blocks the system or busy-spins. While a member is resyncing it
//!   receives new writes to its already-synced region so it never falls behind,
//!   and it becomes a read source only once fully in sync. The array reports
//!   [`ArrayHealth::Recovering`] meanwhile.
//!
//! # RAID0 stripe ([`StripeArray`])
//!
//! The second composition is a RAID0 stripe: the logical block space is cut
//! into fixed-size chunks round-robined across the members, so the array's
//! capacity is the *sum* of the members' and a large transfer is spread over
//! all of them. A stripe has **no redundancy**, and the engine is honest about
//! it: [`StripeArray::assemble`] requires every member present and evenly
//! striped (no coming up "degraded" over a gap it cannot serve), a whole-device
//! fault fails the array closed for good ([`ArrayHealth::Failed`]), and a
//! per-block media error fails only that one request while the still-reachable
//! device keeps serving its other stripes. It shares the mirror's
//! whole-device-fault classification and the [`ArrayHealth`] vocabulary rather
//! than re-inventing them.
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
//!   row and repairs a latent media error from the survivors; like the mirror
//!   it heals *media* errors and leaves *content* arbitration to the
//!   checksummed filesystem layer.
//! - A member going faulted, or a missing slot ([`MemberState::Absent`]),
//!   **degrades the array, never the system** ([`ArrayHealth::Degraded`]); a
//!   *second* loss makes a stripe unrecoverable and the array fails closed
//!   ([`ArrayHealth::Failed`]) rather than fabricate data it cannot
//!   reconstruct.
//! - A returning or replaced member is **rebuilt** by a bounded, interruptible
//!   resync ([`ParityArray::resync_step`]) that reconstructs its blocks from
//!   the survivors a caller-sized budget at a time; the same
//!   [`MirrorArray::remove_member`]/[`MirrorArray::add_member`]-style
//!   disk-replacement workflow ([`ParityArray::remove_member`] /
//!   [`ParityArray::add_member`] / [`ParityArray::replace_member`]) restores
//!   redundancy without a reboot. Parity computation and reconstruction borrow
//!   a caller-owned **scratch** buffer (at least two logical blocks), so the
//!   engine stays allocation-free.
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
//!   heals latent media errors from the survivors like the single-parity array.
//! - A first or second lost member (or an [`MemberState::Absent`] slot)
//!   **degrades the array, never the system** ([`ArrayHealth::Degraded`]); a
//!   *third* loss makes a stripe unreconstructable and the array fails closed
//!   ([`ArrayHealth::Failed`]).
//! - A returning or replaced member is **rebuilt** by a bounded, interruptible
//!   resync ([`DualParityArray::resync_step`]); the same
//!   [`remove_member`](DualParityArray::remove_member) /
//!   [`add_member`](DualParityArray::add_member) /
//!   [`replace_member`](DualParityArray::replace_member) disk-replacement
//!   workflow restores redundancy without a reboot. Both syndromes and the
//!   two-erasure solver borrow a caller-owned **scratch** buffer of at least
//!   [`SCRATCH_BLOCKS`] logical blocks, so the engine stays allocation-free.
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
//!   heals latent media errors from the survivors like the other parity levels.
//! - A first, second, or third lost member (or an [`MemberState::Absent`]
//!   slot) **degrades the array, never the system** ([`ArrayHealth::Degraded`]);
//!   a *fourth* loss makes a stripe unreconstructable and the array fails
//!   closed ([`ArrayHealth::Failed`]).
//! - A returning or replaced member is **rebuilt** by a bounded, interruptible
//!   resync ([`TripleParityArray::resync_step`]); the same
//!   [`remove_member`](TripleParityArray::remove_member) /
//!   [`add_member`](TripleParityArray::add_member) /
//!   [`replace_member`](TripleParityArray::replace_member) disk-replacement
//!   workflow restores redundancy without a reboot. The three syndromes and the
//!   three-erasure solver borrow a caller-owned **scratch** buffer of at least
//!   [`TRIPLE_SCRATCH_BLOCKS`] logical blocks, so the engine stays
//!   allocation-free.
//!
//! # Fail closed
//!
//! At the boundary of what the array can vouch for it returns a typed error
//! and never serves data it cannot trust: a read with no surviving copy, a
//! write no copy accepted, and a flush no copy could commit each fail closed
//! rather than fabricating success. The *operation* fails; the *system*
//! keeps running.
//!
//! # Device-level answers: health and class
//!
//! Because every composition is itself a
//! [`Block`](tairix_abi::driver::block::Block), a consumer queries the *array*
//! for what it would ask a bare disk, so all the compositions answer from
//! their live members through one shared fold per property (the `health`
//! module) rather than inherit a trait default that hides them. Both folds
//! select members with the same participation predicate, so an array can never
//! report one property from one set of members and another from a different
//! set.
//!
//! A consumer that schedules a scrub from a device's `SMART` / `NVMe`
//! telemetry reads
//! [`device_health`](tairix_abi::driver::block::Block::device_health):
//! independent integrity counters sum (saturating), shared conditions take the
//! worst member, a faulted/absent slot or a member with no telemetry
//! contributes nothing, and the array reports `Unavailable` only when no live
//! member exposes telemetry.
//!
//! A consumer that derives its per-request deadline, reissue budget, and
//! recovery grace window from a device's class reads
//! [`device_class`](tairix_abi::driver::block::Block::device_class): the array
//! declares the *most patient* of its live members' classes, because it can
//! only answer as fast as the member it is waiting on — a mirror of an SSD and
//! a spinning disk must be given the spinning disk's spin-up budget. An array
//! with no live member declares the bounded unclassified envelope, so its
//! callers fail closed sooner rather than waiting out disks that are gone.
//!
//! # On-disk metadata and reassembly ([`ArraySuperblock`], [`ArrayIdentity`])
//!
//! An array is discovered, not configured: each member carries a checksummed
//! [`ArraySuperblock`] naming the array, this member's slot, the geometry, and
//! a monotonic generation counter. [`distinct_arrays`] partitions a
//! heterogeneous set of discovered [`Candidate`] members into the distinct
//! arrays present among them, then [`ArrayIdentity::resolve`] reconstructs each
//! array from its members — the freshest member fixes the authoritative shape —
//! and [`ArrayIdentity::fill_slots`] places each member into its slot, marking
//! one that is behind as a stale rebuild target and refusing a foreign,
//! mis-shaped, or duplicate claimant. The decoder is fail-closed on any
//! malformed on-disk byte and fuzzed for panic-freedom.
//!
//! The reassembly verdict is carried into composition through one mapping,
//! [`MemberRole::for_slot`]: a slot the metadata proved is behind
//! (`in_sync == false`) becomes a [`MemberRole::Stale`] member, which
//! [`MirrorArray::assemble`] admits [`MemberState::Resyncing`] — a rebuild
//! target, never an immediate read source — so the array can never serve a
//! reader data from a copy known to be out of date.
//!
//! Turning the whole [`SlotDisposition`] table into a redundant engine's member
//! buffer is the shared [`fill_members`] bridge, so every consumer that
//! assembles a discovered array places its members identically (through
//! [`MemberRole::for_slot`]) rather than hand-rolling the
//! stale/absent/device-tag loop.
//!
//! # Composed-device dispatch ([`RaidArray`])
//!
//! Once a serve process has *discovered* an array and resolved its
//! [`RaidLevel`], it presents exactly one logical
//! [`Block`](tairix_abi::driver::block::Block) device regardless of level.
//! [`RaidArray`] is that single composed-device abstraction (modelled on Linux
//! md's per-personality dispatch): it wraps the level-specific engine and
//! forwards the `Block` I/O path together with the level-agnostic health,
//! self-maintenance (scrub/resync), and member-reconfiguration surface, so
//! neither the autoloaded serve process nor the ARXFS-native composition
//! re-derives the level → engine mapping. Operations that are only meaningful
//! for a *redundant* array fail closed on a RAID0 stripe with
//! [`RaidError::NotRedundant`].
//!
//! # Maintenance scheduling ([`ArrayMaintenance`])
//!
//! Exposing a self-healing surface is not the same as driving it. An array
//! heals itself only if something decides, turn by turn, whether to re-admit a
//! returning member, advance a rebuild, or run a proactive scrub — and when it
//! must do none of those so the foreground workload keeps the array.
//! [`ArrayMaintenance`] is that one decision: a pure, event-timed,
//! allocation-free policy that ranks restoring redundancy above verifying it,
//! paces each chunk against a busy array's duty share, re-probes a faulted
//! member on a bounded escalating cadence rather than a spin, and hands its
//! caller the one-shot deadline to park on. Its cadences come from the array's
//! own discovered [`BlkDeviceClass`](tairix_abi::blkio::BlkDeviceClass) through
//! [`MaintenancePolicy::for_class`], never a frozen scalar.
//!
//! # Scope, and why this is a shared crate
//!
//! This crate is the composition **engine**, the on-disk metadata layer above
//! it, and the maintenance policy that drives them — all pure, host-testable,
//! and allocation-free. It composes devices it is *handed* as `Block`
//! implementations; it never reaches a device itself, so it holds no
//! bring-up, firmware, quirk, or register logic and is not any device's
//! support code.
//!
//! It lives in `lib/` because two independent consumers compose several block
//! devices as one and must not each carry their own copy of the arithmetic:
//! the autoloaded RAID composer driver (`drivers/storage/raid`), which
//! assembles members discovered from their superblocks and serves each array
//! as one published block-service node, and the native filesystem
//! (`drivers/filesystem/arxfs`), whose multi-device volumes drive the same
//! engines directly. A copy in either driver would be unreachable from the
//! other, since one driver crate may not depend on another.
//!
//! The live serve process — reading each discovered device's superblock,
//! assembling the members through [`ArrayIdentity`], turning the scheduler's
//! decisions into real transfers, and publishing the composed device as its
//! own block-service node — is staged in `plans/FIX-IO.md` §2.6 (IO6a–IO6f).
//!
//! # Owning the composed device ([`OwnedRaidArray`])
//!
//! The engines above and [`RaidArray`] all borrow a caller-owned member
//! slice, which is exactly what lets them impose no member ceiling and hold
//! no allocation. A long-running serve process that discovers a variable
//! number of arrays at runtime, though, must own its members on the heap and
//! hold the composed device alongside them — which cannot be one
//! self-referential struct. [`OwnedRaidArray`] is that owning counterpart: it
//! keeps its members in a growable [`Vec`](alloc::vec::Vec) per level, and
//! drives every operation through a transient [`RaidArray`] view built over
//! them, so a member's recorded state (in particular a fault) is carried
//! forward untouched rather than re-derived from a fresh probe on every call.
//!
//! [`RaidLevel`]: tairix_abi::raid::RaidLevel
//! [`SlotDisposition`]: tairix_abi::raid::SlotDisposition
//! [`ArrayHealth`]: tairix_abi::raid::ArrayHealth
//! [`ArrayHealth::Degraded`]: tairix_abi::raid::ArrayHealth::Degraded
//! [`ArrayHealth::Recovering`]: tairix_abi::raid::ArrayHealth::Recovering
//! [`ArrayHealth::Failed`]: tairix_abi::raid::ArrayHealth::Failed
//! [`MemberState::Absent`]: tairix_abi::raid::MemberState::Absent
//! [`MemberState::Resyncing`]: tairix_abi::raid::MemberState::Resyncing

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod array;
mod assemble;
mod backoff;
mod dualparity;
mod gf256;
mod health;
mod maintenance;
mod mirror;
mod owned;
mod parity;
mod raid10;
mod stripe;
mod superblock;
mod triple;

pub use array::{RaidArray, RaidError};
pub use assemble::{fill_members, AssembleError, AssembleMember};
pub use backoff::{RetryCadence, RetryState};
pub use dualparity::{DualParityArray, DualParityError, DualParityMember, SCRATCH_BLOCKS};
pub use maintenance::{
    ArrayMaintenance, MaintenanceAction, MaintenanceError, MaintenancePolicy, MemberRetry,
};
pub use mirror::{MemberRole, MirrorArray, MirrorError, MirrorMember};
pub use owned::OwnedRaidArray;
pub use parity::{ParityArray, ParityError, ParityMember};
pub use raid10::{Raid10Array, Raid10Error};
pub use stripe::{StripeArray, StripeError, StripeMember};
pub use superblock::{
    distinct_arrays, ArrayIdentity, ArraySuperblock, ArrayUuid, AssemblyError, Candidate,
    CandidateVerdict, DistinctArrays, RejectReason, SuperblockError, FORMAT_VERSION, MAGIC,
    WIRE_LEN,
};
pub use triple::{
    TripleParityArray, TripleParityError, TripleParityMember,
    SCRATCH_BLOCKS as TRIPLE_SCRATCH_BLOCKS,
};
