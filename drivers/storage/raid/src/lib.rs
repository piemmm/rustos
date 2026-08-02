//! TAIRiX **RAID array-composer driver** — the autoloaded policy driver that
//! turns discovered array members into served arrays (`plans/FIX-IO.md`
//! `IO6d`).
//!
//! The composition arithmetic is not here: the six levels, the `RaidArray`
//! dispatch, the assembly bridge and the maintenance scheduler are the shared
//! `tairix_raid` crate, because the native filesystem's multi-device volumes
//! drive the same engines. This crate is the *driver*: the bind table its
//! signed bundle publishes, the pure decision logic below, and the `Run`
//! program the kernel spawns for the matched node.
//!
//! # The composer
//!
//! Matched to the synthetic virtual bus the kernel's hardware-tree bootstrap
//! publishes, one instance is the process every per-disk member agent
//! delegates to: it reads each offered device's own superblock, groups the
//! devices into the arrays their metadata describes, and brings each array
//! online as one served block device. [`MemberRegistry`] is that judgement as
//! pure, host-tested logic — which members belong to which array, and when an
//! array may be published without serving data it cannot vouch for — while the
//! composition arithmetic beneath it is the shared `tairix_raid` crate.
//!
//! An array does not merely serve: [`ArrayRuntime`] also drives its
//! self-maintenance between requests — re-admitting a returning member,
//! rebuilding it, verifying the array, and writing the position of a pass that
//! outlives a reboot into its members' own records.
//!
//! # Why the member agent is a sibling crate, not a second role here
//!
//! One signed bundle grants its whole manifest's capability set to every
//! instance loaded from it. This driver binds one endpoint id and needs
//! `CAP_IPC_BIND_PRIVILEGED` and `CAP_HW_EMIT` to do it; the per-disk agent
//! that delegates a device's transport to it runs once per member disk and
//! needs neither. Folding the two roles into one bundle would hand every
//! agent instance authority it has no need of, so the agent is its own bundle:
//! `drivers/storage/raid_member`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod compose;
mod runtime;
mod service;
#[cfg(test)]
mod testkit;

pub use compose::{Admission, ComposerAction, HeldMember, MemberRegistry, MemberStanding};
pub use runtime::{ArrayHealthEvent, ArrayRuntime, MaintenanceStep};
pub use service::{
    assemble_array, read_maintenance_record, read_superblock, write_maintenance_record,
    write_superblock, Assembled, MaintenanceResume, ServiceError,
};

use tairix_abi::{DriverBindKey, HwMatchKey, HW_VIRTUAL_BUS_COMPATIBLE};

/// Bind priority of the array-composer key.
///
/// The synthetic virtual bus node carries exactly one compatible string and
/// no other driver binds it, so the priority only has to be a definite value
/// rather than win a contest; it matches the storage policy drivers'
/// priority so the resolution stays uniform across the class.
const BIND_PRIORITY: u16 = 10;

/// The bind table the signed bundle publishes: the one key naming the
/// synthetic virtual bus the kernel's hardware-tree bootstrap publishes.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(HW_VIRTUAL_BUS_COMPATIBLE) {
        Ok(key) => key,
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];
