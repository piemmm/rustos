//! What the Resources section is about: one device per pane, and the rail
//! entry that reaches it (`plans/NEW-SWITCHBOARD.md` S4).
//!
//! A device is whatever discovery reports — a processor, the machine's RAM,
//! a mounted volume, a managed interface, the display path — plus the
//! `Machine` group's three fact panes. Nothing here is a class: the rail
//! grows with the machine, so twelve cores, four volumes and three
//! interfaces need no redesign and neither does the fifth disk.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::sysinfo::MOUNT_VOLUME_ID_LEN;
use tairix_controls::{ControlRole, PressureKind};

use super::pane::{PaneBlock, PaneHero};
use crate::view::reading::Unmeasured;
use crate::view::ActionVerdict;

/// Which group of the device rail an entry sits in.
///
/// The order is the rail's order, and a group heading is drawn by the entry
/// that *starts* its group, so a heading can never point at a group with no
/// entries in it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DeviceGroup {
    /// The processor and the machine's memory.
    Resources,
    /// One entry per mounted volume.
    Storage,
    /// One entry per managed interface.
    Network,
    /// The display path.
    Graphics,
    /// The machine itself: its identity, its seats, its authority.
    Machine,
}

impl DeviceGroup {
    /// The rail's quiet group heading.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            DeviceGroup::Resources => "RESOURCES",
            DeviceGroup::Storage => "STORAGE",
            DeviceGroup::Network => "NETWORK",
            DeviceGroup::Graphics => "GRAPHICS",
            DeviceGroup::Machine => "MACHINE",
        }
    }
}

/// A device's own stable identity, which the selection remembers.
///
/// A rail position would silently re-point at a different device the moment
/// one above it went away, so the selection is remembered as the subject
/// itself and re-resolved against each fresh sample. The volume and
/// interface variants carry the identity their own report keys on — a
/// volume id and an interface name — never a rail index.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DeviceId {
    /// The processor.
    Cpu,
    /// The machine's memory.
    Memory,
    /// One mounted volume, by its volume id.
    Volume([u8; MOUNT_VOLUME_ID_LEN]),
    /// One managed interface, by its NUL-padded name.
    Interface([u8; IF_NAME_LEN]),
    /// The display path.
    Graphics,
    /// The machine's identity and uptime.
    Identity,
    /// The machine's seats and its logged-in census.
    Sessions,
    /// What this session may do, and its limits.
    Authority,
}

/// A command the Resources section can invoke on the selected device.
///
/// Each variant names something the service can genuinely carry out or an
/// absence the rail states plainly; a command with no endpoint behind it is
/// declared with [`Unmeasured::NoInterface`] so it renders disabled rather
/// than pretending to work.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResourceControl {
    /// Show the Tasks table ordered by this device's own cost, so a busy
    /// device is traced to the tasks on it.
    SortTasksBy(TaskCostColumn),
    /// Drive the relief the pressure model recommends for this resource.
    Relieve,
    /// Put this device's readings on the clipboard.
    CopyReadings,
    /// Run this volume's integrity scrub.
    Scrub,
    /// Discard this volume's unused blocks.
    Trim,
    /// Detach this volume.
    Unmount,
    /// Renew this interface's address lease.
    RenewLease,
    /// Take this interface down.
    InterfaceDown,
    /// Show the compositor's per-frame damage.
    DamageOverlay,
    /// Lock the screen.
    Lock,
    /// End this session.
    LogOut,
    /// Restart the machine.
    Restart,
    /// Shut the machine down.
    ShutDown,
}

/// Which Tasks column a device's "sort tasks by" command orders on.
///
/// Named by the *cost* rather than by a column index so the request cannot
/// drift out of step with the table's own column order.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskCostColumn {
    /// The Tasks table's CPU column.
    Cpu,
    /// The Tasks table's Memory column.
    Memory,
    /// The Tasks table's Disk column.
    Disk,
}

/// One command the device rail offers, with its own verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAction {
    /// What the command does.
    pub control: ResourceControl,
    /// Its label.
    ///
    /// A Resources command is labelled rather than glyphed: the vocabulary
    /// these panes need — scrub, trim, renew a lease, drop a cache — has no
    /// shipped glyph, and an icon without its own built-in artwork is not an
    /// icon this desktop may draw.
    pub label: String,
    /// The weight the plate carries.
    pub role: ControlRole,
    /// Whether the caller may take it, and how a refusal reads.
    pub verdict: ActionVerdict,
}

impl DeviceAction {
    /// A command the service carries out.
    #[must_use]
    pub fn ready(control: ResourceControl, label: &str) -> Self {
        Self {
            control,
            label: String::from(label),
            role: ControlRole::Neutral,
            verdict: ActionVerdict::Ready,
        }
    }

    /// A command with no endpoint behind it, stated plainly rather than
    /// offered.
    ///
    /// Plainly disabled rather than marked for authority: acquiring a
    /// capability would not make an absent endpoint appear, so the Authority
    /// Mark would send a reader to ask for a grant that changes nothing.
    #[must_use]
    pub fn absent(control: ResourceControl, label: &str) -> Self {
        Self {
            verdict: ActionVerdict::DisabledByState,
            ..Self::ready(control, label)
        }
    }

    /// This command with `role`'s weight.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }
}

/// The banner a resource under pressure wears above its own hero.
///
/// A cause and its resource were never two places, so the pressure model's
/// band, how long it has stood there and the relief it recommends are drawn
/// on the pane the reading is about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PressureBanner {
    /// The band's own name, as the pill states it.
    pub band: String,
    /// What has happened, in one line.
    pub summary: String,
    /// What the model recommends, and what reclaim has recovered.
    pub detail: String,
    /// The relief the model recommends, or [`None`] where it recommends
    /// nothing — which the banner says rather than volunteering another
    /// command.
    pub relief: Option<DeviceAction>,
}

/// One device: its rail entry, its pane, and its commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceDevice {
    /// The device's own identity, which the selection remembers.
    pub id: DeviceId,
    /// Which rail group it sits in.
    pub group: DeviceGroup,
    /// Its name, as the rail entry and the pane header state it.
    pub name: String,
    /// Its identity colour, which tints its instruments.
    pub kind: PressureKind,
    /// The rail entry's trailing reading.
    pub reading: super::super::reading::Reading,
    /// The rail entry's own bounded trace, oldest first, in permille.
    ///
    /// Empty for a `Machine` entry: those are facts, not rates, and the
    /// absence of an instrument is what says so.
    pub trend: Vec<u16>,
    /// The pane's headline reading and its instrument.
    pub hero: PaneHero,
    /// The pane's own blocks, in reading order.
    pub blocks: Vec<PaneBlock>,
    /// The banner this device wears, when it is under pressure.
    pub banner: Option<PressureBanner>,
    /// The commands the rail offers for it.
    pub actions: Vec<DeviceAction>,
}

/// Everything the Resources section draws: one device per pane, in rail
/// order, and why the rail is short of a group when it is.
///
/// One value carries every pane, so the view never asks the service a
/// second question mid-render and a pane can never show a figure from a
/// different sample than the rail entry beside it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResourceReport {
    /// The devices, in rail order.
    pub devices: Vec<ResourceDevice>,
    /// Why the mount table produced no `Storage` entries, when that is a
    /// refusal rather than a machine with nothing mounted.
    pub volumes_absent: Option<Unmeasured>,
    /// Why the interface inventory produced no `Network` entries, when that
    /// is a refusal rather than a machine with no interfaces.
    pub interfaces_absent: Option<Unmeasured>,
}
