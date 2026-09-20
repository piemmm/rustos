//! The plan a `network.conf` document implies for the running network stack.
//!
//! The document says what each managed interface *is configured to be*; this
//! is the same statement in the `netstack-v1` messages that ask the running
//! stack to be it. One mapping, because two consumers make it: the device
//! manager delivers it at boot and on every hardware-tree bump, and
//! `configure` pushes it after writing a live edit.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use tairix_abi::net_ipc::{NetBondConfigMsg, NetInterfaceConfigMsg, IF_NAME_LEN};

use crate::{
    BondMode, IfaceKind, InterfaceConfig, Ipv4Method, Ipv6Method, NetworkConfig, MAX_BOND_MEMBERS,
    MIN_BOND_MEMBERS, MIN_MONITOR_INTERVAL_MS,
};

/// Default bond failover-monitor interval when `<bond>.bond.monitor-interval`
/// is unset: the shortest the grammar admits, which is a real anti-flap
/// up-delay rather than the zero that would readmit a flapping member
/// instantly.
const DEFAULT_BOND_MONITOR_MS: u32 = MIN_MONITOR_INTERVAL_MS;

/// The set of per-interface configurations a `network.conf` document
/// implies, ready to deliver to the network stack.
///
/// Plain interfaces and bond members yield an addressing/rename
/// [`NetInterfaceConfigMsg`] in [`Self::messages`]; bonds additionally
/// yield a [`NetBondConfigMsg`] in [`Self::bonds`]. A managed interface
/// that cannot be bound to hardware by identity (neither `match.mac` nor
/// `match.node`) or whose configuration is internally inconsistent is
/// recorded in [`Self::rejected`] so the operator's error is surfaced loud
/// rather than silently ignored.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InterfaceConfigPlan {
    /// One message per deliverable managed interface — a plain interface's
    /// addressing, an address-less member's rename, and each bond's own
    /// addressing (matched by alias).
    pub messages: Vec<NetInterfaceConfigMsg>,
    /// One bond-composition message per managed bond interface.
    pub bonds: Vec<NetBondConfigMsg>,
    /// The aliases of managed interfaces refused for a configuration error
    /// (an ethernet interface — member or plain — carrying neither
    /// `match.mac` nor `match.node` to bind it to hardware, or an
    /// inconsistent bond) (NUL-padded).
    pub rejected: Vec<[u8; IF_NAME_LEN]>,
}

impl InterfaceConfigPlan {
    /// The interface message this plan carries for `alias`, if any.
    ///
    /// Comparing this across two plans is how a consumer tells an interface
    /// an edit actually changed from one it left alone — the device manager
    /// deciding what to re-deliver, and `configure` deciding what to push.
    #[must_use]
    pub fn message_for(&self, alias: &[u8; IF_NAME_LEN]) -> Option<NetInterfaceConfigMsg> {
        self.messages
            .iter()
            .find(|msg| msg.alias == *alias)
            .copied()
    }

    /// The bond composition this plan carries for `alias`, if any.
    #[must_use]
    pub fn bond_for(&self, alias: &[u8; IF_NAME_LEN]) -> Option<NetBondConfigMsg> {
        self.bonds.iter().find(|msg| msg.alias == *alias).copied()
    }

    /// The plan a parsed `network.conf` implies.
    ///
    /// The one mapping from the document to the wire, so the device manager
    /// delivering it at boot and `configure` applying a live edit can never
    /// disagree about what a setting means:
    ///
    /// * A **bond** interface yields a [`NetBondConfigMsg`] in
    ///   [`InterfaceConfigPlan::bonds`] (members, mode, primary, monitor
    ///   interval) *and* a [`NetInterfaceConfigMsg`] carrying the bond's own
    ///   addressing, matched by alias (a bond has no hardware MAC of its own).
    /// * A **bond member** yields an address-less [`NetInterfaceConfigMsg`]
    ///   matched by its hardware identity (`match.mac` or `match.node`) — it
    ///   renames the bound NIC to the member alias so the bond can compose it;
    ///   the member holds no addresses.
    /// * A **plain** interface yields its addressing [`NetInterfaceConfigMsg`]
    ///   matched by its hardware identity (`match.mac` or `match.node`).
    ///
    /// A managed interface that cannot be bound to hardware by identity (a
    /// member or plain interface carrying neither `match.mac` nor
    /// `match.node`), or whose static addressing is internally inconsistent, is
    /// refused into [`InterfaceConfigPlan::rejected`] rather than guessed at
    /// (fail closed). Loopback is left to the stack.
    #[must_use]
    pub fn of(config: &NetworkConfig) -> Self {
        // Every interface enrolled in a bond is owned by that bond.
        let mut members: BTreeSet<&str> = BTreeSet::new();
        for iface in config.interfaces() {
            for member in iface.members() {
                members.insert(member.as_str());
            }
        }

        let mut plan = InterfaceConfigPlan::default();
        for iface in config.interfaces() {
            let alias = name_bytes(&iface.name);
            match iface.kind() {
                // Loopback is the stack's own; it is not a managed device.
                IfaceKind::Loopback => {}
                IfaceKind::Bond => {
                    // The composition message, then the bond's own addressing
                    // (matched by alias — a bond has no hardware MAC).
                    let Some(bond) = bond_config_of(iface) else {
                        plan.rejected.push(alias);
                        continue;
                    };
                    let Some((ipv4, ipv6)) = addressing_of(iface) else {
                        plan.rejected.push(alias);
                        continue;
                    };
                    let Some(dns) = dns_of(iface) else {
                        plan.rejected.push(alias);
                        continue;
                    };
                    plan.bonds.push(bond);
                    plan.messages.push(NetInterfaceConfigMsg {
                        alias,
                        match_mac: None,
                        match_node: None,
                        ipv4,
                        ipv6,
                        mtu: iface.mtu.unwrap_or(0),
                        dns,
                    });
                }
                IfaceKind::Ethernet => {
                    // A member or plain interface binds to hardware by *identity*:
                    // either its stable MAC or its hardware-node location (the
                    // register-window base of its device node). `netconfig`
                    // validation guarantees at most one is set; with neither, the
                    // interface cannot be bound to any device, so it is refused
                    // loud rather than guessed at (fail closed).
                    let match_mac = iface.match_mac.map(|mac| mac.0);
                    let match_node = iface.match_node;
                    if match_mac.is_none() && match_node.is_none() {
                        plan.rejected.push(alias);
                        continue;
                    }
                    if members.contains(iface.name.as_str()) {
                        // A bond member: rename the NIC to the member alias with
                        // no addressing (the bond owns the addresses and its
                        // own DNS servers; `netconfig` forbids a member DNS key).
                        plan.messages.push(NetInterfaceConfigMsg {
                            alias,
                            match_mac,
                            match_node,
                            ipv4: tairix_abi::net_ipc::NetIpv4Config::Disabled,
                            ipv6: tairix_abi::net_ipc::NetIpv6Config::Disabled,
                            mtu: iface.mtu.unwrap_or(0),
                            dns: tairix_abi::net_ipc::NetDnsServers::EMPTY,
                        });
                        continue;
                    }
                    let Some((ipv4, ipv6)) = addressing_of(iface) else {
                        plan.rejected.push(alias);
                        continue;
                    };
                    let Some(dns) = dns_of(iface) else {
                        plan.rejected.push(alias);
                        continue;
                    };
                    plan.messages.push(NetInterfaceConfigMsg {
                        alias,
                        match_mac,
                        match_node,
                        ipv4,
                        ipv6,
                        mtu: iface.mtu.unwrap_or(0),
                        dns,
                    });
                }
            }
        }
        plan
    }
}

/// Map an interface's addressing keys onto the ABI address configs, or
/// [`None`] when a static method carries no address (an inconsistent
/// document the caller refuses — fail closed).
fn addressing_of(
    iface: &InterfaceConfig,
) -> Option<(
    tairix_abi::net_ipc::NetIpv4Config,
    tairix_abi::net_ipc::NetIpv6Config,
)> {
    let ipv4 = match iface.ipv4_method() {
        Ipv4Method::Disabled => tairix_abi::net_ipc::NetIpv4Config::Disabled,
        Ipv4Method::Static => {
            let cidr = iface.ipv4_address?;
            tairix_abi::net_ipc::NetIpv4Config::Static {
                addr: cidr.addr.octets(),
                prefix: cidr.prefix,
                gateway: iface.ipv4_gateway.map(|gw| gw.octets()),
            }
        }
        Ipv4Method::Dhcp => tairix_abi::net_ipc::NetIpv4Config::Dhcp,
    };
    let ipv6 = match iface.ipv6_method() {
        Ipv6Method::Disabled => tairix_abi::net_ipc::NetIpv6Config::Disabled,
        Ipv6Method::Slaac => tairix_abi::net_ipc::NetIpv6Config::Slaac,
        Ipv6Method::Static => {
            let cidr = iface.ipv6_address?;
            tairix_abi::net_ipc::NetIpv6Config::Static {
                addr: cidr.addr.octets(),
                prefix: cidr.prefix,
                gateway: iface.ipv6_gateway.map(|gw| gw.octets()),
            }
        }
        Ipv6Method::Dhcp => tairix_abi::net_ipc::NetIpv6Config::Dhcp,
    };
    Some((ipv4, ipv6))
}

/// Map an interface's `<iface>.dns.servers` list onto the ABI
/// [`NetDnsServers`](tairix_abi::net_ipc::NetDnsServers), or [`None`] when
/// the list is somehow larger than the wire bound (`netconfig` enforces the
/// same [`MAX_DNS_SERVERS`](crate::MAX_DNS_SERVERS) bound, so this
/// only trips on a corrupt in-memory config — the caller refuses it, fail
/// closed). An interface with no static servers yields the empty list.
fn dns_of(iface: &InterfaceConfig) -> Option<tairix_abi::net_ipc::NetDnsServers> {
    let records: Vec<tairix_abi::net_ipc::NetServerAddr> = iface
        .dns_servers()
        .iter()
        .map(|addr| dns_record_of(*addr))
        .collect();
    tairix_abi::net_ipc::NetDnsServers::from_servers(&records).ok()
}

/// Project a configured [`IpAddr`](core::net::IpAddr) onto the ABI
/// [`NetServerAddr`](tairix_abi::net_ipc::NetServerAddr) wire shape
/// (family plus sixteen address bytes; a V4 server uses the first four).
fn dns_record_of(addr: core::net::IpAddr) -> tairix_abi::net_ipc::NetServerAddr {
    use tairix_abi::net_ipc::{NetAddrFamily, NetServerAddr};
    match addr {
        core::net::IpAddr::V4(a) => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&a.octets());
            NetServerAddr {
                family: NetAddrFamily::V4,
                addr: bytes,
            }
        }
        core::net::IpAddr::V6(a) => NetServerAddr {
            family: NetAddrFamily::V6,
            addr: a.octets(),
        },
    }
}

/// Map a bond interface's `bond.*` keys onto a [`NetBondConfigMsg`], or
/// [`None`] when the document is inconsistent (too few/many members, a bad
/// primary — caught by [`NetBondConfigMsg::validate`]); the caller refuses
/// it (fail closed). An unset monitor interval takes
/// [`DEFAULT_BOND_MONITOR_MS`].
fn bond_config_of(iface: &InterfaceConfig) -> Option<NetBondConfigMsg> {
    let members = iface.members();
    if members.len() < MIN_BOND_MEMBERS || members.len() > MAX_BOND_MEMBERS {
        return None;
    }
    let mut table = [[0u8; IF_NAME_LEN]; MAX_BOND_MEMBERS];
    for (index, member) in members.iter().enumerate() {
        table[index] = name_bytes(member);
    }
    let mode = match iface.bond_mode.unwrap_or(BondMode::ActiveBackup) {
        BondMode::ActiveBackup => tairix_abi::net_ipc::NetBondMode::ActiveBackup,
        BondMode::Balance => tairix_abi::net_ipc::NetBondMode::Balance,
    };
    let monitor_ms = iface
        .bond_monitor_interval_ms
        .unwrap_or(DEFAULT_BOND_MONITOR_MS);
    let monitor_interval = tairix_abi::Duration64::new(
        i64::from(monitor_ms / 1000),
        (monitor_ms % 1000) * 1_000_000,
    )
    .ok()?;
    let msg = NetBondConfigMsg {
        alias: name_bytes(&iface.name),
        mode,
        monitor_interval,
        primary: iface.bond_primary.as_deref().map(name_bytes),
        members: table,
        // Bounded to `NET_BOND_MAX_MEMBERS` (≤ 8) above, so this fits u8.
        member_count: u8::try_from(members.len()).ok()?,
    };
    // Validate up front so an inconsistent bond is refused, not delivered.
    msg.validate().ok()?;
    Some(msg)
}

/// Encode an interface alias name into a NUL-padded fixed field, truncating
/// at [`IF_NAME_LEN`] (the `lib/netconfig` grammar already bounds the name
/// below this, so no valid name is ever truncated).
fn name_bytes(name: &str) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    let bytes = name.as_bytes();
    let len = bytes.len().min(IF_NAME_LEN);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// A NUL-padded interface alias, as the wire carries one.
    fn iface_name(text: &str) -> [u8; IF_NAME_LEN] {
        name_bytes(text)
    }

    #[test]
    fn interface_configs_map_from_network_conf() {
        // `wan` is a static-v4 managed interface; `lan` has NO match.mac
        // (rejected); `bond0` composes `eth0`/`eth1` (both address-less
        // members). The bond yields a composition message plus its own
        // (alias-matched) addressing, and each member yields an
        // address-less rename.
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv4.method static
wan.ipv4.address 10.0.0.2/24
wan.ipv4.gateway 10.0.0.1
lan.ipv4.method static
lan.ipv4.address 192.168.0.2/24
bond0.kind bond
bond0.bond.members eth0,eth1
bond0.bond.primary eth0
bond0.ipv4.method static
bond0.ipv4.address 10.0.2.15/24
eth0.match.mac 02:00:00:00:00:02
eth1.match.mac 02:00:00:00:00:03
";
        let config = NetworkConfig::parse(text).expect("parses");
        let plan = InterfaceConfigPlan::of(&config);
        // `wan`, the bond's own addressing, and the two member renames.
        assert_eq!(plan.messages.len(), 4);
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        assert_eq!(wan.match_mac, Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert!(matches!(
            wan.ipv4,
            tairix_abi::net_ipc::NetIpv4Config::Static { .. }
        ));
        // The bond's addressing is matched by alias (no hardware MAC).
        let bond_addr = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("bond0"))
            .expect("bond addressing");
        assert_eq!(bond_addr.match_mac, None);
        assert!(matches!(
            bond_addr.ipv4,
            tairix_abi::net_ipc::NetIpv4Config::Static { .. }
        ));
        // Members are renamed by MAC and hold no addresses.
        for member in ["eth0", "eth1"] {
            let msg = plan
                .messages
                .iter()
                .find(|m| m.alias == iface_name(member))
                .expect("member");
            assert!(msg.match_mac.is_some());
            assert!(matches!(
                msg.ipv4,
                tairix_abi::net_ipc::NetIpv4Config::Disabled
            ));
            assert!(matches!(
                msg.ipv6,
                tairix_abi::net_ipc::NetIpv6Config::Disabled
            ));
        }
        // The bond composition message names its members and primary.
        assert_eq!(plan.bonds.len(), 1);
        let bond = &plan.bonds[0];
        assert_eq!(bond.alias, iface_name("bond0"));
        assert_eq!(bond.member_count, 2);
        assert_eq!(bond.members(), &[iface_name("eth0"), iface_name("eth1")]);
        assert_eq!(bond.primary, Some(iface_name("eth0")));
        assert_eq!(plan.rejected, alloc::vec![iface_name("lan")]);
    }

    #[test]
    fn a_dhcp_interface_maps_to_the_dhcp_addressing() {
        // A `wan` interface configured for DHCPv4 yields a message whose
        // IPv4 addressing is `Dhcp` (no static address fields).
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv4.method dhcp
";
        let config = NetworkConfig::parse(text).expect("parses");
        let plan = InterfaceConfigPlan::of(&config);
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        assert!(matches!(wan.ipv4, tairix_abi::net_ipc::NetIpv4Config::Dhcp));
        assert!(plan.rejected.is_empty());
    }

    #[test]
    fn a_dhcpv6_interface_maps_to_the_dhcp_addressing() {
        // A `wan` interface configured for DHCPv6 yields a message whose
        // IPv6 addressing is `Dhcp` (no static address fields).
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv6.method dhcp
";
        let config = NetworkConfig::parse(text).expect("parses");
        let plan = InterfaceConfigPlan::of(&config);
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        assert!(matches!(wan.ipv6, tairix_abi::net_ipc::NetIpv6Config::Dhcp));
        assert!(plan.rejected.is_empty());
    }

    #[test]
    fn static_dns_servers_map_onto_the_interface_config() {
        use tairix_abi::net_ipc::NetAddrFamily;
        // A `wan` interface names a mixed static DNS-server list; the
        // delivered message carries both servers, in order.
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv4.method dhcp
wan.dns.servers 9.9.9.9,2606:4700:4700::1111
";
        let config = NetworkConfig::parse(text).expect("parses");
        let plan = InterfaceConfigPlan::of(&config);
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        let servers = wan.dns.as_slice();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].family, NetAddrFamily::V4);
        assert_eq!(&servers[0].addr[..4], &[9, 9, 9, 9]);
        assert_eq!(servers[1].family, NetAddrFamily::V6);
        // Round-trips through the wire codec unchanged.
        assert_eq!(
            tairix_abi::net_ipc::NetInterfaceConfigMsg::from_bytes(&wan.to_le_bytes()),
            Ok(*wan)
        );
        assert!(plan.rejected.is_empty());
    }

    #[test]
    fn a_match_node_interface_maps_to_a_node_keyed_message_not_a_reject() {
        // `wan` is bound by hardware node (its register-window base), not
        // MAC; `lan` is a node-bound bond member. Both must yield a
        // node-keyed message — never a reject — and carry no MAC selector.
        let text = "\
wan.match.node 0xa003e00
wan.ipv4.method static
wan.ipv4.address 10.0.0.2/24
bond0.kind bond
bond0.bond.members lan,eth1
bond0.ipv4.method static
bond0.ipv4.address 10.0.2.15/24
lan.match.node 0xa003a00
eth1.match.mac 02:00:00:00:00:03
";
        let config = NetworkConfig::parse(text).expect("parses");
        let plan = InterfaceConfigPlan::of(&config);
        assert!(
            plan.rejected.is_empty(),
            "a node-bound iface is not rejected"
        );
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        assert_eq!(wan.match_mac, None);
        assert_eq!(wan.match_node, Some(0x0a00_3e00));
        // The node-bound member is renamed by node and holds no addresses.
        let lan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("lan"))
            .expect("lan member");
        assert_eq!(lan.match_mac, None);
        assert_eq!(lan.match_node, Some(0x0a00_3a00));
        assert!(matches!(
            lan.ipv4,
            tairix_abi::net_ipc::NetIpv4Config::Disabled
        ));
    }
}
