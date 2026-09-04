//! The interface table: one `lib/net` [`Stack`] per managed NIC, plus
//! the frame-ring glue that pumps frames between the engine and a
//! link-layer driver (`plans/NETWORK.md` §2.2).
//!
//! The table is the service's single source of truth for interface
//! identity: an interface is named by its admin-chosen alias
//! (`wan`, `lan0` — never a discovery-order name), carries exactly one
//! protocol engine, and is observed through the typed record types the
//! `netstack-v1` protocol defines. All protocol behaviour lives in the
//! pure engine; this module only owns, names, and feeds it.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, McastFilter, NetOffloads};
use tairix_abi::driver::net_channel::RxFilterPolicy;
use tairix_abi::driver::net_ring::{FrameOffload, FrameRings};
use tairix_abi::net_ipc::{
    validate_if_name, NetAddrFamily, NetAddrState, NetBondConfigMsg, NetBondMemberRecord,
    NetBondMode, NetCounters, NetIfAddr, NetIfKind, NetInterfaceConfigMsg,
    NetInterfaceCountersRecord, NetInterfaceFactsRecord, NetInterfaceRatesRecord,
    NetInterfaceStateRecord, NetIpv4Config, NetIpv6Config, NetServerAddr, NetworkSettings,
    IF_NAME_LEN, MAX_RESOLVER_SERVERS, NET_IF_MAX_ADDRS,
};
use tairix_abi::{Duration64, Errno, MAX_TIME_SERVERS};
use tairix_hash::HashSeed;
use tairix_net::addr::{Ecn, IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::bond::{flow_hash, Bond, BondConfig, BondEvent, BondMode, MemberId};
use tairix_net::iface::{eui64_interface_id, AddrError, TempAddrSource};
use tairix_net::internet_checksum;
use tairix_net::rate::{RateCounters, RateMeter, RateSelector};
use tairix_net::stack::{
    RxMeta, SendError, Stack, StackConfig, StackEvent, StackOutput, TxFrame, TxOffload,
};
use tairix_net::tcp::TcpSegmentMeta;

use crate::channel::FrameService;

/// The internal role an interface-table entry plays in link aggregation.
///
/// A bond is a virtual interface that owns the addresses, routes, and
/// neighbour cache; its members are the physical NICs that carry its
/// frames but hold no addresses of their own. A plain interface is
/// [`BondRole::None`].
#[derive(Clone, Debug, Default)]
enum BondRole {
    /// Not part of any bond (a plain interface or the loopback).
    #[default]
    None,
    /// A bond virtual interface: the [`Bond`] decision engine and the
    /// declared member aliases (in configured order). The owning
    /// [`Interface`]'s [`Stack`] is the bond's own stack.
    Bond {
        /// The link-aggregation decision engine.
        engine: Bond,
        /// The declared member aliases, in configured order.
        members: Vec<[u8; IF_NAME_LEN]>,
    },
    /// A member NIC enrolled in the named bond. Its own [`Stack`] is
    /// dormant (address-less); received frames fold into the bond's stack
    /// and it refuses direct address assignment.
    Member {
        /// The bond alias this NIC is enrolled in.
        bond: [u8; IF_NAME_LEN],
    },
}

/// Frames produced for one or more interfaces, each batch tagged by the
/// interface alias that emitted it, for the caller to queue onto that
/// interface's TX ring. The one definition every egress helper
/// ([`Netstack::originate`], the multicast join/leave paths) returns.
pub type FrameBatch = Vec<([u8; IF_NAME_LEN], Vec<TxFrame>)>;

/// The rename [`Netstack::apply_interface_config`] performed:
/// `Some((old_name, new_name))` when the matched interface was renamed to
/// its admin alias, else `None`. The service layer uses it to retarget the
/// interface's bound driver channel (keyed by name).
pub type IfaceRename = Option<([u8; IF_NAME_LEN], [u8; IF_NAME_LEN])>;

/// Mirror `iface`'s engine group memberships into its device's group filter
/// when they have changed since the last successful push.
///
/// A device that does no group filtering needs nothing (and is never called,
/// so an unfiltered channel costs no IPC per pump). Otherwise the set is
/// rebuilt and pushed only when the engine's multicast revision has moved,
/// so a steady interface pays one integer comparison per pump.
///
/// Returns the number of addresses the device refused, when it refused: the
/// caller audits that, because the groups that did not fit are genuinely no
/// longer delivered. A refusal deliberately does **not** record the revision
/// as pushed, so the next pump retries — a set that shrinks back within the
/// device's slots recovers on its own.
/// Publish `policy` to this channel's device, when it has changed.
///
/// Compared against the last published value rather than tracked by a
/// revision counter: the policy is a small `Copy` value, the comparison is
/// far cheaper than the IPC it avoids, and there is no second piece of
/// state that could fall out of step with the addresses it describes.
fn push_rx_filter<F: FrameService>(channel: &mut Interface, policy: &RxFilterPolicy, fs: &mut F) {
    // Recorded on the *channel's* interface, not the stack's: a bond's two
    // members share one stack, so a record kept there would let the first
    // member pumped mark the policy pushed and the second never receive it.
    if channel.pushed_rx_filter.as_ref() == Some(policy) {
        return;
    }
    // A refusal leaves the recorded policy alone, so the next pump retries.
    // Until it lands the driver keeps its previous (wider) filter, which
    // can only cost work, never a frame.
    if fs.set_rx_filter(*policy).is_ok() {
        channel.pushed_rx_filter = Some(*policy);
    }
}

fn push_multicast<F: FrameService>(
    iface: &mut Interface,
    fs: &mut F,
    scratch: &mut Vec<MacAddress>,
) -> Option<usize> {
    if matches!(iface.facts.multicast_filter, McastFilter::Unfiltered) {
        return None;
    }
    let revision = iface.stack.multicast_revision();
    if iface.pushed_multicast == Some(revision) {
        return None;
    }
    iface.stack.multicast_macs(scratch);
    match fs.set_multicast_groups(scratch) {
        Ok(()) => {
            iface.pushed_multicast = Some(revision);
            None
        }
        Err(_) => Some(scratch.len()),
    }
}

/// What one [`Netstack::service_interface`] pump produced.
///
/// The engine events the pump routes to the socket layer, plus the
/// interface's live link state **if it changed** since the last pump.
/// The driver reports its link on every doorbell (a virtio config-change
/// interrupt woke the stack for exactly this), and a change is the sole
/// live source of a bond failover: the service layer feeds it to
/// [`Netstack::on_member_link_change`], which drives the bond and returns
/// the presence re-announcement to transmit.
pub struct ServiceOutcome {
    /// The typed engine events the pump reported.
    pub events: Vec<StackEvent>,
    /// The interface's new link state, `Some` only when it differs from
    /// the state recorded before this pump.
    pub link_change: Option<LinkState>,
    /// How many group addresses the device refused to admit, `Some` only
    /// when this pump tried to reprogram its filter and it would not fit.
    /// The service audits it; reception of the groups that did not fit is
    /// genuinely lost, so it is never silent.
    pub multicast_refused: Option<usize>,
}

/// What the caller already knows about a device before a pump runs.
///
/// A driver harvests received frames into the shared ring on its own device
/// interrupt and states what it saw in the notify that wakes the stack, so a
/// pump driven by one starts already knowing the link and whether the driver
/// is holding its completion source masked. That is what lets a pure receive
/// cost no doorbell at all. A timer- or admin-driven pump knows neither and
/// passes [`Default`].
#[derive(Copy, Clone, Debug, Default)]
pub struct ServiceHint {
    /// The link state the waking notify carried, if a notify woke this pump.
    pub link: Option<LinkState>,
    /// The driver masked its completion source. This pump must doorbell
    /// after draining even with nothing to transmit, or the device stays
    /// masked and receives nothing further.
    pub back_pressure: bool,
    /// The device's cumulative receive-pre-filter count the notify carried.
    /// A pure receive rings no doorbell, so this is the only report of it
    /// the stack ever sees on the interrupt path — without it
    /// `stats:net/<iface>/rx.filtered` would sit frozen at whatever the
    /// last transmit happened to observe.
    pub filtered: Option<u64>,
}

/// One managed interface: its admin-chosen alias, link kind, and the
/// per-interface dual-stack protocol engine.
pub struct Interface {
    name: [u8; IF_NAME_LEN],
    kind: NetIfKind,
    facts: DeviceFacts,
    stack: Stack,
    /// The tickless windowed-throughput meter (`stats:net/<iface>/…`).
    rates: RateMeter,
    /// This entry's link-aggregation role (plain, bond, or member).
    role: BondRole,
    /// The NIC's stable hardware location — the register-window base of
    /// the device manager's matched hardware-tree node — or `0` when none
    /// was resolved (a software bond, or a device with no register window).
    /// A `network.conf` `<iface>.match.node` binding selects this interface
    /// by it, independent of MAC or discovery order.
    node_location: u64,
    /// The statically configured recursive DNS servers for this interface
    /// (`<iface>.dns.servers`), in declared order — the last value the
    /// device manager delivered, empty when none. They join this
    /// interface's DHCP-learned servers in [`Netstack::resolver_servers`].
    static_dns: Vec<NetServerAddr>,
    /// The engine multicast revision last successfully programmed into the
    /// device's group filter, or [`None`] while nothing has been programmed.
    /// The pump reprograms only when the engine's revision moves off this.
    pushed_multicast: Option<u64>,
    /// The receive pre-filter policy last accepted by this interface's
    /// device, so an unchanged address set costs no IPC.
    pushed_rx_filter: Option<RxFilterPolicy>,
    /// This entry's *own device's* cumulative pre-filter count, as of the
    /// last report or notify seen. Stored rather than accumulated because
    /// the device counter is itself cumulative — a report this stack never
    /// asked for is not lost.
    ///
    /// Recorded on the channel, like [`Self::pushed_rx_filter`] and for the
    /// same reason: a bond's members share one stack, so a count kept on
    /// the stack target would be overwritten by whichever member was pumped
    /// last. A bond reports the sum of its members'.
    rx_filtered: u64,
}

impl Interface {
    /// The interface's admin-chosen alias, NUL-padded.
    #[must_use]
    pub fn name(&self) -> [u8; IF_NAME_LEN] {
        self.name
    }

    /// Borrow the protocol engine (read-only observers).
    #[must_use]
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// Borrow the protocol engine mutably (diagnostic senders).
    pub fn stack_mut(&mut self) -> &mut Stack {
        &mut self.stack
    }
}

/// A factory for per-interface RFC 8981 temporary-address randomness
/// sources (`net.ipv6.privacy`).
///
/// Entropy lives at the service seam: the `Run` glue injects a factory
/// backed by the kernel CSPRNG, host tests a deterministic one, and the
/// pure `lib/net` engine consults the source it is handed only while
/// privacy addresses are enabled. Each managed [`Stack`] is given a
/// fresh source drawn from this factory at construction.
pub type TempAddrFactory = Box<dyn FnMut() -> Box<dyn TempAddrSource>>;

/// A factory for per-interface DHCPv4 client randomness sources
/// (`<iface>.ipv4.method = dhcp`).
///
/// Like [`TempAddrFactory`], entropy lives at the service seam: the `Run`
/// glue injects a factory backed by the kernel CSPRNG, host tests a
/// deterministic one, and the pure `lib/net` engine only *calls* the
/// closure it is handed to draw the RFC 2131 transaction id and backoff
/// jitter. A fresh source is drawn each time an interface is (re-)configured
/// for DHCPv4 and handed to its [`Stack`] through [`Stack::enable_dhcp`].
pub type DhcpRngFactory = Box<dyn FnMut() -> Box<dyn FnMut() -> u32>>;

/// The service's interface table and the engine glue around it.
///
/// Grows on demand — an interface is added per discovered NIC, never
/// from a compile-time ceiling. Reply paging bounds what one IPC
/// answer carries; it never bounds how many interfaces exist.
pub struct Netstack {
    interfaces: Vec<Interface>,
    /// Stack-wide `net.*` policy (`plans/NETWORK.md` §6.2). Its safe
    /// default (both families on, SYN cookies `auto`) holds until an
    /// FS-capable component delivers the real `system.conf` policy over
    /// the `ApplyNetworkSettings` admin op; the delivered policy governs
    /// both interfaces added afterwards and, by re-application, every
    /// interface already present.
    settings: NetworkSettings,
    /// Reusable RX pump scratch, sized lazily to the widest ring
    /// slot — allocated once, never per pumped frame (hot path).
    scratch: Vec<u8>,
    /// Reusable engine output, passed to every [`Stack`] call so the
    /// pump reuses one set of frame/event buffers across frames instead
    /// of allocating a fresh output per call — the allocation-free hot
    /// path the engine's [`StackOutput`] contract provides.
    out: StackOutput,
    /// Injected source of RFC 8981 temporary-address randomness (see
    /// [`TempAddrFactory`]). Each managed [`Stack`] draws a fresh
    /// source from it at construction.
    temp_factory: TempAddrFactory,
    /// Injected source of DHCPv4 client randomness (see
    /// [`DhcpRngFactory`]). A fresh source is drawn each time an interface
    /// is configured for DHCPv4 and handed to its [`Stack`].
    dhcp_rng_factory: DhcpRngFactory,
    /// Reusable buffer for the group-address set pushed to a filtering
    /// device — allocated once, rebuilt only when an engine's multicast
    /// revision moves.
    mcast_scratch: Vec<MacAddress>,
    /// The local datagram ports last published to every managed engine, and
    /// the scratch the next candidate set is built into. Two buffers rather
    /// than one so the compare needs no allocation: on a change the pair is
    /// swapped.
    datagram_ports: Vec<u16>,
    datagram_ports_scratch: Vec<u16>,
    /// The key a bond's transmit flow hash is taken under. Injected like the
    /// randomness factories above, because a remote peer chooses the tuple
    /// being hashed and must not be able to predict which member it selects.
    flow_key: HashSeed,
}

impl Netstack {
    /// An empty table with the injected randomness factories (the service
    /// layer owns entropy): `temp_factory` for RFC 8981 temporary
    /// addresses (`net.ipv6.privacy`), `dhcp_rng_factory` for the RFC
    /// 2131 DHCPv4 client (`<iface>.ipv4.method = dhcp`), and `flow_key`
    /// for a bond's transmit flow hash.
    #[must_use]
    pub fn new(
        temp_factory: TempAddrFactory,
        dhcp_rng_factory: DhcpRngFactory,
        flow_key: HashSeed,
    ) -> Self {
        Self {
            interfaces: Vec::new(),
            settings: NetworkSettings::default(),
            scratch: Vec::new(),
            out: StackOutput::default(),
            temp_factory,
            dhcp_rng_factory,
            mcast_scratch: Vec::new(),
            datagram_ports: Vec::new(),
            datagram_ports_scratch: Vec::new(),
            flow_key,
        }
    }

    /// Number of managed interfaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.interfaces.len()
    }

    /// Whether no interface is managed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty()
    }

    /// Add a managed interface.
    ///
    /// `interface_id` is the injected 64-bit interface identifier the
    /// SLAAC engine forms addresses from and `ipv4_ident_seed` the
    /// CSPRNG-drawn first IPv4 identification value — both drawn by
    /// the caller (the service layer owns entropy, the engine stays
    /// pure).
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — an invalid alias or device facts the
    ///   engine refuses.
    /// * [`Errno::AlreadyExists`] — the alias is already bound.
    // Each parameter is an independent, caller-drawn fact about the new
    // interface (name, kind, device facts, the two engine seeds, the
    // hardware location, and the clock); bundling them into a throwaway
    // struct would only obscure the call sites, so the argument list is
    // deliberately flat.
    #[allow(clippy::too_many_arguments)]
    pub fn add_interface(
        &mut self,
        name: [u8; IF_NAME_LEN],
        kind: NetIfKind,
        facts: DeviceFacts,
        interface_id: [u8; 8],
        ipv4_ident_seed: u16,
        node_location: u64,
        now: Duration64,
    ) -> Result<(), Errno> {
        validate_if_name(&name)?;
        if self.find(name).is_some() {
            return Err(Errno::AlreadyExists);
        }
        // A new interface adopts the current stack-wide family policy at
        // construction: a disabled family forms no address (no link-local
        // for v6) and the engine answers nothing for it.
        let mut config = StackConfig::new(facts, interface_id, ipv4_ident_seed);
        config.ipv4_enabled = self.settings.ipv4_enabled;
        config.iface.ipv6_enabled = self.settings.ipv6_enabled;
        config.iface.privacy = self.settings.ipv6_privacy;
        let temp_source = (self.temp_factory)();
        let mut stack = Stack::new(&config, temp_source, now).map_err(|_| Errno::OutOfRange)?;
        // A new interface joins with the set already published, so a socket
        // bound before it appeared still receives broadcast on it.
        stack.set_datagram_ports(self.published_datagram_ports());
        self.interfaces.push(Interface {
            name,
            kind,
            facts,
            stack,
            rates: RateMeter::new(),
            role: BondRole::None,
            node_location,
            static_dns: Vec::new(),
            pushed_multicast: None,
            pushed_rx_filter: None,
            rx_filtered: 0,
        });
        Ok(())
    }

    fn find(&self, name: [u8; IF_NAME_LEN]) -> Option<usize> {
        self.interfaces.iter().position(|i| i.name == name)
    }

    /// The current stack-wide `net.*` policy.
    #[must_use]
    pub fn settings(&self) -> NetworkSettings {
        self.settings
    }

    /// Apply a stack-wide `net.*` policy (`ApplyNetworkSettings`).
    ///
    /// Stores the policy so interfaces added later adopt it, and
    /// re-applies the family switches to every interface already managed
    /// — enabling a family re-forms its auto-configured address
    /// (link-local for IPv6), disabling it flushes the family's addresses
    /// and routes so the interface answers nothing. Idempotent, so a
    /// redelivery of the same policy is a no-op. The SYN-cookie mode is
    /// read at `listen` time from [`Self::settings`], so it needs no
    /// per-interface re-application here.
    pub fn apply_settings(&mut self, settings: NetworkSettings, now: Duration64) {
        self.settings = settings;
        for interface in &mut self.interfaces {
            interface.stack.set_ipv4_enabled(settings.ipv4_enabled);
            interface.stack.set_ipv6_enabled(settings.ipv6_enabled, now);
            interface.stack.set_privacy(settings.ipv6_privacy, now);
        }
    }

    /// Apply one managed interface's declarative configuration
    /// (`network.conf`, `plans/NETWORK.md` N9b-3-1), delivered by the
    /// device manager over the [`NetInterfaceConfigMsg`] admin message.
    ///
    /// The interface is located by its **stable hardware identity**, in
    /// precedence order: a MAC selector matches the interface whose device
    /// MAC matches (netstack is the only holder of each interface's MAC,
    /// from the driver's facts); else a hardware-node selector
    /// ([`NetInterfaceConfigMsg::match_node`]) matches the interface whose
    /// recorded hardware location — the register-window base of the device
    /// manager's matched hardware-tree node — equals it (the
    /// `<iface>.match.node` binding, independent of MAC and discovery
    /// order); else the message matches an interface
    /// already bearing the alias. A matched interface is *renamed* to the
    /// admin-chosen alias. An interface not yet present is
    /// [`Errno::NotFound`] — the caller retries when the driver binds — and
    /// an alias already taken by a *different* interface is
    /// [`Errno::AlreadyExists`].
    ///
    /// The whole message is validated ([`NetInterfaceConfigMsg::validate`])
    /// **before** any state is touched, so a refusal leaves the interface
    /// untouched — the application is atomic per interface. Re-applying the
    /// same configuration is idempotent: a static address already assigned
    /// is a success, not a duplicate error.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — the message failed validation, or the
    ///   engine refused a validated field.
    /// * [`Errno::NotFound`] — no interface matched the selector.
    /// * [`Errno::AlreadyExists`] — the alias is bound to another
    ///   interface.
    ///
    /// On success returns the rename it performed, if any:
    /// `Some((old_name, new_name))` when the matched interface was renamed
    /// to its admin alias, else `None`. The caller (the service layer) uses
    /// it to retarget the interface's bound driver channel, whose stored
    /// name would otherwise still be the pre-rename one — leaving the
    /// renamed interface unpumpable (`service_interface` looks an interface
    /// up by name).
    pub fn apply_interface_config(
        &mut self,
        msg: &NetInterfaceConfigMsg,
        now: Duration64,
    ) -> Result<IfaceRename, Errno> {
        // Validate the whole message up front so the mutation below is
        // atomic: after this every engine call can only fail on a resource
        // limit the fresh config never reaches, so a partial apply is not
        // possible (fail closed, leave the interface untouched).
        msg.validate()?;
        // Locate the interface by its stable hardware identity, in
        // precedence order: an explicit MAC selector, else an explicit
        // hardware-node location (the register-window base the driver bind
        // recorded), else the alias itself. A node selector never matches
        // an interface with no resolved location (a `0` `node_location`),
        // and `msg.match_node` is always non-zero (the config rejects `0`).
        let index = if let Some(mac) = msg.match_mac {
            self.interfaces
                .iter()
                .position(|i| i.facts.mac.as_octets() == &mac)
                .ok_or(Errno::NotFound)?
        } else if let Some(node) = msg.match_node {
            self.interfaces
                .iter()
                .position(|i| i.node_location != 0 && i.node_location == node)
                .ok_or(Errno::NotFound)?
        } else {
            self.find(msg.alias).ok_or(Errno::NotFound)?
        };
        // A bond member owns no addresses: the bond does. Refuse a direct
        // address assignment (static v4/v6, DHCPv4, or SLAAC) to an
        // enrolled member (fail closed) — the member is a pure frame
        // conduit. A disabled family is harmless (a member's families are
        // already disabled).
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            let assigns_address =
                matches!(msg.ipv4, NetIpv4Config::Static { .. } | NetIpv4Config::Dhcp)
                    || matches!(
                        msg.ipv6,
                        NetIpv6Config::Static { .. } | NetIpv6Config::Slaac | NetIpv6Config::Dhcp
                    );
            if assigns_address {
                return Err(Errno::PermissionDenied);
            }
        }
        // Rename a MAC-matched interface to its admin-chosen alias, unless
        // the alias is already taken by a *different* interface (fail
        // closed rather than collide two aliases). Report the rename so the
        // caller can retarget the interface's bound driver channel (whose
        // stored name is the pre-rename one).
        let mut renamed = None;
        if self.interfaces[index].name != msg.alias {
            if let Some(other) = self.find(msg.alias) {
                if other != index {
                    return Err(Errno::AlreadyExists);
                }
            }
            let old = self.interfaces[index].name;
            self.interfaces[index].name = msg.alias;
            renamed = Some((old, msg.alias));
        }
        // A DHCPv4/DHCPv6 interface needs a fresh CSPRNG source for its
        // client, drawn before borrowing the interface's stack (the factory
        // is a sibling field of the interface table). `.0` feeds the DHCPv4
        // client, `.1` the DHCPv6 client; the non-selected method draws
        // `None`, and a re-applied DHCP config drops its draw unused.
        let dhcp_rng = (
            matches!(msg.ipv4, NetIpv4Config::Dhcp).then(|| (self.dhcp_rng_factory)()),
            matches!(msg.ipv6, NetIpv6Config::Dhcp).then(|| (self.dhcp_rng_factory)()),
        );
        // Record the interface's statically configured DNS servers (the
        // last delivered list wins, empty when none), so the active
        // resolver set reflects the current config. Members carry none
        // (`netconfig` forbids a member DNS key; the message is empty).
        self.interfaces[index].static_dns = msg.dns.as_slice().to_vec();
        let stack = &mut self.interfaces[index].stack;
        if msg.mtu != 0 {
            stack.set_mtu(msg.mtu);
        }
        match msg.ipv4 {
            NetIpv4Config::Disabled => {
                stack.disable_dhcp();
                stack.set_ipv4_enabled(false);
            }
            NetIpv4Config::Static {
                addr,
                prefix,
                gateway,
            } => {
                stack.disable_dhcp();
                stack.set_ipv4_enabled(true);
                stack
                    .set_ipv4_config(Ipv4Addr::from(addr), prefix, gateway.map(Ipv4Addr::from))
                    .map_err(|_| Errno::OutOfRange)?;
            }
            NetIpv4Config::Dhcp => {
                // Re-applying the same DHCP config is idempotent: keep the
                // running client (and its lease) rather than restart
                // acquisition. A fresh interface starts the client now.
                if !stack.dhcp_active() {
                    if let Some(rng) = dhcp_rng.0 {
                        stack.enable_dhcp(rng);
                    }
                }
            }
        }
        apply_ipv6_config(stack, msg.ipv6, dhcp_rng.1, now)?;
        Ok(renamed)
    }

    /// Borrow a managed interface by alias.
    #[must_use]
    pub fn interface(&self, name: [u8; IF_NAME_LEN]) -> Option<&Interface> {
        self.find(name).map(|i| &self.interfaces[i])
    }

    /// Borrow a managed interface mutably by alias.
    pub fn interface_mut(&mut self, name: [u8; IF_NAME_LEN]) -> Option<&mut Interface> {
        self.find(name).map(move |i| &mut self.interfaces[i])
    }

    /// The managed aliases, in table order.
    #[must_use]
    pub fn names(&self) -> Vec<[u8; IF_NAME_LEN]> {
        self.interfaces.iter().map(|i| i.name).collect()
    }

    /// Assign a static address to a named interface.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name`.
    /// * [`Errno::PermissionDenied`] — the interface is an enrolled bond
    ///   member (it owns no addresses; the bond does).
    /// * [`Errno::OutOfRange`] — the engine refused the address
    ///   (bad prefix, gateway off-subnet, table full).
    pub fn addr_add(
        &mut self,
        name: [u8; IF_NAME_LEN],
        family: NetAddrFamily,
        prefix: u8,
        addr: [u8; 16],
        now: Duration64,
    ) -> Result<(), Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            return Err(Errno::PermissionDenied);
        }
        let stack = &mut self.interfaces[index].stack;
        match family {
            NetAddrFamily::V4 => stack
                .set_ipv4_config(v4_of(addr), prefix, None)
                .map_err(|_| Errno::OutOfRange),
            NetAddrFamily::V6 => stack
                .add_ipv6_static(Ipv6Addr::from(addr), prefix, now)
                .map_err(|_| Errno::OutOfRange),
        }
    }

    /// Add a route through a named interface.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name`.
    /// * [`Errno::PermissionDenied`] — the interface is an enrolled bond
    ///   member (it owns no routes; the bond does).
    /// * [`Errno::OutOfRange`] — the engine refused the route.
    pub fn route_add(
        &mut self,
        name: [u8; IF_NAME_LEN],
        family: NetAddrFamily,
        prefix: u8,
        dest: [u8; 16],
        next_hop: Option<[u8; 16]>,
    ) -> Result<(), Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            return Err(Errno::PermissionDenied);
        }
        let stack = &mut self.interfaces[index].stack;
        match family {
            NetAddrFamily::V4 => stack
                .add_route_v4(v4_of(dest), prefix, next_hop.map(v4_of))
                .map_err(|_| Errno::OutOfRange),
            NetAddrFamily::V6 => stack
                .add_route_v6(Ipv6Addr::from(dest), prefix, next_hop.map(Ipv6Addr::from))
                .map_err(|_| Errno::OutOfRange),
        }
    }

    /// The whole table's live stack counters, one record per interface,
    /// from `offset` in table order.
    #[must_use]
    pub fn counters_records(&self, offset: u32, limit: u16) -> Vec<NetInterfaceCountersRecord> {
        self.interfaces
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| {
                let c = i.stack.counters();
                NetInterfaceCountersRecord {
                    name: i.name,
                    counters: NetCounters {
                        rx_frames: c.rx_frames,
                        rx_bytes: c.rx_bytes,
                        rx_dropped: c.rx_dropped,
                        tx_frames: c.tx_frames,
                        tx_bytes: c.tx_bytes,
                        icmp_errors_sent: c.icmp_errors_sent,
                        icmp_errors_suppressed: c.icmp_errors_suppressed,
                        reassembly_expired: c.reassembly_expired,
                        pending_dropped: c.pending_dropped,
                        // A driver statistic, not a stack one: the pre-filter
                        // shed these before the stack ever saw them.
                        rx_filtered: self.filtered_frames_of(i),
                    },
                }
            })
            .collect()
    }

    /// The pre-filter count to report for `iface`: its own device's, or for
    /// a bond the sum over its members' devices.
    ///
    /// A bond has no device of its own, and every other counter on its
    /// record already aggregates its members (they feed one stack), so a
    /// bond reporting one member's figure would be inconsistent with its own
    /// `rx_frames`. A member whose name no longer resolves contributes
    /// nothing rather than poisoning the total.
    fn filtered_frames_of(&self, iface: &Interface) -> u64 {
        let BondRole::Bond { members, .. } = &iface.role else {
            return iface.rx_filtered;
        };
        members
            .iter()
            .filter_map(|name| self.find(*name))
            .fold(0u64, |total, index| {
                total.saturating_add(self.interfaces[index].rx_filtered)
            })
    }

    /// The whole table's live throughput rates over `window`, one record
    /// per interface, from `offset` in table order.
    ///
    /// Reading also records a fresh counter snapshot, so repeated polling
    /// builds a usable window even on an interface the frame pump is not
    /// otherwise servicing. Each record carries the window that *actually*
    /// elapsed for that interface (a just-created or long-idle interface
    /// reports a shorter — possibly zero — window rather than a fabricated
    /// figure).
    pub fn rates_records(
        &mut self,
        offset: u32,
        limit: u16,
        window: Duration64,
        now: Duration64,
    ) -> Vec<NetInterfaceRatesRecord> {
        self.interfaces
            .iter_mut()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| {
                let current = rate_counters_of(&i.stack);
                i.rates.record(now, current);
                let rx_pps = i.rates.rate(now, current, window, RateSelector::RxPackets);
                let tx_pps = i.rates.rate(now, current, window, RateSelector::TxPackets);
                let rx_bps = i.rates.rate(now, current, window, RateSelector::RxBits);
                let tx_bps = i.rates.rate(now, current, window, RateSelector::TxBits);
                NetInterfaceRatesRecord {
                    name: i.name,
                    // Every selector shares one baseline, so one window.
                    window: rx_pps.window,
                    rx_pps: rx_pps.value,
                    rx_bps: rx_bps.value,
                    tx_pps: tx_pps.value,
                    tx_bps: tx_bps.value,
                }
            })
            .collect()
    }

    /// Originate a UDP datagram from every interface that can carry it,
    /// returning the frames each produced tagged by interface alias so the
    /// caller can queue them onto that interface's TX ring.
    ///
    /// Egress selection is deterministic and per-link: interfaces are
    /// tried in table order. A **unicast** destination is sent out the
    /// first interface whose engine accepts it (one link reaches the
    /// destination); a **multicast** group is a per-link concept, so it is
    /// sent out *every* interface that accepts it. An interface's engine
    /// parks the datagram on neighbour resolution when needed, emitting the
    /// resolution frames now and the datagram once the neighbour answers —
    /// exactly the unicast echo behaviour.
    ///
    /// # Errors
    ///
    /// * [`Errno::MessageTooLarge`] — the datagram is too large to fit or
    ///   fragment onto the path (an oversize IPv6 datagram is
    ///   source-fragmented) or overflows the length field, on an interface
    ///   that otherwise matched.
    /// * [`Errno::NetworkUnreachable`] — no interface has a route to the
    ///   destination, a usable source address, or an up link.
    /// * [`Errno::OutOfRange`] — the destination is not a legal datagram
    ///   destination (broadcast / unspecified).
    pub fn originate(
        &mut self,
        dest: IpAddr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<FrameBatch, Errno> {
        let multicast = match dest {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        };
        let mut batches: FrameBatch = Vec::new();
        // Remember the most specific refusal so a genuine "too large"
        // (actionable) is surfaced rather than masked as "unreachable".
        let mut deferred: Option<Errno> = None;
        let Self {
            interfaces,
            out,
            flow_key,
            ..
        } = self;
        let flow_key = *flow_key;
        for iface in interfaces.iter_mut() {
            match iface
                .stack
                .send_datagram(dest, source_port, destination_port, payload, now, out)
            {
                Ok(()) => {
                    let frames = core::mem::take(&mut out.frames);
                    // A bond tags its egress by the member the flow selects
                    // (so the caller routes it onto a real channel); a plain
                    // interface tags by its own alias. A bond with no
                    // eligible member drops the frames (fail closed).
                    let flow = flow_of(flow_key, dest, source_port, destination_port);
                    if let Some(tag) = egress_tag(&iface.role, iface.name, flow) {
                        batches.push((tag, frames));
                        if !multicast {
                            // One link carries a unicast datagram; stop.
                            break;
                        }
                    } else {
                        deferred = deferred.or(Some(Errno::NetworkUnreachable));
                    }
                }
                Err(SendError::TooLarge) => deferred = Some(Errno::MessageTooLarge),
                Err(SendError::NotUnicast) => return Err(Errno::OutOfRange),
                // No route / no source / link down on this interface: try
                // the next one, remembering the fail-closed default.
                Err(_) => deferred = deferred.or(Some(Errno::NetworkUnreachable)),
            }
        }
        if batches.is_empty() {
            return Err(deferred.unwrap_or(Errno::NetworkUnreachable));
        }
        Ok(batches)
    }

    /// Originate an ICMP/`ICMPv6` echo request to the unicast `dest`,
    /// returning the frames the first interface that can reach it produced
    /// tagged by its alias. An echo destination is always unicast, so —
    /// unlike [`originate`](Self::originate) — the first accepting interface
    /// carries it and the search stops there.
    ///
    /// # Errors
    ///
    /// * [`Errno::MessageTooLarge`] — the payload is too large to fit or
    ///   fragment onto the path (an oversize IPv6 request is
    ///   source-fragmented) on an interface that otherwise matched.
    /// * [`Errno::OutOfRange`] — the destination is not a legal echo target
    ///   (multicast / unspecified).
    /// * [`Errno::NetworkUnreachable`] — no interface has a route to the
    ///   destination, a usable source address, or an up link.
    pub fn originate_echo(
        &mut self,
        dest: IpAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        now: Duration64,
    ) -> Result<FrameBatch, Errno> {
        let mut deferred: Option<Errno> = None;
        let Self {
            interfaces,
            out,
            flow_key,
            ..
        } = self;
        let flow_key = *flow_key;
        for iface in interfaces.iter_mut() {
            match iface
                .stack
                .send_echo_request(dest, identifier, sequence, payload, now, out)
            {
                Ok(()) => {
                    let frames = core::mem::take(&mut out.frames);
                    let flow = flow_of(flow_key, dest, identifier, sequence);
                    if let Some(tag) = egress_tag(&iface.role, iface.name, flow) {
                        return Ok(alloc::vec![(tag, frames)]);
                    }
                    deferred = deferred.or(Some(Errno::NetworkUnreachable));
                }
                Err(SendError::TooLarge) => deferred = Some(Errno::MessageTooLarge),
                Err(SendError::NotUnicast) => return Err(Errno::OutOfRange),
                // No route / no source / link down on this interface: try
                // the next, remembering the fail-closed default.
                Err(_) => deferred = deferred.or(Some(Errno::NetworkUnreachable)),
            }
        }
        Err(deferred.unwrap_or(Errno::NetworkUnreachable))
    }

    /// Originate one TCP segment out the interface named `name`.
    ///
    /// A connected stream is bound to one egress interface for its life
    /// (chosen by [`egress_mss_for`](Self::egress_mss_for) at connect), so
    /// every later segment — data, ACKs, retransmits — is sent through this
    /// fixed link.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name` (its link is
    ///   gone; the caller drops the segment and the engine retransmits).
    /// * [`Errno::NetworkUnreachable`] / [`Errno::MessageTooLarge`] — the
    ///   engine refused the segment (no route/source, link down, or a
    ///   segment past the path MTU).
    #[allow(clippy::too_many_arguments)]
    pub fn send_tcp_on(
        &mut self,
        name: [u8; IF_NAME_LEN],
        dest: IpAddr,
        meta: &TcpSegmentMeta,
        payload: &[u8],
        gso_size: Option<u16>,
        ecn: Ecn,
        now: Duration64,
    ) -> Result<Vec<TxFrame>, Errno> {
        let index = self.find(name).ok_or(Errno::NotFound)?;
        let Self {
            interfaces, out, ..
        } = self;
        match interfaces[index]
            .stack
            .send_tcp(dest, meta, payload, gso_size, ecn, now, out)
        {
            Ok(()) => Ok(core::mem::take(&mut out.frames)),
            Err(SendError::TooLarge) => Err(Errno::MessageTooLarge),
            Err(_) => Err(Errno::NetworkUnreachable),
        }
    }

    /// The largest TCP super-segment payload a connection out the interface
    /// named `name` may batch for segmentation offload, or `0` when the
    /// interface is unknown or its device did not negotiate the offload
    /// (so the connection stays per-MSS). Seeds a new connection's
    /// [`TcpConfig::tso_max_payload`](tairix_net::tcp::conn::TcpConfig::tso_max_payload).
    #[must_use]
    pub fn tso_max_payload_on(&self, name: [u8; IF_NAME_LEN]) -> u16 {
        self.find(name)
            .map_or(0, |index| self.interfaces[index].stack.tso_max_payload())
    }

    /// Choose the egress interface for a new TCP connection to `dest` and
    /// return its alias together with the effective local maximum segment
    /// size ([`Stack::tcp_local_mss`]) for that interface and `dest`'s
    /// family: the first interface that can reach `dest`.
    ///
    /// The connection is bound to this interface for its life, and the MSS
    /// seeds its [`TcpConfig`](tairix_net::tcp::conn::TcpConfig) so every
    /// segment fits the link before it is built (RFC 6691) — an IPv6 link's
    /// 40-byte header is accounted here, not discovered as a dropped
    /// full-size segment.
    ///
    /// # Errors
    ///
    /// [`Errno::NetworkUnreachable`] — no interface has a route, a usable
    /// source address, and an up link to `dest`.
    pub fn egress_mss_for(
        &mut self,
        dest: IpAddr,
        now: Duration64,
    ) -> Result<([u8; IF_NAME_LEN], u16), Errno> {
        for iface in &mut self.interfaces {
            if let Some(mss) = iface.stack.tcp_local_mss(dest, now) {
                return Ok((iface.name, mss));
            }
        }
        Err(Errno::NetworkUnreachable)
    }

    /// Whether any managed interface owns the local address `addr` of
    /// `family` — the check a socket `bind` to a *specific* local address
    /// makes before accepting it (fail closed: an address no interface
    /// holds could never source a datagram).
    #[must_use]
    pub fn has_local_address(&self, family: NetAddrFamily, addr: [u8; 16]) -> bool {
        self.interfaces.iter().any(|iface| match family {
            NetAddrFamily::V4 => iface
                .stack
                .iface()
                .ipv4()
                .is_some_and(|(a, _)| a == v4_of(addr)),
            NetAddrFamily::V6 => iface
                .stack
                .iface()
                .ipv6_addresses()
                .iter()
                .any(|info| info.addr == Ipv6Addr::from(addr)),
        })
    }

    /// Join multicast `group` on every managed interface (multicast
    /// membership is a per-link property), returning the frames each
    /// interface emitted (the IGMP/MLD report) tagged by alias.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `group` is not a multicast group.
    /// * [`Errno::LimitExceeded`] — an interface's bounded membership
    ///   table is full (fail closed).
    pub fn join_multicast_all(
        &mut self,
        group: IpAddr,
        now: Duration64,
    ) -> Result<FrameBatch, Errno> {
        let mut batches = FrameBatch::new();
        let Self {
            interfaces, out, ..
        } = self;
        for iface in interfaces.iter_mut() {
            // A member owns no membership; the bond does. Skip members.
            if matches!(iface.role, BondRole::Member { .. }) {
                continue;
            }
            match iface.stack.join_multicast(group, now) {
                // A fresh join emits a membership report to announce it.
                Ok(true) => {
                    iface.stack.advance(now, out);
                    let frames = core::mem::take(&mut out.frames);
                    if let Some(tag) = egress_tag(&iface.role, iface.name, 0) {
                        batches.push((tag, frames));
                    }
                }
                // Already a member (a prior reference): no new report.
                Ok(false) => {}
                Err(tairix_net::stack::McastError::NotMulticast) => return Err(Errno::OutOfRange),
                Err(tairix_net::stack::McastError::CapacityExhausted) => {
                    return Err(Errno::LimitExceeded)
                }
            }
        }
        Ok(batches)
    }

    /// Publish the local datagram ports a broadcast datagram may be
    /// delivered to, to every managed engine.
    ///
    /// The socket table is the authority; each engine holds the set because
    /// the decision belongs on its receive path, and the driver's receive
    /// pre-filter learns it through the policy the pump pushes. The set is
    /// compared against the last published one rather than tracked by a
    /// revision counter, so no socket operation has to remember to bump
    /// anything — a set that did not change costs one slice comparison.
    pub fn publish_datagram_ports<I: Iterator<Item = u16>>(&mut self, ports: I) {
        self.datagram_ports_scratch.clear();
        self.datagram_ports_scratch.extend(ports);
        self.datagram_ports_scratch.sort_unstable();
        self.datagram_ports_scratch.dedup();
        if self.datagram_ports_scratch == self.datagram_ports {
            return;
        }
        core::mem::swap(&mut self.datagram_ports, &mut self.datagram_ports_scratch);
        for iface in &mut self.interfaces {
            iface.stack.set_datagram_ports(&self.datagram_ports);
        }
    }

    /// The datagram ports last published, so a freshly added interface
    /// starts from the same set as its siblings.
    fn published_datagram_ports(&self) -> &[u16] {
        &self.datagram_ports
    }

    /// Leave multicast `group` on every managed interface, returning the
    /// frames each interface emitted (an IGMP Leave when the last
    /// reference dropped) tagged by alias.
    pub fn leave_multicast_all(&mut self, group: IpAddr, now: Duration64) -> FrameBatch {
        let mut batches = FrameBatch::new();
        let Self {
            interfaces, out, ..
        } = self;
        for iface in interfaces.iter_mut() {
            if matches!(iface.role, BondRole::Member { .. }) {
                continue;
            }
            if iface.stack.leave_multicast(group, now) {
                iface.stack.advance(now, out);
                let frames = core::mem::take(&mut out.frames);
                if let Some(tag) = egress_tag(&iface.role, iface.name, 0) {
                    batches.push((tag, frames));
                }
            }
        }
        batches
    }

    /// The whole table's static facts, one record per interface, from
    /// `offset` in table order.
    #[must_use]
    pub fn facts_records(&self, offset: u32, limit: u16) -> Vec<NetInterfaceFactsRecord> {
        self.interfaces
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| NetInterfaceFactsRecord {
                name: i.name,
                kind: i.kind,
                mac: *i.facts.mac.as_octets(),
                mtu: i.facts.mtu,
                offloads: i.facts.offloads.bits(),
                rx_queues: i.facts.rx_queues,
            })
            .collect()
    }

    /// The whole table's live link/address state, one record per
    /// interface, from `offset` in table order.
    #[must_use]
    pub fn state_records(&self, offset: u32, limit: u16) -> Vec<NetInterfaceStateRecord> {
        self.interfaces
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|i| {
                let mut addrs = [NetInterfaceStateRecord::EMPTY_ADDR; NET_IF_MAX_ADDRS];
                // Bounded by NET_IF_MAX_ADDRS (8), so u8 holds it exactly.
                let mut count: u8 = 0;
                if let Some((addr, prefix)) = i.stack.iface().ipv4() {
                    addrs[usize::from(count)] = NetIfAddr {
                        family: NetAddrFamily::V4,
                        prefix,
                        state: NetAddrState::Preferred,
                        addr: v4_bytes(addr),
                    };
                    count += 1;
                }
                for info in i.stack.iface().ipv6_addresses() {
                    if usize::from(count) == NET_IF_MAX_ADDRS {
                        break;
                    }
                    addrs[usize::from(count)] = NetIfAddr {
                        family: NetAddrFamily::V6,
                        prefix: info.prefix_len,
                        state: if info.tentative {
                            NetAddrState::Tentative
                        } else if info.deprecated {
                            NetAddrState::Deprecated
                        } else {
                            NetAddrState::Preferred
                        },
                        addr: info.addr.octets(),
                    };
                    count += 1;
                }
                // A bond's link is up when any member is eligible; a plain
                // interface's link is its device's reported link.
                let link_up = match &i.role {
                    BondRole::Bond { engine, .. } => engine.is_up(),
                    _ => i.facts.link == LinkState::Up,
                };
                NetInterfaceStateRecord {
                    name: i.name,
                    link_up,
                    addr_count: count,
                    addrs,
                }
            })
            .collect()
    }

    /// The host's active recursive-resolver server set (`plans/DNS.md`
    /// DNS2): every managed interface's statically configured servers
    /// (`<iface>.dns.servers`) followed by its DHCP-learned servers,
    /// walked in table order, deduplicated, and bounded by
    /// [`MAX_RESOLVER_SERVERS`].
    ///
    /// This is the one source of truth the `ResolverServers` broker read
    /// serves — to the system-information `net_resolver_servers` query and
    /// to a userland resolver client alike, so the two can never disagree.
    /// The DHCP part is derived on demand from each interface's *current*
    /// lease(s) (`Stack::dhcp_dns_servers`), so it tracks acquisition and
    /// withdrawal exactly and needs no stored copy to drift; the static
    /// part is the last `network.conf` DNS list the device manager
    /// delivered. Static servers rank first as the admin's explicit choice.
    #[must_use]
    pub fn resolver_servers(&self) -> Vec<NetServerAddr> {
        let mut out: Vec<NetServerAddr> = Vec::new();
        let mut push = |record: NetServerAddr| {
            if out.len() < MAX_RESOLVER_SERVERS && !out.contains(&record) {
                out.push(record);
            }
        };
        for iface in &self.interfaces {
            // Statically configured servers first (the admin's explicit
            // choice), then the interface's DHCP-learned servers.
            for record in &iface.static_dns {
                push(*record);
            }
            for server in iface.stack.dhcp_dns_servers() {
                push(server_addr_of(server));
            }
        }
        out
    }

    /// The network time servers the host's DHCP client(s) learned, in table
    /// order, deduplicated, and bounded by [`MAX_TIME_SERVERS`].
    ///
    /// The one source of truth the `TimeServers` broker read serves — to the
    /// system-information `net_time_servers` query and so to the clock
    /// service (`plans/TIMESYNC.md` §3). Derived on demand from each
    /// interface's *current* lease, so it tracks acquisition and withdrawal
    /// exactly and holds no stored copy to drift.
    ///
    /// Unlike the resolver set this one has no static tier: a statically
    /// chosen time server is the clock service's own configuration, which
    /// outranks what the network offers rather than joining it — so mixing
    /// the two here would destroy the distinction the service needs.
    #[must_use]
    pub fn time_servers(&self) -> Vec<NetServerAddr> {
        let mut out: Vec<NetServerAddr> = Vec::new();
        for iface in &self.interfaces {
            for server in iface.stack.dhcp_ntp_servers() {
                let record = server_addr_of(server);
                if out.len() < MAX_TIME_SERVERS && !out.contains(&record) {
                    out.push(record);
                }
            }
        }
        out
    }

    /// Pump one interface's frames through the frame service `fs` once:
    /// queue the engine's due output into the TX ring, drain every received
    /// frame back through the engine (whose replies are queued and flushed
    /// in the same pass), and doorbell the device when it has work only it
    /// can do.
    ///
    /// The pump is written once against the [`FrameService`] seam, so it
    /// drives an in-process [`Net`](tairix_abi::driver::net::Net) device
    /// ([`LocalFrameService`](crate::LocalFrameService)) and a cross-process
    /// driver ([`NetChannelClient`](crate::NetChannelClient)) identically:
    /// the service owns the frame region and each doorbell is either a direct
    /// `Net::service` or an `ipc_call` to the driver process.
    ///
    /// # The doorbell is not unconditional
    ///
    /// A cross-process doorbell is two process switches, and a *received*
    /// frame needs none of them: the driver harvested it into the shared ring
    /// on its own device interrupt before waking this stack, and the ring's
    /// atomic counters are what make reading it here safe. So the doorbell is
    /// rung only when the device has work this side has created —
    /// something in the transmit ring — or when `hint` says the driver
    /// masked its completion source for back-pressure and needs releasing.
    /// An idle interface receiving background traffic therefore costs one
    /// wake and no calls.
    ///
    /// Returns the pump's [`ServiceOutcome`]: the typed [`StackEvent`]s the
    /// engine reported, plus the interface's live link state if it changed
    /// since the last pump.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no interface bears `name`.
    /// * [`Errno::DeviceFault`] — the driver failed.
    /// * [`Errno::BadMagic`] — the ring state is corrupt.
    pub fn service_interface<F: FrameService>(
        &mut self,
        name: [u8; IF_NAME_LEN],
        fs: &mut F,
        now: Duration64,
        hint: ServiceHint,
    ) -> Result<ServiceOutcome, Errno> {
        let channel_index = self.find(name).ok_or(Errno::NotFound)?;
        // The link recorded before this pump: the value a no-doorbell pass
        // reports, so an unchanged link stays unchanged.
        let last_link = self.interfaces[channel_index].facts.link;
        // A member NIC has no stack of its own: its frames flow through the
        // bond's stack (the bond owns the addresses/routes). Its replies
        // are queued back onto this member's ring, which is correct for the
        // transmit member — the member a peer reaches the bond on. A plain
        // interface targets its own stack.
        let index = self.stack_target_index(channel_index);
        let geometry = fs.geometry();
        let class = fs.class();
        // Size the reusable scratch to the receive slot capacity once (the
        // scratch only ever holds a received frame; the larger transmit
        // capacity is the driver's staging concern).
        let slot_capacity = geometry.rx_slot_capacity() as usize;
        if self.scratch.len() < slot_capacity {
            self.scratch.resize(slot_capacity, 0);
        }
        // Split borrow: the pump reads `scratch` while it drives one
        // interface's engine. `fs` is a distinct object, borrowed only
        // while a ring view is bound over its region (never across a
        // doorbell).
        let Self {
            interfaces,
            scratch,
            out,
            settings: _,
            temp_factory: _,
            dhcp_rng_factory: _,
            mcast_scratch,
            datagram_ports: _,
            datagram_ports_scratch: _,
            flow_key: _,
        } = self;
        let iface = &mut interfaces[index];
        let mut events = Vec::new();

        // Timer-due engine output first (retransmits, DAD probes, RS),
        // queued into the TX ring bound over the service's own region.
        iface.stack.advance(now, out);
        events.append(&mut out.events);
        // Mirror the engine's group memberships into a filtering device
        // *before* the doorbell, so a group this advance joined (a fresh
        // address's solicited-node group, whose DAD probe the same advance
        // just queued) is admitted before any answer to it could arrive.
        let multicast_refused = push_multicast(iface, fs, mcast_scratch);
        // Keep the device's receive pre-filter in step with the addresses
        // this advance may have assigned, before any answer to them could
        // arrive. The addresses are the stack target's; the device is this
        // channel's, and so is the record of what it was last sent.
        let rx_policy = iface.stack.rx_filter_policy();
        push_rx_filter(&mut interfaces[channel_index], &rx_policy, fs);
        let iface = &mut interfaces[index];
        // The device's cumulative pre-filter count: whatever the waking
        // notify carried, superseded by any doorbell report this pump gets.
        // Monotonic, so the latest observation is always the right one.
        let mut filtered = hint.filtered;
        // A driver holding its completion source masked must be released
        // whatever else this pump finds, and an in-process device is only
        // ever run by the doorbell itself.
        let mut doorbell = hint.back_pressure || fs.receive_needs_doorbell();
        {
            let mut rings = FrameRings::bind(fs.region_mut(), geometry, class)?;
            queue_frames(&mut rings, &out.frames);
            // Anything in the transmit ring is work only the device can do.
            // A ring whose counters will not read is reported as needing the
            // doorbell, so the driver surfaces the fault as a typed reply
            // instead of this pump swallowing it.
            doorbell |= !rings.tx.is_empty().unwrap_or(false);
        }
        // The driver stamps its live link on every service report; keep the
        // latest across both doorbells of this pump. Without a doorbell the
        // link is what the waking notify stated, or the last recorded value.
        let mut reported_link = if doorbell {
            let report = fs.service()?;
            filtered = Some(report.filtered);
            report.link
        } else {
            hint.link.unwrap_or(last_link)
        };

        // Feed delivered frames through the engine; its replies join
        // the TX ring. A multiqueue device (`plans/NETWORK.md` N7c-2)
        // steers received frames across several receive rings, so drain
        // every one — the engine is a single stack, so all queues feed it
        // and its replies share the one transmit ring. Bounded by each
        // ring's slot count per pass, so a hostile flood cannot pin this
        // loop.
        let mut replied = false;
        {
            let mut rings = FrameRings::bind(fs.region_mut(), geometry, class)?;
            for q in 0..rings.rx_queues() {
                loop {
                    let mut offload = FrameOffload::None;
                    let popped = rings.rx_ring(q)?.pop_with(&mut offload, scratch);
                    match popped {
                        Ok(Some(len)) => {
                            // Resolve the device's per-frame checksum offload:
                            // complete a partial checksum in place, or report a
                            // device-validated one so the engine can skip the
                            // redundant fold. A bogus offset fails closed to the
                            // software path (the engine then drops the frame).
                            let rx = resolve_rx_offload(offload, &mut scratch[..len]);
                            iface.stack.on_frame_meta(&scratch[..len], rx, now, out);
                            events.append(&mut out.events);
                            replied |= !out.frames.is_empty();
                            queue_frames(&mut rings, &out.frames);
                        }
                        Ok(None) => break,
                        // A corrupt slot was consumed; skip it and go on.
                        Err(Errno::LengthOutOfRange) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
        }
        if replied {
            let report = fs.service()?;
            filtered = Some(report.filtered);
            reported_link = report.link;
        }
        // Snapshot the post-pump counters for the throughput meter. Cheap
        // and self-throttling: the meter drops a sample taken within its
        // sampling gap of the last.
        iface.rates.record(now, rate_counters_of(&iface.stack));
        // `iface`'s borrow has ended (its last use is above), so re-indexing
        // the serviced channel entry is sound. Both of the facts below are
        // the *channel's*, not the stack target's: the pre-filter count
        // belongs to the device that shed the frames, and a link change on
        // the serviced NIC is what a bond failover keys on.
        let channel = &mut interfaces[channel_index];
        if let Some(filtered) = filtered {
            channel.rx_filtered = filtered;
        }
        let link_change = (reported_link != channel.facts.link).then_some(reported_link);
        Ok(ServiceOutcome {
            events,
            link_change,
            multicast_refused,
        })
    }

    /// The earliest engine deadline across every interface, if any —
    /// the one-shot timer the event loop arms. Folds each interface's
    /// protocol-engine deadline and, for a bond, its failover health
    /// monitor's next admission deadline (tickless: `None` when no member
    /// is awaiting readmission).
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        self.interfaces
            .iter()
            .flat_map(|i| {
                let bond = match &i.role {
                    BondRole::Bond { engine, .. } => engine.next_deadline(),
                    _ => None,
                };
                [i.stack.next_deadline(), bond]
            })
            .flatten()
            .min_by_key(|d| (d.secs(), d.subsec_nanos()))
    }
}

/// One member's live health, for the `state:net/<bond>/…` observability
/// read: the member alias, whether its link is up, and whether it is
/// currently eligible to carry traffic (admitted past the anti-flap
/// up-delay).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BondMemberHealth {
    /// The member interface's admin-chosen alias, NUL-padded.
    pub member: [u8; IF_NAME_LEN],
    /// Whether the member's link is currently up.
    pub link_up: bool,
    /// Whether the member is eligible to carry traffic now.
    pub eligible: bool,
}

// --- Link aggregation (bond) composition --------------------------------
impl Netstack {
    /// Map the pumped channel's interface index onto the stack that
    /// processes its frames: a member's frames flow through its bond's
    /// stack; every other interface uses its own.
    fn stack_target_index(&self, channel_index: usize) -> usize {
        if let BondRole::Member { bond } = self.interfaces[channel_index].role {
            if let Some(bond_index) = self.find(bond) {
                return bond_index;
            }
        }
        channel_index
    }

    /// Map the wire bond mode onto the engine policy.
    fn engine_mode(mode: NetBondMode) -> BondMode {
        match mode {
            NetBondMode::ActiveBackup => BondMode::ActiveBackup,
            NetBondMode::Balance => BondMode::Balance,
        }
    }

    /// Compose (or reconfigure) a bond interface from a
    /// [`NetBondConfigMsg`] (`plans/NETWORK.md` §6.3), delivered by the
    /// device manager over the admin endpoint.
    ///
    /// A bond is a virtual interface that owns the addresses, routes, and
    /// neighbour cache (applied separately by a [`NetInterfaceConfigMsg`]
    /// naming the bond); its members are the physical NICs that carry its
    /// frames but hold no addresses. The bond inherits the first declared
    /// member's device identity (MAC/MTU) — Linux's default bond-MAC
    /// policy — kept stable for the bond's life so a peer's ARP/ND cache
    /// survives failover.
    ///
    /// Every declared member must already be present (its driver bound and
    /// the interface renamed to the member alias); an absent member yields
    /// [`Errno::NotFound`] so the caller retries when the driver binds
    /// (the [`Self::apply_interface_config`] contract). Re-applying is
    /// idempotent and reconciles the running bond (mode, primary, monitor
    /// interval, and membership) in place — the runtime-reload path.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — the message failed validation or the
    ///   engine refused the bond's device facts.
    /// * [`Errno::NotFound`] — a declared member is not present yet.
    /// * [`Errno::AlreadyExists`] — the bond alias names a non-bond
    ///   interface, or a declared member is a bond or already enrolled in
    ///   a *different* bond.
    pub fn apply_bond_config(
        &mut self,
        msg: &NetBondConfigMsg,
        now: Duration64,
    ) -> Result<(), Errno> {
        msg.validate()?;
        // Every declared member must be present and free to enrol. Checked
        // up front so the composition below is atomic (fail closed).
        for member in msg.members() {
            let idx = self.find(*member).ok_or(Errno::NotFound)?;
            if matches!(self.interfaces[idx].kind, NetIfKind::Bond) {
                return Err(Errno::AlreadyExists);
            }
            match self.interfaces[idx].role {
                BondRole::None => {}
                BondRole::Member { bond } if bond == msg.alias => {}
                // Enrolled in another bond, or itself a bond: refuse.
                _ => return Err(Errno::AlreadyExists),
            }
        }
        let mode = Self::engine_mode(msg.mode);
        let primary = msg.primary.map(member_id);

        let bond_index = if let Some(idx) = self.find(msg.alias) {
            if !matches!(self.interfaces[idx].role, BondRole::Bond { .. }) {
                // The alias is taken by a non-bond interface.
                return Err(Errno::AlreadyExists);
            }
            idx
        } else {
            let first = self.find(msg.members()[0]).ok_or(Errno::NotFound)?;
            self.create_bond_interface(msg.alias, mode, msg.monitor_interval, primary, first, now)?;
            self.find(msg.alias).ok_or(Errno::NotFound)?
        };
        self.reconcile_bond(
            bond_index,
            mode,
            msg.monitor_interval,
            primary,
            msg.members(),
            now,
        );
        Ok(())
    }

    /// Create the bond virtual interface, its own [`Stack`] built from the
    /// first member's device identity (MAC/MTU) with **no** offloads (the
    /// bond negotiates none of its own — a member's offloads are the
    /// member's), and an empty [`Bond`] engine seeded with the policy.
    fn create_bond_interface(
        &mut self,
        alias: [u8; IF_NAME_LEN],
        mode: BondMode,
        monitor_interval: Duration64,
        primary: Option<MemberId>,
        first_member_index: usize,
        now: Duration64,
    ) -> Result<(), Errno> {
        let mut facts = self.interfaces[first_member_index].facts;
        // The virtual bond negotiates no offloads of its own; frames it
        // hands a member are plain (the member checksums/segments in its
        // own right where its config negotiated it).
        facts.offloads = NetOffloads::empty();
        let interface_id = eui64_interface_id(*facts.mac.as_octets());
        let octets = *facts.mac.as_octets();
        let ipv4_ident_seed = u16::from_le_bytes([octets[4], octets[5]]);
        let mut config = StackConfig::new(facts, interface_id, ipv4_ident_seed);
        config.ipv4_enabled = self.settings.ipv4_enabled;
        config.iface.ipv6_enabled = self.settings.ipv6_enabled;
        config.iface.privacy = self.settings.ipv6_privacy;
        let temp_source = (self.temp_factory)();
        let mut stack = Stack::new(&config, temp_source, now).map_err(|_| Errno::OutOfRange)?;
        // The bond has no admitted member yet, so its aggregate link is
        // down until the failover monitor admits one.
        stack.set_link(LinkState::Down);
        let engine = Bond::new(&BondConfig {
            mode,
            monitor_interval,
            primary,
        });
        self.interfaces.push(Interface {
            name: alias,
            kind: NetIfKind::Bond,
            facts,
            stack,
            rates: RateMeter::new(),
            role: BondRole::Bond {
                engine,
                members: Vec::new(),
            },
            // A bond is composed in software; it has no hardware location,
            // so it can never be selected by a `match.node` binding.
            node_location: 0,
            // Populated when the bond's own interface config is applied.
            static_dns: Vec::new(),
            pushed_multicast: None,
            pushed_rx_filter: None,
            rx_filtered: 0,
        });
        Ok(())
    }

    /// Reconcile a bond's engine policy and membership to the declared set
    /// (the create path starts from an empty membership; the reload path
    /// diffs the running one). Enrols newly-declared members (disabling
    /// their own stacks so they hold no addresses) and releases members no
    /// longer declared (restoring them to plain interfaces).
    fn reconcile_bond(
        &mut self,
        bond_index: usize,
        mode: BondMode,
        monitor_interval: Duration64,
        primary: Option<MemberId>,
        declared: &[[u8; IF_NAME_LEN]],
        now: Duration64,
    ) {
        // Snapshot the currently-enrolled members to diff against.
        let current: Vec<[u8; IF_NAME_LEN]> = match &self.interfaces[bond_index].role {
            BondRole::Bond { members, .. } => members.clone(),
            _ => return,
        };
        let bond_alias = self.interfaces[bond_index].name;

        // Release members no longer declared: leave the engine and become
        // plain interfaces again (re-adopting the stack-wide family policy).
        for member in &current {
            if !declared.contains(member) {
                if let BondRole::Bond { engine, .. } = &mut self.interfaces[bond_index].role {
                    let _ = engine.remove_member(member_id(*member));
                }
                if let Some(idx) = self.find(*member) {
                    self.interfaces[idx].role = BondRole::None;
                    self.reenable_member_stack(idx, now);
                }
            }
        }

        // Reassert the engine policy (idempotent).
        if let BondRole::Bond { engine, .. } = &mut self.interfaces[bond_index].role {
            engine.set_mode(mode);
            engine.set_monitor_interval(monitor_interval);
            engine.set_primary(primary);
        }

        // Enrol newly-declared members: disable the member's own stack (it
        // owns no addresses) and add it to the engine with its link state.
        for member in declared {
            if !current.contains(member) {
                let Some(idx) = self.find(*member) else {
                    continue;
                };
                self.disable_member_stack(idx, now);
                self.interfaces[idx].role = BondRole::Member { bond: bond_alias };
                let link = self.interfaces[idx].facts.link;
                if let BondRole::Bond { engine, .. } = &mut self.interfaces[bond_index].role {
                    if engine.add_member(member_id(*member)).is_ok() {
                        let _ = engine.set_member_link(member_id(*member), link, now);
                    }
                }
            }
        }

        // Record the declared membership (in configured order) and sync the
        // bond stack's link to the engine's aggregate up-state.
        if let BondRole::Bond { members, .. } = &mut self.interfaces[bond_index].role {
            *members = declared.to_vec();
        }
        self.sync_bond_link(bond_index);
    }

    /// Disable a member interface's own protocol stack so it forms and
    /// holds no addresses — a member is a pure frame conduit for its bond.
    fn disable_member_stack(&mut self, index: usize, now: Duration64) {
        let stack = &mut self.interfaces[index].stack;
        stack.set_ipv4_enabled(false);
        stack.set_ipv6_enabled(false, now);
    }

    /// Restore a released member to a plain interface: re-adopt the
    /// stack-wide family policy so it can once again form its own
    /// addresses.
    fn reenable_member_stack(&mut self, index: usize, now: Duration64) {
        let settings = self.settings;
        let stack = &mut self.interfaces[index].stack;
        stack.set_ipv4_enabled(settings.ipv4_enabled);
        stack.set_ipv6_enabled(settings.ipv6_enabled, now);
    }

    /// Report a member NIC's link-state change to its bond and act on the
    /// resulting transmit-path events, returning any gratuitous
    /// ARP/unsolicited-NA frames tagged by the newly-selected member (for
    /// the caller to transmit). A link report for a NIC that is not an
    /// enrolled member is ignored (no change).
    pub fn set_member_link(
        &mut self,
        member: [u8; IF_NAME_LEN],
        link: LinkState,
        now: Duration64,
    ) -> FrameBatch {
        let Some(member_index) = self.find(member) else {
            return FrameBatch::new();
        };
        let BondRole::Member { bond } = self.interfaces[member_index].role else {
            return FrameBatch::new();
        };
        // Track the member's own link for observability.
        self.interfaces[member_index].facts.link = link;
        let Some(bond_index) = self.find(bond) else {
            return FrameBatch::new();
        };
        let events = match &mut self.interfaces[bond_index].role {
            BondRole::Bond { engine, .. } => engine.set_member_link(member_id(member), link, now),
            _ => Vec::new(),
        };
        self.apply_bond_events(bond_index, &events, now)
    }

    /// Apply a live link-state change the service pump observed on a NIC's
    /// driver report (a virtio config-change interrupt: a member unplugged,
    /// a carrier lost or regained), returning any gratuitous presence
    /// announcement the resulting bond path change requires (tagged by the
    /// newly-selected member) for the caller to transmit.
    ///
    /// For a bond **member** this is the sole live source of a failover: it
    /// reports the member's new link to the bond engine (via
    /// [`set_member_link`](Self::set_member_link)), which fails the transmit
    /// path over immediately on a down link and readmits a recovered member
    /// after its anti-flap up-delay. For a **plain** interface it records
    /// the link on the interface's own stack so egress selection stops
    /// choosing a down link (no announcement to transmit). An unknown
    /// interface is ignored.
    pub fn on_member_link_change(
        &mut self,
        name: [u8; IF_NAME_LEN],
        link: LinkState,
        now: Duration64,
    ) -> FrameBatch {
        let Some(index) = self.find(name) else {
            return FrameBatch::new();
        };
        if matches!(self.interfaces[index].role, BondRole::Member { .. }) {
            // `set_member_link` records the member's link and drives the
            // bond's failover, returning the announcement frames.
            return self.set_member_link(name, link, now);
        }
        // A plain interface: record the link on its facts and its own stack
        // so a down link is no longer chosen for egress.
        self.interfaces[index].facts.link = link;
        self.interfaces[index].stack.set_link(link);
        FrameBatch::new()
    }

    /// Advance every bond's failover health monitor (admitting members
    /// past their anti-flap up-delay), returning any gratuitous
    /// announcements the resulting path changes require, tagged by the
    /// member each must go out. Folded into the service's timer sweep.
    pub fn advance_bonds(&mut self, now: Duration64) -> FrameBatch {
        let mut batch = FrameBatch::new();
        let bonds: Vec<usize> = (0..self.interfaces.len())
            .filter(|&i| matches!(self.interfaces[i].role, BondRole::Bond { .. }))
            .collect();
        for bond_index in bonds {
            let events = match &mut self.interfaces[bond_index].role {
                BondRole::Bond { engine, .. } => engine.advance(now),
                _ => Vec::new(),
            };
            batch.append(&mut self.apply_bond_events(bond_index, &events, now));
        }
        batch
    }

    /// Sync a bond's own stack link to the engine's aggregate up-state.
    fn sync_bond_link(&mut self, bond_index: usize) {
        let up = match &self.interfaces[bond_index].role {
            BondRole::Bond { engine, .. } => engine.is_up(),
            _ => return,
        };
        self.interfaces[bond_index].stack.set_link(if up {
            LinkState::Up
        } else {
            LinkState::Down
        });
    }

    /// Act on a bond's transmit-path events: keep the bond stack's link in
    /// sync, and on a [`BondEvent::PathChanged`] re-announce the bond's
    /// presence (gratuitous ARP / unsolicited NA) so peers relearn the
    /// path, returning those frames tagged by the newly-selected member.
    fn apply_bond_events(
        &mut self,
        bond_index: usize,
        events: &[BondEvent],
        now: Duration64,
    ) -> FrameBatch {
        let mut batch = FrameBatch::new();
        self.sync_bond_link(bond_index);
        let path_changed = events.iter().any(|e| matches!(e, BondEvent::PathChanged));
        if !path_changed {
            return batch;
        }
        // Emit the presence announcement on the member the flow-agnostic
        // selection now points at (the active member in active-backup).
        let member = match &self.interfaces[bond_index].role {
            BondRole::Bond { engine, .. } => engine.transmit_member(0),
            _ => None,
        };
        let Some(member) = member else {
            return batch;
        };
        let Self {
            interfaces, out, ..
        } = self;
        interfaces[bond_index].stack.announce_presence(out, now);
        let frames = core::mem::take(&mut out.frames);
        if !frames.is_empty() {
            batch.push((member, frames));
        }
        batch
    }

    /// Resolve the physical channel alias a logical interface transmits on
    /// for the given flow: a bond selects its member (active member in
    /// active-backup; flow-hashed in balance), a plain interface is itself,
    /// and a member — never addressed directly — resolves to nothing.
    /// `None` when the interface is unknown or the bond has no eligible
    /// member (fail closed).
    #[must_use]
    pub fn egress_member(&self, name: [u8; IF_NAME_LEN], flow: u64) -> Option<[u8; IF_NAME_LEN]> {
        let index = self.find(name)?;
        egress_tag(&self.interfaces[index].role, name, flow)
    }

    /// The live per-member health of the bond named `bond`
    /// (`state:net/<bond>/…`), in configured order, or `None` if `bond` is
    /// not a bond interface.
    #[must_use]
    pub fn bond_member_health(&self, bond: [u8; IF_NAME_LEN]) -> Option<Vec<BondMemberHealth>> {
        let index = self.find(bond)?;
        match &self.interfaces[index].role {
            BondRole::Bond { engine, members } => Some(
                members
                    .iter()
                    .map(|member| BondMemberHealth {
                        member: *member,
                        link_up: engine
                            .is_member_link_up(member_id(*member))
                            .unwrap_or(false),
                        eligible: engine
                            .is_member_eligible(member_id(*member))
                            .unwrap_or(false),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The bond's currently-active member (`state:net/<bond>/active-member`)
    /// in active-backup, or `None` in balance mode / when the bond is down /
    /// when `bond` is not a bond.
    #[must_use]
    pub fn bond_active_member(&self, bond: [u8; IF_NAME_LEN]) -> Option<[u8; IF_NAME_LEN]> {
        let index = self.find(bond)?;
        match &self.interfaces[index].role {
            BondRole::Bond { engine, .. } => engine.active_member(),
            _ => None,
        }
    }

    /// Every bond's members and their live health, one record per (bond,
    /// member) pair, flattened in interface-table order then configured
    /// member order, from the `offset`th pair and at most `limit` records
    /// (the [`NetstackRequest::BondMembers`](tairix_abi::net_ipc::NetstackRequest::BondMembers)
    /// page backing `info:net/<bond>/members`,
    /// `state:net/<bond>/active-member`, and per-member health).
    ///
    /// Only bond interfaces contribute; a plain interface or an enrolled
    /// member emits nothing. Each record marks whether the member is the
    /// bond's currently-active transmitting member (active-backup only) and
    /// carries its link/eligibility health from the engine.
    #[must_use]
    pub fn bond_member_records(&self, offset: u32, limit: u16) -> Vec<NetBondMemberRecord> {
        let mut records = Vec::new();
        let mut index: u32 = 0;
        for iface in &self.interfaces {
            let BondRole::Bond { engine, members } = &iface.role else {
                continue;
            };
            let active = engine.active_member();
            for member in members {
                if index < offset {
                    index += 1;
                    continue;
                }
                if records.len() >= limit as usize {
                    return records;
                }
                records.push(NetBondMemberRecord {
                    bond: iface.name,
                    member: *member,
                    active: active == Some(*member),
                    link_up: engine
                        .is_member_link_up(member_id(*member))
                        .unwrap_or(false),
                    eligible: engine
                        .is_member_eligible(member_id(*member))
                        .unwrap_or(false),
                });
                index += 1;
            }
        }
        records
    }
}
/// Map the engine's monotonic stack counters onto the four accumulators
/// the [`RateMeter`] takes throughput rates over.
fn rate_counters_of(stack: &Stack) -> RateCounters {
    let c = stack.counters();
    RateCounters {
        rx_packets: c.rx_frames,
        rx_bytes: c.rx_bytes,
        tx_packets: c.tx_frames,
        tx_bytes: c.tx_bytes,
    }
}

/// Resolve a received frame's per-frame offload metadata into the
/// [`RxMeta`] the engine consumes, completing a device-partial checksum
/// in place when required.
///
/// * [`FrameOffload::Validated`] — the device verified the transport
///   checksum; the engine may skip the software fold.
/// * [`FrameOffload::NeedsChecksum`] — the device delivered only the
///   partial (pseudo-header) sum; [`complete_partial_checksum`] finishes
///   it in place and the engine re-verifies it in software (belt and
///   braces: a mis-completed frame is dropped, never wrongly accepted).
/// * [`FrameOffload::None`], [`FrameOffload::TxChecksum`] /
///   [`FrameOffload::TxSegment`] (transmit-only descriptors a device must
///   never deliver on receive), or a bogus partial — the software path.
fn resolve_rx_offload(offload: FrameOffload, frame: &mut [u8]) -> RxMeta {
    match offload {
        FrameOffload::Validated => RxMeta::validated(),
        FrameOffload::NeedsChecksum {
            csum_start,
            csum_offset,
        } => {
            complete_partial_checksum(frame, usize::from(csum_start), usize::from(csum_offset));
            // Re-verify our completion in software rather than trust it.
            RxMeta::none()
        }
        // A transmit offload (checksum or segmentation) has no meaning on a
        // received frame; fall back to the software path (fail closed).
        FrameOffload::None | FrameOffload::TxChecksum { .. } | FrameOffload::TxSegment { .. } => {
            RxMeta::none()
        }
    }
}

/// Complete a device-partial transport checksum: fold the frame from
/// `csum_start` to the end (the field there holds the pseudo-header
/// partial sum) and store the result at `csum_start + csum_offset`.
///
/// Fails closed on out-of-range offsets — the frame is left with its
/// partial checksum, which the engine's software fold then rejects.
/// Returns whether the completion was applied.
fn complete_partial_checksum(frame: &mut [u8], csum_start: usize, csum_offset: usize) -> bool {
    let Some(field) = csum_start.checked_add(csum_offset) else {
        return false;
    };
    let Some(field_end) = field.checked_add(2) else {
        return false;
    };
    if csum_start >= frame.len() || field_end > frame.len() {
        return false;
    }
    let sum = internet_checksum(&frame[csum_start..]);
    frame[field..field_end].copy_from_slice(&sum.to_be_bytes());
    true
}

/// Map the engine's per-frame [`TxOffload`] onto the ring's
/// transport-neutral [`FrameOffload`], so a frame the engine asked the
/// device to checksum carries that request across the ring to the driver
/// (`plans/NETWORK.md` §2.3). A device that did not negotiate the offload
/// never receives one: the engine only attaches [`TxOffload::PartialChecksum`]
/// when the interface's `NetOffloads` advertised it.
pub(crate) fn frame_offload(offload: TxOffload) -> FrameOffload {
    match offload {
        TxOffload::None => FrameOffload::None,
        TxOffload::PartialChecksum {
            csum_start,
            csum_offset,
        } => FrameOffload::TxChecksum {
            csum_start,
            csum_offset,
        },
        TxOffload::TcpSegment {
            csum_start,
            csum_offset,
            gso_size,
            hdr_len,
            ipv6,
        } => FrameOffload::TxSegment {
            csum_start,
            csum_offset,
            gso_size,
            hdr_len,
            ipv6,
        },
    }
}

/// Queue engine output frames, dropping (never wedging on) overflow:
/// the engine's own retransmission machinery recovers a lost frame,
/// and its counters account the drop when the peer never answers. Each
/// frame carries its transmit offload across the ring to the driver.
fn queue_frames(rings: &mut FrameRings<'_>, frames: &[TxFrame]) {
    for frame in frames {
        if rings
            .tx
            .push_with(frame_offload(frame.offload), &frame.bytes)
            .is_err()
        {
            break;
        }
    }
}

/// Pre-queue an outbound frame batch onto a frame service's TX ring.
///
/// A socket `send`, multicast `join`/`leave`, or `close` returns the frames
/// the engine produced for a named interface (a [`FrameBatch`] entry); the
/// service layer stages them here so the next
/// [`service_interface`](Netstack::service_interface) doorbell transmits
/// them alongside the engine's timer-due output — one pump, one doorbell.
/// Overflow is dropped (the engine retransmits); the ring is never
/// wedged.
///
/// # Errors
///
/// [`Errno::BadMagic`] if the service's region does not hold a valid ring
/// header for its declared geometry.
pub fn queue_tx<F: FrameService>(fs: &mut F, frames: &[TxFrame]) -> Result<(), Errno> {
    let geometry = fs.geometry();
    let class = fs.class();
    let mut rings = FrameRings::bind(fs.region_mut(), geometry, class)?;
    queue_frames(&mut rings, frames);
    Ok(())
}

fn v4_of(bytes: [u8; 16]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

/// An interface alias as a bond [`MemberId`]. The two are the same
/// fixed-width name; this names the intent (a member is keyed in the bond
/// engine by its interface alias).
fn member_id(name: [u8; IF_NAME_LEN]) -> MemberId {
    name
}

/// Resolve the physical channel alias a logical interface transmits on for
/// the given flow: a bond selects its member (the active member in
/// active-backup, flow-hashed in balance), a plain interface is itself,
/// and a member — never addressed directly — resolves to nothing. `None`
/// when a bond has no eligible member (fail closed).
fn egress_tag(role: &BondRole, name: [u8; IF_NAME_LEN], flow: u64) -> Option<[u8; IF_NAME_LEN]> {
    match role {
        BondRole::Bond { engine, .. } => engine.transmit_member(flow),
        BondRole::Member { .. } => None,
        BondRole::None => Some(name),
    }
}

/// A deterministic transmit flow hash over a destination and its transport
/// ports, for bond balance-mode member selection (`plans/NETWORK.md`
/// §6.3). The source address is left empty — a destination plus ports keys
/// a flow to one member for its life, and active-backup ignores the hash.
fn flow_of(key: HashSeed, dest: IpAddr, port_a: u16, port_b: u16) -> u64 {
    match dest {
        IpAddr::V4(v4) => flow_hash(key, &[], &v4.octets(), port_a, port_b),
        IpAddr::V6(v6) => flow_hash(key, &[], &v6.octets(), port_a, port_b),
    }
}

/// Project a resolved [`IpAddr`] onto the ABI [`NetServerAddr`] wire
/// shape (family plus the sixteen address bytes, a V4 server using the
/// first four).
fn server_addr_of(addr: IpAddr) -> NetServerAddr {
    match addr {
        IpAddr::V4(a) => NetServerAddr {
            family: NetAddrFamily::V4,
            addr: v4_bytes(a),
        },
        IpAddr::V6(a) => NetServerAddr {
            family: NetAddrFamily::V6,
            addr: a.octets(),
        },
    }
}

fn v4_bytes(addr: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&addr.octets());
    out
}

/// Apply an interface's IPv6 addressing (`network.conf` `<iface>.ipv6.*`)
/// to its `stack`. Split out of [`Netstack::apply_interface_config`] so
/// each family's addressing reads as one unit. `dhcp6_rng` is the
/// pre-drawn CSPRNG source for a DHCPv6 interface (the factory is a sibling
/// field of the interface table, so the draw happens before the stack is
/// borrowed); it is consumed only when DHCPv6 starts and dropped otherwise.
///
/// # Errors
///
/// [`Errno::OutOfRange`] when the engine refuses a static address or its
/// gateway (fail closed).
fn apply_ipv6_config(
    stack: &mut Stack,
    ipv6: NetIpv6Config,
    dhcp6_rng: Option<Box<dyn FnMut() -> u32>>,
    now: Duration64,
) -> Result<(), Errno> {
    match ipv6 {
        NetIpv6Config::Disabled => {
            stack.disable_dhcp6();
            stack.set_ipv6_enabled(false, now);
        }
        NetIpv6Config::Slaac => {
            stack.disable_dhcp6();
            stack.set_ipv6_enabled(true, now);
        }
        NetIpv6Config::Static {
            addr,
            prefix,
            gateway,
        } => {
            stack.disable_dhcp6();
            stack.set_ipv6_enabled(true, now);
            match stack.add_ipv6_static(Ipv6Addr::from(addr), prefix, now) {
                // A re-applied identical address is not an error: this
                // apply is idempotent (config-vs-bind ordering aside).
                Ok(()) | Err(AddrError::Duplicate) => {}
                Err(_) => return Err(Errno::OutOfRange),
            }
            if let Some(gw) = gateway {
                stack
                    .add_route_v6(Ipv6Addr::UNSPECIFIED, 0, Some(Ipv6Addr::from(gw)))
                    .map_err(|_| Errno::OutOfRange)?;
            }
        }
        NetIpv6Config::Dhcp => {
            // Re-applying the same DHCPv6 config is idempotent: keep the
            // running client (and its lease) rather than restart
            // acquisition. A fresh interface starts the client now
            // (enabling IPv6 forms the link-local it rides on).
            if !stack.dhcp6_active() {
                if let Some(rng) = dhcp6_rng {
                    stack.enable_dhcp6(rng, now);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod offload_tests {
    use super::{complete_partial_checksum, resolve_rx_offload};
    use alloc::vec;
    use tairix_abi::driver::net_ring::FrameOffload;
    use tairix_net::addr::Ipv4Addr;
    use tairix_net::stack::RxMeta;
    use tairix_net::udp::{self, Pseudo, UdpDatagram, PROTOCOL_UDP};

    const SRC: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const DST: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 9);

    /// A UDP datagram whose checksum field holds only the pseudo-header
    /// partial sum — exactly what a `NEEDS_CSUM` device delivers — paired
    /// with the correct checksum the completion must reproduce.
    fn partial_udp_datagram() -> (vec::Vec<u8>, [u8; 2]) {
        let pseudo = Pseudo::V4 {
            source: SRC,
            destination: DST,
        };
        let payload = b"partial-checksum";
        let mut dg = vec![0u8; udp::UDP_HEADER_LEN + payload.len()];
        udp::write(pseudo, 4000, 53, payload, &mut dg).expect("write");
        let correct = [dg[6], dg[7]];
        // A CHECKSUM_PARTIAL sender seeds the field with the raw
        // ones-complement sum of the pseudo-header.
        let udp_len = u16::try_from(dg.len()).expect("fits");
        let partial = !pseudo.seed(PROTOCOL_UDP, udp_len).finish();
        dg[6..8].copy_from_slice(&partial.to_be_bytes());
        (dg, correct)
    }

    #[test]
    fn complete_partial_checksum_reproduces_the_transport_checksum() {
        let (mut dg, correct) = partial_udp_datagram();
        assert!(complete_partial_checksum(&mut dg, 0, 6));
        assert_eq!([dg[6], dg[7]], correct);
        // The completed datagram now verifies against the pseudo-header.
        assert!(UdpDatagram::parse(
            Pseudo::V4 {
                source: SRC,
                destination: DST,
            },
            &dg,
        )
        .is_some());
    }

    #[test]
    fn complete_partial_checksum_fails_closed_on_out_of_range_offsets() {
        let mut frame = [0u8; 16];
        let before = frame;
        // csum_start at/after the end.
        assert!(!complete_partial_checksum(&mut frame, 16, 0));
        // The checksum field would run past the end.
        assert!(!complete_partial_checksum(&mut frame, 15, 4));
        // A bogus partial leaves the frame untouched (fail closed).
        assert_eq!(frame, before);
    }

    #[test]
    fn resolve_rx_offload_maps_each_tag() {
        let mut frame = [0u8; 32];
        assert_eq!(
            resolve_rx_offload(FrameOffload::None, &mut frame),
            RxMeta::none()
        );
        assert_eq!(
            resolve_rx_offload(FrameOffload::Validated, &mut frame),
            RxMeta::validated()
        );
        // A NeedsChecksum completes the fold in place, then reports the
        // software path so the engine re-verifies our completion.
        let (mut dg, correct) = partial_udp_datagram();
        let rx = resolve_rx_offload(
            FrameOffload::NeedsChecksum {
                csum_start: 0,
                csum_offset: 6,
            },
            &mut dg,
        );
        assert_eq!(rx, RxMeta::none());
        assert_eq!([dg[6], dg[7]], correct);
    }
}
