//! The read-only fact columns: what this machine *is*, what its clock
//! says, and which name servers it resolves through.
//!
//! None of these panes has a settable in it. About states the machine's
//! identity, its version, how long it has been running, and its processors
//! and memory; Date & Time states the wall clock and where the reading came
//! from, and offers the one command that changes it — which is not a
//! setting at all but a re-authenticated run of the application that owns
//! the clock. DNS states the resolver set the network stack is actually
//! using, which is the statically configured and the DHCP-learned servers
//! aggregated into one answer.
//!
//! Byte counts read in the desktop's own prose ladder — the one the Storage
//! pane's capacities use — rather than the `df` spelling, because a machine's
//! memory is prose here and a column of figures there.
//!
//! Every figure is a measurement the caller took through the System
//! Information API. A reading that did not arrive renders unmeasured; none
//! is derived here, and none is remembered from a previous look.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::net_ipc::NetServerAddr;
use tairix_abi::sysinfo::{CpuInfoRecord, SystemIdentity, Uptime};
use tairix_abi::time::{WallClockReading, WallTimeState};
use tairix_controls::{FieldControl, FieldGroup, FieldLayout, FieldRow};
use tairix_geometry::{Rect, Scale};
use tairix_netconfig::IfaceKey;
use tairix_procinfo::{format_uptime, render_server};
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_util::size::{format_binary, SIZE_TEXT_MAX};

use crate::stack;

/// The label of every About reading, in the order the pane lists them.
pub(crate) const ABOUT_FACTS: &[&str] = &[
    "Name",
    "Machine ID",
    "Version",
    "Uptime",
    "Processor",
    "Cores",
    "Memory",
];

/// The label of every Date & Time reading.
pub(crate) const CLOCK_FACTS: &[&str] = &["Clock", "Set from"];

/// What the Ethernet pane contributes to the search index.
///
/// One term rather than one per interface: the plates are discovered at
/// runtime from a store this application may not read for itself, and a
/// reader searches for the subject.
pub(crate) const ADDRESSING_FACTS: &[&str] = &["Addressing"];

/// What the DNS pane contributes to the search index.
///
/// One term rather than one per server: the rows are discovered at runtime
/// and a reader searches for the subject, not for an address.
pub(crate) const RESOLVER_FACTS: &[&str] = &["Name servers"];

/// The label each name-server row carries.
const SERVER_LABEL: &str = "Name server";

/// What the pane states when the stack holds no resolver at all.
///
/// Distinct from a refused reading: the query answered, and what it
/// answered was an empty set. A machine with no name server resolves
/// nothing, which is worth saying plainly rather than showing an empty
/// plate.
const NO_SERVERS: &str = "none — this machine resolves no names";

/// What a reading states when the caller could not take it.
const UNMEASURED: &str = "not measured";

/// The caption of the one Ethernet plate that states something other than
/// an interface.
const ADDRESSING_CAPTION: &str = "CONFIGURED ADDRESSING";

/// What the Ethernet pane states before anyone has asked.
const NOT_ASKED: &str =
    "not read — this machine's addressing is not public, so reading it needs an account that may";

/// What it states when the store declares no interface at all.
const NO_INTERFACES: &str = "none — no interface is configured on this machine";

/// What it states when the configuration is larger than the reply carries.
///
/// The seam bounds what a program may print back to an unprivileged
/// caller, so a very large document is not shown here at all rather than
/// shown in part.
const TOO_LARGE: &str = "too large to show here — run `configure` from a shell to read it in full";

/// What it states when the run answered something that is not a listing.
const UNREADABLE_LISTING: &str = "the command answered something this window could not read";

/// What the identity states where the installer has not minted one.
const UNPROVISIONED: &str = "not set";

/// The machine readings the About pane draws, as the caller answered them.
///
/// Each is an [`Option`] because each is a separate query: one that was
/// refused or has not landed renders unmeasured rather than borrowing a
/// neighbour's success.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MachineFacts {
    /// The machine's identity and OS version.
    pub identity: Option<SystemIdentity>,
    /// How long the machine has been running.
    pub uptime: Option<Uptime>,
    /// The processors the machine reported, in index order.
    pub cpus: Vec<CpuInfoRecord>,
    /// The machine's total usable RAM, in bytes.
    pub memory_bytes: Option<u64>,
    /// The wall clock and where its reading came from.
    pub clock: Option<WallClockReading>,
}

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

/// What the Ethernet pane knows of the machine's configured addressing.
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
    /// The store's per-interface settings, grouped by interface alias in
    /// the order the document declares them.
    Listed(Vec<InterfaceReading>),
    /// The run happened, but printed more than the seam carries.
    Overran,
    /// Nothing was read, and this is why.
    Refused(String),
}

/// One interface's settings as the store holds them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterfaceReading {
    /// The interface's alias, which is the plate's caption.
    pub alias: String,
    /// Its set keys, as the registry's suffix and the stored value.
    pub settings: Vec<(IfaceKey, String)>,
}

/// Where a fact column's readings were taken from.
///
/// Carried by the column itself so the shell can ask for the reading a
/// pane is about without re-deriving it from the registry: the column
/// knows what it states.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Subject {
    /// Readings of the machine itself, which the machine desk takes.
    Machine,
    /// The live resolver set, which the network desk takes.
    Resolvers,
    /// The configured addressing, which no desk takes on its own: it is
    /// answered only when a reader asks for it under an account that may
    /// read the store.
    Addressing,
}

/// A read-only column of facts: one captioned plate, scrolled like every
/// other plate column.
pub(crate) struct Facts {
    /// One captioned plate each, in listing order.
    groups: Vec<FieldGroup>,
    /// What the column states, which decides whose reading it wants.
    subject: Subject,
    /// The first plate drawn.
    first: usize,
}

impl Facts {
    /// Every plate's rows in order, for a test that asks what the pane
    /// states.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> Vec<tairix_controls::FieldRow> {
        self.groups
            .iter()
            .flat_map(|group| group.rows().iter().cloned())
            .collect()
    }

    /// What this column states.
    pub(crate) const fn subject(&self) -> Subject {
        self.subject
    }

    /// The About column for `facts`.
    pub(crate) fn about(facts: &MachineFacts) -> Self {
        Self::of("THIS MACHINE", Subject::Machine, about_rows(facts))
    }

    /// The Date & Time column for `facts`.
    pub(crate) fn clock(facts: &MachineFacts) -> Self {
        Self::of("THE CLOCK", Subject::Machine, clock_rows(facts))
    }

    /// The DNS column for `facts`.
    pub(crate) fn resolvers(facts: &NetworkFacts) -> Self {
        Self::of("NAME SERVERS", Subject::Resolvers, resolver_rows(facts))
    }

    /// The Ethernet column for `facts`: one plate per configured
    /// interface, or the one plate that says why there is nothing to show.
    pub(crate) fn addressing(facts: &NetworkFacts) -> Self {
        Self {
            groups: addressing_groups(&facts.addressing),
            subject: Subject::Addressing,
            first: 0,
        }
    }

    /// One captioned plate carrying `rows`.
    ///
    /// Read-only rows of the same family every other pane composes, rather
    /// than a second read-only instrument inside the plate: a volume card
    /// already states its facts this way, and one label-and-reading row is
    /// all either needs.
    fn of(caption: &'static str, subject: Subject, rows: Vec<FieldRow>) -> Self {
        Self {
            groups: alloc::vec![FieldGroup::new(caption, rows)],
            subject,
            first: 0,
        }
    }

    /// How many plates the column has.
    pub(crate) fn len(&self) -> usize {
        self.groups.len()
    }

    /// Draw from plate `index`.
    pub(crate) fn set_first(&mut self, index: usize) {
        self.first = index.min(self.len().saturating_sub(1));
    }

    /// The height the column needs.
    pub(crate) fn measured_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let gap = stack::gap(scale, theme);
        self.groups
            .iter()
            .fold(gap.saturating_mul(2), |total, group| {
                total.saturating_add(group.measured_height(scale, theme))
            })
    }

    /// How many plates the column seats from the one it draws from.
    pub(crate) fn seated(&self, bounds: Rect, scale: Scale, theme: &Theme) -> usize {
        self.placed(bounds, scale, theme).len()
    }

    /// Where each drawn plate sits.
    fn placed(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<(usize, Rect)> {
        stack::place(bounds, self.first, self.len(), scale, theme, |index| {
            self.groups
                .get(index)
                .map_or(0, |group| group.measured_height(scale, theme))
        })
    }

    /// Paint the column.
    pub(crate) fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        for (index, rect) in self.placed(bounds, scale, theme) {
            if let Some(group) = self.groups.get(index) {
                group.render(surface, FieldLayout::new(rect, 0), scale, theme);
            }
        }
    }
}

/// The About readings, in listing order.
fn about_rows(facts: &MachineFacts) -> Vec<FieldRow> {
    let identity = facts.identity.as_ref();
    alloc::vec![
        reading(
            ABOUT_FACTS[0],
            identity.map_or_else(unmeasured, |id| {
                let name = core::str::from_utf8(id.hostname_bytes()).unwrap_or("");
                if name.is_empty() {
                    String::from(UNPROVISIONED)
                } else {
                    name.to_string()
                }
            }),
        ),
        reading(ABOUT_FACTS[1], identity.map_or_else(unmeasured, machine_id),),
        reading(ABOUT_FACTS[2], identity.map_or_else(unmeasured, version)),
        reading(
            ABOUT_FACTS[3],
            facts.uptime.map_or_else(unmeasured, |up| format_uptime(
                up.since_boot.saturating_total_nanos()
            )),
        ),
        reading(ABOUT_FACTS[4], processor(&facts.cpus)),
        reading(
            ABOUT_FACTS[5],
            if facts.cpus.is_empty() {
                unmeasured()
            } else {
                facts.cpus.len().to_string()
            },
        ),
        reading(
            ABOUT_FACTS[6],
            facts.memory_bytes.map_or_else(unmeasured, prose_bytes),
        ),
    ]
}

/// The Date & Time readings.
fn clock_rows(facts: &MachineFacts) -> Vec<FieldRow> {
    let clock = facts.clock;
    alloc::vec![
        reading(
            CLOCK_FACTS[0],
            clock.map_or_else(unmeasured, |clock| {
                if clock.state() == WallTimeState::Unset {
                    String::from("not set")
                } else {
                    // The instant itself, in the one spelling `lib/abi`
                    // gives it: rendering a civil date needs the zone
                    // store, which is another pane's subject.
                    alloc::format!("{} s since the epoch", clock.time().secs())
                }
            }),
        ),
        reading(
            CLOCK_FACTS[1],
            clock.map_or_else(unmeasured, |clock| String::from(provenance(clock.state()))),
        ),
    ]
}

/// The name-server readings: one row per server the stack answered with.
///
/// Discovered rather than declared, exactly as the Storage pane's volumes
/// are: the set is whatever the stack holds, so the column is as long as
/// the answer and never a fixed table of slots waiting to be filled.
fn resolver_rows(facts: &NetworkFacts) -> Vec<FieldRow> {
    let Some(servers) = facts.resolvers.as_ref() else {
        return alloc::vec![reading(SERVER_LABEL, unmeasured())];
    };
    if servers.is_empty() {
        return alloc::vec![reading(SERVER_LABEL, String::from(NO_SERVERS))];
    }
    servers
        .iter()
        .map(|server| reading(SERVER_LABEL, render_server(server)))
        .collect()
}

impl Addressing {
    /// The addressing a `configure` listing states.
    ///
    /// The listing carries both registries, so each line is read against
    /// the per-interface one and anything it does not name — every machine
    /// setting, and any line this build does not understand — is dropped
    /// rather than guessed at. The registry is the shared engine's, so this
    /// surface and the tool that printed the lines cannot disagree on what
    /// a key means. Non-UTF-8 output is no listing at all and is refused.
    #[must_use]
    pub fn from_listing(output: &[u8]) -> Self {
        let Ok(text) = core::str::from_utf8(output) else {
            return Self::Refused(String::from(UNREADABLE_LISTING));
        };
        let mut interfaces: Vec<InterfaceReading> = Vec::new();
        for line in text.lines() {
            let Some((name, value)) = line.split_once(' ') else {
                continue;
            };
            let Some((alias, suffix)) = name.split_once('.') else {
                continue;
            };
            if !tairix_netconfig::valid_iface_name(alias) {
                continue;
            }
            let Some(key) = IfaceKey::from_name(suffix) else {
                continue;
            };
            if !interfaces.iter().any(|iface| iface.alias == alias) {
                interfaces.push(InterfaceReading {
                    alias: String::from(alias),
                    settings: Vec::new(),
                });
            }
            if let Some(entry) = interfaces.iter_mut().find(|iface| iface.alias == alias) {
                entry.settings.push((key, String::from(value)));
            }
        }
        Self::Listed(interfaces)
    }
}

/// The Ethernet plates: one per configured interface, or the single plate
/// that states why there is nothing to show.
fn addressing_groups(addressing: &Addressing) -> Vec<FieldGroup> {
    match addressing {
        Addressing::Unasked => alloc::vec![FieldGroup::new(
            ADDRESSING_CAPTION,
            alloc::vec![reading(ADDRESSING_FACTS[0], String::from(NOT_ASKED))],
        )],
        Addressing::Refused(reason) => alloc::vec![FieldGroup::new(
            ADDRESSING_CAPTION,
            alloc::vec![reading(ADDRESSING_FACTS[0], reason.clone())],
        )],
        Addressing::Overran => alloc::vec![FieldGroup::new(
            ADDRESSING_CAPTION,
            alloc::vec![reading(ADDRESSING_FACTS[0], String::from(TOO_LARGE))],
        )],
        Addressing::Listed(interfaces) if interfaces.is_empty() => {
            alloc::vec![FieldGroup::new(
                ADDRESSING_CAPTION,
                alloc::vec![reading(ADDRESSING_FACTS[0], String::from(NO_INTERFACES))],
            )]
        }
        Addressing::Listed(interfaces) => interfaces
            .iter()
            .map(|iface| {
                FieldGroup::new(
                    iface.alias.clone(),
                    iface
                        .settings
                        .iter()
                        .map(|(key, value)| reading(setting_label(*key), value.clone()))
                        .collect(),
                )
            })
            .collect(),
    }
}

/// The reader's name for one per-interface store key.
const fn setting_label(key: IfaceKey) -> &'static str {
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

/// Where a wall-clock reading came from, in a reader's words.
const fn provenance(state: WallTimeState) -> &'static str {
    match state {
        WallTimeState::Unset => "nothing yet",
        WallTimeState::Firmware => "this machine's own clock chip",
        WallTimeState::Trusted => "the network",
        WallTimeState::Adjusted => "someone who set it",
    }
}

/// The machine id as text, or the unprovisioned statement.
fn machine_id(identity: &SystemIdentity) -> String {
    if identity.machine_id.iter().all(|byte| *byte == 0) {
        return String::from(UNPROVISIONED);
    }
    let mut text = String::with_capacity(identity.machine_id.len() * 2);
    for byte in identity.machine_id {
        text.push(hex(byte >> 4));
        text.push(hex(byte & 0x0F));
    }
    text
}

/// One lower-case hexadecimal digit.
const fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

/// The OS version as `major.minor.patch`.
fn version(identity: &SystemIdentity) -> String {
    alloc::format!(
        "{}.{}.{}",
        identity.version_major,
        identity.version_minor,
        identity.version_patch
    )
}

/// The processor the machine reported, or the unmeasured statement.
///
/// The first core's model name: a machine with cores of different classes
/// still has one processor a reader would name, and the core count beside
/// it is what says how many there are.
fn processor(cpus: &[CpuInfoRecord]) -> String {
    let Some(first) = cpus.first() else {
        return unmeasured();
    };
    let name = core::str::from_utf8(first.model_bytes()).unwrap_or("");
    if name.is_empty() {
        return unmeasured();
    }
    name.to_string()
}

/// What a reading the caller could not take says.
fn unmeasured() -> String {
    String::from(UNMEASURED)
}

/// A byte count in the desktop's own prose ladder.
fn prose_bytes(bytes: u64) -> String {
    let mut buf = [0u8; SIZE_TEXT_MAX];
    String::from(format_binary(bytes, &mut buf))
}

/// One label-and-reading row.
fn reading(label: &'static str, value: String) -> FieldRow {
    FieldRow::new(label, FieldControl::Reading(value))
}
