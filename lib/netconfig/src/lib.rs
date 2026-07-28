//! The network interface-configuration store engine.
//!
//! TAIRiX keeps its administrator-settable per-interface network
//! configuration in one text document on the encrypted root volume,
//! [`CONFIG_PATH`](`/System/Settings/Network/network.conf`). This crate is
//! the **single definition** of that document: the per-interface line
//! grammar, the closed key registry, each key's typed value set, the
//! bounded fail-closed parser, and the canonical render. The `configure`
//! command app and the installer write the store through this engine and
//! the one reader — the `netstack` service, at start and on a typed
//! `CAP_NET_ADMIN` reload — reads it through the same engine, so the two
//! can never diverge.
//!
//! The store is parsed only **after** the operator's `Root filesystem
//! passphrase:` unlocks the encrypted root — it lives inside
//! `/System/Settings`, which does not exist before the mount — so an absent
//! store simply means "no managed interfaces beyond loopback"
//! ([`NetworkConfig::default`]), never an error.
//!
//! # Grammar
//!
//! The text is a sequence of lines. A `#` begins a comment that runs to the
//! end of the line; blank and comment-only lines are ignored. Every other
//! line is one setting: a key of the shape `<iface>.<suffix>`, whitespace,
//! and that key's value. The `<iface>` part is an admin-chosen interface
//! *alias* (`wan`, `lan0`) — a stable name bound to hardware by identity
//! (`<iface>.match.mac` / `<iface>.match.node`), never a discovery-order
//! name. The `<suffix>` is drawn from the closed [`IfaceKey`] registry.
//! Each `(iface, suffix)` pair may appear at most once.
//!
//! # Security
//!
//! The store text is **untrusted input** to every consumer: the parser is
//! bounded ([`MAX_CONFIG_LEN`], [`MAX_INTERFACES`], [`MAX_BOND_MEMBERS`]),
//! allocation-bounded, and fails closed ([`ConfigError`]) on anything it
//! does not fully understand — an unknown key, an out-of-set or malformed
//! value, a duplicate, an oversized document, or a semantically
//! inconsistent interface (a `bond.*` key on a non-bond, a static address
//! without the matching `static` method). A `netstack` that cannot fully
//! parse the store keeps its running configuration untouched rather than
//! guessing at a partial intent; the write path (`configure`) refuses the
//! edit outright. The engine itself performs no I/O and holds no authority:
//! reading and writing the file go through the secured VFS under the
//! caller's own kernel-attested identity, and the per-inode policy on
//! `/System/Settings` decides who may write.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The directory that holds the network-configuration store, inside the
/// writable `/System/Settings` subtree of the encrypted root volume.
pub const CONFIG_DIR: &str = "/System/Settings/Network";

/// The configuration store document the `configure` command and installer
/// write and the `netstack` service reads.
pub const CONFIG_PATH: &str = "/System/Settings/Network/network.conf";

/// Maximum length, in bytes, of a store text [`NetworkConfig::parse`] will
/// consider. A larger input is refused outright ([`ConfigError::TooLong`])
/// rather than scanned — the store is small, and an unboundedly large one
/// is a defect, not a workload.
pub const MAX_CONFIG_LEN: usize = 16 * 1024;

/// Maximum number of interfaces a single store may declare. A document
/// naming more is refused ([`ConfigError::TooManyInterfaces`]).
pub const MAX_INTERFACES: usize = 32;

/// Maximum number of member NICs a single bond may enrol
/// ([`ConfigError::TooManyMembers`] otherwise).
pub const MAX_BOND_MEMBERS: usize = tairix_abi::net_ipc::NET_BOND_MAX_MEMBERS;

/// Maximum number of recursive DNS servers a single interface may name in
/// its `<iface>.dns.servers` list. This is the one definition of that
/// bound, shared with the active-resolver-set ceiling the network stack
/// aggregates into ([`tairix_abi::net_ipc::MAX_RESOLVER_SERVERS`]), so the
/// per-interface static list and the host-wide set can never disagree. A
/// longer list is refused ([`ConfigError::TooManyDnsServers`]).
pub const MAX_DNS_SERVERS: usize = tairix_abi::net_ipc::MAX_RESOLVER_SERVERS;

/// Maximum length, in bytes, of an interface alias name. Matches the
/// familiar Unix `IFNAMSIZ - 1` bound; a longer name fails closed.
pub const MAX_IFACE_NAME_LEN: usize = 15;

/// Smallest MTU the store accepts: the IPv6 minimum link MTU (RFC 8200).
/// A dual-stack interface must carry IPv6, so no smaller MTU is meaningful.
pub const MIN_MTU: u16 = 1280;

/// Largest MTU the store accepts (the 16-bit IP total-length ceiling).
pub const MAX_MTU: u16 = u16::MAX;

/// The link kind an interface presents (`<iface>.kind`).
///
/// A closed set: future link kinds (VLAN, bridge, …) extend it in place
/// when they land with their machinery, never speculatively.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum IfaceKind {
    /// A physical NIC bound by identity — the default, and the kind an
    /// interface with no `kind` key implies.
    #[default]
    Ethernet,
    /// A bond: a virtual interface `netstack` composes over two or more
    /// member NICs for aggregation and failover (see the `bond.*` keys).
    Bond,
    /// The host loopback interface.
    Loopback,
}

impl IfaceKind {
    /// The canonical value spelling (`ethernet` / `bond` / `loopback`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ethernet => "ethernet",
            Self::Bond => "bond",
            Self::Loopback => "loopback",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (case-sensitive — one canonical spelling per value).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "ethernet" => Some(Self::Ethernet),
            "bond" => Some(Self::Bond),
            "loopback" => Some(Self::Loopback),
            _ => None,
        }
    }
}

/// How an interface obtains its IPv4 address (`<iface>.ipv4.method`).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Ipv4Method {
    /// No IPv4 on this interface — the default. It binds no v4 address and
    /// answers no v4 packets.
    #[default]
    Disabled,
    /// A statically configured address (`<iface>.ipv4.address`) and
    /// optional gateway (`<iface>.ipv4.gateway`).
    Static,
    /// A DHCPv4-leased address (RFC 2131): the stack runs a DHCP client on
    /// this interface and applies the leased address, mask, and default
    /// route. No static `<iface>.ipv4.address`/`gateway` is set.
    Dhcp,
}

impl Ipv4Method {
    /// The canonical value spelling (`disabled` / `static` / `dhcp`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Static => "static",
            Self::Dhcp => "dhcp",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "disabled" => Some(Self::Disabled),
            "static" => Some(Self::Static),
            "dhcp" => Some(Self::Dhcp),
            _ => None,
        }
    }
}

/// How an interface obtains its IPv6 address (`<iface>.ipv6.method`).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Ipv6Method {
    /// Stateless address autoconfiguration (RFC 4862) — the default for a
    /// dual-stack NIC: derive the link-local and any advertised prefix
    /// address from Router Advertisements.
    #[default]
    Slaac,
    /// A statically configured address (`<iface>.ipv6.address`) and
    /// optional gateway (`<iface>.ipv6.gateway`).
    Static,
    /// A stateful DHCPv6-leased address (RFC 8415): the stack runs a
    /// DHCPv6 client on this interface and applies the leased IA_NA
    /// address. The interface keeps its autoconfigured link-local (DHCPv6
    /// rides on it); no static `<iface>.ipv6.address`/`gateway` is set.
    Dhcp,
    /// No IPv6 on this interface: it binds no v6 address (not even the
    /// link-local) and answers no v6 packets.
    Disabled,
}

impl Ipv6Method {
    /// The canonical value spelling (`slaac` / `static` / `dhcp` /
    /// `disabled`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slaac => "slaac",
            Self::Static => "static",
            Self::Dhcp => "dhcp",
            Self::Disabled => "disabled",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "slaac" => Some(Self::Slaac),
            "static" => Some(Self::Static),
            "dhcp" => Some(Self::Dhcp),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// A bond's transmit policy (`<iface>.bond.mode`).
///
/// A closed set; LACP/802.3ad is a future in-place extension, not
/// speculated here.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BondMode {
    /// One transmitting member at a time, with ordered failover to the next
    /// healthy member (the default). A declared `bond.primary` makes it a
    /// deliberate failover interface.
    #[default]
    ActiveBackup,
    /// Flow-hashed transmit spread across healthy members: one flow stays
    /// on one member, so a TCP stream never reorders across links.
    Balance,
}

impl BondMode {
    /// The canonical value spelling (`active-backup` / `balance`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveBackup => "active-backup",
            Self::Balance => "balance",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "active-backup" => Some(Self::ActiveBackup),
            "balance" => Some(Self::Balance),
            _ => None,
        }
    }
}

/// A 48-bit Ethernet MAC address, used to bind an interface alias to a NIC
/// by its stable hardware identity (`<iface>.match.mac`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// Parse the canonical lower-case colon-separated hex spelling
    /// (`aa:bb:cc:dd:ee:ff`). Returns `None` for any other shape — wrong
    /// group count, a non-hex or non-two-digit group, or upper-case hex
    /// (the render is lower-case, so there is exactly one valid spelling).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let mut octets = [0u8; 6];
        let mut groups = value.split(':');
        for octet in &mut octets {
            let group = groups.next()?;
            let bytes = group.as_bytes();
            if bytes.len() != 2 {
                return None;
            }
            *octet = (hex_digit(bytes[0])? << 4) | hex_digit(bytes[1])?;
        }
        if groups.next().is_some() {
            return None;
        }
        Some(Self(octets))
    }

    /// Render the canonical lower-case colon-separated spelling.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(17);
        for (index, octet) in self.0.iter().enumerate() {
            if index != 0 {
                out.push(':');
            }
            out.push(hex_char(octet >> 4));
            out.push(hex_char(octet & 0x0F));
        }
        out
    }
}

/// Decode a single lower-case (or digit) ASCII hex byte to its 0..=15
/// value; `None` for anything else (upper-case included — one spelling).
const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Render a 0..=15 nibble as its lower-case ASCII hex character.
const fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

/// An IPv4 address with a routing prefix length (`a.b.c.d/prefix`,
/// `prefix` in `0..=32`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ipv4Cidr {
    /// The interface address.
    pub addr: Ipv4Addr,
    /// The on-link prefix length, in bits (`0..=32`).
    pub prefix: u8,
}

impl Ipv4Cidr {
    /// Parse the canonical `a.b.c.d/prefix` spelling. Returns `None` unless
    /// both halves are present, the address parses, and the prefix is a
    /// decimal in `0..=32`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (addr, prefix) = value.split_once('/')?;
        let addr: Ipv4Addr = addr.parse().ok()?;
        let prefix = parse_prefix(prefix, 32)?;
        Some(Self { addr, prefix })
    }

    /// Render the canonical `a.b.c.d/prefix` spelling.
    #[must_use]
    pub fn render(&self) -> String {
        render_cidr(&self.addr, self.prefix)
    }
}

/// An IPv6 address with a routing prefix length (`addr/prefix`, `prefix` in
/// `0..=128`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ipv6Cidr {
    /// The interface address.
    pub addr: Ipv6Addr,
    /// The on-link prefix length, in bits (`0..=128`).
    pub prefix: u8,
}

impl Ipv6Cidr {
    /// Parse the canonical `addr/prefix` spelling. Returns `None` unless
    /// both halves are present, the address parses, and the prefix is a
    /// decimal in `0..=128`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (addr, prefix) = value.split_once('/')?;
        let addr: Ipv6Addr = addr.parse().ok()?;
        let prefix = parse_prefix(prefix, 128)?;
        Some(Self { addr, prefix })
    }

    /// Render the canonical `addr/prefix` spelling.
    #[must_use]
    pub fn render(&self) -> String {
        render_cidr(&self.addr, self.prefix)
    }
}

/// Parse a decimal prefix length, rejecting anything that is not a bare
/// run of ASCII digits within `0..=max` (no sign, no whitespace, no
/// leading `+`, no overflow).
fn parse_prefix(text: &str, max: u8) -> Option<u8> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u8 = text.parse().ok()?;
    (value <= max).then_some(value)
}

/// Render an address and prefix as `addr/prefix` (shared by both families).
fn render_cidr(addr: &dyn fmt::Display, prefix: u8) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    // Writing to a `String` is infallible.
    let _ = write!(out, "{addr}/{prefix}");
    out
}

/// Smallest number of member NICs a bond must enrol. A bond exists to
/// aggregate or fail over across links, which is meaningless below two.
pub const MIN_BOND_MEMBERS: usize = 2;

/// Smallest bond member-health monitor interval, in milliseconds. A shorter
/// interval would busy the health probe without improving failover.
pub const MIN_MONITOR_INTERVAL_MS: u32 = 100;

/// Largest bond member-health monitor interval, in milliseconds.
pub const MAX_MONITOR_INTERVAL_MS: u32 = 60_000;

/// One key suffix of the closed per-interface configuration registry.
///
/// The full key on a line is `<iface>.<suffix>` where `<suffix>` is one of
/// these variants' [`name`](IfaceKey::name)s. Adding a key means adding a
/// variant here, its row in [`IfaceKey::ALL`], its field on
/// [`InterfaceConfig`], and its arms in the parser and render — the
/// compiler then forces every consumer to state what the new key means.
/// There is no free-form key namespace: an unknown suffix fails closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IfaceKey {
    /// `kind` — the interface's link kind ([`IfaceKind`]).
    Kind,
    /// `match.mac` — bind this alias to the NIC with this MAC.
    MatchMac,
    /// `match.node` — bind this alias to the NIC at this stable hardware
    /// location (its register-window base, in hex).
    MatchNode,
    /// `ipv4.method` — how the interface obtains its IPv4 address.
    Ipv4Method,
    /// `ipv4.address` — the static IPv4 address (`a.b.c.d/prefix`).
    Ipv4Address,
    /// `ipv4.gateway` — the IPv4 default gateway.
    Ipv4Gateway,
    /// `ipv6.method` — how the interface obtains its IPv6 address.
    Ipv6Method,
    /// `ipv6.address` — the static IPv6 address (`addr/prefix`).
    Ipv6Address,
    /// `ipv6.gateway` — the IPv6 default gateway.
    Ipv6Gateway,
    /// `dns.servers` — the comma-separated recursive DNS servers to use on
    /// this interface (IPv4 and/or IPv6 addresses).
    DnsServers,
    /// `mtu` — the interface MTU.
    Mtu,
    /// `bond.members` — the comma-separated member NIC aliases.
    BondMembers,
    /// `bond.mode` — the bond transmit policy ([`BondMode`]).
    BondMode,
    /// `bond.monitor-interval` — the member-health probe interval, in ms.
    BondMonitorInterval,
    /// `bond.primary` — the preferred active member (for `active-backup`).
    BondPrimary,
}

impl IfaceKey {
    /// Every registry key suffix, in the canonical listing (and render)
    /// order.
    pub const ALL: &'static [Self] = &[
        Self::Kind,
        Self::MatchMac,
        Self::MatchNode,
        Self::Ipv4Method,
        Self::Ipv4Address,
        Self::Ipv4Gateway,
        Self::Ipv6Method,
        Self::Ipv6Address,
        Self::Ipv6Gateway,
        Self::DnsServers,
        Self::Mtu,
        Self::BondMembers,
        Self::BondMode,
        Self::BondMonitorInterval,
        Self::BondPrimary,
    ];

    /// The canonical suffix spelling (the part after `<iface>.`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::MatchMac => "match.mac",
            Self::MatchNode => "match.node",
            Self::Ipv4Method => "ipv4.method",
            Self::Ipv4Address => "ipv4.address",
            Self::Ipv4Gateway => "ipv4.gateway",
            Self::Ipv6Method => "ipv6.method",
            Self::Ipv6Address => "ipv6.address",
            Self::Ipv6Gateway => "ipv6.gateway",
            Self::DnsServers => "dns.servers",
            Self::Mtu => "mtu",
            Self::BondMembers => "bond.members",
            Self::BondMode => "bond.mode",
            Self::BondMonitorInterval => "bond.monitor-interval",
            Self::BondPrimary => "bond.primary",
        }
    }

    /// Decode a suffix spelling; `None` for anything outside the registry
    /// (suffixes are case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|key| key.name() == name)
    }

    /// A stable index into a fixed `[_; IfaceKey::ALL.len()]` array, used to
    /// detect a repeated key on one interface.
    #[must_use]
    const fn index(self) -> usize {
        self as usize
    }
}

/// Why a store text (or a single setting) was refused, without its line.
///
/// Every variant is a fail-closed refusal: the parser yields no
/// [`NetworkConfig`] and a writer applies nothing, rather than guess at a
/// malformed, partial, or inconsistent intent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// The store text is longer than [`MAX_CONFIG_LEN`].
    TooLong,
    /// The store declares more than [`MAX_INTERFACES`] interfaces.
    TooManyInterfaces,
    /// A bond enrols more than [`MAX_BOND_MEMBERS`] members.
    TooManyMembers,
    /// An interface's `dns.servers` list names more than
    /// [`MAX_DNS_SERVERS`] servers.
    TooManyDnsServers,
    /// An interface alias name is empty, over-long, or malformed.
    InvalidInterfaceName,
    /// A line has no `<iface>.<suffix>` shape, or names a suffix outside
    /// the closed registry.
    UnknownKey,
    /// A line names a key but carries no value.
    MissingValue,
    /// A line's value is malformed or outside its key's closed set.
    InvalidValue,
    /// The same `<iface>.<suffix>` key appeared more than once.
    DuplicateKey,
    /// The interface set is semantically inconsistent — a `bond.*` key on a
    /// non-bond, a bond with too few members, a static method with no
    /// address (or an address with a non-static method), a bond member that
    /// is undeclared, itself carries an address, or is enrolled twice, or a
    /// `bond.primary` that is not a member.
    InconsistentInterface,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooLong => "configuration exceeds the maximum length",
            Self::TooManyInterfaces => "configuration declares too many interfaces",
            Self::TooManyMembers => "a bond enrols too many members",
            Self::TooManyDnsServers => "an interface names too many DNS servers",
            Self::InvalidInterfaceName => "an interface name is empty, too long, or malformed",
            Self::UnknownKey => "configuration names an unknown key",
            Self::MissingValue => "a configuration key is missing its value",
            Self::InvalidValue => "a configuration value is malformed or outside its key's set",
            Self::DuplicateKey => "configuration repeats a key",
            Self::InconsistentInterface => "the interface configuration is inconsistent",
        };
        f.write_str(message)
    }
}

/// A parse failure with the line it was found on, where a line is
/// meaningful.
///
/// Line-level failures (an unknown key, a bad value) carry the 1-based
/// source line; whole-document failures (over-length, too many interfaces,
/// a semantic inconsistency spanning lines) carry `line: None`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    /// The 1-based source line, or `None` for a whole-document failure.
    pub line: Option<usize>,
    /// What went wrong.
    pub kind: ConfigError,
}

impl ParseError {
    /// A line-level failure on the 1-based `line`.
    #[must_use]
    const fn at(line: usize, kind: ConfigError) -> Self {
        Self {
            line: Some(line),
            kind,
        }
    }

    /// A whole-document failure with no single responsible line.
    #[must_use]
    const fn whole(kind: ConfigError) -> Self {
        Self { line: None, kind }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

/// One managed interface's configuration.
///
/// Every field is `Option`: `None` means "the key was not set in the store"
/// and the interface takes that key's documented default (see the `*()`
/// accessors). This is what lets [`NetworkConfig::render`] emit only the
/// keys the operator actually wrote and round-trip exactly through
/// [`NetworkConfig::parse`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InterfaceConfig {
    /// The interface alias name (`<iface>`), validated on parse.
    pub name: String,
    /// `<iface>.kind` (default [`IfaceKind::Ethernet`]).
    pub kind: Option<IfaceKind>,
    /// `<iface>.match.mac`.
    pub match_mac: Option<MacAddr>,
    /// `<iface>.match.node` — the stable **hardware location** the alias
    /// binds to: a NIC's register-window base (the device's fixed position
    /// on the bus), independent of MAC or discovery order. Written in hex
    /// (`0x…`); `netstack` matches it against the location the device
    /// manager resolved from the matched hardware-tree node.
    pub match_node: Option<u64>,
    /// `<iface>.ipv4.method` (default [`Ipv4Method::Disabled`]).
    pub ipv4_method: Option<Ipv4Method>,
    /// `<iface>.ipv4.address`.
    pub ipv4_address: Option<Ipv4Cidr>,
    /// `<iface>.ipv4.gateway`.
    pub ipv4_gateway: Option<Ipv4Addr>,
    /// `<iface>.ipv6.method` (default [`Ipv6Method::Slaac`]).
    pub ipv6_method: Option<Ipv6Method>,
    /// `<iface>.ipv6.address`.
    pub ipv6_address: Option<Ipv6Cidr>,
    /// `<iface>.ipv6.gateway`.
    pub ipv6_gateway: Option<Ipv6Addr>,
    /// `<iface>.dns.servers` — the recursive DNS servers to use on this
    /// interface, in declared order (each an IPv4 or IPv6 address). These
    /// join the DHCP-learned servers in the host's active resolver set
    /// (`plans/DNS.md`).
    pub dns_servers: Option<Vec<IpAddr>>,
    /// `<iface>.mtu`.
    pub mtu: Option<u16>,
    /// `<iface>.bond.members` (order preserved as written).
    pub bond_members: Option<Vec<String>>,
    /// `<iface>.bond.mode` (default [`BondMode::ActiveBackup`]).
    pub bond_mode: Option<BondMode>,
    /// `<iface>.bond.monitor-interval`, in milliseconds.
    pub bond_monitor_interval_ms: Option<u32>,
    /// `<iface>.bond.primary`.
    pub bond_primary: Option<String>,
}

impl InterfaceConfig {
    /// A fresh interface with the given `name` and every key unset.
    #[must_use]
    pub fn new(name: String) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }

    /// The effective link kind (the set value, else the default).
    #[must_use]
    pub fn kind(&self) -> IfaceKind {
        self.kind.unwrap_or_default()
    }

    /// The effective IPv4 method (the set value, else the default).
    #[must_use]
    pub fn ipv4_method(&self) -> Ipv4Method {
        self.ipv4_method.unwrap_or_default()
    }

    /// The effective IPv6 method (the set value, else the default).
    #[must_use]
    pub fn ipv6_method(&self) -> Ipv6Method {
        self.ipv6_method.unwrap_or_default()
    }

    /// Whether any `bond.*` key is set on this interface.
    #[must_use]
    fn has_bond_key(&self) -> bool {
        self.bond_members.is_some()
            || self.bond_mode.is_some()
            || self.bond_monitor_interval_ms.is_some()
            || self.bond_primary.is_some()
    }

    /// The effective member list (empty when unset).
    #[must_use]
    pub fn members(&self) -> &[String] {
        self.bond_members.as_deref().unwrap_or(&[])
    }

    /// The statically configured recursive DNS servers (empty when unset).
    #[must_use]
    pub fn dns_servers(&self) -> &[IpAddr] {
        self.dns_servers.as_deref().unwrap_or(&[])
    }

    /// Whether any addressing key was explicitly set on this interface. A
    /// bond member relies on the implicit defaults (which the owning bond
    /// overrides), so it may set none of these.
    #[must_use]
    fn has_explicit_address_key(&self) -> bool {
        self.ipv4_method.is_some()
            || self.ipv4_address.is_some()
            || self.ipv4_gateway.is_some()
            || self.ipv6_method.is_some()
            || self.ipv6_address.is_some()
            || self.ipv6_gateway.is_some()
            || self.dns_servers.is_some()
    }

    /// The canonical rendered value of `key` on this interface, or `None`
    /// when the key is unset (and so is not written).
    #[must_use]
    fn render_value(&self, key: IfaceKey) -> Option<String> {
        Some(match key {
            IfaceKey::Kind => String::from(self.kind?.as_str()),
            IfaceKey::MatchMac => self.match_mac?.render(),
            IfaceKey::MatchNode => render_node_match(self.match_node?),
            IfaceKey::Ipv4Method => String::from(self.ipv4_method?.as_str()),
            IfaceKey::Ipv4Address => self.ipv4_address?.render(),
            IfaceKey::Ipv4Gateway => render_display(&self.ipv4_gateway?),
            IfaceKey::Ipv6Method => String::from(self.ipv6_method?.as_str()),
            IfaceKey::Ipv6Address => self.ipv6_address?.render(),
            IfaceKey::Ipv6Gateway => render_display(&self.ipv6_gateway?),
            IfaceKey::DnsServers => render_dns_servers(self.dns_servers.as_ref()?),
            IfaceKey::Mtu => render_display(&self.mtu?),
            IfaceKey::BondMembers => self.bond_members.as_ref()?.join(","),
            IfaceKey::BondMode => String::from(self.bond_mode?.as_str()),
            IfaceKey::BondMonitorInterval => render_display(&self.bond_monitor_interval_ms?),
            IfaceKey::BondPrimary => self.bond_primary.clone()?,
        })
    }

    /// Set `key` from its store `value`, validating the value against the
    /// key's typed set. The interface is left unchanged on error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidValue`] when the value is malformed or outside
    /// the key's set, [`ConfigError::TooManyMembers`] for an over-long bond
    /// member list, or [`ConfigError::InvalidInterfaceName`] for a
    /// malformed member/primary name.
    fn set_key(&mut self, key: IfaceKey, value: &str) -> Result<(), ConfigError> {
        match key {
            IfaceKey::Kind => {
                self.kind = Some(IfaceKind::from_value(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::MatchMac => {
                self.match_mac = Some(MacAddr::parse(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::MatchNode => {
                self.match_node = Some(parse_node_match(value)?);
            }
            IfaceKey::Ipv4Method => {
                self.ipv4_method =
                    Some(Ipv4Method::from_value(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::Ipv4Address => {
                self.ipv4_address = Some(Ipv4Cidr::parse(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::Ipv4Gateway => {
                self.ipv4_gateway = Some(value.parse().map_err(|_| ConfigError::InvalidValue)?);
            }
            IfaceKey::Ipv6Method => {
                self.ipv6_method =
                    Some(Ipv6Method::from_value(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::Ipv6Address => {
                self.ipv6_address = Some(Ipv6Cidr::parse(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::Ipv6Gateway => {
                self.ipv6_gateway = Some(value.parse().map_err(|_| ConfigError::InvalidValue)?);
            }
            IfaceKey::DnsServers => {
                self.dns_servers = Some(parse_dns_servers(value)?);
            }
            IfaceKey::Mtu => {
                let mtu = parse_bounded_u32(value, u32::from(MIN_MTU), u32::from(MAX_MTU))?;
                self.mtu = Some(u16::try_from(mtu).map_err(|_| ConfigError::InvalidValue)?);
            }
            IfaceKey::BondMembers => {
                self.bond_members = Some(parse_member_list(value)?);
            }
            IfaceKey::BondMode => {
                self.bond_mode =
                    Some(BondMode::from_value(value).ok_or(ConfigError::InvalidValue)?);
            }
            IfaceKey::BondMonitorInterval => {
                self.bond_monitor_interval_ms = Some(parse_bounded_u32(
                    value,
                    MIN_MONITOR_INTERVAL_MS,
                    MAX_MONITOR_INTERVAL_MS,
                )?);
            }
            IfaceKey::BondPrimary => {
                validate_iface_name(value)?;
                self.bond_primary = Some(String::from(value));
            }
        }
        Ok(())
    }
}

/// Render any [`fmt::Display`] value to an owned `String` (infallible into a
/// `String`).
fn render_display(value: &dyn fmt::Display) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let _ = write!(out, "{value}");
    out
}

/// Validate an interface alias name: 1..=[`MAX_IFACE_NAME_LEN`] bytes, a
/// leading ASCII letter, then ASCII letters, digits, `-`, or `_`. The name
/// may not contain `.` (the key separator) — that is what makes the
/// `<iface>.<suffix>` split unambiguous.
fn validate_iface_name(name: &str) -> Result<(), ConfigError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_IFACE_NAME_LEN {
        return Err(ConfigError::InvalidInterfaceName);
    }
    if !bytes[0].is_ascii_alphabetic() {
        return Err(ConfigError::InvalidInterfaceName);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
    {
        return Err(ConfigError::InvalidInterfaceName);
    }
    Ok(())
}

/// Parse a decimal integer in `min..=max`, rejecting a non-digit run, an
/// empty string, or an out-of-range value (fail-closed, no saturation).
fn parse_bounded_u32(text: &str, min: u32, max: u32) -> Result<u32, ConfigError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ConfigError::InvalidValue);
    }
    let value: u32 = text.parse().map_err(|_| ConfigError::InvalidValue)?;
    if value < min || value > max {
        return Err(ConfigError::InvalidValue);
    }
    Ok(value)
}

/// Parse a `<iface>.match.node` value: a NIC's stable register-window base
/// written in hex with a mandatory `0x`/`0X` prefix and 1..=16 hex digits.
///
/// The value is a hardware *location*, so `0` is not a valid identity (it is
/// the [`NetstackRequest::BindDriver`](tairix_abi::net_ipc::NetstackRequest::BindDriver)
/// "no location resolved" sentinel) and is refused. Fail closed on a missing
/// prefix, an empty or over-long digit run, or a non-hex digit — never a
/// silent truncation or a decimal guess.
fn parse_node_match(value: &str) -> Result<u64, ConfigError> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .ok_or(ConfigError::InvalidValue)?;
    if digits.is_empty() || digits.len() > 16 {
        return Err(ConfigError::InvalidValue);
    }
    let base = u64::from_str_radix(digits, 16).map_err(|_| ConfigError::InvalidValue)?;
    if base == 0 {
        return Err(ConfigError::InvalidValue);
    }
    Ok(base)
}

/// Render a `<iface>.match.node` value canonically: lowercase hex with a
/// `0x` prefix (the exact form [`parse_node_match`] round-trips).
fn render_node_match(base: u64) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let _ = write!(out, "0x{base:x}");
    out
}

/// Parse a `,`-separated bond member list into validated, distinct names.
///
/// # Errors
///
/// [`ConfigError::TooManyMembers`] above [`MAX_BOND_MEMBERS`],
/// [`ConfigError::InvalidInterfaceName`] for a malformed name, and
/// [`ConfigError::InvalidValue`] for an empty segment or a duplicate.
fn parse_member_list(value: &str) -> Result<Vec<String>, ConfigError> {
    let mut members: Vec<String> = Vec::new();
    for segment in value.split(',') {
        if segment.is_empty() {
            return Err(ConfigError::InvalidValue);
        }
        validate_iface_name(segment)?;
        if members.iter().any(|m| m == segment) {
            return Err(ConfigError::InvalidValue);
        }
        if members.len() == MAX_BOND_MEMBERS {
            return Err(ConfigError::TooManyMembers);
        }
        members.push(String::from(segment));
    }
    Ok(members)
}

/// Parse a `,`-separated recursive-DNS-server list into validated, distinct
/// unicast addresses (IPv4 and/or IPv6), preserving declaration order.
///
/// A recursive resolver is a specific host, so each entry must be a genuine
/// unicast address: the unspecified address, any multicast group, and the
/// IPv4 limited broadcast are refused (a resolver could never live there).
/// The loopback address is allowed — a local resolver at `127.0.0.1` / `::1`
/// is legitimate.
///
/// # Errors
///
/// [`ConfigError::TooManyDnsServers`] above [`MAX_DNS_SERVERS`], and
/// [`ConfigError::InvalidValue`] for an empty segment, a malformed or
/// non-unicast address, or a duplicate.
fn parse_dns_servers(value: &str) -> Result<Vec<IpAddr>, ConfigError> {
    let mut servers: Vec<IpAddr> = Vec::new();
    for segment in value.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            return Err(ConfigError::InvalidValue);
        }
        let addr: IpAddr = segment.parse().map_err(|_| ConfigError::InvalidValue)?;
        if !is_unicast_resolver(addr) {
            return Err(ConfigError::InvalidValue);
        }
        if servers.contains(&addr) {
            return Err(ConfigError::InvalidValue);
        }
        if servers.len() == MAX_DNS_SERVERS {
            return Err(ConfigError::TooManyDnsServers);
        }
        servers.push(addr);
    }
    Ok(servers)
}

/// Whether `addr` is a plausible recursive-resolver address: a unicast host
/// address, not the unspecified address, a multicast group, or the IPv4
/// limited broadcast. Loopback is permitted (a local resolver).
fn is_unicast_resolver(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(a) => !(a.is_unspecified() || a.is_multicast() || a.is_broadcast()),
        IpAddr::V6(a) => !(a.is_unspecified() || a.is_multicast()),
    }
}

/// Render a DNS-server list as its canonical `,`-joined spelling (each
/// address in its `core::net` canonical form), the exact form
/// [`parse_dns_servers`] round-trips.
fn render_dns_servers(servers: &[IpAddr]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for (index, addr) in servers.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        let _ = write!(out, "{addr}");
    }
    out
}

/// A parsed, validated network configuration: the ordered set of managed
/// interfaces the store declares.
///
/// [`NetworkConfig::default`] is the empty configuration an **absent** store
/// implies — "no managed interfaces beyond loopback" — so a `netstack` that
/// finds no store file (a fresh installation, a boot before the root
/// unlock) runs on defaults without a special case.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetworkConfig {
    /// The managed interfaces, in the order first named in the store.
    interfaces: Vec<InterfaceConfig>,
}

impl NetworkConfig {
    /// The managed interfaces, in declaration order.
    #[must_use]
    pub fn interfaces(&self) -> &[InterfaceConfig] {
        &self.interfaces
    }

    /// The interface named `name`, if declared.
    #[must_use]
    pub fn interface(&self, name: &str) -> Option<&InterfaceConfig> {
        self.interfaces.iter().find(|iface| iface.name == name)
    }

    /// Parse and validate a store `text`.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] — carrying the offending 1-based line where
    /// one is meaningful — if `text` exceeds [`MAX_CONFIG_LEN`], declares
    /// more than [`MAX_INTERFACES`] interfaces, names an unknown or
    /// malformed key, carries a malformed or out-of-set value, repeats a
    /// key, or describes a semantically inconsistent interface set (see
    /// [`ConfigError::InconsistentInterface`]). The parser fails closed: a
    /// store it cannot fully understand yields no [`NetworkConfig`], so a
    /// consumer keeps its running configuration untouched.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        if text.len() > MAX_CONFIG_LEN {
            return Err(ParseError::whole(ConfigError::TooLong));
        }

        let mut config = Self::default();
        // One "seen" row per interface, parallel to `config.interfaces`, so
        // a repeated `<iface>.<suffix>` on one interface is caught.
        let mut seen: Vec<[bool; IfaceKey::ALL.len()]> = Vec::new();

        for (offset, raw) in text.lines().enumerate() {
            let lineno = offset + 1;
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }

            let mut fields = line.splitn(2, char::is_whitespace);
            let key_text = fields.next().unwrap_or_default();
            let value = fields.next().map(str::trim).filter(|v| !v.is_empty());

            let (iface_name, suffix) = key_text
                .split_once('.')
                .ok_or(ParseError::at(lineno, ConfigError::UnknownKey))?;
            validate_iface_name(iface_name).map_err(|kind| ParseError::at(lineno, kind))?;
            let key = IfaceKey::from_name(suffix)
                .ok_or(ParseError::at(lineno, ConfigError::UnknownKey))?;
            let value = value.ok_or(ParseError::at(lineno, ConfigError::MissingValue))?;

            let index = config.interface_index_or_insert(iface_name, &mut seen)?;
            if seen[index][key.index()] {
                return Err(ParseError::at(lineno, ConfigError::DuplicateKey));
            }
            seen[index][key.index()] = true;

            config.interfaces[index]
                .set_key(key, value)
                .map_err(|kind| ParseError::at(lineno, kind))?;
        }

        config.validate().map_err(ParseError::whole)?;
        Ok(config)
    }

    /// Find the index of the interface named `name`, inserting a fresh one
    /// (and its `seen` row) if it is new.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooManyInterfaces`] (as a whole-document
    /// [`ParseError`]) when a new interface would exceed [`MAX_INTERFACES`].
    fn interface_index_or_insert(
        &mut self,
        name: &str,
        seen: &mut Vec<[bool; IfaceKey::ALL.len()]>,
    ) -> Result<usize, ParseError> {
        if let Some(index) = self.interfaces.iter().position(|iface| iface.name == name) {
            return Ok(index);
        }
        if self.interfaces.len() == MAX_INTERFACES {
            return Err(ParseError::whole(ConfigError::TooManyInterfaces));
        }
        self.interfaces
            .push(InterfaceConfig::new(String::from(name)));
        seen.push([false; IfaceKey::ALL.len()]);
        Ok(self.interfaces.len() - 1)
    }

    /// Check the whole-document semantic invariants that a per-line parse
    /// cannot: bond consistency and cross-interface membership.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InconsistentInterface`] on any violation.
    fn validate(&self) -> Result<(), ConfigError> {
        for iface in &self.interfaces {
            Self::validate_interface(iface)?;
        }
        self.validate_bond_membership()?;
        Ok(())
    }

    /// Per-interface consistency: bond keys only on a bond, a bond's own
    /// member count and primary, and the method↔address agreement.
    fn validate_interface(iface: &InterfaceConfig) -> Result<(), ConfigError> {
        let is_bond = iface.kind() == IfaceKind::Bond;

        if iface.has_bond_key() && !is_bond {
            return Err(ConfigError::InconsistentInterface);
        }
        // An interface's hardware identity is exactly one thing: match by
        // MAC or by hardware node, never both (an ambiguous identity is
        // refused, not silently disambiguated).
        if iface.match_mac.is_some() && iface.match_node.is_some() {
            return Err(ConfigError::InconsistentInterface);
        }
        // Only a physical NIC has a hardware location. A bond is composed
        // in software and the loopback is the stack's own, so neither may
        // be bound by node.
        if iface.match_node.is_some() && iface.kind() != IfaceKind::Ethernet {
            return Err(ConfigError::InconsistentInterface);
        }
        if is_bond {
            if iface.members().len() < MIN_BOND_MEMBERS {
                return Err(ConfigError::InconsistentInterface);
            }
            if let Some(primary) = &iface.bond_primary {
                if !iface.members().iter().any(|m| m == primary) {
                    return Err(ConfigError::InconsistentInterface);
                }
            }
        }

        match iface.ipv4_method() {
            Ipv4Method::Static => {
                if iface.ipv4_address.is_none() {
                    return Err(ConfigError::InconsistentInterface);
                }
            }
            // Neither disabled nor DHCP may carry a static address or
            // gateway (DHCP supplies both from the lease).
            Ipv4Method::Disabled | Ipv4Method::Dhcp => {
                if iface.ipv4_address.is_some() || iface.ipv4_gateway.is_some() {
                    return Err(ConfigError::InconsistentInterface);
                }
            }
        }
        match iface.ipv6_method() {
            Ipv6Method::Static => {
                if iface.ipv6_address.is_none() {
                    return Err(ConfigError::InconsistentInterface);
                }
            }
            // None of SLAAC, DHCPv6, or disabled may carry a static
            // address or gateway (DHCPv6 supplies the address from the
            // lease; SLAAC from Router Advertisements).
            Ipv6Method::Slaac | Ipv6Method::Dhcp | Ipv6Method::Disabled => {
                if iface.ipv6_address.is_some() || iface.ipv6_gateway.is_some() {
                    return Err(ConfigError::InconsistentInterface);
                }
            }
        }
        Ok(())
    }

    /// Cross-interface bond membership: every member must be a declared
    /// ethernet interface, carry no addressing of its own, and be enrolled
    /// in at most one bond.
    fn validate_bond_membership(&self) -> Result<(), ConfigError> {
        let mut enrolled: Vec<&str> = Vec::new();
        for bond in &self.interfaces {
            if bond.kind() != IfaceKind::Bond {
                continue;
            }
            for member in bond.members() {
                let iface = self
                    .interface(member)
                    .ok_or(ConfigError::InconsistentInterface)?;
                if iface.kind() != IfaceKind::Ethernet {
                    return Err(ConfigError::InconsistentInterface);
                }
                if iface.has_explicit_address_key() {
                    return Err(ConfigError::InconsistentInterface);
                }
                if enrolled.contains(&member.as_str()) {
                    return Err(ConfigError::InconsistentInterface);
                }
                enrolled.push(member);
            }
        }
        Ok(())
    }

    /// Render the canonical store text: the explanatory header comment and,
    /// for each interface in declaration order, one `<iface>.<suffix> value`
    /// line per key that is set, in [`IfaceKey::ALL`] order.
    ///
    /// Only keys the operator set are written, so a render/parse round trip
    /// is exact and the document shows exactly the live configuration —
    /// never a wall of defaults.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# TAIRiX network interface configuration.\n\
             # Managed by the `configure` command and the installer; parsed\n\
             # by netstack after the root filesystem is unlocked. One\n\
             # `<interface>.<key> value` setting per line.\n",
        );
        for iface in &self.interfaces {
            for key in IfaceKey::ALL {
                if let Some(value) = iface.render_value(*key) {
                    out.push_str(&iface.name);
                    out.push('.');
                    out.push_str(key.name());
                    out.push(' ');
                    out.push_str(&value);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Return the portion of `line` before its first `#`, dropping an inline or
/// whole-line comment. No registry key or value contains `#`, so cutting at
/// the first one is unambiguous.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::format;
    use std::string::{String, ToString};

    use super::*;

    fn err(text: &str) -> ConfigError {
        NetworkConfig::parse(text).expect_err("must fail").kind
    }

    #[test]
    fn an_empty_store_has_no_managed_interfaces() {
        let config = NetworkConfig::parse("").expect("parses");
        assert_eq!(config, NetworkConfig::default());
        assert!(config.interfaces().is_empty());
    }

    #[test]
    fn comments_blank_lines_and_whitespace_are_tolerated() {
        let text = "\
# a leading comment
\t
   wan.kind    ethernet   # a physical NIC
";
        let config = NetworkConfig::parse(text).expect("parses");
        assert_eq!(
            config.interface("wan").expect("wan").kind(),
            IfaceKind::Ethernet
        );
    }

    #[test]
    fn a_static_dual_stack_ethernet_parses() {
        let text = "\
wan.match.mac 52:54:00:12:34:56
wan.ipv4.method static
wan.ipv4.address 192.168.1.10/24
wan.ipv4.gateway 192.168.1.1
wan.ipv6.method static
wan.ipv6.address 2001:db8::10/64
wan.ipv6.gateway 2001:db8::1
wan.mtu 9000
";
        let config = NetworkConfig::parse(text).expect("parses");
        let wan = config.interface("wan").expect("wan");
        assert_eq!(
            wan.match_mac,
            Some(MacAddr([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]))
        );
        assert_eq!(wan.ipv4_method(), Ipv4Method::Static);
        assert_eq!(
            wan.ipv4_address,
            Some(Ipv4Cidr {
                addr: Ipv4Addr::new(192, 168, 1, 10),
                prefix: 24,
            })
        );
        assert_eq!(wan.ipv4_gateway, Some(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(wan.ipv6_method(), Ipv6Method::Static);
        assert_eq!(wan.mtu, Some(9000));
    }

    #[test]
    fn defaults_apply_to_unset_keys() {
        let config = NetworkConfig::parse("lan.match.node 0xa000000\n").expect("parses");
        let lan = config.interface("lan").expect("lan");
        assert_eq!(lan.kind(), IfaceKind::Ethernet);
        assert_eq!(lan.ipv4_method(), Ipv4Method::Disabled);
        assert_eq!(lan.ipv6_method(), Ipv6Method::Slaac);
        assert_eq!(lan.match_node, Some(0x0a00_0000));
    }

    #[test]
    fn match_node_parses_hex_and_round_trips_and_fails_closed() {
        // A managed interface bound by its hardware location renders back
        // to the canonical lowercase-hex form (round-trip exact).
        let text = "wan.match.node 0xA000000\n";
        let config = NetworkConfig::parse(text).expect("parses");
        assert_eq!(
            config.interface("wan").unwrap().match_node,
            Some(0xa00_0000)
        );
        assert_eq!(
            NetworkConfig::parse(&config.render()).unwrap().render(),
            config.render()
        );
        assert!(config.render().contains("wan.match.node 0xa000000"));
        // Fail closed: missing prefix, empty/over-long digits, non-hex, or
        // the zero sentinel.
        assert_eq!(err("wan.match.node a000000\n"), ConfigError::InvalidValue);
        assert_eq!(err("wan.match.node 0x\n"), ConfigError::InvalidValue);
        assert_eq!(
            err("wan.match.node 0x10000000000000000\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(err("wan.match.node 0xgg\n"), ConfigError::InvalidValue);
        assert_eq!(err("wan.match.node 0x0\n"), ConfigError::InvalidValue);
    }

    #[test]
    fn match_node_is_rejected_on_a_non_ethernet_or_alongside_a_mac() {
        // A bond has no hardware of its own, so it cannot be bound by node.
        assert_eq!(
            err("bond0.kind bond\nbond0.bond.members eth0,eth1\n\
                 bond0.match.node 0xa000000\neth0.match.mac 52:54:00:00:00:01\n\
                 eth1.match.mac 52:54:00:00:00:02\n"),
            ConfigError::InconsistentInterface
        );
        // An interface's identity is one thing: MAC or node, never both.
        assert_eq!(
            err("wan.match.mac 52:54:00:12:34:56\nwan.match.node 0xa000000\n"),
            ConfigError::InconsistentInterface
        );
    }

    #[test]
    fn unknown_key_fails_closed_with_its_line() {
        let e = NetworkConfig::parse("wan.kind ethernet\nwan.bogus x\n").expect_err("fails");
        assert_eq!(e.kind, ConfigError::UnknownKey);
        assert_eq!(e.line, Some(2));
    }

    #[test]
    fn a_keyless_word_is_an_unknown_key() {
        assert_eq!(err("noseparator value\n"), ConfigError::UnknownKey);
    }

    #[test]
    fn invalid_values_fail_closed() {
        assert_eq!(err("wan.kind switch\n"), ConfigError::InvalidValue);
        assert_eq!(err("wan.ipv4.method auto\n"), ConfigError::InvalidValue);
        // Case-sensitive: one canonical spelling.
        assert_eq!(err("wan.kind Ethernet\n"), ConfigError::InvalidValue);
        assert_eq!(
            err("wan.match.mac 52:54:00:12:34\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(
            err("wan.match.mac 52:54:00:12:34:5G\n"),
            ConfigError::InvalidValue
        );
        // Upper-case hex is not the canonical spelling.
        assert_eq!(
            err("wan.match.mac AA:BB:CC:DD:EE:FF\n"),
            ConfigError::InvalidValue
        );
    }

    #[test]
    fn address_and_prefix_bounds_are_enforced() {
        assert_eq!(
            err("wan.ipv4.method static\nwan.ipv4.address 10.0.0.1\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(
            err("wan.ipv4.method static\nwan.ipv4.address 10.0.0.1/33\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(
            err("wan.ipv6.method static\nwan.ipv6.address 2001:db8::1/129\n"),
            ConfigError::InvalidValue
        );
        // A negative or non-digit prefix is refused, never parsed loosely.
        assert_eq!(
            err("wan.ipv4.method static\nwan.ipv4.address 10.0.0.1/-1\n"),
            ConfigError::InvalidValue
        );
    }

    #[test]
    fn mtu_out_of_range_fails_closed() {
        assert_eq!(err("wan.mtu 1279\n"), ConfigError::InvalidValue);
        assert_eq!(err("wan.mtu 65536\n"), ConfigError::InvalidValue);
        assert_eq!(err("wan.mtu 0x600\n"), ConfigError::InvalidValue);
        assert!(NetworkConfig::parse("wan.mtu 1280\n").is_ok());
    }

    #[test]
    fn missing_value_fails_closed() {
        assert_eq!(err("wan.kind\n"), ConfigError::MissingValue);
        assert_eq!(err("wan.kind   # no value\n"), ConfigError::MissingValue);
    }

    #[test]
    fn duplicate_key_on_one_interface_fails_closed() {
        let e = NetworkConfig::parse("wan.mtu 1500\nwan.mtu 9000\n").expect_err("fails");
        assert_eq!(e.kind, ConfigError::DuplicateKey);
        assert_eq!(e.line, Some(2));
    }

    #[test]
    fn the_same_key_on_two_interfaces_is_not_a_duplicate() {
        let config = NetworkConfig::parse("a.mtu 1500\nb.mtu 9000\n").expect("parses");
        assert_eq!(config.interface("a").unwrap().mtu, Some(1500));
        assert_eq!(config.interface("b").unwrap().mtu, Some(9000));
    }

    #[test]
    fn malformed_interface_names_fail_closed() {
        assert_eq!(err("0eth.mtu 1500\n"), ConfigError::InvalidInterfaceName);
        assert_eq!(err("et h.mtu 1500\n"), ConfigError::UnknownKey); // space splits key/value
        assert_eq!(err("eth$0.mtu 1500\n"), ConfigError::InvalidInterfaceName);
        let long = format!("{}.mtu 1500\n", "e".repeat(MAX_IFACE_NAME_LEN + 1));
        assert_eq!(err(&long), ConfigError::InvalidInterfaceName);
    }

    #[test]
    fn too_many_interfaces_fails_closed() {
        use std::fmt::Write as _;
        let mut text = String::new();
        for index in 0..=MAX_INTERFACES {
            let _ = writeln!(text, "eth{index}.mtu 1500");
        }
        assert_eq!(err(&text), ConfigError::TooManyInterfaces);
    }

    #[test]
    fn an_oversized_store_is_refused_before_scanning() {
        let mut text = String::from("wan.mtu 1500\n");
        while text.len() <= MAX_CONFIG_LEN {
            text.push_str("# padding comment line\n");
        }
        assert_eq!(err(&text), ConfigError::TooLong);
    }

    #[test]
    fn a_full_bond_parses() {
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth1.match.mac 52:54:00:00:00:02
bond0.kind bond
bond0.bond.members eth0,eth1
bond0.bond.mode active-backup
bond0.bond.primary eth0
bond0.bond.monitor-interval 200
bond0.ipv6.method slaac
";
        let config = NetworkConfig::parse(text).expect("parses");
        let bond = config.interface("bond0").expect("bond0");
        assert_eq!(bond.kind(), IfaceKind::Bond);
        assert_eq!(bond.members(), &["eth0".to_string(), "eth1".to_string()]);
        assert_eq!(bond.bond_mode, Some(BondMode::ActiveBackup));
        assert_eq!(bond.bond_primary.as_deref(), Some("eth0"));
        assert_eq!(bond.bond_monitor_interval_ms, Some(200));
    }

    #[test]
    fn a_bond_key_on_a_non_bond_is_inconsistent() {
        let text = "eth0.match.mac 52:54:00:00:00:01\neth1.match.mac 52:54:00:00:00:02\nwan.bond.members eth0,eth1\n";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn a_bond_needs_at_least_two_members() {
        let text = "eth0.match.mac 52:54:00:00:00:01\nbond0.kind bond\nbond0.bond.members eth0\n";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn a_bond_primary_must_be_a_member() {
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth1.match.mac 52:54:00:00:00:02
bond0.kind bond
bond0.bond.members eth0,eth1
bond0.bond.primary eth2
";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn a_bond_member_must_be_declared() {
        let text = "bond0.kind bond\nbond0.bond.members eth0,eth1\n";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn a_bond_member_may_not_carry_an_address() {
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth0.ipv4.method static
eth0.ipv4.address 10.0.0.2/24
eth1.match.mac 52:54:00:00:00:02
bond0.kind bond
bond0.bond.members eth0,eth1
";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn a_member_may_not_be_enrolled_in_two_bonds() {
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth1.match.mac 52:54:00:00:00:02
eth2.match.mac 52:54:00:00:00:03
bond0.kind bond
bond0.bond.members eth0,eth1
bond1.kind bond
bond1.bond.members eth0,eth2
";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn a_bond_may_not_enrol_a_bond() {
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth1.match.mac 52:54:00:00:00:02
inner.kind bond
inner.bond.members eth0,eth1
outer.kind bond
outer.bond.members inner,eth0
";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn too_many_bond_members_fails_closed() {
        let members: std::vec::Vec<String> =
            (0..=MAX_BOND_MEMBERS).map(|i| format!("eth{i}")).collect();
        let text = format!(
            "bond0.kind bond\nbond0.bond.members {}\n",
            members.join(",")
        );
        assert_eq!(err(&text), ConfigError::TooManyMembers);
    }

    #[test]
    fn a_duplicate_or_empty_member_fails_closed() {
        assert_eq!(
            err("bond0.bond.members eth0,eth0\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(err("bond0.bond.members eth0,\n"), ConfigError::InvalidValue);
        assert_eq!(err("bond0.bond.members ,eth0\n"), ConfigError::InvalidValue);
    }

    #[test]
    fn static_method_requires_an_address_and_vice_versa() {
        assert_eq!(
            err("wan.ipv4.method static\n"),
            ConfigError::InconsistentInterface
        );
        assert_eq!(
            err("wan.ipv6.method static\n"),
            ConfigError::InconsistentInterface
        );
        // An address (or gateway) with a non-static method is inconsistent.
        assert_eq!(
            err("wan.ipv4.gateway 10.0.0.1\n"),
            ConfigError::InconsistentInterface
        );
        assert_eq!(
            err("wan.ipv6.method slaac\nwan.ipv6.address 2001:db8::1/64\n"),
            ConfigError::InconsistentInterface
        );
    }

    #[test]
    fn dhcp_ipv4_method_parses_and_forbids_a_static_address() {
        // A plain DHCPv4 interface parses and needs no address.
        let config = NetworkConfig::parse("wan.ipv4.method dhcp\n").expect("parses");
        assert_eq!(
            config.interface("wan").expect("wan").ipv4_method(),
            Ipv4Method::Dhcp
        );
        // A DHCP interface may not also carry a static address or gateway.
        assert_eq!(
            err("wan.ipv4.method dhcp\nwan.ipv4.address 10.0.0.5/24\n"),
            ConfigError::InconsistentInterface
        );
        assert_eq!(
            err("wan.ipv4.method dhcp\nwan.ipv4.gateway 10.0.0.1\n"),
            ConfigError::InconsistentInterface
        );
        // It round-trips through render/parse.
        let rendered = config.render();
        assert!(rendered.contains("wan.ipv4.method dhcp"));
        assert_eq!(NetworkConfig::parse(&rendered).expect("re-parses"), config);
    }

    #[test]
    fn dhcp_ipv6_method_parses_and_forbids_a_static_address() {
        // A plain DHCPv6 interface parses and needs no address.
        let config = NetworkConfig::parse("wan.ipv6.method dhcp\n").expect("parses");
        assert_eq!(
            config.interface("wan").expect("wan").ipv6_method(),
            Ipv6Method::Dhcp
        );
        // A DHCPv6 interface may not also carry a static address or gateway.
        assert_eq!(
            err("wan.ipv6.method dhcp\nwan.ipv6.address 2001:db8::5/64\n"),
            ConfigError::InconsistentInterface
        );
        assert_eq!(
            err("wan.ipv6.method dhcp\nwan.ipv6.gateway 2001:db8::1\n"),
            ConfigError::InconsistentInterface
        );
        // It round-trips through render/parse.
        let rendered = config.render();
        assert!(rendered.contains("wan.ipv6.method dhcp"));
        assert_eq!(NetworkConfig::parse(&rendered).expect("re-parses"), config);
    }

    #[test]
    fn dns_servers_parse_render_round_trip_a_mixed_list() {
        let text = "wan.dns.servers 9.9.9.9,2606:4700:4700::1111,127.0.0.1\n";
        let config = NetworkConfig::parse(text).expect("parses");
        let wan = config.interface("wan").expect("wan");
        assert_eq!(
            wan.dns_servers(),
            &[
                IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
                IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ]
        );
        // Round-trips exactly (canonical spelling, declaration order kept).
        let rendered = config.render();
        assert!(rendered.contains("wan.dns.servers 9.9.9.9,2606:4700:4700::1111,127.0.0.1"));
        assert_eq!(NetworkConfig::parse(&rendered).expect("re-parses"), config);
    }

    #[test]
    fn dns_servers_tolerate_whitespace_between_entries() {
        let config = NetworkConfig::parse("wan.dns.servers 1.1.1.1, 8.8.8.8\n").expect("parses");
        assert_eq!(
            config.interface("wan").expect("wan").dns_servers(),
            &[
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            ]
        );
    }

    #[test]
    fn dns_servers_fail_closed_on_bad_or_non_unicast_or_duplicate_entries() {
        // A malformed address, an empty segment, and a duplicate all fail.
        assert_eq!(
            err("wan.dns.servers not-an-ip\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(err("wan.dns.servers 1.1.1.1,\n"), ConfigError::InvalidValue);
        assert_eq!(
            err("wan.dns.servers 1.1.1.1,1.1.1.1\n"),
            ConfigError::InvalidValue
        );
        // Non-unicast resolver addresses are refused: unspecified, multicast,
        // and the IPv4 limited broadcast (a resolver could never live there).
        assert_eq!(err("wan.dns.servers 0.0.0.0\n"), ConfigError::InvalidValue);
        assert_eq!(
            err("wan.dns.servers 224.0.0.1\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(
            err("wan.dns.servers 255.255.255.255\n"),
            ConfigError::InvalidValue
        );
        assert_eq!(err("wan.dns.servers ::\n"), ConfigError::InvalidValue);
        assert_eq!(err("wan.dns.servers ff02::1\n"), ConfigError::InvalidValue);
    }

    #[test]
    fn too_many_dns_servers_fails_closed() {
        // One past MAX_DNS_SERVERS distinct servers is refused.
        use core::fmt::Write as _;
        let mut value = String::new();
        for index in 0..=MAX_DNS_SERVERS {
            if index != 0 {
                value.push(',');
            }
            let _ = write!(value, "10.0.0.{index}");
        }
        let text = format!("wan.dns.servers {value}\n");
        assert_eq!(err(&text), ConfigError::TooManyDnsServers);
    }

    #[test]
    fn a_bond_member_may_not_carry_dns_servers() {
        // DNS servers are addressing the bond owns, so a member carrying
        // them is inconsistent (like a member carrying an address).
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth0.dns.servers 1.1.1.1
eth1.match.mac 52:54:00:00:00:02
bond0.kind bond
bond0.bond.members eth0,eth1
";
        assert_eq!(err(text), ConfigError::InconsistentInterface);
    }

    #[test]
    fn monitor_interval_bounds_are_enforced() {
        let below = format!(
            "bond0.bond.monitor-interval {}\n",
            MIN_MONITOR_INTERVAL_MS - 1
        );
        let above = format!(
            "bond0.bond.monitor-interval {}\n",
            MAX_MONITOR_INTERVAL_MS + 1
        );
        assert_eq!(err(&below), ConfigError::InvalidValue);
        assert_eq!(err(&above), ConfigError::InvalidValue);
    }

    #[test]
    fn render_parse_round_trips_a_bond_configuration() {
        let text = "\
eth0.match.mac 52:54:00:00:00:01
eth1.match.mac 52:54:00:00:00:02
bond0.kind bond
bond0.ipv4.method static
bond0.ipv4.address 10.0.0.5/24
bond0.ipv4.gateway 10.0.0.1
bond0.ipv6.method static
bond0.ipv6.address 2001:db8::5/64
bond0.ipv6.gateway 2001:db8::1
bond0.mtu 1500
bond0.bond.members eth0,eth1
bond0.bond.mode balance
bond0.bond.monitor-interval 100
bond0.bond.primary eth0
";
        let config = NetworkConfig::parse(text).expect("parses");
        let rendered = config.render();
        let reparsed = NetworkConfig::parse(&rendered).expect("re-parses");
        assert_eq!(config, reparsed);
    }

    #[test]
    fn render_preserves_interface_declaration_order() {
        let config = NetworkConfig::parse("zeta.mtu 1500\nalpha.mtu 9000\n").expect("parses");
        let rendered = config.render();
        let zeta = rendered.find("zeta.mtu").expect("zeta rendered");
        let alpha = rendered.find("alpha.mtu").expect("alpha rendered");
        assert!(zeta < alpha, "declaration order not preserved");
    }

    #[test]
    fn render_emits_only_set_keys() {
        let config = NetworkConfig::parse("wan.mtu 1500\n").expect("parses");
        let rendered = config.render();
        assert!(rendered.contains("wan.mtu 1500"));
        assert!(!rendered.contains("wan.kind"));
        assert!(!rendered.contains("wan.ipv4.method"));
    }

    #[test]
    fn mac_round_trips_its_canonical_spelling() {
        let mac = MacAddr([0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f]);
        assert_eq!(mac.render(), "0a:1b:2c:3d:4e:5f");
        assert_eq!(MacAddr::parse(&mac.render()), Some(mac));
    }

    #[test]
    fn cidr_round_trips_its_canonical_spelling() {
        let v4 = Ipv4Cidr {
            addr: Ipv4Addr::new(10, 0, 0, 1),
            prefix: 8,
        };
        assert_eq!(Ipv4Cidr::parse(&v4.render()), Some(v4));
        let v6 = Ipv6Cidr {
            addr: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            prefix: 64,
        };
        assert_eq!(Ipv6Cidr::parse(&v6.render()), Some(v6));
    }

    #[test]
    fn key_registry_round_trips_names() {
        for key in IfaceKey::ALL {
            assert_eq!(IfaceKey::from_name(key.name()), Some(*key));
        }
        assert_eq!(IfaceKey::from_name("Kind"), None, "keys are case-sensitive");
        assert_eq!(IfaceKey::from_name(""), None);
    }

    #[test]
    fn value_enums_round_trip_their_spellings() {
        for kind in [IfaceKind::Ethernet, IfaceKind::Bond, IfaceKind::Loopback] {
            assert_eq!(IfaceKind::from_value(kind.as_str()), Some(kind));
        }
        for method in [Ipv4Method::Disabled, Ipv4Method::Static, Ipv4Method::Dhcp] {
            assert_eq!(Ipv4Method::from_value(method.as_str()), Some(method));
        }
        for method in [
            Ipv6Method::Slaac,
            Ipv6Method::Static,
            Ipv6Method::Dhcp,
            Ipv6Method::Disabled,
        ] {
            assert_eq!(Ipv6Method::from_value(method.as_str()), Some(method));
        }
        for mode in [BondMode::ActiveBackup, BondMode::Balance] {
            assert_eq!(BondMode::from_value(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn error_display_is_stable_and_line_numbered() {
        assert_eq!(
            format!("{}", ConfigError::UnknownKey),
            "configuration names an unknown key",
        );
        let e = ParseError::at(7, ConfigError::InvalidValue);
        assert_eq!(
            format!("{e}"),
            "line 7: a configuration value is malformed or outside its key's set"
        );
        let whole = ParseError::whole(ConfigError::TooLong);
        assert_eq!(
            format!("{whole}"),
            "configuration exceeds the maximum length"
        );
    }

    #[test]
    fn path_constants_are_inside_the_settings_subtree() {
        assert!(CONFIG_PATH.starts_with(CONFIG_DIR));
        assert!(CONFIG_PATH.starts_with("/System/Settings/"));
    }
}
