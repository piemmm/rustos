//! RustOS USB mass-storage **class driver** — shared library.
//!
//! This crate is a `lib` (the loadable-module identity — the [`BIND_KEYS`]
//! bind table `devmgr` matches a discovered mass-storage interface node
//! against — plus the host-testable device logic: the configuration-
//! descriptor reader ([`desc`]), the transport-neutral SCSI command layer
//! ([`scsi`]), the three wire transports — Bulk-Only ([`bot`]),
//! Control/Bulk/Interrupt ([`cbi`], the USB floppy transport), and USB
//! Attached SCSI ([`uas`]) — and the block-service state machine
//! ([`serve`])) **and** a `Run` binary (`src/main.rs`, the autoloaded
//! class-driver process). The class driver touches no controller register
//! and holds no DMA: it binds the per-interface node the host-controller
//! driver emits, drives its device's wire transport and command set over
//! the bus-agnostic URB transport, and serves each logical unit as a
//! block-service endpoint behind a per-LUN storage hardware-tree node
//! (`plans/DEVICES.md` D2/D5, `plans/USB.md` U4 shape).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

pub mod bot;
pub mod cbi;
pub mod desc;
pub mod scsi;
pub mod serve;
#[cfg(test)]
mod testutil;
pub mod uas;

use rustos_abi::{DriverBindKey, HwMatchKey};

/// SCSI-transparent Bulk-Only Transport (class `0x08`, sub-class `0x06`,
/// protocol `0x50`) — the ubiquitous USB disk/stick.
pub const MSD_BOT_SCSI_CLASS: u32 = 0x08_06_50;

/// UFI over Bulk-Only Transport (sub-class `0x04`, protocol `0x50`) — a
/// USB floppy drive speaking BOT framing.
pub const MSD_BOT_UFI_CLASS: u32 = 0x08_04_50;

/// UFI over Control/Bulk/Interrupt (sub-class `0x04`, protocol `0x00`) —
/// the classic USB floppy drive.
pub const MSD_CBI_UFI_CLASS: u32 = 0x08_04_00;

/// USB Attached SCSI (sub-class `0x06`, protocol `0x62`).
pub const MSD_UAS_CLASS: u32 = 0x08_06_62;

/// The bind priority [`BIND_KEYS`] carries.
///
/// A class-wildcard match (any vendor/product), so it ranks below a
/// vendor-specific storage driver naming an exact device id.
const BIND_PRIORITY: u16 = 5;

/// This driver's hardware bind table: every **mass-storage interface**
/// whose transport + command set this driver implements, by class alone
/// (vendor/product wildcard) — SCSI-transparent BOT and UAS, and UFI
/// floppies over BOT and CBI.
///
/// It binds the per-interface node the host-controller driver emits — never
/// the controller node — so any such device behind any USB host autoloads
/// it. This `const` is the single source of truth the signed-manifest bind
/// table is authored from (`tools/xtask` image builder) and `devmgr`
/// resolves a discovered mass-storage interface node against.
pub const BIND_KEYS: &[DriverBindKey] = &[
    DriverBindKey::new(BIND_PRIORITY, HwMatchKey::usb(0, 0, MSD_BOT_SCSI_CLASS)),
    DriverBindKey::new(BIND_PRIORITY, HwMatchKey::usb(0, 0, MSD_BOT_UFI_CLASS)),
    DriverBindKey::new(BIND_PRIORITY, HwMatchKey::usb(0, 0, MSD_CBI_UFI_CLASS)),
    DriverBindKey::new(BIND_PRIORITY, HwMatchKey::usb(0, 0, MSD_UAS_CLASS)),
];
