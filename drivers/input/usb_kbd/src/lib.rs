//! TAIRiX USB HID boot-keyboard **class driver** — shared library.
//!
//! This crate is a `lib` (the loadable-module identity: the [`BIND_KEYS`] bind
//! table `devmgr` matches a discovered HID boot-keyboard interface node
//! against, host-compilable so the image builder can author the signed
//! manifest from it) **and** a `Run` binary (`src/main.rs`, the autoloaded
//! class-driver process). The class driver touches no controller register and
//! holds no DMA: it binds the per-interface node the host-controller driver
//! emits, submits interrupt-IN URBs over the bus-agnostic URB transport, and
//! injects decoded keystrokes (`plans/USB.md` U4, `AGENTS.md` §2.20 / §17.4).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::{DriverBindKey, HwMatchKey};

/// The 24-bit USB class code of an HID **boot keyboard** interface: class
/// `0x03` (HID), sub-class `0x01` (boot), protocol `0x01` (keyboard).
const HID_BOOT_KEYBOARD_CLASS: u32 = 0x03_01_01;

/// The bind priority [`BIND_KEYS`] carries.
///
/// A class-wildcard match (any vendor/product), so it ranks below a
/// vendor-specific HID driver naming an exact device id.
const BIND_PRIORITY: u16 = 5;

/// This driver's hardware bind table: any HID boot-protocol **keyboard
/// interface**, by class alone (vendor/product wildcard).
///
/// It binds the per-interface node the host-controller driver emits — never
/// the controller node — so any boot keyboard behind any USB host autoloads
/// it. This `const` is the single source of truth the signed-manifest bind
/// table is authored from (`tools/xtask` image builder) and `devmgr` resolves
/// a discovered HID interface node against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::usb(0, 0, HID_BOOT_KEYBOARD_CLASS),
)];
