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

use tairix_abi::net_ipc::{NetInterfaceConfigMsg, NetworkSettings, IF_NAME_LEN};
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
    /// is not mounted yet (before the root unlock) or the read failed. A
    /// [`None`] is not an error: the caller keeps the network stack on its
    /// safe defaults and retries on the next generation bump.
    fn load(&mut self) -> Option<NetworkSettings>;
}

/// Map a parsed [`system.conf`](tairix_sysconfig::SystemConfig) onto the
/// stack-wide [`NetworkSettings`] the network stack enforces.
///
/// The mapping is exact and the single definition both the service binary and
/// its tests use (`AGENTS.md` §2.2): `net.ipv4.enabled` / `net.ipv6.enabled`
/// gate the families, and `net.tcp.syncookies always` selects unconditional
/// SYN cookies (`auto` leaves the bounded backlog). `net.ipv6.privacy` has no
/// enforcement consumer yet, so it is deliberately not carried.
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
    }
}

/// The device manager's memory of whether it has delivered the stack-wide
/// `net.*` policy to the network stack.
///
/// Delivery happens exactly once: the configuration store is static after the
/// root unlock (runtime reload is a later increment), so once the policy has
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
        // The store is not readable yet (pre-unlock, or a failed read): the
        // stack keeps its safe defaults and this is retried on the next
        // generation bump. Not logged — an absent store before the unlock is
        // the expected early-boot state, not an anomaly.
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
/// A managed non-bond interface that carries a stable `match.mac` identity
/// yields one [`NetInterfaceConfigMsg`] in [`Self::messages`]; a managed
/// non-bond interface with *no* `match.mac` cannot be bound to hardware by
/// identity (its `match.node` binding is a later increment) and is recorded
/// in [`Self::rejected`] so the operator's configuration error is surfaced
/// loud rather than silently ignored. Bond interfaces and their members are
/// omitted (bonding is a later increment).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InterfaceConfigPlan {
    /// One message per deliverable managed interface.
    pub messages: Vec<NetInterfaceConfigMsg>,
    /// The aliases of managed non-bond interfaces refused for want of a
    /// `match.mac` identity selector (NUL-padded).
    pub rejected: Vec<[u8; IF_NAME_LEN]>,
}

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
    /// when it could not be read — the store is not mounted yet (before the
    /// root unlock), the read failed, or the document did not parse. A
    /// [`None`] is not an error: the caller retries on the next generation
    /// bump (fail closed — never a half-applied guess).
    fn load(&mut self) -> Option<InterfaceConfigPlan>;
}

/// Map a parsed [`network.conf`](tairix_netconfig::NetworkConfig) into the
/// per-interface [`InterfaceConfigPlan`] the device manager delivers.
///
/// The mapping is the single definition both the service binary and its
/// tests use (`AGENTS.md` §2.2). Only managed, non-bond, non-member
/// interfaces are delivered: bond interfaces and any interface enrolled as
/// a bond member are omitted (bonding is a later increment). A managed
/// interface with no `match.mac` — or one whose static addressing is
/// internally inconsistent — is refused into [`InterfaceConfigPlan::rejected`]
/// rather than guessed at.
#[cfg(feature = "program")]
#[must_use]
pub fn interface_configs_from_config(
    config: &tairix_netconfig::NetworkConfig,
) -> InterfaceConfigPlan {
    use tairix_netconfig::{IfaceKind, Ipv4Method, Ipv6Method};

    // Every interface enrolled in a bond is owned by that bond, not
    // configured directly — collect them so they are skipped below.
    let mut members: BTreeSet<&str> = BTreeSet::new();
    for iface in config.interfaces() {
        for member in iface.members() {
            members.insert(member.as_str());
        }
    }

    let mut plan = InterfaceConfigPlan::default();
    // Map each managed, non-bond, non-member interface into a message.
    for iface in config.interfaces() {
        // Bond and loopback interfaces, and any bond member, are not
        // directly delivered this increment.
        if iface.kind() != IfaceKind::Ethernet || members.contains(iface.name.as_str()) {
            continue;
        }
        let alias = name_bytes(&iface.name);
        let Some(mac) = iface.match_mac else {
            plan.rejected.push(alias);
            continue;
        };
        let ipv4 = match iface.ipv4_method() {
            Ipv4Method::Disabled => tairix_abi::net_ipc::NetIpv4Config::Disabled,
            Ipv4Method::Static => {
                // A static method with no address is an inconsistent
                // document; refuse it rather than guess (fail closed).
                let Some(cidr) = iface.ipv4_address else {
                    plan.rejected.push(alias);
                    continue;
                };
                tairix_abi::net_ipc::NetIpv4Config::Static {
                    addr: cidr.addr.octets(),
                    prefix: cidr.prefix,
                    gateway: iface.ipv4_gateway.map(|gw| gw.octets()),
                }
            }
        };
        let ipv6 = match iface.ipv6_method() {
            Ipv6Method::Disabled => tairix_abi::net_ipc::NetIpv6Config::Disabled,
            Ipv6Method::Slaac => tairix_abi::net_ipc::NetIpv6Config::Slaac,
            Ipv6Method::Static => {
                let Some(cidr) = iface.ipv6_address else {
                    plan.rejected.push(alias);
                    continue;
                };
                tairix_abi::net_ipc::NetIpv6Config::Static {
                    addr: cidr.addr.octets(),
                    prefix: cidr.prefix,
                    gateway: iface.ipv6_gateway.map(|gw| gw.octets()),
                }
            }
        };
        plan.messages.push(NetInterfaceConfigMsg {
            alias,
            match_mac: Some(mac.0),
            ipv4,
            ipv6,
            mtu: iface.mtu.unwrap_or(0),
        });
    }
    plan
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
/// The plan is read once through `source` and cached (the store is static
/// after the root unlock); until it is readable this is a no-op that
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
            // Not readable yet (pre-unlock, or a failed/unparseable read):
            // retried on the next bump. Not logged — an absent store before
            // the unlock is the expected early-boot state.
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
                    "network.conf interface has no match.mac identity; skipped",
                    name,
                );
            }
        }
        state.rejected_logged = true;
    }

    // Deliver every not-yet-delivered interface. `NetInterfaceConfigMsg` is
    // `Copy`, so collect the pending set to end the immutable borrow of
    // `state.plan` before recording deliveries into `state.delivered`.
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
            // The interface has not bound yet: the expected state until its
            // driver comes up, retried silently on the next bump.
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
    }

    impl RecordingNetstack {
        fn new(results: Vec<Result<(), Errno>>) -> Self {
            Self {
                applied: RefCell::new(Vec::new()),
                results: RefCell::new(results),
                ifconfigs: RefCell::new(Vec::new()),
                ifconfig_results: RefCell::new(Vec::new()),
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
        fn bind_driver(&mut self, _e: u64, _i: &[u8; IF_NAME_LEN]) -> Result<(), Errno> {
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

    fn settings(v4: bool, v6: bool, cookies: bool) -> NetworkSettings {
        NetworkSettings {
            ipv4_enabled: v4,
            ipv6_enabled: v6,
            syncookies_always: cookies,
        }
    }

    #[cfg(feature = "program")]
    #[test]
    fn settings_map_from_the_config_registry() {
        let mut config = tairix_sysconfig::SystemConfig::default();
        assert_eq!(
            settings_from_config(&config),
            settings(true, true, false),
            "the registry defaults map to families-on, cookies-auto"
        );
        config.net_ipv6_enabled = tairix_sysconfig::NetToggle::Disabled;
        config.net_tcp_syncookies = tairix_sysconfig::SynCookies::Always;
        assert_eq!(settings_from_config(&config), settings(true, false, true));
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
        let policy = settings(true, false, true);
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
        let policy = settings(false, true, false);
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
            ipv4: tairix_abi::net_ipc::NetIpv4Config::Disabled,
            ipv6: tairix_abi::net_ipc::NetIpv6Config::Slaac,
            mtu: 0,
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
        // `wan` is a static-v4 managed interface with a MAC identity; `lan`
        // is a managed interface with NO match.mac (rejected); the bond and
        // its members are omitted.
        let text = "\
wan.match.mac aa:bb:cc:dd:ee:ff
wan.ipv4.method static
wan.ipv4.address 10.0.0.2/24
wan.ipv4.gateway 10.0.0.1
lan.ipv4.method static
lan.ipv4.address 192.168.0.2/24
bond0.kind bond
bond0.match.mac 02:00:00:00:00:01
bond0.bond.members eth0,eth1
eth0.match.mac 02:00:00:00:00:02
eth1.match.mac 02:00:00:00:00:03
";
        let config = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let plan = interface_configs_from_config(&config);
        // Only `wan` is deliverable; `lan` is rejected for want of match.mac;
        // the bond and its members are omitted.
        assert_eq!(plan.messages.len(), 1, "only wan is delivered");
        assert_eq!(plan.messages[0].alias, iface_name("wan"));
        assert_eq!(
            plan.messages[0].match_mac,
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert!(matches!(
            plan.messages[0].ipv4,
            tairix_abi::net_ipc::NetIpv4Config::Static { .. }
        ));
        assert_eq!(plan.rejected, alloc::vec![iface_name("lan")]);
    }
}
