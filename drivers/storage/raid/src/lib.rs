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
//!   writing the good data back, forcing the device to reallocate the sector
//!   — the auto-scrub a mirror exists to provide.
//! - **Writes** fan out to every copy. A member that fails a write is
//!   dropped from the array (a write error is a member fault, not an array
//!   fault); the write still succeeds as long as one copy accepted it.
//! - A member going faulted **degrades the array, never the system**: the
//!   survivors keep serving and the array reports [`ArrayHealth::Degraded`].
//! - A returning member is **rebuilt** by a bounded, interruptible resync
//!   ([`MirrorArray::resync_step`]) that copies the array contents from an
//!   in-sync member a caller-sized chunk at a time (`AGENTS.md` §26.6), so a
//!   100 TB+ member rebuild never blocks the system or busy-spins. While a
//!   member is resyncing it receives new writes to its already-synced region
//!   so it never falls behind, and it becomes a read source only once fully
//!   in sync. The array reports [`ArrayHealth::Recovering`] meanwhile.
//!
//! # Fail closed (`AGENTS.md` §5.4)
//!
//! At the boundary of what the array can vouch for it returns a typed error
//! and never serves data it cannot trust: a read with no surviving copy, a
//! write no copy accepted, and a flush no copy could commit each fail closed
//! rather than fabricating success. The *operation* fails; the *system*
//! keeps running.
//!
//! # On-disk metadata and reassembly ([`ArraySuperblock`], [`ArrayIdentity`])
//!
//! An array is discovered, not configured: each member carries a checksummed
//! [`ArraySuperblock`] naming the array, this member's slot, the geometry, and
//! a monotonic generation counter. [`ArrayIdentity::resolve`] reconstructs the
//! array from a set of discovered [`Candidate`] members — the freshest member
//! fixes the authoritative shape — and [`ArrayIdentity::fill_slots`] places
//! each member into its slot, marking one that is behind as a stale rebuild
//! target and refusing a foreign, mis-shaped, or duplicate claimant. The
//! decoder is fail-closed on any malformed on-disk byte (`AGENTS.md` §5.4,
//! §26.5) and fuzzed for panic-freedom.
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

mod mirror;
mod superblock;

pub use mirror::{ArrayHealth, MemberState, MirrorArray, MirrorError, MirrorMember};
pub use superblock::{
    ArrayIdentity, ArraySuperblock, ArrayUuid, AssemblyError, Candidate, CandidateVerdict,
    RaidLevel, RejectReason, SlotDisposition, SuperblockError, FORMAT_VERSION, MAGIC, WIRE_LEN,
};
