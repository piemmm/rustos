//! The desktop's system quick-actions vocabulary: the closed set of
//! commands the Switchboard capsule's context menu offers, and the one
//! ordered table that defines the menu's shape.
//!
//! [`ROWS`] is the single definition of what the menu is: each entry names
//! the command, its label, whether it opens a new visual group, and its
//! role. The rendered rows, the row → command mapping, and the tests all
//! read that table, so a row can never exist without a command behind it (or
//! the reverse).
//!
//! A command that destroys work in progress carries its confirmation
//! requirement in the *type* of the outcome it reports rather than in a
//! column here, so the session cannot apply it without asking first.
//!
//! Nothing here holds or checks authority. A row renders permitted only
//! because a process that *does* hold the authority attested to it, and a
//! row whose backing is absent renders non-actionable with a stated reason
//! rather than being offered and then failing. The one exception is a
//! command the machine does not *have* — fast user switching on a desktop
//! no session authority started — where there is no refusal to explain and
//! the row is left out entirely ([`SystemRow::offered_by`]).

use tairix_abi::switchboard_ipc::CommandSection;
use tairix_abi::PowerAction;
use tairix_controls::{AuthorityState, ControlRole, ControlState, MenuItem, MenuMark};
use tairix_proglib::EntryId;
use tairix_theme::Appearance;

use crate::input::TaskbarResponse;

/// One system quick action the menu can offer.
///
/// A closed set: every variant has a row in [`ROWS`] and a typed outcome the
/// session knows how to apply. There is no "other" or free-form command.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SystemAction {
    /// Show what this machine is, in the Switchboard's overview.
    About,
    /// Show what the machine is doing, in the Switchboard's task list.
    SystemMonitor,
    /// Launch the terminal.
    TaskShell,
    /// Switch the desktop to this appearance.
    Appearance(Appearance),
    /// Secure the screen behind this user's password, leaving the session
    /// and everything running in it untouched.
    Lock,
    /// Step this session aside so somebody else can log in, leaving it live
    /// and resumable.
    SwitchUser,
    /// End this desktop session and return to the login prompt.
    LogOut,
    /// Restart the machine.
    Restart,
    /// Power the machine off.
    ShutDown,
}

/// One row of the system menu: its command, its wording, and its posture.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SystemRow {
    /// The command choosing this row asks for.
    pub action: SystemAction,
    /// The row's label.
    pub label: &'static str,
    /// Whether this row opens a new visual group (a divider is drawn in the
    /// gap above it).
    pub group_break: bool,
    /// The row's role, which decides whether it carries the danger rail.
    pub role: ControlRole,
}

impl SystemRow {
    /// Whether `permits` offers this row at all.
    ///
    /// Only [`SystemAction::SwitchUser`] is conditional, and it is absent
    /// rather than refused: switching away needs a session authority to
    /// step aside *to* and to be woken back by, and a desktop with none has
    /// no such facility to explain the absence of. Every other row is a
    /// thing this desktop plainly can do, so a missing backing is stated as
    /// a reason on a rendered row instead.
    #[must_use]
    pub const fn offered_by(&self, permits: SystemPermits) -> bool {
        !matches!(self.action, SystemAction::SwitchUser) || permits.switch_user_available
    }
}

/// The catalog identifier of the terminal bundle the *Task Shell* row
/// launches.
///
/// The row is only actionable when this bundle is in the catalog the session
/// handed the bar, so choosing it can never ask for a program that is not
/// installed.
pub const TASK_SHELL_BUNDLE: &str = "os.tairix.terminal";

/// The system menu, in order. This is the single definition of the menu's
/// shape; the rendered rows and the row → command mapping are both derived
/// from it, never written out a second time.
///
/// The grouping separates what the rows *do*: inspecting the machine, then
/// changing how it looks, then securing, leaving, or stopping it. The two
/// power rows are destructive and confirmed; nothing above them is. Locking
/// heads the last group because it is the one way out of the session that
/// keeps the session, and switching away follows it as the other —
/// everything below them ends work in progress.
pub const ROWS: &[SystemRow] = &[
    SystemRow {
        action: SystemAction::About,
        label: "About This System",
        group_break: false,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::SystemMonitor,
        label: "System Monitor",
        group_break: false,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::TaskShell,
        label: "Task Shell",
        group_break: false,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::Appearance(Appearance::Light),
        label: "Light Appearance",
        group_break: true,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::Appearance(Appearance::Dark),
        label: "Dark Appearance",
        group_break: false,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::Lock,
        label: "Lock Screen",
        group_break: true,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::SwitchUser,
        label: "Switch User…",
        group_break: false,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::LogOut,
        label: "Log Out",
        group_break: false,
        role: ControlRole::Neutral,
    },
    SystemRow {
        action: SystemAction::Restart,
        label: "Restart",
        group_break: false,
        role: ControlRole::Destructive,
    },
    SystemRow {
        action: SystemAction::ShutDown,
        label: "Shut Down",
        group_break: false,
        role: ControlRole::Destructive,
    },
];

/// Why the appearance already in use cannot be chosen again.
pub const REASON_ALREADY_IN_USE: &str = "Already in use";

/// Why a launch row is offered but cannot act: the desktop found no such
/// bundle installed, so choosing it could only fail.
pub const REASON_NOT_INSTALLED: &str = "Not installed";

/// Why the power rows cannot act: no process has attested that it can
/// perform the transition.
///
/// This is the state before the system-overview service has published, as
/// well as after it has published that it holds no power authority — the
/// desktop treats "not told" and "told no" alike, because neither is
/// permission.
pub const REASON_NO_POWER_AUTHORITY: &str = "The system service cannot power this machine";

/// Why the lock row cannot act: this session has no console whose password
/// prompt could unlock it again.
///
/// Locking a screen that can never be unlocked is a trap, not a security
/// measure, so the row refuses up front rather than stranding the user.
pub const REASON_NO_UNLOCK_PROMPT: &str = "This session has no password prompt to unlock with";

/// What the menu is allowed to offer, as attested by the processes that
/// would actually carry each command out.
///
/// The taskbar holds none of this authority itself; it renders what it was
/// told. Every field defaults to the refusing answer, so a menu built
/// before anything has been attested offers nothing it cannot deliver.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // Independent yes/no attestations, one per backing; naming each is the clarity.
pub struct SystemPermits {
    /// The appearance the desktop is showing right now, so the matching row
    /// is marked as the one in use.
    pub appearance: Appearance,
    /// Whether a process has attested that it holds the authority to
    /// restart or power off this machine.
    pub power: bool,
    /// Whether the terminal bundle the Task Shell row launches is present
    /// in the desktop's catalog.
    pub task_shell_installed: bool,
    /// Whether this session can put a password prompt in front of the
    /// screen — that is, whether it runs on a console whose login
    /// supervisor can be asked to re-verify the signed-in user.
    pub lock_available: bool,
    /// Whether this session can step aside for another user — that is,
    /// whether it holds the wake mailbox a session authority would resume
    /// it through.
    pub switch_user_available: bool,
}

/// The rows `permits` offers, each with its position in [`ROWS`].
///
/// The one filter the rendered rows read. The *position* travels with the row
/// because it is the id an answer names, so a hidden row shifts nothing and
/// the row → command mapping needs no copy of the filter.
fn offered(permits: SystemPermits) -> impl Iterator<Item = (usize, &'static SystemRow)> {
    ROWS.iter()
        .enumerate()
        .filter(move |(_, row)| row.offered_by(permits))
}

/// Build the menu's rows for `permits`, each with its own position in
/// [`ROWS`] — which is the id an answer names, so a row left out shifts no
/// other row's meaning.
///
/// Every offered row is rendered, in [`ROWS`] order: a command whose
/// backing is missing is shown non-actionable with the reason stated, never
/// hidden and never silently offered. Hiding it would leave the user
/// guessing why the desktop cannot do something it plainly should; offering
/// it would promise an action that cannot happen. A facility the machine
/// does not have at all ([`SystemRow::offered_by`]) is the exception and is
/// not rendered.
#[must_use]
pub(crate) fn rows(permits: SystemPermits) -> alloc::vec::Vec<(usize, MenuItem)> {
    offered(permits)
        .map(|(index, row)| {
            let item = MenuItem::new(row.label)
                .with_group_break(row.group_break)
                .with_role(row.role);
            let item = match row.action {
                // The two appearances are a group of alternatives exactly one
                // of which holds, so the one in force is the group's chosen
                // member: a bullet, disabled, with its reason.
                SystemAction::Appearance(choice) if choice == permits.appearance => item
                    .with_mark(MenuMark::Radio)
                    .with_state(ControlState::disabled())
                    .with_reason(REASON_ALREADY_IN_USE),
                SystemAction::TaskShell if !permits.task_shell_installed => item
                    .with_state(ControlState::disabled())
                    .with_reason(REASON_NOT_INSTALLED),
                SystemAction::Lock if !permits.lock_available => item
                    .with_state(
                        ControlState::default().with_authority(AuthorityState::NeedsCapability),
                    )
                    .with_reason(REASON_NO_UNLOCK_PROMPT),
                SystemAction::Restart | SystemAction::ShutDown if !permits.power => item
                    .with_state(
                        ControlState::default().with_authority(AuthorityState::NeedsCapability),
                    )
                    .with_reason(REASON_NO_POWER_AUTHORITY),
                _ => item,
            };
            (index, item)
        })
        .collect()
}

/// What the row at position `index` of [`ROWS`] asks the embedder for, or
/// `None` for a position no row holds (fail closed — never guess at a
/// command).
///
/// Indexed over [`ROWS`] itself rather than over the rows a menu happened to
/// offer, so a row `permits` left out cannot shift the meaning of another.
///
/// The two inspection rows reuse the capsule's own Switchboard-opening
/// response and the launch row reuses the bar's one launch response, so no
/// command here introduces a second path to a destination the bar already
/// reaches.
#[must_use]
pub(crate) fn response_at(index: usize) -> Option<TaskbarResponse> {
    Some(match ROWS.get(index)?.action {
        SystemAction::About => TaskbarResponse::OpenSwitchboard {
            section: CommandSection::System,
        },
        SystemAction::SystemMonitor => TaskbarResponse::OpenSwitchboard {
            section: CommandSection::Tasks,
        },
        // The row is only actionable when this identifier resolved against
        // the catalog, so a refusal here cannot happen through the menu;
        // reporting nothing rather than a launch that must fail is still the
        // honest answer if it ever did.
        SystemAction::TaskShell => TaskbarResponse::LibraryLaunch {
            entry: EntryId::new(TASK_SHELL_BUNDLE).ok()?,
        },
        SystemAction::Appearance(appearance) => TaskbarResponse::SetAppearance { appearance },
        SystemAction::Lock => TaskbarResponse::LockSession,
        SystemAction::SwitchUser => TaskbarResponse::SwitchUser,
        SystemAction::LogOut => TaskbarResponse::LogOut,
        SystemAction::Restart => TaskbarResponse::ConfirmSystemPower {
            action: PowerAction::Restart,
        },
        SystemAction::ShutDown => TaskbarResponse::ConfirmSystemPower {
            action: PowerAction::PowerOff,
        },
    })
}
