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
//! # Scope
//!
//! This crate is the host-testable composition **engine**. The autoloaded
//! serve process that assembles members from discovered array metadata and
//! drives resync off the members' recovery signals rides with the
//! multi-device volume-assembly work (`plans/FIX-IO.md` IO6 remaining); the
//! engine is proven host-side first, exactly as the other FIX-IO primitives
//! landed their shared logic before their live wiring.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod mirror;

pub use mirror::{ArrayHealth, MemberState, MirrorArray, MirrorError, MirrorMember};
