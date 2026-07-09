//! RustOS USB mass-storage **class driver** — shared library.
//!
//! This crate is a `lib` (the loadable-module identity — the [`BIND_KEYS`]
//! bind table `devmgr` matches a discovered mass-storage interface node
//! against — plus the host-testable device logic: the configuration-
//! descriptor reader ([`desc`]), the Bulk-Only Transport + SCSI engine
//! ([`bot`]), and the block-service state machine ([`serve`])) **and** a
//! `Run` binary (`src/main.rs`, the autoloaded class-driver process). The
//! class driver touches no controller register and holds no DMA: it binds
//! the per-interface node the host-controller driver emits, drives CBW/CSW
//! framing and the SCSI transparent subset over the bus-agnostic URB
//! transport, and serves each logical unit as a block-service endpoint
//! behind a per-LUN storage hardware-tree node (`plans/DEVICES.md` D2,
//! `plans/USB.md` U4 shape).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

pub mod bot;
pub mod desc;
pub mod serve;

use rustos_abi::{DriverBindKey, HwMatchKey};

/// The 24-bit USB class code of a **mass-storage** interface this driver
/// serves: class `0x08` (mass storage), sub-class `0x06` (SCSI transparent
/// command set), protocol `0x50` (Bulk-Only Transport).
pub const MSD_INTERFACE_CLASS: u32 = 0x08_06_50;

/// The bind priority [`BIND_KEYS`] carries.
///
/// A class-wildcard match (any vendor/product), so it ranks below a
/// vendor-specific storage driver naming an exact device id.
const BIND_PRIORITY: u16 = 5;

/// This driver's hardware bind table: any SCSI-transparent Bulk-Only
/// **mass-storage interface**, by class alone (vendor/product wildcard).
///
/// It binds the per-interface node the host-controller driver emits — never
/// the controller node — so any bulk-only disk behind any USB host autoloads
/// it. This `const` is the single source of truth the signed-manifest bind
/// table is authored from (`tools/xtask` image builder) and `devmgr`
/// resolves a discovered mass-storage interface node against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::usb(0, 0, MSD_INTERFACE_CLASS),
)];
