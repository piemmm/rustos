//! RustOS USB HID boot-mouse **class driver** — shared library.
//!
//! This crate is a `lib` (the loadable-module identity: the [`BIND_KEYS`] bind
//! table `devmgr` matches a discovered HID boot-mouse interface node against,
//! host-compilable so the image builder can author the signed manifest from
//! it) **and** a `Run` binary (`src/main.rs`, the autoloaded class-driver
//! process). The class driver touches no controller register and holds no
//! DMA: it binds the per-interface node the host-controller driver emits,
//! submits interrupt-IN URBs over the bus-agnostic URB transport, and injects
//! decoded pointer records (`plans/USB.md` §1.2) — it names no controller, no
//! bus, and no board.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::{DriverBindKey, HwMatchKey};

/// The 24-bit USB class code of an HID **boot mouse** interface: class
/// `0x03` (HID), sub-class `0x01` (boot), protocol `0x02` (mouse).
const HID_BOOT_MOUSE_CLASS: u32 = 0x03_01_02;

/// The bind priority [`BIND_KEYS`] carries.
///
/// A class-wildcard match (any vendor/product), so it ranks below a
/// vendor-specific HID driver naming an exact device id.
const BIND_PRIORITY: u16 = 5;

/// This driver's hardware bind table: any HID boot-protocol **mouse
/// interface**, by class alone (vendor/product wildcard).
///
/// It binds the per-interface node the host-controller driver emits — never
/// the controller node — so any boot mouse behind any USB host autoloads it.
/// This `const` is the single source of truth the signed-manifest bind table
/// is authored from (`tools/xtask` image builder) and `devmgr` resolves a
/// discovered HID interface node against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::usb(0, 0, HID_BOOT_MOUSE_CLASS),
)];
