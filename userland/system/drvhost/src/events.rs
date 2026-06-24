//! Stable [`rustos_log::EventId`] constants emitted by the driver host.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. The driver host occupies `7000..8000`. Once
//! shipped the numeric values must never be re-used or re-numbered —
//! external audit-log consumers rely on them.

use rustos_log::EventId;

/// Range start (inclusive) reserved for `drvhost` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const DRVHOST_RANGE_START: u32 = 7_000;
/// Range end (exclusive) reserved for `drvhost` event identifiers.
pub const DRVHOST_RANGE_END: u32 = 8_000;

/// A driver image was loaded successfully.
pub const DRIVER_LOADED: EventId = EventId(7_001);
/// A driver image was rejected: manifest header decode failed or magic / abi mismatch.
pub const DRIVER_LOAD_REJECTED_MANIFEST: EventId = EventId(7_002);
/// A driver image was rejected: pinned syscall-table hash disagrees with host.
pub const DRIVER_LOAD_REJECTED_SYSCALL_HASH: EventId = EventId(7_003);
/// A driver image was rejected: signer key is not on the host trust anchor list.
pub const DRIVER_LOAD_REJECTED_TRUST: EventId = EventId(7_004);
/// A driver image was rejected: ed25519 signature verification failed.
pub const DRIVER_LOAD_REJECTED_SIGNATURE: EventId = EventId(7_005);
/// A driver image was rejected: requested capabilities exceed caller's set.
pub const DRIVER_LOAD_REJECTED_CAPABILITY: EventId = EventId(7_006);
/// A driver image was rejected: declared `kind = InKernel` without `CAP_DRV_KERNEL`.
pub const DRIVER_LOAD_REJECTED_KERNEL_KIND: EventId = EventId(7_007);
/// A driver image was rejected: caller lacks `CAP_DRV_LOAD`.
pub const DRIVER_LOAD_REJECTED_DRV_LOAD: EventId = EventId(7_008);
/// A driver image was rejected: the spawner has no driver program for it.
pub const DRIVER_LOAD_REJECTED_SPAWN: EventId = EventId(7_009);
/// A driver image was rejected: driver's own `register` entry point failed.
pub const DRIVER_LOAD_REJECTED_REGISTER: EventId = EventId(7_010);
/// A driver image was rejected: a bind-table entry failed to decode.
pub const DRIVER_LOAD_REJECTED_BIND_KEY: EventId = EventId(7_011);
/// A driver was unloaded.
pub const DRIVER_UNLOADED: EventId = EventId(7_020);
/// A driver was reloaded (re-read, re-verified, re-issued handle).
pub const DRIVER_RELOADED: EventId = EventId(7_021);
/// A signed-store bundle was accepted as an autoload candidate: its
/// manifest parsed and its bind table decoded fail-closed. This is a *match* candidate only — signature and
/// capability verification still happen at the load gate when (and only
/// when) the candidate wins a node.
pub const DRIVER_STORE_CANDIDATE: EventId = EventId(7_030);
/// A signed-store bundle was skipped during the scan: its manifest or
/// bind table failed to decode, or its bytes were unreadable. Skipping
/// one malformed bundle never aborts the scan.
pub const DRIVER_STORE_ENTRY_SKIPPED: EventId = EventId(7_031);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in [
            DRIVER_LOADED,
            DRIVER_LOAD_REJECTED_MANIFEST,
            DRIVER_LOAD_REJECTED_SYSCALL_HASH,
            DRIVER_LOAD_REJECTED_TRUST,
            DRIVER_LOAD_REJECTED_SIGNATURE,
            DRIVER_LOAD_REJECTED_CAPABILITY,
            DRIVER_LOAD_REJECTED_KERNEL_KIND,
            DRIVER_LOAD_REJECTED_DRV_LOAD,
            DRIVER_LOAD_REJECTED_SPAWN,
            DRIVER_LOAD_REJECTED_REGISTER,
            DRIVER_LOAD_REJECTED_BIND_KEY,
            DRIVER_UNLOADED,
            DRIVER_RELOADED,
            DRIVER_STORE_CANDIDATE,
            DRIVER_STORE_ENTRY_SKIPPED,
        ] {
            assert!(id.0 >= DRVHOST_RANGE_START && id.0 < DRVHOST_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = [
            DRIVER_LOADED.0,
            DRIVER_LOAD_REJECTED_MANIFEST.0,
            DRIVER_LOAD_REJECTED_SYSCALL_HASH.0,
            DRIVER_LOAD_REJECTED_TRUST.0,
            DRIVER_LOAD_REJECTED_SIGNATURE.0,
            DRIVER_LOAD_REJECTED_CAPABILITY.0,
            DRIVER_LOAD_REJECTED_KERNEL_KIND.0,
            DRIVER_LOAD_REJECTED_DRV_LOAD.0,
            DRIVER_LOAD_REJECTED_SPAWN.0,
            DRIVER_LOAD_REJECTED_REGISTER.0,
            DRIVER_LOAD_REJECTED_BIND_KEY.0,
            DRIVER_UNLOADED.0,
            DRIVER_RELOADED.0,
            DRIVER_STORE_CANDIDATE.0,
            DRIVER_STORE_ENTRY_SKIPPED.0,
        ];
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate drvhost EventId");
        }
    }
}
