//! TAIRiX `lsusb`: list the discovered USB devices
//! (`plans/DEVICES.md` DEVICE1 V3).
//!
//! The listing is rendered from the hardware tree — the single
//! architecture-neutral device inventory — fetched through the
//! `sysinfo-v1` `HARDWARE_TREE` query (gated by `CAP_SYSINFO_HW`; there is
//! no `/proc` and no kernel bypass). Human-readable vendor/product names
//! come from the vetted `usb.ids` snapshot compiled into the compact
//! table this bundle ships as `Resources/usb.ids.bin`, decoded through
//! `lib/devids`; an identity the database does not name renders only its
//! numeric `ID vvvv:pppp` form (exactly as `usbutils` omits an unknown
//! string), never fabricated, and a missing or corrupt table degrades the
//! listing to bare ids with the reason on standard error — the inventory
//! itself is never withheld over a naming aid.
//!
//! The option surface follows `usbutils` `lsusb` over what the TAIRiX
//! model actually carries: `-v` interface class/subclass/protocol names,
//! `-t` topology, `-d [<vendor>]:[<product>]` and `-s [[<bus>]:][<devnum>]`
//! filters. Where the model genuinely differs the tool diverges
//! deliberately and documents it: TAIRiX has no Linux bus/devnum
//! registry, so a device's bus number is its parent (controller) node's
//! stable hardware-tree id and its device number is its own node id, and
//! the inventory records one node per *interface* — a multi-interface
//! device lists once per interface.
//!
//! This crate is the pure engine — decode, select, render — over injected
//! seams (`tairix_procinfo::Transport`, [`io::Output`],
//! `tairix_help::HelpSource`), host-tested with no kernel. The
//! freestanding `Run` binary in `src/run.rs` binds the production seams.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, DeviceFilter, Options, ParseError, SlotFilter};
pub use error::LsusbError;
pub use io::Output;
