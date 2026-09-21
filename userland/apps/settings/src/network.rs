//! The network store's per-interface settings as form rows, and the two
//! readings the networking panes state around them.
//!
//! The third store this surface edits, after the desktop's own document
//! ([`crate::form`]) and the machine's `system.conf` ([`crate::machine`]).
//! Its engine is `lib/netconfig` and its one command app is `configure`, the
//! same tool the machine store is written by, so Settings grows no second
//! writer here either.
//!
//! What makes it different from the other two is that this application
//! cannot read it. `network.conf` carries each interface's hardware identity
//! and this machine's static address book — the readings the System
//! Information API gates — so the document arrives only as the relayed
//! output of a `configure` run an account authorised, and the plates are
//! discovered from *that* listing rather than declared by a table here.
//!
//! Nothing here performs I/O or holds authority. A row reports the value the
//! reader typed or chose against a working copy of the document; the shell
//! turns the difference into the one elevated command that writes it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::net_ipc::NetServerAddr;
use tairix_controls::{
    ComboBox, ControlState, FieldControl, FieldGroup, FieldRow, TextField, ValidationState,
};
use tairix_netconfig::{valid_iface_name, IfaceKey, Ipv4Method, Ipv6Method, NetworkConfig};
use tairix_procinfo::render_server;
use tairix_util::conf::ValueShape;

/// What the Ethernet pane contributes to the search index.
///
/// One term rather than one per interface: the plates are discovered at
/// runtime from a store this application may not read for itself, and a
/// reader searches for the subject.
pub(crate) const ADDRESSING_FACTS: &[&str] = &["Addressing"];

/// What the DNS pane contributes to the search index.
pub(crate) const RESOLVER_FACTS: &[&str] = &["Name servers"];

/// The label each live name-server row carries.
const SERVER_LABEL: &str = "Name server";

/// The caption of the DNS pane's live-reading plate.
const RESOLVERS_CAPTION: &str = "NAME SERVERS IN USE";

/// The caption of the one plate that states something other than an
/// interface.
const ADDRESSING_CAPTION: &str = "CONFIGURED ADDRESSING";

/// What the pane states when the stack holds no resolver at all.
///
/// Distinct from a refused reading: the query answered, and what it
/// answered was an empty set. A machine with no name server resolves
/// nothing, which is worth saying plainly rather than showing an empty
/// plate.
const NO_SERVERS: &str = "none — this machine resolves no names";

/// What a reading states when the caller could not take it.
const UNMEASURED: &str = "not measured";

/// What the networking panes state before anyone has asked.
const NOT_ASKED: &str =
    "not read — this machine's addressing is not public, so reading it needs an account that may";

/// What they state when the store declares no interface at all.
const NO_INTERFACES: &str = "none — no interface is configured on this machine";

/// What they state when the configuration is larger than the reply carries.
///
/// The seam bounds what a program may print back to an unprivileged
/// caller, so a very large document is not shown here at all rather than
/// shown in part.
const TOO_LARGE: &str = "too large to show here — run `configure` from a shell to read it in full";

/// What they state when the run answered something that is not a listing.
const UNREADABLE_LISTING: &str = "the command answered something this window could not read";

/// What a plate says about an interface no device can ever be bound to.
///
/// `configure` states the same limit when such an interface is written; the
/// pane can see it in the document it is editing, so it says so before the
/// write rather than onto a console the reader is not looking at.
const UNBINDABLE: &str =
    "This interface names neither a hardware address nor a hardware location, so no device is \
     ever bound to it.";

/// The one word for a key the document does not declare: the leading
/// choice of a closed key's list, and what an empty entry shows in place of
/// a value.
const UNSET: &str = "not set";

/// The network readings the DNS and Ethernet panes draw, as the caller
/// answered them.
///
/// An [`Option`] for the same reason every machine reading is one: a query
/// that was refused or has not landed is not an empty answer, and the pane
/// says which of the two it is holding.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetworkFacts {
    /// The recursive resolvers the stack is actually using: the statically
    /// configured and the DHCP-learned servers, aggregated and
    /// deduplicated by the stack into the one answer every client reads.
    pub resolvers: Option<Vec<NetServerAddr>>,
    /// What this application has been told of the machine's configured
    /// addressing, which is nothing until a reader asks for it under an
    /// account that may read it.
    pub addressing: Addressing,
}

impl NetworkFacts {
    /// The resolver set as a slice, for a row builder that only reads it.
    #[must_use]
    pub(crate) fn resolvers_slice(&self) -> Option<&[NetServerAddr]> {
        self.resolvers.as_deref()
    }
}

/// What the networking panes know of the machine's configured addressing.
///
/// The store that holds it carries each interface's hardware identity and
/// this machine's static address book — the readings the System
/// Information API gates behind `CAP_SYSINFO_HW` and `CAP_SYSINFO_GLOBAL`
/// — so this application, which holds no authority at all, cannot read it
/// and must not be given a way round those gates. It is answered instead
/// by an account that may, running the tool that owns the store, with the
/// run's output relayed back through the supervisor's elevated-read seam.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Addressing {
    /// Nobody has asked yet. The pane states what it would take.
    #[default]
    Unasked,
    /// The document the listing stated, which is what every plate is
    /// discovered from and what a staged change is measured against.
    Listed(NetworkConfig),
    /// The run happened, but printed more than the seam carries.
    Overran,
    /// Nothing was read, and this is why.
    Refused(String),
}

impl Addressing {
    /// The addressing a `configure` listing states.
    ///
    /// The listing carries both registries, so each line is read against
    /// the per-interface one and anything it does not name — every machine
    /// setting, and any line this build does not understand — is dropped
    /// before the rest is parsed back as the document it came from. The
    /// engine is the shared one, so this surface and the tool that printed
    /// the lines cannot disagree on what a key means, and a listing the
    /// engine will not take whole is no document at all.
    #[must_use]
    pub fn from_listing(output: &[u8]) -> Self {
        let Ok(text) = core::str::from_utf8(output) else {
            return Self::unreadable();
        };
        let mut document = String::with_capacity(text.len());
        for line in text.lines() {
            let Some((name, _)) = line.split_once(' ') else {
                continue;
            };
            let Some((alias, suffix)) = name.split_once('.') else {
                continue;
            };
            if !valid_iface_name(alias) || IfaceKey::from_name(suffix).is_none() {
                continue;
            }
            document.push_str(line);
            document.push('\n');
        }
        NetworkConfig::parse(&document).map_or_else(|_| Self::unreadable(), Self::Listed)
    }

    /// The document this addressing states, or `None` while there is none
    /// to stage against.
    #[must_use]
    pub(crate) fn document(&self) -> Option<&NetworkConfig> {
        match self {
            Self::Listed(config) => Some(config),
            Self::Unasked | Self::Overran | Self::Refused(_) => None,
        }
    }

    /// The one plate a pane draws instead of its interfaces, or `None`
    /// when there are interfaces to draw.
    fn statement(&self) -> Option<FieldGroup> {
        let said = match self {
            Self::Unasked => NOT_ASKED,
            Self::Overran => TOO_LARGE,
            Self::Refused(reason) => reason.as_str(),
            Self::Listed(config) if config.interfaces().is_empty() => NO_INTERFACES,
            Self::Listed(_) => return None,
        };
        Some(FieldGroup::new(
            ADDRESSING_CAPTION,
            alloc::vec![reading(ADDRESSING_FACTS[0], String::from(said))],
        ))
    }

    /// A run whose output is not a listing this build can read.
    fn unreadable() -> Self {
        Self::Refused(String::from(UNREADABLE_LISTING))
    }
}

/// The plates the networking panes draw over `addressing`: one per declared
/// interface holding `keys`, or the single plate that says why there is
/// nothing to show.
///
/// The interfaces come from the **captured** document, so the set of plates
/// is what the reader asked to see and stays put while they edit — clearing
/// an interface's last key drops it from a committed working copy, and a
/// plate that vanished mid-edit would take its own rows with it. Their
/// values come from the working copy.
pub(crate) fn interface_groups(
    addressing: &Addressing,
    staged: &[(IfaceSetting, String)],
    keys: &[IfaceKey],
) -> (Vec<FieldGroup>, Vec<Vec<IfaceSetting>>) {
    if let Some(statement) = addressing.statement() {
        return (alloc::vec![statement], alloc::vec![Vec::new()]);
    }
    let Some(captured) = addressing.document() else {
        return (Vec::new(), Vec::new());
    };
    let mut groups = Vec::with_capacity(captured.interfaces().len());
    let mut settings = Vec::with_capacity(captured.interfaces().len());
    for (index, declared) in captured.interfaces().iter().enumerate() {
        let shown: Vec<IfaceSetting> = keys
            .iter()
            .map(|key| IfaceSetting {
                iface: index,
                key: *key,
            })
            .filter(|setting| settable(setting.key) || setting.held(captured).is_some())
            .collect();
        let rows = shown
            .iter()
            .map(|setting| row_of(*setting, Some(captured), staged))
            .collect();
        let mut group = FieldGroup::new(declared.name.clone(), rows);
        if declared.match_mac.is_none() && declared.match_node.is_none() {
            group = group.with_footnote(UNBINDABLE);
        }
        groups.push(group);
        settings.push(shown);
    }
    (groups, settings)
}

/// The plate the DNS pane states its live reading in: one row per resolver
/// the stack answered with.
pub(crate) fn resolver_group(resolvers: Option<&[NetServerAddr]>) -> FieldGroup {
    let rows = match resolvers {
        None => alloc::vec![reading(SERVER_LABEL, String::from(UNMEASURED))],
        Some([]) => alloc::vec![reading(SERVER_LABEL, String::from(NO_SERVERS))],
        Some(servers) => servers
            .iter()
            .map(|server| reading(SERVER_LABEL, render_server(server)))
            .collect(),
    };
    FieldGroup::new(RESOLVERS_CAPTION, rows)
}

/// One key of one interface: which interface, by its index in the captured
/// document, and which of its settings.
///
/// An index rather than the alias itself because the owner table a form
/// keeps beside its rows is `Copy` and lives alongside the two static
/// stores' settables; the alias it stands for is read back from the
/// document the plates were discovered from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct IfaceSetting {
    /// Which interface of the captured document.
    pub(crate) iface: usize,
    /// Which of its settings.
    pub(crate) key: IfaceKey,
}

impl IfaceSetting {
    /// The alias this setting names, or `None` for an index no longer in
    /// the document the plates were discovered from.
    pub(crate) fn alias(self, captured: &NetworkConfig) -> Option<&str> {
        captured
            .interfaces()
            .get(self.iface)
            .map(|iface| iface.name.as_str())
    }

    /// The full `<iface>.<suffix>` key name this setting writes.
    pub(crate) fn name(self, captured: &NetworkConfig) -> Option<String> {
        let alias = self.alias(captured)?;
        Some(alloc::format!("{alias}.{}", self.key.name()))
    }

    /// What the captured document declares for this setting, spelled as
    /// the store spells it, or `None` where it declares nothing.
    pub(crate) fn held(self, captured: &NetworkConfig) -> Option<String> {
        captured
            .interface(self.alias(captured)?)?
            .render_value(self.key)
    }
}

/// The row `setting` draws: what the reader has made it say, else what the
/// capture declares.
pub(crate) fn row_of(
    setting: IfaceSetting,
    captured: Option<&NetworkConfig>,
    staged: &[(IfaceSetting, String)],
) -> FieldRow {
    let edited = staged
        .iter()
        .find(|(held, _)| *held == setting)
        .map(|(_, value)| value.as_str());
    let value = match edited {
        // The reader cleared it, which is this registry's *remove*.
        Some("") => None,
        Some(text) => Some(String::from(text)),
        None => captured.and_then(|document| setting.held(document)),
    };
    row(setting.key, value)
}

/// Whether the store would take `value` for `key`, where empty is the
/// reader clearing the key rather than a value at all.
pub(crate) fn admits(key: IfaceKey, value: &str) -> bool {
    value.is_empty() || key.admits(value)
}

/// Whether this surface offers `key` as a control rather than a reading.
///
/// Addressing is a settings pane's job. An interface's hardware identity
/// (`kind` and the two `match.*` keys) and a bond's composition are not:
/// one says which device the alias stands for and the other rewires the
/// machine's link layer, and both belong to whoever is holding the cable.
pub(crate) const fn settable(key: IfaceKey) -> bool {
    matches!(
        key,
        IfaceKey::Ipv4Method
            | IfaceKey::Ipv4Address
            | IfaceKey::Ipv4Gateway
            | IfaceKey::Ipv6Method
            | IfaceKey::Ipv6Address
            | IfaceKey::Ipv6Gateway
            | IfaceKey::DnsServers
            | IfaceKey::Mtu
    )
}

/// The reader's name for one per-interface store key.
pub(crate) const fn label(key: IfaceKey) -> &'static str {
    match key {
        IfaceKey::Kind => "Kind",
        IfaceKey::MatchMac => "Bound to hardware address",
        IfaceKey::MatchNode => "Bound to hardware location",
        IfaceKey::Ipv4Method => "IPv4",
        IfaceKey::Ipv4Address => "IPv4 address",
        IfaceKey::Ipv4Gateway => "IPv4 gateway",
        IfaceKey::Ipv6Method => "IPv6",
        IfaceKey::Ipv6Address => "IPv6 address",
        IfaceKey::Ipv6Gateway => "IPv6 gateway",
        IfaceKey::DnsServers => "Name servers",
        IfaceKey::Mtu => "MTU",
        IfaceKey::BondMembers => "Bond members",
        IfaceKey::BondMode => "Bond mode",
        IfaceKey::BondMonitorInterval => "Bond monitor interval",
        IfaceKey::BondPrimary => "Bond primary member",
    }
}

/// The sentence beneath the label: what this setting actually decides.
const fn purpose(key: IfaceKey) -> &'static str {
    match key {
        IfaceKey::Kind => "What kind of link this alias names.",
        IfaceKey::MatchMac => "The hardware address a device must carry to be bound to this alias.",
        IfaceKey::MatchNode => {
            "The position on the bus a device must sit at to be bound to this alias, whatever \
             address it carries."
        }
        IfaceKey::Ipv4Method => "How this interface obtains its IPv4 address.",
        IfaceKey::Ipv4Address => {
            "The address this interface takes while its IPv4 method is static."
        }
        IfaceKey::Ipv4Gateway => {
            "Where this interface sends IPv4 traffic for anywhere off its own network."
        }
        IfaceKey::Ipv6Method => "How this interface obtains its IPv6 address.",
        IfaceKey::Ipv6Address => {
            "The address this interface takes while its IPv6 method is static."
        }
        IfaceKey::Ipv6Gateway => {
            "Where this interface sends IPv6 traffic for anywhere off its own network."
        }
        IfaceKey::DnsServers => {
            "The recursive name servers to use on this interface. They join whatever a lease \
             supplies in the one set the stack resolves through."
        }
        IfaceKey::Mtu => "The largest packet this interface sends in one piece.",
        IfaceKey::BondMembers => "The interfaces this bond transmits over.",
        IfaceKey::BondMode => "How the bond spreads traffic over its members.",
        IfaceKey::BondMonitorInterval => "How often each member is probed for health.",
        IfaceKey::BondPrimary => "The member the bond prefers while it is healthy.",
    }
}

/// What a settable row says beneath its label: its purpose, then what the
/// store will and will not take.
///
/// The accepted form is the key's own, from the shared configuration
/// vocabulary, so the sentence a reader corrects a refused value by and the
/// grammar the engine admits cannot drift.
fn stated(key: IfaceKey) -> String {
    let mut text = String::from(purpose(key));
    match key.shape() {
        ValueShape::Closed(_) => {
            if let Some(default) = fallback(key) {
                text.push_str(" Not set, this interface uses `");
                text.push_str(default);
                text.push_str("`.");
            }
        }
        ValueShape::Free(form) => {
            text.push_str(" Takes ");
            text.push_str(form);
            text.push_str("; empty removes it.");
        }
    }
    text
}

/// The spelling a closed key's interface falls back to while the document
/// does not declare it, read from that key's own default rather than named
/// again here.
fn fallback(key: IfaceKey) -> Option<&'static str> {
    match key {
        IfaceKey::Ipv4Method => Some(Ipv4Method::default().as_str()),
        IfaceKey::Ipv6Method => Some(Ipv6Method::default().as_str()),
        _ => None,
    }
}

/// The row `key` draws holding `value`.
///
/// A closed key is a choice list over the spellings the engine itself
/// admits, led by the one choice the document alone can express: that the
/// key is not declared. An open one is an entry holding what the store
/// holds, where empty is that same absence.
fn row(key: IfaceKey, value: Option<String>) -> FieldRow {
    if !settable(key) {
        return FieldRow::new(label(key), FieldControl::Reading(value.unwrap_or_default()))
            .with_description(purpose(key));
    }
    let control = match key.shape() {
        ValueShape::Closed(values) => {
            let mut combo = ComboBox::new(choices(values));
            combo.set_selected(chosen(values, value.as_deref()));
            FieldControl::Combo(combo)
        }
        ValueShape::Free(_) => FieldControl::Text(
            TextField::new()
                .with_text(value.clone().unwrap_or_default())
                .with_placeholder(UNSET),
        ),
    };
    let refused = value.is_some_and(|held| !admits(key, &held));
    FieldRow::new(label(key), control)
        .with_description(stated(key))
        .with_state(ControlState {
            validation: ValidationState::of(!refused),
            ..ControlState::idle()
        })
}

/// The choices a closed key offers: not declared at all, then the engine's
/// own spellings in its own order.
fn choices(values: &[&str]) -> Vec<String> {
    let mut choices = Vec::with_capacity(values.len().saturating_add(1));
    choices.push(String::from(UNSET));
    choices.extend(values.iter().map(|value| (*value).to_string()));
    choices
}

/// Which of [`choices`] `value` is.
fn chosen(values: &[&str], value: Option<&str>) -> usize {
    let Some(value) = value else {
        return 0;
    };
    values
        .iter()
        .position(|offered| *offered == value)
        .map_or(0, |at| at.saturating_add(1))
}

/// What choosing one of a closed key's offered entries means.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Choice {
    /// The leading entry: the document does not declare the key at all.
    Undeclared,
    /// One of the engine's own spellings.
    Spelled(&'static str),
}

/// What choice `index` of a closed key means.
///
/// Fails closed on an index outside the list this very surface built, so a
/// routing defect stages nothing rather than a value the reader did not
/// choose.
pub(crate) fn choice(values: &[&'static str], index: usize) -> Option<Choice> {
    match index.checked_sub(1) {
        None => Some(Choice::Undeclared),
        Some(at) => values.get(at).copied().map(Choice::Spelled),
    }
}

/// One label-and-reading row.
fn reading(label: &'static str, value: String) -> FieldRow {
    FieldRow::new(label, FieldControl::Reading(value))
}
