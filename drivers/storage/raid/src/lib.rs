//! TAIRiX **RAID composition driver** — the autoloaded policy driver that
//! turns discovered array members into served arrays (`plans/FIX-IO.md` IO6).
//!
//! The composition arithmetic is not here: the six levels, the `RaidArray`
//! dispatch, the assembly bridge and the maintenance scheduler are the shared
//! `tairix_raid` crate, because the native filesystem's multi-device volumes
//! drive the same engines. This crate is the *driver*: the bind table its
//! signed bundle publishes, the pure decision logic below, and the `Run`
//! program the kernel spawns for a matched node.
//!
//! # The member agent
//!
//! An array is several block devices driven as one, so one process must hold
//! client authority over every member at once. A driver is spawned for exactly
//! one matched hardware-tree node and receives exactly that node's resource
//! grants, so no process is born able to reach a whole array. The member agent
//! closes that gap without widening anyone's authority: matched to the
//! `tairix,raid-member` node the volume manager emits for a device whose first
//! block probed as array metadata, it **delegates** that one device's block
//! transport and data window to the composer's reserved rendezvous and then
//! offers the device to it. Authority flows one device at a time, from the
//! process that legitimately holds it, to a rendezvous only a privileged
//! binder can serve.
//!
//! The agent then holds the membership open: the composer answers the offer
//! only when the membership ends, so one outstanding call carries the whole
//! lifecycle. The agent parks on that reply and is woken by it — or by the
//! composer's endpoint being torn down, which cancels the call. Nothing polls
//! the composer and nothing polls the device.
//!
//! [`MemberAgent`] is that lifecycle as pure, host-tested logic; the `Run`
//! program supplies the clock, the syscalls, and the audit trail.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod agent;

pub use agent::{AgentStep, MemberAgent, REOFFER_BASE_NS, REOFFER_CEILING_NS};

use tairix_abi::raid_ipc::RAID_MEMBER_COMPATIBLE;
use tairix_abi::{DriverBindKey, HwMatchKey};

/// Bind priority of the member-agent key.
///
/// A member node carries exactly one compatible string and no other driver
/// binds it, so the priority only has to be a definite value rather than win a
/// contest; it matches the storage policy drivers' priority so the resolution
/// stays uniform across the class.
const BIND_PRIORITY: u16 = 10;

/// The bind table the signed bundle publishes.
///
/// One key today: a device the volume manager recognised as an array member.
/// The composer half binds the virtual bus the kernel publishes, and enters
/// this table with the stage that adds it.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(RAID_MEMBER_COMPATIBLE) {
        Ok(key) => key,
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];
