//! The typed readings the System section renders: its pages, its four
//! header readings, and the per-page bodies behind them.
//!
//! Every reading here is either a real measurement or an explicit
//! statement that there is none. A missing figure is carried as
//! [`Unmeasured`], which names *why* it is missing, so the surface can
//! say "not permitted" where the caller's authority stops and
//! "unavailable" where the reading is permitted but the service could not
//! answer — two facts a reader must never see conflated into one blank.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{ControlRole, PressureKind};

use crate::sample::Absence;
use crate::view::UNMEASURED_READING;

/// Why a reading is not shown, in the words the surface uses.
///
/// The view renders this text beside the unmeasured mark, so a reader can
/// tell a refusal from a fault without opening a log. It is a display
/// vocabulary, not an authority decision: the service has already made
/// that decision and is reporting it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Unmeasured {
    /// The capability gating the reading is outside this session's
    /// ceiling, so the query was never issued and never will be.
    NotPermitted,
    /// The reading is permitted, but the service could not answer it.
    Unavailable,
    /// No interface exists to ask the question at all — no query, no
    /// syscall, no service. Distinct from the two above because neither a
    /// grant nor a retry would produce the figure.
    NoInterface,
}

impl Unmeasured {
    /// The short phrase the surface prints after the unmeasured mark.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Unmeasured::NotPermitted => "not permitted",
            Unmeasured::Unavailable => "unavailable",
            Unmeasured::NoInterface => "no interface",
        }
    }

    /// The display reason for a [`Absence`] the sample already resolved,
    /// so the view never re-derives a verdict the service reached.
    #[must_use]
    pub const fn from_absence(absence: Absence) -> Self {
        match absence {
            Absence::NotPermitted => Unmeasured::NotPermitted,
            Absence::Unavailable => Unmeasured::Unavailable,
        }
    }
}

/// A statement that `subject` is absent, in the one wording the whole
/// product uses.
///
/// Every screen has something it cannot show — a list with no registry
/// behind it, a page with no interface to ask, a reading outside this
/// session's ceiling — and each must say so rather than render an empty
/// space a reader would read as "nothing is wrong". Wording that once,
/// here, is what stops each section inventing its own phrasing for the
/// same refusal, and keeps "not permitted", "unavailable" and "no
/// interface" three visibly different statements.
#[must_use]
pub fn absence_statement(subject: &str, reason: Unmeasured) -> String {
    alloc::format!("{UNMEASURED_READING}: {subject} — {}", reason.reason())
}

/// What a detail pane says while its master list has nothing selected.
///
/// `subject` names the thing to pick, with its article ("a fault", "a
/// cause", "an activity"), so the sentence reads naturally in every
/// section. Wording it once here is what keeps three master/detail panes
/// from each inventing their own phrasing for the same empty state, and it
/// is a prompt rather than an absence: nothing is missing, the reader has
/// simply not chosen yet, so it never wears the unmeasured mark.
#[must_use]
pub fn selection_prompt(subject: &str) -> String {
    alloc::format!("Select {subject} to see its detail.")
}

/// A reading that is either measured or explicitly not.
///
/// Used wherever a figure may be missing, so no call site can accidentally
/// render an absent value as an empty string, a dash, or a fabricated
/// zero — the type makes the honest form the only reachable one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reading {
    /// A real measurement, already formatted for display.
    Measured(String),
    /// No measurement, and why.
    Absent(Unmeasured),
}

impl Reading {
    /// A measured reading from `text`.
    #[must_use]
    pub fn measured(text: impl Into<String>) -> Self {
        Reading::Measured(text.into())
    }

    /// The measured text, or [`None`] when the reading is absent.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Reading::Measured(text) => Some(text.as_str()),
            Reading::Absent(_) => None,
        }
    }

    /// Why this reading is absent, or [`None`] when it is measured.
    #[must_use]
    pub const fn absence(&self) -> Option<Unmeasured> {
        match self {
            Reading::Measured(_) => None,
            Reading::Absent(reason) => Some(*reason),
        }
    }
}

/// A reading as the text a fact or tile shows: the measurement itself, or
/// the unmeasured mark followed by why there is none.
///
/// The one place a [`Reading`] becomes display text, so every section that
/// carries one — System's pages, Recovery's facts and impact tiles,
/// Pressure's detail, Activities' combined totals — renders the same
/// absent reading identically rather than each spelling its own variant of
/// the unmeasured mark.
#[must_use]
pub fn reading_text(reading: &Reading) -> String {
    match reading {
        Reading::Measured(text) => text.clone(),
        Reading::Absent(reason) => {
            alloc::format!("{UNMEASURED_READING} — {}", reason.reason())
        }
    }
}

/// One page of the System section, selected from the sidebar.
///
/// The pages are a fixed, ordered set rather than a data-driven list: each
/// has its own body type and its own layout, so a page that is not in this
/// enum has nothing to draw.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SystemPage {
    /// The machine's identity, its services, and its permissions summary.
    Overview,
    /// Per-core load and the memory/kernel-heap detail.
    Resources,
    /// The mounted volumes, their capacity and their measured health.
    Storage,
    /// Per-interface facts, link state, addresses and throughput.
    Network,
    /// The machine's seats and its logged-in census.
    Session,
    /// What this session may do, and its resource limits.
    Permissions,
    /// The service inventory — no interface exists to enumerate it.
    Services,
    /// Power state and control — no interface exists to read or drive it.
    Power,
}

impl SystemPage {
    /// The pages in sidebar order.
    pub const ALL: [SystemPage; 8] = [
        SystemPage::Overview,
        SystemPage::Resources,
        SystemPage::Storage,
        SystemPage::Network,
        SystemPage::Session,
        SystemPage::Permissions,
        SystemPage::Services,
        SystemPage::Power,
    ];

    /// The page's sidebar label.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            SystemPage::Overview => "Overview",
            SystemPage::Resources => "Resources",
            SystemPage::Storage => "Storage",
            SystemPage::Network => "Network",
            SystemPage::Session => "Session",
            SystemPage::Permissions => "Permissions",
            SystemPage::Services => "Services",
            SystemPage::Power => "Power",
        }
    }

    /// The page's zero-based sidebar index.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            SystemPage::Overview => 0,
            SystemPage::Resources => 1,
            SystemPage::Storage => 2,
            SystemPage::Network => 3,
            SystemPage::Session => 4,
            SystemPage::Permissions => 5,
            SystemPage::Services => 6,
            SystemPage::Power => 7,
        }
    }

    /// The page at a sidebar index, or [`None`] out of range — a lookup
    /// that fails closed rather than wrapping onto an unrelated page.
    #[must_use]
    pub fn from_index(index: usize) -> Option<SystemPage> {
        SystemPage::ALL.get(index).copied()
    }
}

/// One labelled fact: a name and the reading behind it.
///
/// The one row shape every fact list on this screen uses, so a fact that
/// is measured and one that is not are laid out identically and a reader's
/// eye does not have to re-learn the page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemFact {
    /// What the fact is called (e.g. "Hostname").
    pub label: String,
    /// Its reading, measured or explicitly absent.
    pub value: Reading,
}

impl SystemFact {
    /// A fact named `label` reading `value`.
    #[must_use]
    pub fn new(label: impl Into<String>, value: Reading) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

/// One mounted volume, as the Storage page states it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageVolume {
    /// The backing device or source the volume is mounted from.
    pub source: String,
    /// Where it is mounted in the storage namespace.
    pub mount_point: String,
    /// The filesystem driving it.
    pub filesystem: String,
    /// The medium it lives on (e.g. rotational, solid state).
    pub medium: String,
    /// Whether the volume is available, in the mount table's own words.
    pub availability: String,
    /// Used and total capacity, derived from the volume's block counts.
    pub capacity: Reading,
    /// The volume's measured I/O health, or why it is not known — a
    /// failing disk is exactly what a reader opens this page for.
    pub health: Reading,
    /// The health's severity, so a fault is emphasised rather than being
    /// one more grey line.
    pub health_state: HealthSeverity,
}

/// How badly a volume is faring, as the mount table reports it.
///
/// Its own three-state vocabulary rather than a task's recovery posture: a
/// disk is not a process, and borrowing a type whose other states can never
/// arise here would leave unreachable cases for a reader to puzzle over.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum HealthSeverity {
    /// The volume is available and reports no fault.
    #[default]
    Healthy,
    /// The volume is serving, but degraded or recovering.
    Degraded,
    /// The volume is unavailable: dirty, lost, or in recovery conflict.
    Failing,
}

/// One network interface, as the Network page states it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkInterface {
    /// The interface's name.
    pub name: String,
    /// Its fixed facts: hardware address, MTU and kind.
    pub facts: Vec<SystemFact>,
    /// Whether the link is up, or why that is not known.
    pub link: Reading,
    /// Its configured addresses, each already formatted with its prefix
    /// length. Empty is a real answer — an interface with no address —
    /// and the view says so rather than leaving a gap.
    pub addresses: Vec<String>,
    /// Why the address list is absent, when it is not merely empty.
    pub addresses_absent: Option<Unmeasured>,
    /// Receive throughput.
    pub rx: Reading,
    /// Transmit throughput.
    pub tx: Reading,
}

/// One configured seat, as the Session page states it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSeat {
    /// The seat's identifier.
    pub name: String,
    /// Who owns it, or that it is unowned.
    pub owner: Reading,
    /// Which console it holds in the foreground.
    pub console: Reading,
}

/// One resource limit and its live usage, as the Permissions page states
/// it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitRow {
    /// The limit's name.
    pub name: String,
    /// The soft bound this session runs under.
    pub soft: String,
    /// The hard ceiling it may not raise without authority.
    pub hard: String,
    /// Live usage against the bound.
    pub usage: Reading,
}

/// One system-level action, shown in this section's action rail.
///
/// The section commands one subject — the machine — so its actions belong
/// to the screen rather than to any row, and each states its own verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemAction {
    /// The action's label (e.g. "Lock", "Shut Down").
    pub label: String,
    /// The action's role; it drives the button's emphasis.
    pub role: ControlRole,
    /// Whether the caller may perform the action (fail closed when
    /// false).
    pub allowed: bool,
    /// Why the action cannot be taken, when it cannot. `None` for an
    /// allowed action; a denied one distinguishes "your authority stops
    /// here" from "there is nothing to drive".
    pub refusal: Option<Unmeasured>,
}

/// The whole System screen's readings, assembled by the service and
/// rendered by the view.
///
/// One value carries every page, so the view never has to ask the service
/// a second question mid-render and a page can never show a figure from a
/// different sample than the header above it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemReport {
    /// The four header readings, in the fixed order CPU, Memory, Disk,
    /// Network.
    pub headline: Vec<HeadlineTile>,
    /// The machine's identity facts, shown on the Overview page.
    pub machine: Vec<SystemFact>,
    /// The permissions summary line the Overview page carries.
    pub authority: Vec<SystemFact>,
    /// Per-core load, shown on the Resources page.
    pub cores: Vec<SystemFact>,
    /// The memory and kernel-heap detail, shown on the Resources page.
    pub memory: Vec<SystemFact>,
    /// What the desktop's last composited frame cost, shown on the
    /// Resources page.
    pub compositor: Vec<SystemFact>,
    /// The mounted volumes.
    pub volumes: Vec<StorageVolume>,
    /// Why the mount table is absent, when it is.
    pub volumes_absent: Option<Unmeasured>,
    /// The network interfaces.
    pub interfaces: Vec<NetworkInterface>,
    /// Why the interface inventory is absent, when it is.
    pub interfaces_absent: Option<Unmeasured>,
    /// The machine's seats.
    pub seats: Vec<SessionSeat>,
    /// Why the seat list is absent, when it is.
    pub seats_absent: Option<Unmeasured>,
    /// The logged-in census the load reading carries.
    pub census: Vec<SystemFact>,
    /// This session's resource limits and their live usage.
    pub limits: Vec<LimitRow>,
    /// Why the limit report is absent, when it is.
    pub limits_absent: Option<Unmeasured>,
    /// The system actions the rail offers.
    pub actions: Vec<SystemAction>,
}

/// One header reading: the four tiles across the top of the screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlineTile {
    /// The tile's name (e.g. "CPU").
    pub name: String,
    /// Its reading, measured or explicitly absent.
    pub value: Reading,
    /// The unit the reading is in, shown beside it. Empty when the
    /// reading's own text already carries its unit.
    pub unit: String,
    /// A second line of context under the reading (e.g. the CPU model, or
    /// how much of a volume is free).
    pub detail: Reading,
    /// Which resource this is, mapping to its semantic emphasis.
    pub kind: PressureKind,
    /// Whether the service's own pressure latch is under for this
    /// resource, so a strained reading is emphasised rather than being one
    /// number among four.
    pub pressured: bool,
    /// The tile's instrument: a trend plots recent history, a track fills
    /// a proportional bar.
    pub instrument: TileInstrument,
}

/// Which instrument a header tile draws under its reading.
///
/// CPU and Network are rates whose shape over time is the useful reading,
/// so they trend; Memory and Disk are fractions of a fixed whole, so they
/// track. The choice belongs to the reading, not to the renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TileInstrument {
    /// A sparkline over the recent history, in permille, oldest first.
    Trend(Vec<u16>),
    /// A proportional bar at this permille fraction, or an unmeasured
    /// track when the fraction is not known.
    Track(Option<u16>),
}
