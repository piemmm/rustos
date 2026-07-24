//! Stable [`tairix_log::EventId`] constants emitted by `netstack`.
//!
//! Per `lib/log` convention every subsystem owns a 1 000-wide reserved
//! range. The network-stack service occupies `16000..17000` (adjacent to
//! the display service's `15000..16000`). Once shipped the numeric values
//! must never be re-used or re-numbered — external audit-log consumers
//! rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `netstack` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const NETSTACK_RANGE_START: u32 = 16_000;
/// Range end (exclusive) reserved for `netstack` event identifiers.
pub const NETSTACK_RANGE_END: u32 = 17_000;

/// An admin mutation (address or route add) was applied.
///
/// Recorded at `Info`: interface configuration changes are rare,
/// security-relevant state transitions that must always surface.
pub const ADMIN_APPLIED: EventId = EventId(16_001);
/// A request was refused because the caller lacks its required
/// capability (`CAP_NET_ADMIN` for the admin surface,
/// `CAP_SYSINFO_INTROSPECT` for the broker reads).
///
/// A denial is a security-relevant decision in its own right and is
/// always recorded, at `Warn`.
pub const REQUEST_DENIED: EventId = EventId(16_002);
/// A request was rejected before dispatch: the frame failed to decode.
pub const REQUEST_MALFORMED: EventId = EventId(16_003);
/// An admin mutation named an interface the stack does not manage, or
/// the engine refused the new configuration (bad prefix, table full).
pub const ADMIN_REFUSED: EventId = EventId(16_004);

/// A socket-service request was refused before dispatch: the frame failed
/// to decode.
pub const SOCKET_MALFORMED: EventId = EventId(16_005);
/// A socket-service request was denied because the caller lacks `CAP_NET`.
///
/// A denial is a security-relevant decision recorded at `Warn`.
pub const SOCKET_DENIED: EventId = EventId(16_006);
/// A socket was opened for a principal (recorded at `Info`).
pub const SOCKET_OPENED: EventId = EventId(16_007);
/// A socket-service operation was refused after the capability check
/// (a bounded limit reached, an address in use, no route): recorded at
/// `Warn` so exhaustion and misuse surface.
pub const SOCKET_REFUSED: EventId = EventId(16_008);

/// A NIC driver's device channel was bound to a managed interface
/// (the `BindDriver` admin op provisioned the shared frame region,
/// attached the driver, and added the interface): recorded at `Info`.
pub const DRIVER_BOUND: EventId = EventId(16_009);
/// A `BindDriver` request was denied because the caller lacks
/// `CAP_NET_ADMIN`: a security-relevant decision recorded at `Warn`.
pub const DRIVER_BIND_DENIED: EventId = EventId(16_010);
/// A `BindDriver` request passed the capability check but could not be
/// carried out (no free channel slot, the driver refused `Facts`/
/// `Attach`, a shared-memory or wait-set operation failed): recorded at
/// `Warn` so a provisioning failure surfaces and the interface stays
/// unbound (fail closed).
pub const DRIVER_BIND_FAILED: EventId = EventId(16_011);
/// An inbound ICMP/`ICMPv6` echo request addressed to one of this
/// stack's interfaces was answered — the engine served it and the reply
/// is queued on the same interface. Recorded at `Info`: it is the
/// witness that a frame crossed the stack ↔ driver boundary and was
/// handled end to end (the two-process live-boot vertical gates on it).
pub const INBOUND_ECHO_SERVED: EventId = EventId(16_012);

/// A stream socket was placed into the passive LISTEN state on its bound
/// local port (recorded at `Info`): a new service is now reachable.
pub const SOCKET_LISTENING: EventId = EventId(16_013);
/// An inbound connection was claimed off a listener by
/// [`Accept`](tairix_abi::net::SocketRequest::Accept), creating a child
/// stream socket (recorded at `Info`).
pub const SOCKET_ACCEPTED: EventId = EventId(16_014);

/// The stack-wide `net.*` policy was applied over the
/// [`ApplyNetworkSettings`](tairix_abi::net_ipc::NetstackRequest::ApplyNetworkSettings)
/// admin op (family enable/disable, SYN-cookie mode): a
/// security-relevant configuration change, recorded at `Info`.
pub const NETWORK_SETTINGS_APPLIED: EventId = EventId(16_015);

/// A per-interface `network.conf` configuration was applied over the
/// [`NetInterfaceConfigMsg`](tairix_abi::net_ipc::NetInterfaceConfigMsg)
/// admin message (static addressing, MTU, family enable): a
/// security-relevant configuration change, recorded at `Info`.
pub const INTERFACE_CONFIG_APPLIED: EventId = EventId(16_016);

/// A bond (link-aggregation) interface was composed or reconfigured over
/// the [`NetBondConfigMsg`](tairix_abi::net_ipc::NetBondConfigMsg) admin
/// message (members, mode, primary, monitor interval): a
/// security-relevant configuration change, recorded at `Info`.
pub const BOND_CONFIG_APPLIED: EventId = EventId(16_017);
/// A bond-configuration request passed the capability check but was
/// refused (a member is not present yet, an alias clash, or a validation
/// failure): recorded at `Warn` so a bad configuration surfaces and the
/// bond is left untouched (fail closed).
pub const BOND_CONFIG_REFUSED: EventId = EventId(16_018);
/// A bond's transmit path changed member (failover or deliberate
/// failback): the bond re-announced its presence so peers relearn the
/// path. Recorded at `Info` — a dead member is a visible, audited fact.
pub const BOND_FAILOVER: EventId = EventId(16_019);

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_APPLIED, ADMIN_REFUSED, DRIVER_BIND_DENIED, DRIVER_BIND_FAILED, DRIVER_BOUND,
        INBOUND_ECHO_SERVED, INTERFACE_CONFIG_APPLIED, NETSTACK_RANGE_END, NETSTACK_RANGE_START,
        NETWORK_SETTINGS_APPLIED, REQUEST_DENIED, REQUEST_MALFORMED, SOCKET_ACCEPTED,
        SOCKET_DENIED, SOCKET_LISTENING, SOCKET_MALFORMED, SOCKET_OPENED, SOCKET_REFUSED,
    };
    use super::{BOND_CONFIG_APPLIED, BOND_CONFIG_REFUSED, BOND_FAILOVER};

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in [
            ADMIN_APPLIED,
            REQUEST_DENIED,
            REQUEST_MALFORMED,
            ADMIN_REFUSED,
            SOCKET_MALFORMED,
            SOCKET_DENIED,
            SOCKET_OPENED,
            SOCKET_REFUSED,
            DRIVER_BOUND,
            DRIVER_BIND_DENIED,
            DRIVER_BIND_FAILED,
            INBOUND_ECHO_SERVED,
            SOCKET_LISTENING,
            SOCKET_ACCEPTED,
            NETWORK_SETTINGS_APPLIED,
            INTERFACE_CONFIG_APPLIED,
            BOND_CONFIG_APPLIED,
            BOND_CONFIG_REFUSED,
            BOND_FAILOVER,
        ] {
            assert!(id.0 >= NETSTACK_RANGE_START && id.0 < NETSTACK_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = [
            ADMIN_APPLIED.0,
            REQUEST_DENIED.0,
            REQUEST_MALFORMED.0,
            ADMIN_REFUSED.0,
            SOCKET_MALFORMED.0,
            SOCKET_DENIED.0,
            SOCKET_OPENED.0,
            SOCKET_REFUSED.0,
            DRIVER_BOUND.0,
            DRIVER_BIND_DENIED.0,
            DRIVER_BIND_FAILED.0,
            INBOUND_ECHO_SERVED.0,
            SOCKET_LISTENING.0,
            SOCKET_ACCEPTED.0,
            NETWORK_SETTINGS_APPLIED.0,
            INTERFACE_CONFIG_APPLIED.0,
            BOND_CONFIG_APPLIED.0,
            BOND_CONFIG_REFUSED.0,
            BOND_FAILOVER.0,
        ];
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate netstack EventId");
        }
    }
}
