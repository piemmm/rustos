//! The TAIRiX block-service **client** (`plans/FIX-IO.md` IO6): the one
//! [`tairix_abi::driver::block::Block`] implementation over the fixed-frame
//! `tairix_abi::blkio` request/reply pair, and the production async
//! transport its consumers issue that protocol over.
//!
//! A user-space block driver (the first is the USB mass-storage class
//! driver) exposes each logical unit it brings up as a **block-service call
//! endpoint** plus a shared-memory data window, both forwarded as grants on
//! the storage-class hardware-tree node it emits. Two consumers inherit
//! those grants and drive the device through [`RemoteBlock`]:
//!
//! - the volume-manager policy driver, which only ever inspects a device's
//!   layout (partition table, filesystem signatures) and never commits
//!   anything to it, and
//! - the RAID array composer, which both reads a member's superblock and
//!   durably writes to the array it assembles.
//!
//! A copy of this client per consumer would let the wire discipline, the
//! geometry validation, and the bounded-reissue policy silently drift
//! between them, so it lives here once and both link it. Which authority a
//! connected client holds is an explicit, named stance rather than an
//! accident of which methods a caller happens not to call:
//! [`RemoteBlock::connect_read_only`] never lets a write or a flush reach
//! the wire, however the device answers, while
//! [`RemoteBlock::connect_read_write`] allows both, still subject to the
//! device's own write-protect flag.
//!
//! Everything the device reports is untrusted: the geometry is validated at
//! connect time before any consumer sees it, every reply frame is decoded
//! fail-closed, and a transfer never reads more bytes out of the shared
//! window than the request named.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

mod client;
mod transport;

pub use client::{BlkCall, RemoteBlock};
pub use transport::RtBlkCall;
