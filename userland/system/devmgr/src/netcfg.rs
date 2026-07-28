//! Delivering the stack-wide `net.*` policy to the network stack.
//!
//! `netstack` is the network-parsing sandbox and holds no filesystem
//! capability, so it cannot read `/System/Settings/Configuration/system.conf`
//! itself. The device manager already holds `CAP_NET_ADMIN` and drives the
//! network stack's admin endpoint (see [`crate::netbind`]), so it is the
//! component that reads the stack-wide `net.*` settings from the
//! configuration store post-unlock and delivers them to `netstack` over the
//! capability-gated `ApplyNetworkSettings` admin op (`plans/NETWORK.md`
//! N9b-2).
//!
//! This module is the pure, host-testable policy for that delivery: read the
//! settings through the [`NetworkConfigSource`] seam and, until they have
//! been delivered, push them through the [`crate::netbind::NetstackBind`]
//! seam. Delivery is fail-soft — the store may not be mounted yet (before the
//! root unlock) and the stack may not be up yet, so a failed attempt is
//! logged and retried on the next hardware-tree generation bump, exactly like
//! an unavailable driver store. Until the real policy lands, `netstack`'s own
//! safe defaults (both families enabled, SYN cookies `auto`) hold.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use tairix_abi::net_ipc::{NetBondConfigMsg, NetInterfaceConfigMsg, NetworkSettings, IF_NAME_LEN};
use tairix_abi::Errno;
use tairix_log::{log as log_event, Event, Field, FieldValue, Level, Sink};

use crate::events;
use crate::netbind::NetstackBind;

/// The device manager's read of the stack-wide `net.*` policy from the
/// system-configuration store.
///
/// The production implementation reads
/// `/System/Settings/Configuration/system.conf` and maps it through the one
/// shared `lib/sysconfig` engine ([`settings_from_config`]); it is a seam so
/// the delivery policy is host-testable against a scripted double.
pub trait NetworkConfigSource {
    /// Load the current stack-wide network settings.
    ///
    /// Returns [`Some`] when the store was read and parsed (the real policy,
    /// ready to deliver), and [`None`] when it could not be read — the store
    /// service is not reachable yet, the file is absent, or the read failed.
    /// A [`None`] is not an error: the caller keeps the network stack on its
    /// safe defaults and retries on the next generation bump.
    fn load(&mut self) -> Option<NetworkSettings>;
}

/// Map a parsed [`system.conf`](tairix_sysconfig::SystemConfig) onto the
/// stack-wide [`NetworkSettings`] the network stack enforces.
///
/// The mapping is exact and the single definition both the service binary and
/// its tests use (`AGENTS.md` §2.2): `net.ipv4.enabled` / `net.ipv6.enabled`
/// gate the families, `net.tcp.syncookies always` selects unconditional SYN
/// cookies (`auto` leaves the bounded backlog), `net.ipv6.privacy` enables
/// RFC 8981 temporary (privacy) IPv6 addresses, `net.tcp.keepalive`
/// enables RFC 9293 §3.8.4 TCP keepalive probing on idle connections, and
/// `net.tcp.ecn` enables RFC 3168 Explicit Congestion Notification.
#[cfg(feature = "program")]
#[must_use]
pub fn settings_from_config(config: &tairix_sysconfig::SystemConfig) -> NetworkSettings {
    NetworkSettings {
        ipv4_enabled: config.net_ipv4_enabled.is_enabled(),
        ipv6_enabled: config.net_ipv6_enabled.is_enabled(),
        syncookies_always: matches!(
            config.net_tcp_syncookies,
            tairix_sysconfig::SynCookies::Always
        ),
        ipv6_privacy: config.net_ipv6_privacy.is_enabled(),
        tcp_keepalive: config.net_tcp_keepalive.is_enabled(),
        tcp_ecn: config.net_tcp_ecn.is_enabled(),
    }
}

/// The device manager's memory of whether it has delivered the stack-wide
/// `net.*` policy to the network stack.
///
/// Delivery happens exactly once: the read-only `/System` configuration store
/// is static (runtime reload is a later increment), so once the policy has
/// been read and the stack accepted it, no further read or push is made.
#[derive(Default)]
pub struct NetConfigState {
    delivered: bool,
}

impl NetConfigState {
    /// A fresh state with nothing delivered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the policy has already been delivered and accepted.
    #[must_use]
    pub fn is_delivered(&self) -> bool {
        self.delivered
    }
}

/// Deliver the stack-wide `net.*` policy to the network stack, once.
///
/// A no-op after a successful delivery. Otherwise it reads the policy through
/// `source`; if the store is not yet readable ([`None`]) it leaves the stack
/// on its safe defaults and returns (retried on the next bump). If a policy
/// is read, it is pushed through `netstack`: success is recorded (no further
/// attempts), and a refusal is logged fail-soft and retried next bump — the
/// stack may not have bound its admin endpoint yet.
pub fn deliver_network_settings(
    source: &mut dyn NetworkConfigSource,
    state: &mut NetConfigState,
    netstack: &mut dyn NetstackBind,
    sink: &dyn Sink,
) {
    if state.delivered {
        return;
    }
    let Some(settings) = source.load() else {
        // The store is not readable yet (the store service not reachable yet,
        // or an absent/failed read): the stack keeps its safe defaults and
        // this is retried on the next generation bump. Not logged — an absent
        // store early in boot is the expected state, not an anomaly.
        return;
    };
    match netstack.apply_settings(settings) {
        Ok(()) => {
            state.delivered = true;
            log_event(
                sink,
                &Event {
                    level: Level::Info,
                    id: events::NETWORK_SETTINGS_DELIVERED,
                    message: "network settings delivered to the network stack",
                    fields: &[],
                },
            );
        }
        Err(_) => {
            log_event(
                sink,
                &Event {
                    level: Level::Warn,
                    id: events::NETWORK_SETTINGS_DELIVERY_FAILED,
                    message: "network settings delivery to the network stack failed; will retry",
                    fields: &[],
                },
            );
        }
    }
}

/// The set of per-interface configurations the device manager derives from
/// `network.conf`, ready to deliver to the network stack.
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

/// Default bond failover-monitor interval when `<bond>.bond.monitor-interval`
/// is unset: a short anti-flap up-delay that is nonetheless positive (the
/// engine refuses a zero interval, which would readmit a flapping member
/// instantly).
#[cfg(feature = "program")]
const DEFAULT_BOND_MONITOR_MS: u32 = 100;

/// The device manager's read of the per-interface `network.conf`
/// configuration (`plans/NETWORK.md` §6.1).
///
/// The production implementation reads
/// `/System/Settings/Network/network.conf` and maps it through the one
/// shared `lib/netconfig` engine ([`interface_configs_from_config`]); it is
/// a seam so the delivery policy is host-testable against a scripted double.
pub trait NetworkInterfaceConfigSource {
    /// Load the current per-interface configuration plan.
    ///
    /// Returns [`Some`] when the store was read and parsed, and [`None`]
    /// when it could not be read — the store service is not reachable yet,
    /// the file is absent, the read failed, or the document did not parse. A
    /// [`None`] is not an error: the caller retries on the next generation
    /// bump (fail closed — never a half-applied guess).
    fn load(&mut self) -> Option<InterfaceConfigPlan>;
}

/// Map a parsed [`network.conf`](tairix_netconfig::NetworkConfig) into the
/// per-interface [`InterfaceConfigPlan`] the device manager delivers.
///
/// The mapping is the single definition both the service binary and its
/// tests use (`AGENTS.md` §2.2):
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
#[cfg(feature = "program")]
#[must_use]
pub fn interface_configs_from_config(
    config: &tairix_netconfig::NetworkConfig,
) -> InterfaceConfigPlan {
    use tairix_netconfig::IfaceKind;

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

/// Map an interface's addressing keys onto the ABI address configs, or
/// [`None`] when a static method carries no address (an inconsistent
/// document the caller refuses — fail closed).
#[cfg(feature = "program")]
fn addressing_of(
    iface: &tairix_netconfig::InterfaceConfig,
) -> Option<(
    tairix_abi::net_ipc::NetIpv4Config,
    tairix_abi::net_ipc::NetIpv6Config,
)> {
    use tairix_netconfig::{Ipv4Method, Ipv6Method};
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
/// same [`MAX_DNS_SERVERS`](tairix_netconfig::MAX_DNS_SERVERS) bound, so this
/// only trips on a corrupt in-memory config — the caller refuses it, fail
/// closed). An interface with no static servers yields the empty list.
#[cfg(feature = "program")]
fn dns_of(iface: &tairix_netconfig::InterfaceConfig) -> Option<tairix_abi::net_ipc::NetDnsServers> {
    let records: Vec<tairix_abi::net_ipc::NetResolverServer> = iface
        .dns_servers()
        .iter()
        .map(|addr| dns_record_of(*addr))
        .collect();
    tairix_abi::net_ipc::NetDnsServers::from_servers(&records).ok()
}

/// Project a configured [`IpAddr`](core::net::IpAddr) onto the ABI
/// [`NetResolverServer`](tairix_abi::net_ipc::NetResolverServer) wire shape
/// (family plus sixteen address bytes; a V4 server uses the first four).
#[cfg(feature = "program")]
fn dns_record_of(addr: core::net::IpAddr) -> tairix_abi::net_ipc::NetResolverServer {
    use tairix_abi::net_ipc::{NetAddrFamily, NetResolverServer};
    match addr {
        core::net::IpAddr::V4(a) => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&a.octets());
            NetResolverServer {
                family: NetAddrFamily::V4,
                addr: bytes,
            }
        }
        core::net::IpAddr::V6(a) => NetResolverServer {
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
#[cfg(feature = "program")]
fn bond_config_of(iface: &tairix_netconfig::InterfaceConfig) -> Option<NetBondConfigMsg> {
    use tairix_netconfig::BondMode as CfgMode;
    let members = iface.members();
    if members.len() < 2 || members.len() > tairix_abi::net_ipc::NET_BOND_MAX_MEMBERS {
        return None;
    }
    let mut table = [[0u8; IF_NAME_LEN]; tairix_abi::net_ipc::NET_BOND_MAX_MEMBERS];
    for (index, member) in members.iter().enumerate() {
        table[index] = name_bytes(member);
    }
    let mode = match iface.bond_mode.unwrap_or(CfgMode::ActiveBackup) {
        CfgMode::ActiveBackup => tairix_abi::net_ipc::NetBondMode::ActiveBackup,
        CfgMode::Balance => tairix_abi::net_ipc::NetBondMode::Balance,
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
#[cfg(feature = "program")]
fn name_bytes(name: &str) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    let bytes = name.as_bytes();
    let len = bytes.len().min(IF_NAME_LEN);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

/// The device manager's memory of which per-interface configurations it has
/// delivered to the network stack, and whether the config-error rejects
/// have been surfaced.
///
/// Unlike the stack-wide settings (delivered once), each interface's
/// configuration is delivered when *its* interface binds — asynchronously,
/// as the driver comes up — so the plan is retried on every generation bump
/// until each interface has accepted its configuration. A delivered
/// interface is skipped thereafter (idempotent).
#[derive(Default)]
pub struct NetIfConfigState {
    plan: Option<InterfaceConfigPlan>,
    delivered: BTreeSet<[u8; IF_NAME_LEN]>,
    delivered_bonds: BTreeSet<[u8; IF_NAME_LEN]>,
    rejected_logged: bool,
}

impl NetIfConfigState {
    /// A fresh state with nothing loaded or delivered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Deliver each managed interface's `network.conf` configuration to the
/// network stack, retrying until each interface has accepted it.
///
/// The plan is read once through `source` and cached (the read-only
/// `/System` store is static); until it is readable this is a no-op that
/// retries on the next bump. Any config-error rejects (a managed non-bond
/// interface with no `match.mac`) are surfaced loud, once. Each not-yet-
/// delivered interface's configuration is then pushed: an [`Errno::NotFound`]
/// means the interface has not bound yet — the expected early state, retried
/// silently — a success records the interface as delivered, and any other
/// refusal is logged fail-soft and retried on the next bump.
pub fn deliver_interface_configs(
    source: &mut dyn NetworkInterfaceConfigSource,
    state: &mut NetIfConfigState,
    netstack: &mut dyn NetstackBind,
    sink: &dyn Sink,
) {
    if state.plan.is_none() {
        let Some(plan) = source.load() else {
            // Not readable yet (the store service not reachable yet, or an
            // absent/failed/unparseable read): retried on the next bump. Not
            // logged — an absent store early in boot is the expected state.
            return;
        };
        state.plan = Some(plan);
    }

    // Surface any config-error rejects loud, exactly once.
    if !state.rejected_logged {
        if let Some(plan) = &state.plan {
            for name in &plan.rejected {
                audit_iface(
                    sink,
                    events::NETWORK_IFCONFIG_REJECTED,
                    Level::Warn,
                    "network.conf interface has no match.mac/match.node identity; skipped",
                    name,
                );
            }
        }
        state.rejected_logged = true;
    }

    // Deliver the per-interface configs and the bond compositions, repeating
    // while a pass records a new delivery. One pass is not enough because the
    // three kinds form a dependency chain that resolves in order:
    //   1. a member/plain interface's config binds once its driver is up;
    //   2. a bond composes once its member aliases exist (step 1);
    //   3. a **bond** interface's own addressing (a per-interface config
    //      whose alias is the bond) applies only once the bond exists (step
    //      2) — an earlier attempt returns `NotFound` and is not recorded.
    // Re-running the per-interface pass after composing the bonds is what
    // lets the bond's address land in the same bump the bond was composed,
    // rather than waiting for an unrelated later bump that may never come.
    // Bounded to one pass per pending item (progress each round guarantees
    // termination well inside it): a hostile or misconfigured store can never
    // spin this. Items still `NotFound` after the loop (an unbound driver)
    // are left for the next bump, exactly as before.
    let max_passes = match &state.plan {
        Some(plan) => plan.messages.len() + plan.bonds.len() + 1,
        None => 0,
    };
    for _ in 0..max_passes {
        let before = state.delivered.len() + state.delivered_bonds.len();

        // `NetInterfaceConfigMsg` is `Copy`, so collect the pending set to end
        // the immutable borrow of `state.plan` before recording deliveries.
        let pending: Vec<NetInterfaceConfigMsg> = match &state.plan {
            Some(plan) => plan
                .messages
                .iter()
                .filter(|msg| !state.delivered.contains(&msg.alias))
                .copied()
                .collect(),
            None => Vec::new(),
        };
        for msg in &pending {
            match netstack.apply_interface_config(msg) {
                Ok(()) => {
                    state.delivered.insert(msg.alias);
                    audit_iface(
                        sink,
                        events::NETWORK_IFCONFIG_DELIVERED,
                        Level::Info,
                        "per-interface network configuration delivered",
                        &msg.alias,
                    );
                }
                // The interface has not bound yet (or its bond is not composed
                // yet): the expected state, retried on a later pass or bump.
                Err(Errno::NotFound) => {}
                Err(_) => {
                    audit_iface(
                        sink,
                        events::NETWORK_IFCONFIG_DELIVERY_FAILED,
                        Level::Warn,
                        "per-interface network configuration refused; will retry",
                        &msg.alias,
                    );
                }
            }
        }

        // Deliver every not-yet-composed bond. A bond needs its members
        // renamed first, so an early attempt returns `NotFound` — retried on
        // a later pass or bump, exactly like an unbound interface.
        let pending_bonds: Vec<NetBondConfigMsg> = match &state.plan {
            Some(plan) => plan
                .bonds
                .iter()
                .filter(|msg| !state.delivered_bonds.contains(&msg.alias))
                .copied()
                .collect(),
            None => Vec::new(),
        };
        for msg in &pending_bonds {
            match netstack.apply_bond_config(msg) {
                Ok(()) => {
                    state.delivered_bonds.insert(msg.alias);
                    audit_iface(
                        sink,
                        events::NETWORK_IFCONFIG_DELIVERED,
                        Level::Info,
                        "bond interface composed",
                        &msg.alias,
                    );
                }
                Err(Errno::NotFound) => {}
                Err(_) => {
                    audit_iface(
                        sink,
                        events::NETWORK_IFCONFIG_DELIVERY_FAILED,
                        Level::Warn,
                        "bond interface composition refused; will retry",
                        &msg.alias,
                    );
                }
            }
        }

        // A pass that recorded no new delivery has reached a fixed point:
        // every remaining item is waiting on something outside this bump (an
        // unbound driver), so stop rather than spin.
        if state.delivered.len() + state.delivered_bonds.len() == before {
            break;
        }
    }
}

/// Emit one audit record carrying the interface alias.
fn audit_iface(
    sink: &dyn Sink,
    id: tairix_log::EventId,
    level: Level,
    message: &'static str,
    iface: &[u8; IF_NAME_LEN],
) {
    let len = iface.iter().position(|&b| b == 0).unwrap_or(IF_NAME_LEN);
    let name = core::str::from_utf8(&iface[..len]).unwrap_or("?");
    log_event(
        sink,
        &Event {
            level,
            id,
            message,
            fields: &[Field {
                key: "iface",
                value: FieldValue::Str(name),
            }],
        },
    );
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;
    use tairix_abi::net_ipc::IF_NAME_LEN;
    use tairix_abi::Errno;
    use tairix_log::Event;

    /// A scripted config source: hands out a queued `load` result per call.
    struct ScriptedSource {
        results: RefCell<Vec<Option<NetworkSettings>>>,
    }

    impl ScriptedSource {
        fn new(results: Vec<Option<NetworkSettings>>) -> Self {
            Self {
                results: RefCell::new(results),
            }
        }
    }

    impl NetworkConfigSource for ScriptedSource {
        fn load(&mut self) -> Option<NetworkSettings> {
            self.results.borrow_mut().pop().flatten()
        }
    }

    /// A recording netstack seam: captures each delivered policy and answers
    /// each `apply_settings` with a scripted result.
    struct RecordingNetstack {
        applied: RefCell<Vec<NetworkSettings>>,
        results: RefCell<Vec<Result<(), Errno>>>,
        ifconfigs: RefCell<Vec<NetInterfaceConfigMsg>>,
        ifconfig_results: RefCell<Vec<Result<(), Errno>>>,
        bonds: RefCell<Vec<NetBondConfigMsg>>,
    }

    impl RecordingNetstack {
        fn new(results: Vec<Result<(), Errno>>) -> Self {
            Self {
                applied: RefCell::new(Vec::new()),
                results: RefCell::new(results),
                ifconfigs: RefCell::new(Vec::new()),
                ifconfig_results: RefCell::new(Vec::new()),
                bonds: RefCell::new(Vec::new()),
            }
        }

        /// A recorder scripted with per-`apply_interface_config` results
        /// (consumed front-to-back).
        fn with_ifconfig_results(results: Vec<Result<(), Errno>>) -> Self {
            let mut me = Self::new(Vec::new());
            // Reverse so `pop` returns them in call order.
            let mut reversed = results;
            reversed.reverse();
            me.ifconfig_results = RefCell::new(reversed);
            me
        }
    }

    impl NetstackBind for RecordingNetstack {
        fn bind_driver(
            &mut self,
            _e: u64,
            _i: &[u8; IF_NAME_LEN],
            _node_location: u64,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_settings(&mut self, settings: NetworkSettings) -> Result<(), Errno> {
            self.applied.borrow_mut().push(settings);
            self.results.borrow_mut().pop().unwrap_or(Ok(()))
        }

        fn apply_interface_config(&mut self, config: &NetInterfaceConfigMsg) -> Result<(), Errno> {
            self.ifconfigs.borrow_mut().push(*config);
            self.ifconfig_results.borrow_mut().pop().unwrap_or(Ok(()))
        }

        fn apply_bond_config(&mut self, config: &NetBondConfigMsg) -> Result<(), Errno> {
            self.bonds.borrow_mut().push(*config);
            Ok(())
        }
    }

    struct RecordingSink {
        ids: RefCell<Vec<u32>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                ids: RefCell::new(Vec::new()),
            }
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    // A flat test builder mirroring the six independent wire flags of
    // `NetworkSettings`; an enum would only obscure the mapping the test
    // is asserting.
    #[allow(clippy::fn_params_excessive_bools)]
    fn settings(
        v4: bool,
        v6: bool,
        cookies: bool,
        privacy: bool,
        keepalive: bool,
        ecn: bool,
    ) -> NetworkSettings {
        NetworkSettings {
            ipv4_enabled: v4,
            ipv6_enabled: v6,
            syncookies_always: cookies,
            ipv6_privacy: privacy,
            tcp_keepalive: keepalive,
            tcp_ecn: ecn,
        }
    }

    #[cfg(feature = "program")]
    #[test]
    fn settings_map_from_the_config_registry() {
        let mut config = tairix_sysconfig::SystemConfig::default();
        assert_eq!(
            settings_from_config(&config),
            settings(true, true, false, false, false, false),
            "the registry defaults map to families-on, cookies-auto, privacy-off, keepalive-off, ecn-off"
        );
        config.net_ipv6_enabled = tairix_sysconfig::NetToggle::Disabled;
        config.net_tcp_syncookies = tairix_sysconfig::SynCookies::Always;
        config.net_ipv6_privacy = tairix_sysconfig::NetToggle::Enabled;
        config.net_tcp_keepalive = tairix_sysconfig::NetToggle::Enabled;
        config.net_tcp_ecn = tairix_sysconfig::NetToggle::Enabled;
        assert_eq!(
            settings_from_config(&config),
            settings(true, false, true, true, true, true)
        );
    }

    #[test]
    fn absent_store_keeps_defaults_and_retries() {
        let mut source = ScriptedSource::new(alloc::vec![None]);
        let mut state = NetConfigState::new();
        let mut netstack = RecordingNetstack::new(Vec::new());
        let sink = RecordingSink::new();
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(!state.is_delivered(), "an unreadable store defers delivery");
        assert!(netstack.applied.borrow().is_empty(), "nothing pushed");
        assert!(
            sink.ids.borrow().is_empty(),
            "the expected early state is quiet"
        );
    }

    #[test]
    fn a_read_policy_is_delivered_once() {
        let policy = settings(true, false, true, true, true, true);
        let mut source = ScriptedSource::new(alloc::vec![Some(policy), Some(policy)]);
        let mut state = NetConfigState::new();
        let mut netstack = RecordingNetstack::new(alloc::vec![Ok(()), Ok(())]);
        let sink = RecordingSink::new();
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(state.is_delivered());
        assert_eq!(*netstack.applied.borrow(), alloc::vec![policy]);
        assert_eq!(
            sink.ids.borrow().as_slice(),
            &[events::NETWORK_SETTINGS_DELIVERED.0]
        );
        // A second pass delivers nothing more (the store is static).
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert_eq!(netstack.applied.borrow().len(), 1, "delivered exactly once");
    }

    #[test]
    fn a_refused_delivery_is_retried() {
        let policy = settings(false, true, false, false, false, false);
        let mut source = ScriptedSource::new(alloc::vec![Some(policy), Some(policy)]);
        let mut state = NetConfigState::new();
        // First apply refused (stack not up yet), second accepted.
        let mut netstack = RecordingNetstack::new(alloc::vec![Ok(()), Err(Errno::NotConnected)]);
        let sink = RecordingSink::new();
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(!state.is_delivered(), "a refused delivery is not recorded");
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(state.is_delivered(), "retried and delivered");
        assert_eq!(*netstack.applied.borrow(), alloc::vec![policy, policy]);
        assert_eq!(
            sink.ids.borrow().as_slice(),
            &[
                events::NETWORK_SETTINGS_DELIVERY_FAILED.0,
                events::NETWORK_SETTINGS_DELIVERED.0
            ]
        );
    }

    // --- Per-interface configuration delivery (N9b-3-1) -----------------

    /// A scripted per-interface config source: hands out a queued `load`
    /// result per call.
    struct ScriptedIfSource {
        results: RefCell<Vec<Option<InterfaceConfigPlan>>>,
    }

    impl ScriptedIfSource {
        fn new(results: Vec<Option<InterfaceConfigPlan>>) -> Self {
            Self {
                results: RefCell::new(results),
            }
        }
    }

    impl NetworkInterfaceConfigSource for ScriptedIfSource {
        fn load(&mut self) -> Option<InterfaceConfigPlan> {
            self.results.borrow_mut().pop().flatten()
        }
    }

    fn iface_name(text: &str) -> [u8; IF_NAME_LEN] {
        let mut out = [0u8; IF_NAME_LEN];
        out[..text.len()].copy_from_slice(text.as_bytes());
        out
    }

    fn a_config(alias: &str, mac: [u8; 6]) -> NetInterfaceConfigMsg {
        NetInterfaceConfigMsg {
            alias: iface_name(alias),
            match_mac: Some(mac),
            match_node: None,
            ipv4: tairix_abi::net_ipc::NetIpv4Config::Disabled,
            ipv6: tairix_abi::net_ipc::NetIpv6Config::Slaac,
            mtu: 0,
            dns: tairix_abi::net_ipc::NetDnsServers::EMPTY,
        }
    }

    #[test]
    fn an_absent_interface_config_store_is_quiet_and_retries() {
        let mut source = ScriptedIfSource::new(alloc::vec![None]);
        let mut state = NetIfConfigState::new();
        let mut netstack = RecordingNetstack::new(Vec::new());
        let sink = RecordingSink::new();
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        assert!(netstack.ifconfigs.borrow().is_empty(), "nothing pushed");
        assert!(sink.ids.borrow().is_empty(), "the early state is quiet");
    }

    #[test]
    fn an_interface_config_is_delivered_when_the_interface_binds() {
        let plan = InterfaceConfigPlan {
            messages: alloc::vec![a_config("wan", [1, 2, 3, 4, 5, 6])],
            bonds: Vec::new(),
            rejected: Vec::new(),
        };
        // The source is read once and cached; the stack answers NotFound
        // (not bound yet) then Ok (bound), then is not called again.
        let mut source = ScriptedIfSource::new(alloc::vec![Some(plan)]);
        let mut state = NetIfConfigState::new();
        let mut netstack =
            RecordingNetstack::with_ifconfig_results(alloc::vec![Err(Errno::NotFound), Ok(())]);
        let sink = RecordingSink::new();

        // First bump: interface not bound yet — retried silently.
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        assert_eq!(netstack.ifconfigs.borrow().len(), 1);
        assert!(
            sink.ids.borrow().is_empty(),
            "a not-yet-bound iface is quiet"
        );

        // Second bump: the interface bound; the config is delivered.
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        assert_eq!(netstack.ifconfigs.borrow().len(), 2);
        assert_eq!(
            sink.ids.borrow().as_slice(),
            &[events::NETWORK_IFCONFIG_DELIVERED.0]
        );

        // Third bump: already delivered — nothing pushed.
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        assert_eq!(
            netstack.ifconfigs.borrow().len(),
            2,
            "a delivered interface is not re-pushed"
        );
    }

    #[test]
    fn a_managed_interface_without_match_mac_is_rejected_once() {
        let plan = InterfaceConfigPlan {
            messages: Vec::new(),
            bonds: Vec::new(),
            rejected: alloc::vec![iface_name("wan")],
        };
        let mut source = ScriptedIfSource::new(alloc::vec![Some(plan)]);
        let mut state = NetIfConfigState::new();
        let mut netstack = RecordingNetstack::new(Vec::new());
        let sink = RecordingSink::new();
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        assert_eq!(
            sink.ids.borrow().as_slice(),
            &[events::NETWORK_IFCONFIG_REJECTED.0],
            "the config error is surfaced loud exactly once"
        );
        assert!(netstack.ifconfigs.borrow().is_empty());
    }

    #[cfg(feature = "program")]
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
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
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

    #[cfg(feature = "program")]
    #[test]
    fn a_dhcp_interface_maps_to_the_dhcp_addressing() {
        // A `wan` interface configured for DHCPv4 yields a message whose
        // IPv4 addressing is `Dhcp` (no static address fields).
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv4.method dhcp
";
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        assert!(matches!(wan.ipv4, tairix_abi::net_ipc::NetIpv4Config::Dhcp));
        assert!(plan.rejected.is_empty());
    }

    #[cfg(feature = "program")]
    #[test]
    fn a_dhcpv6_interface_maps_to_the_dhcp_addressing() {
        // A `wan` interface configured for DHCPv6 yields a message whose
        // IPv6 addressing is `Dhcp` (no static address fields).
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv6.method dhcp
";
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
        let wan = plan
            .messages
            .iter()
            .find(|m| m.alias == iface_name("wan"))
            .expect("wan");
        assert!(matches!(wan.ipv6, tairix_abi::net_ipc::NetIpv6Config::Dhcp));
        assert!(plan.rejected.is_empty());
    }

    #[cfg(feature = "program")]
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
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
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

    #[cfg(feature = "program")]
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
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
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

    #[cfg(feature = "program")]
    #[test]
    fn a_bond_is_delivered_after_its_members() {
        let text = "\
bond0.kind bond
bond0.bond.members eth0,eth1
eth0.match.mac 02:00:00:00:00:02
eth1.match.mac 02:00:00:00:00:03
";
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
        let mut source = ScriptedIfSource::new(alloc::vec![Some(plan)]);
        let mut state = NetIfConfigState::new();
        // Members bind (Ok), then the bond composes (Ok).
        let mut netstack = RecordingNetstack::new(Vec::new());
        let sink = RecordingSink::new();
        deliver_interface_configs(&mut source, &mut state, &mut netstack, &sink);
        // The bond's own addressing and both member renames were delivered.
        assert_eq!(
            netstack.ifconfigs.borrow().len(),
            3,
            "bond addressing + two member renames"
        );
        assert_eq!(netstack.bonds.borrow().len(), 1, "the bond composed");
        assert_eq!(netstack.bonds.borrow()[0].alias, iface_name("bond0"));
    }
}
