//! TAIRiX **RAID member-agent driver** — the autoloaded per-disk driver that
//! delegates one array member's block transport to the array composer
//! (`plans/FIX-IO.md` `IO6c`).
//!
//! An array is several block devices driven as one, so one process must hold
//! client authority over every member at once. A driver is spawned for
//! exactly one matched hardware-tree node and receives exactly that node's
//! resource grants, so no process is born able to reach a whole array. The
//! member agent closes that gap without widening anyone's authority: matched
//! to a node the volume manager emits for a device the array subsystem may
//! reach, it **delegates** that one device's block transport and data window
//! to the composer's reserved rendezvous and then offers the device to it.
//! Authority flows one device at a time, from the process that legitimately
//! holds it, to a rendezvous only a privileged binder can serve.
//!
//! Two kinds of device are reached this way, and the agent treats them
//! identically because the difference is the composer's to determine, not
//! the agent's: a `tairix,raid-member` node names a device whose first block
//! probed as array metadata, and a `tairix,raid-candidate` node names a whole
//! device that probed as carrying nothing at all — the disks a *new* array is
//! created over, which have no metadata to be recognised by and so would
//! otherwise be unreachable. In both cases the agent offers the device and
//! the composer reads it to decide what it is.
//!
//! The agent then holds the membership open: the composer answers the offer
//! only when the membership ends, so one outstanding call carries the whole
//! lifecycle. The agent parks on that reply and is woken by it — or by the
//! composer's endpoint being torn down, which cancels the call. Nothing polls
//! the composer and nothing polls the device.
//!
//! [`MemberAgent`] is that lifecycle as pure, host-tested logic; the `Run`
//! program supplies the clock, the syscalls, and the audit trail.
//!
//! # Why this is its own bundle
//!
//! The array composer that assembles discovered members into served arrays is
//! a sibling driver crate, `drivers/storage/raid`, sharing the composition
//! arithmetic in the device-agnostic `tairix_raid` crate rather than any code
//! here. A signed bundle grants its whole manifest's capability set to every
//! instance loaded from it, and one instance of this driver runs per member
//! disk, so a bundle carrying both roles would hand every per-disk agent the
//! composer's privileged-endpoint-bind and node-emit authority it has no need
//! of. Keeping the two roles in separate bundles keeps each instance holding
//! only the authority its own job requires.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod agent;

pub use agent::{AgentStep, MemberAgent, REOFFER_BASE_NS, REOFFER_CEILING_NS};

use tairix_abi::raid_ipc::{RAID_CANDIDATE_COMPATIBLE, RAID_MEMBER_COMPATIBLE};
use tairix_abi::{DriverBindKey, HwMatchKey};

/// Bind priority of the member-agent keys.
///
/// Each of these nodes carries exactly one compatible string and no other
/// driver binds it, so the priority only has to be a definite value rather
/// than win a contest; it matches the storage policy drivers' priority so the
/// resolution stays uniform across the class.
const BIND_PRIORITY: u16 = 10;

/// Build the bind key for `compatible` at the agent's priority.
///
/// A compatible string too long for a match key is a compile-time error
/// rather than a driver that silently binds nothing.
const fn bind_key(compatible: &'static [u8]) -> DriverBindKey {
    DriverBindKey::new(
        BIND_PRIORITY,
        match HwMatchKey::compatible(compatible) {
            Ok(key) => key,
            Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
        },
    )
}

/// The bind table the signed bundle publishes: a device the volume manager
/// recognised as an array member, and a whole device it found empty enough to
/// create an array over.
pub const BIND_KEYS: &[DriverBindKey] = &[
    bind_key(RAID_MEMBER_COMPATIBLE),
    bind_key(RAID_CANDIDATE_COMPATIBLE),
];
