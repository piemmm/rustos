//! TAIRiX `lspci`: list the discovered PCI/PCIe devices
//! (`plans/DEVICES.md` DEVICE1 V2).
//!
//! The listing is rendered from the hardware tree — the single
//! architecture-neutral device inventory — fetched through the
//! `sysinfo-v1` `HARDWARE_TREE` query (gated by `CAP_SYSINFO_HW`; there is
//! no `/proc` and no kernel bypass). Human-readable vendor/device/class
//! names come from the vetted `pci.ids` snapshot compiled into the compact
//! table this bundle ships as `Resources/pci.ids.bin`, decoded through
//! `lib/devids`; an identity the database does not name is rendered
//! numerically, never fabricated, and a missing or corrupt table degrades
//! the listing to numeric ids with the reason on standard error — the
//! inventory itself is never withheld over a naming aid.
//!
//! The option surface follows `pciutils` `lspci` over what the TAIRiX
//! model actually carries: `-n` / `-nn` numeric modes,
//! `-v` declared resources, `-t` topology, `-d` / `-s` filters. Where the
//! model genuinely differs the tool diverges deliberately and documents
//! it: a function's address is its stable hardware-tree node id (TAIRiX
//! records no PCI bus/device/function triple), and `-s` selects that node
//! id. The kernel-binding view (`lspci -k`) awaits the driver-binding
//! records query and is not offered until it can be served honestly.
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
pub use command::{parse, Command, DeviceFilter, NameMode, Options, ParseError};
pub use error::LspciError;
pub use io::Output;
