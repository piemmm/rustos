//! Stable [`tairix_log::EventId`] constants emitted by the device
//! manager.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. The device manager occupies
//! `13000..14000`. Once shipped the numeric values must never be
//! re-used or re-numbered — external audit-log consumers rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `devmgr` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const DEVMGR_RANGE_START: u32 = 13_000;
/// Range end (exclusive) reserved for `devmgr` event identifiers.
pub const DEVMGR_RANGE_END: u32 = 14_000;

/// A hardware-tree node was bound: its winning driver is loaded.
pub const NODE_BOUND: EventId = EventId(13_001);
/// A hardware-tree node matched no driver bind table and was left
/// unbound — never an error. Emitted at `Debug`: an
/// unmatched node is the routine, high-volume case (most nodes on a real
/// device tree have no driver), so it is filtered out by the default
/// `Info` threshold and never floods the slow diagnostic UART — lower the
/// level to trace which nodes were left unbound.
/// A *binding*, a packaging tie, or a load refusal stays visible.
pub const NODE_UNBOUND: EventId = EventId(13_002);
/// Two or more drivers matched a node at the same highest priority; the
/// unbroken tie is a packaging defect, so the node is refused a binding.
pub const NODE_TIE_REJECTED: EventId = EventId(13_003);
/// A node's winning driver failed to load through the driver-host load
/// gate; the node stays unbound (fail closed).
pub const NODE_LOAD_FAILED: EventId = EventId(13_004);
/// The read-only `/System` driver-store catalogue could not be fetched (the
/// store endpoint is unbound or the store is unreadable). The device
/// manager loads nothing but keeps observing the hardware tree — never an
/// error (fail-soft).
pub const DRIVER_STORE_UNAVAILABLE: EventId = EventId(13_005);
/// A hardware-tree snapshot was read: its generation and node count. Emitted
/// at `Debug` (verbose boot/hotplug diagnostics, filtered out by default).
pub const TREE_OBSERVED: EventId = EventId(13_006);
/// One node of the observed hardware tree: its id, parent, class, and match-
/// key count. Emitted at `Debug`, so one record per node is filtered out on a
/// default-`Info` boot and only a build running this service below `Info`
/// observes it.
pub const NODE_OBSERVED: EventId = EventId(13_007);
/// A bound driver was unloaded because its hardware-tree node vanished
/// (hot-removal). The mirror of [`NODE_BOUND`]: the device
/// manager diffed a generation bump, found a bound node gone, and asked the
/// kernel to tear its driver down. Carries the unbound `node` and the torn-
/// down driver `handle`.
pub const NODE_UNLOADED: EventId = EventId(13_008);
/// The reactive observe loop ended because a hardware-tree seam operation
/// (`hw_tree_read` / `hw_tree_wait` / snapshot decode) reported an error the
/// loop fails closed on. Carries the `errno` so the failing seam is
/// diagnosable from the log; the service then exits non-zero and PID 1
/// `init` supervises the relaunch. A silent exit would hide which seam
/// refused — every abnormal exit states its reason.
pub const TREE_SEAM_FAILED: EventId = EventId(13_009);
/// A discovered NIC device-channel node (`compatible = "tairix,netchan"`,
/// emitted by a bound NIC driver process) was handed to the network stack:
/// the device manager `ipc_call`ed `netstack` `BindDriver` with the node's
/// endpoint and a derived interface alias, and the stack accepted it.
pub const NETSTACK_BOUND: EventId = EventId(13_010);
/// A NIC device-channel node was observed but could not be handed to the
/// network stack (the stack refused the bind, or its endpoint was
/// unreachable). The channel stays unbound and the hand-off is retried on
/// the next generation bump — never an error (fail-soft, like the driver
/// store being unavailable).
pub const NETSTACK_BIND_FAILED: EventId = EventId(13_011);
/// The stack-wide `net.*` policy read from `system.conf` was delivered to
/// the network stack and accepted (the `ApplyNetworkSettings` admin call
/// succeeded). Recorded at `Info`: a one-time boot configuration step.
pub const NETWORK_SETTINGS_DELIVERED: EventId = EventId(13_012);
/// The `net.*` policy was read but the network stack refused it or was
/// unreachable (its admin endpoint not yet bound). Retried on the next
/// generation bump — never an error (fail-soft, like the driver store
/// being unavailable). Recorded at `Warn`.
pub const NETWORK_SETTINGS_DELIVERY_FAILED: EventId = EventId(13_013);
/// A per-interface `network.conf` configuration was delivered to the
/// network stack and accepted (the `NetInterfaceConfigMsg` admin call
/// succeeded). Recorded at `Info`: an interface's addressing came up.
pub const NETWORK_IFCONFIG_DELIVERED: EventId = EventId(13_014);
/// A per-interface configuration was read but the network stack refused it
/// for a reason other than "interface not bound yet" (which is the silent,
/// expected retry state). Retried on the next generation bump — never an
/// error (fail-soft). Recorded at `Warn`.
pub const NETWORK_IFCONFIG_DELIVERY_FAILED: EventId = EventId(13_015);
/// A managed ethernet interface in `network.conf` declares neither a
/// `match.mac` nor a `match.node` identity selector, so the device manager
/// cannot bind it to hardware by stable identity. The interface is skipped
/// and the operator's configuration error is surfaced loud, once. Recorded
/// at `Warn`.
pub const NETWORK_IFCONFIG_REJECTED: EventId = EventId(13_016);

/// A readable `/System/Settings/` configuration document could not be
/// parsed, so nothing was delivered from it.
///
/// The administrator-writable override is hand-editable, and a document the
/// engine cannot fully understand is refused whole rather than half-applied.
/// Recorded at `Warn`: staying on the safe defaults is correct, but silently
/// ignoring an operator's edit is not — without this the file would simply
/// appear to have no effect. (`configure` renders canonically, so this
/// surfaces a hand edit.)
pub const CONFIG_DOCUMENT_MALFORMED: EventId = EventId(13_017);

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [EventId; 17] = [
        NODE_BOUND,
        NODE_UNBOUND,
        NODE_TIE_REJECTED,
        NODE_LOAD_FAILED,
        DRIVER_STORE_UNAVAILABLE,
        TREE_OBSERVED,
        NODE_OBSERVED,
        NODE_UNLOADED,
        TREE_SEAM_FAILED,
        NETSTACK_BOUND,
        CONFIG_DOCUMENT_MALFORMED,
        NETSTACK_BIND_FAILED,
        NETWORK_SETTINGS_DELIVERED,
        NETWORK_SETTINGS_DELIVERY_FAILED,
        NETWORK_IFCONFIG_DELIVERED,
        NETWORK_IFCONFIG_DELIVERY_FAILED,
        NETWORK_IFCONFIG_REJECTED,
    ];

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!(id.0 >= DEVMGR_RANGE_START && id.0 < DEVMGR_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = ALL.map(|id| id.0);
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate devmgr EventId");
        }
    }
}
