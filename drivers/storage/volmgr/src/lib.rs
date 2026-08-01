//! TAIRiX **volume-manager policy driver** — shared library.
//!
//! This crate is a `lib` (the loadable-module identity — the [`BIND_KEYS`]
//! bind table `devmgr` matches a discovered block-service storage node
//! against — plus the host-testable policy engine: the partition +
//! filesystem probe plan ([`plan`]) and the catalog naming policy
//! ([`name`])) **and** a `Run` binary (`src/main.rs`, the autoloaded
//! per-node policy process). The blkio block client it probes a device
//! through is the shared `tairix_blkclient::RemoteBlock`, opened
//! read-only: this driver only ever inspects a device's layout and commits
//! nothing to it.
//!
//! # What it is
//!
//! The volume manager owns volume *policy* the way `devmgr` owns driver
//! policy (`plans/DEVICES.md` D3c). A block driver that brings a
//! hot-pluggable unit up publishes it as a storage-class hardware-tree
//! node carrying two transport grants — a blkio call endpoint and a shared
//! data window (`tairix_abi::blkio`). `devmgr` matches that node against
//! this crate's bind table and loads one volume-manager instance for it;
//! the kernel spawns the instance holding **exactly that node's** grants,
//! so an instance can probe and publish its own unit and can never reach a
//! sibling device's transport (no ambient authority). The instance:
//!
//! 1. connects the blkio client and validates the device geometry,
//! 2. probes the partition table (`lib/partition`; a device with no table
//!    is probed whole),
//! 3. probes each candidate extent's head for a supported filesystem
//!    signature (`lib/fsprobe`),
//! 4. derives the deterministic catalog name (the volume's own label
//!    sanitised through the alias character rules, else `<fstype><n>`;
//!    a collision appends the volume-identity fingerprint), and
//! 5. asks the kernel to attach and publish each recognised volume
//!    (the `CAP_FS_MOUNT`-gated, audited `volume_attach` syscall — the
//!    kernel re-validates everything and performs the mount itself).
//!
//! It then exits `0`: publication is a run-to-completion job, and the
//! kernel-held mount outlives the instance. Removal handling (detach,
//! retained dirty state) is the staged D4 work.
//!
//! Nothing here names a bus, a controller, or a vendor: the bind table
//! selects on the block-service node's own compatible key, and every byte
//! of device data arrives over the public blkio ABI.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

pub mod name;
pub mod plan;

use tairix_abi::raid_ipc::RAID_ARRAY_COMPATIBLE;
use tairix_abi::{DriverBindKey, HwMatchKey};

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match on the block-service node ranks like the other exact-compatible
/// drivers.
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: every block-service storage node
/// whose volumes this policy driver owns — the per-LUN node the USB
/// mass-storage class driver emits, and the composed-array node the RAID
/// array composer emits. Future hot-pluggable block sources join by adding
/// their node's compatible key here — never by naming a bus in the engine.
/// This `const` is the single source of truth the signed-manifest bind
/// table is authored from (`tools/xtask` image builder) and `devmgr`
/// resolves a discovered storage node against.
///
/// An array is deliberately probed by the *same* driver as a leaf disk and
/// through the same code: to this policy engine a composed array is simply a
/// block device with geometry, so its partitions and filesystems are found,
/// named, and attached by one implementation rather than a second one that
/// could drift from it. That an array is several disks underneath is the
/// composer's business, and arrays stack over arrays for free because the
/// node an array publishes is indistinguishable in kind from a disk's.
pub const BIND_KEYS: &[DriverBindKey] = &[
    DriverBindKey::new(
        BIND_PRIORITY,
        match HwMatchKey::compatible(b"tairix,usb-msd-lun") {
            Ok(key) => key,
            // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
            // too-long literal would be a compile-time const-eval error here,
            // never a runtime panic.
            Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
        },
    ),
    DriverBindKey::new(
        BIND_PRIORITY,
        match HwMatchKey::compatible(RAID_ARRAY_COMPATIBLE) {
            Ok(key) => key,
            // Unreachable, as above: the shared constant is a short literal.
            Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
        },
    ),
];
