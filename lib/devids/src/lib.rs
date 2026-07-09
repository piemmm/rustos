//! RustOS PCI/USB ID-database engine (`lib/devids`).
//!
//! The `lspci` and `lsusb` command apps (plans/DEVICES.md DEVICE1) render the
//! numeric identities the hardware tree already carries — PCI
//! `vendor:device:class`, USB `vid:pid:class` — as human-readable names. The
//! names come from vetted, provenance-pinned snapshots of the public PCI and
//! USB ID databases (`pci.ids`, `usb.ids`), committed under
//! `lib/devids/assets/` and compiled by `cargo xtask devids` into the compact
//! binary tables each command bundle ships as a resource
//! (`lspci.app/Resources/pci.ids.bin`, `lsusb.app/Resources/usb.ids.bin`).
//!
//! This crate is the one definition of every step of that pipeline, so the
//! generator, the CI drift gate, and the runtime consumers can never diverge:
//!
//! - [`textdb`] (feature `textdb`, on by default; needs `alloc`) parses the
//!   snapshot text under the exact shared `pci.ids`/`usb.ids` line grammar,
//!   applies the strict fail-closed vetting filter (the raw upstream download
//!   is untrusted input whose strings end up on users' terminals), and
//!   encodes the compact sorted lookup tables.
//! - [`DevIds`] decodes and binary-searches a compiled table without
//!   allocating: O(log n) lookups over sorted fixed-width records, every
//!   offset and length validated fail-closed up front. The table ships on the
//!   read-only system volume but is still parsed as data, never trusted
//!   blindly.
//!
//! An id the database does not name resolves to `None`; the consumer renders
//! the numeric form rather than fabricating a name.
//!
//! The bounds in this module are fixed security bounds on untrusted input,
//! not scalable capacities: they carry generous headroom over today's
//! databases (~46 000 entries, ~1.7 MiB) and widening them "to be flexible"
//! would be a security regression.

#![no_std]

#[cfg(feature = "textdb")]
extern crate alloc;

mod tables;
#[cfg(feature = "textdb")]
pub mod textdb;

#[cfg(test)]
mod tests;

pub use tables::{DevIds, TableError, TABLE_MAGIC};

/// Which public ID database a snapshot or compiled table carries.
///
/// The two databases share one line grammar and one table format; the kind
/// selects the per-database strictness (which tagged sections exist, whether
/// vendor entries may have `subvendor subdevice` children) and is embedded in
/// the compiled table header so a consumer cannot load the wrong database.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DbKind {
    /// The PCI ID database (`pci.ids`): PCI/PCIe vendors, devices,
    /// subsystems, and the `C` class/subclass/prog-if tables.
    Pci,
    /// The USB ID database (`usb.ids`): USB vendors, products, the `C`
    /// class/subclass/protocol tables, and the auxiliary HID/audio/physical
    /// tables.
    Usb,
}

impl DbKind {
    /// The kind discriminant stored in a compiled table header.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            DbKind::Pci => 1,
            DbKind::Usb => 2,
        }
    }
}

/// Largest snapshot text accepted by the vetting parser, in bytes.
///
/// Today's databases are ~1.7 MiB and ~0.7 MiB; a "database" larger than
/// this is rejected whole rather than parsed.
pub const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// Largest name accepted for any entry, in bytes.
///
/// The longest name in today's databases is 153 bytes.
pub const MAX_NAME_BYTES: usize = 512;

/// Largest per-table record count accepted, at vetting and at decode.
///
/// The largest table in today's databases (PCI devices) has ~21 000 records.
pub const MAX_TABLE_ENTRIES: u32 = 262_144;
