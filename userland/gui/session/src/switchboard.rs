//! Serving the desktop-session half of the Switchboard command channel
//! (`plans/NEW-TASKBAR.md` T11): the two owner-directed requests the
//! monitor's panel raises over other processes' windows
//! ([`SwitchboardRequest::ActivateOwner`] /
//! [`SwitchboardRequest::RestartOwner`]), and the reverse direction — the
//! tray-icon press that opens the panel and the seat's unresponsive-owner
//! report — that the session sends on the service's own command mailbox.
//!
//! Every side effect (raising a window, relaunching a bundle, sending on
//! the mailbox) is an injected seam ([`OwnerWindow`], the `relaunch`
//! closure, [`SwitchboardMailbox`]), so the decisions here — which owner is
//! valid, where an open with no live service is remembered, when a report
//! is worth sending — are pure and host-tested without a running kernel.

use tairix_abi::switchboard_ipc::{
    CommandSection, SeatReport, SwitchboardCommand, SwitchboardRequest, SEAT_REPORT_OWNERS_MAX,
};
use tairix_abi::{Errno, ProcId};
use tairix_controls::Section;
use tairix_wm::{Compositor, WindowId};

use crate::config::SWITCHBOARD_RUN_PATH;
use crate::confirm::Answer;
use crate::launch::LaunchTable;
use crate::shell::DesktopShell;

/// What a successfully served request answers with, beyond the shared
/// status word: nothing (every operation but a publish), or the serving
/// session's own kernel-attested identity a successful publish must carry
/// so the publisher can authenticate the commands this session later sends
/// on its mailbox.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardOutcome {
    /// The plain status frame suffices.
    Plain,
    /// A successful publish additionally carries this identity.
    Published(ProcId),
}

/// A served request refused, and why — carrying both the wire [`Errno`]
/// the caller receives and the `stderr` line the session states in its
/// own house style, so the one refusal decision names both without the
/// pure serving path performing any I/O itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardRefusal {
    /// The caller is not the live Switchboard instance this session
    /// launched.
    Unattested,
    /// The request frame itself failed to decode.
    Malformed(Errno),
    /// An owner-directed operation named an owner this session cannot act
    /// on: [`SwitchboardRequest::ActivateOwner`] naming an owner with no
    /// live window, or [`SwitchboardRequest::RestartOwner`] naming an
    /// owner with no recorded launch.
    UnknownOwner,
}

impl SwitchboardRefusal {
    /// The wire [`Errno`] the caller receives.
    #[must_use]
    pub const fn errno(self) -> Errno {
        match self {
            Self::Unattested => Errno::PermissionDenied,
            Self::Malformed(err) => err,
            Self::UnknownOwner => Errno::NotFound,
        }
    }

    /// The `stderr` diagnosis line, without the leading `desktop: ` prefix
    /// or trailing newline the caller adds in its own house style.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unattested => "switchboard request from an unattested caller refused",
            Self::Malformed(_) => "malformed switchboard request refused",
            Self::UnknownOwner => "switchboard request names an unknown owner",
        }
    }
}

/// The session's live window ownership, as [`serve_switchboard_request`]
/// needs it to validate and act on [`SwitchboardRequest::ActivateOwner`]:
/// whether a kernel task id currently owns a served window on this seat.
pub trait OwnerWindow {
    /// The compositor window of `owner`'s front served window, if it
    /// currently owns one — resolved through the window engine's own
    /// attested ownership records, never a window title or other
    /// app-controlled data.
    fn window_of(&self, owner: u64) -> Option<WindowId>;
}

/// The session side of one served Switchboard request: the desktop state
/// an accepted request may mutate, the launch bookkeeping that both
/// attests the caller and resolves an owner's bundle, and the injected
/// seams its side effects run through.
///
/// Grouping them keeps the serving decision readable as "this session,
/// this caller, this frame" — the state and the seams that act on it
/// travel together instead of as a queue of loose parameters no call site
/// can read. It is borrowed for the duration of a single request and owns
/// nothing: constructing one allocates no memory and starts no work.
pub struct SwitchboardServe<'a> {
    /// The desktop model an accepted request mutates: the tray capsule a
    /// publish drives, the window stack an activation raises within.
    pub shell: &'a mut DesktopShell,
    /// The compositor those model changes are staged against.
    pub compositor: &'a mut Compositor,
    /// The session's launch bookkeeping: both the record the caller is
    /// attested against and the bundle a restart is resolved through.
    pub launched: &'a mut LaunchTable,
    /// The live window ownership an activation is validated against.
    pub owner_windows: &'a dyn OwnerWindow,
    /// The session's one attested spawn-and-record path, which a restart
    /// re-enters rather than launching by a second route.
    pub relaunch: &'a mut dyn FnMut(&mut LaunchTable, &str, &str),
    /// This session's own kernel-attested identity, which a successful
    /// publish answers with so the publisher can authenticate the commands
    /// the session later sends on its mailbox.
    pub self_proc_id: ProcId,
}

/// Attest the caller against the live Switchboard launch-table entry,
/// decode the request fail-closed, and serve it.
///
/// Only the Switchboard child this session spawned (`caller_pid`, the
/// kernel-attested `call_peer_origin` pid, never a wire claim) may call:
/// anything else is [`SwitchboardRefusal::Unattested`] and never mutates
/// the model. `PublishSummary` relays the summary to the taskbar capsule;
/// `ActivateOwner` raises `owner`'s front window, resolved through the
/// live window ownership; `RestartOwner` resolves `owner` to the bundle
/// the launch table recorded it as and re-launches it through the
/// session's own attested launch path. Either owner-directed operation
/// naming an owner this session cannot act on is
/// [`SwitchboardRefusal::UnknownOwner`].
///
/// # Errors
///
/// The [`SwitchboardRefusal`] naming why the request was refused; the
/// model is never mutated on a refusal.
pub fn serve_switchboard_request(
    serve: SwitchboardServe<'_>,
    caller_pid: u64,
    request: &[u8],
) -> Result<SwitchboardOutcome, SwitchboardRefusal> {
    let SwitchboardServe {
        shell,
        compositor,
        launched,
        owner_windows,
        relaunch,
        self_proc_id,
    } = serve;
    if launched.running_from(SWITCHBOARD_RUN_PATH) != Some(caller_pid) {
        return Err(SwitchboardRefusal::Unattested);
    }
    let request = SwitchboardRequest::from_bytes(request).map_err(SwitchboardRefusal::Malformed)?;
    match request {
        SwitchboardRequest::PublishSummary { summary } => {
            shell.set_tray_summary(compositor, Some(summary));
            Ok(SwitchboardOutcome::Published(self_proc_id))
        }
        SwitchboardRequest::ActivateOwner { owner } => {
            let window = owner_windows
                .window_of(owner)
                .ok_or(SwitchboardRefusal::UnknownOwner)?;
            let _ = shell.raise_window(compositor, window);
            Ok(SwitchboardOutcome::Plain)
        }
        SwitchboardRequest::RestartOwner { owner } => {
            let (run_path, label) = {
                let app = launched
                    .get(owner)
                    .ok_or(SwitchboardRefusal::UnknownOwner)?;
                (app.run_path.clone(), app.label.clone())
            };
            relaunch(launched, &run_path, &label);
            Ok(SwitchboardOutcome::Plain)
        }
    }
}

/// One non-blocking send of a command to a live Switchboard instance's own
/// mailbox: `ipc_send` in production, a recording fake under test. The
/// desktop never spins or blocks waiting for the panel to catch up, so a
/// refused send (a full mailbox, or an instance that has started but not
/// yet bound one) is the implementation's own `stderr` diagnosis to make,
/// never a reason for the caller to retry in a loop.
pub trait SwitchboardMailbox {
    /// Send `command` to the instance named by `pid`, answering whether
    /// the instance's mailbox took it.
    ///
    /// The answer is load-bearing rather than advisory: a spawned instance
    /// is not a *ready* one, so a caller relaying a user's gesture must be
    /// able to tell a delivered command from one that fell on the floor
    /// and act on the difference.
    #[must_use]
    fn send(&mut self, pid: u64, command: SwitchboardCommand) -> bool;
}

/// The wire section naming the panel section the taskbar's own gesture
/// asked for — a quick press on the capsule opening the overview, a hold
/// opening recovery — so the section the user asked for is the section the
/// panel opens on, decided once in the bar and relayed unchanged.
#[must_use]
pub const fn command_section(section: Section) -> CommandSection {
    match section {
        Section::Tasks => CommandSection::Tasks,
        Section::Jobs => CommandSection::Jobs,
        Section::Pressure => CommandSection::Pressure,
        Section::Activities => CommandSection::Activities,
        Section::Recovery => CommandSection::Recovery,
        Section::Overview => CommandSection::Overview,
    }
}

/// Handle the taskbar's request to open the Switchboard window at
/// `section`: send `OpenPanel` to the live instance named by `live`, or —
/// when none is live, or the live one could not take the command —
/// remember the section as the one pending open (replacing any earlier
/// one, never queueing a second) for the next publish to deliver, spawning
/// a fresh instance through `revive` when there is none to publish.
///
/// Returns the pid of an instance `revive` spawned, if it spawned one.
///
/// The gesture itself is the demand for a live instance, so it is never
/// left unanswered merely because none happened to be running yet — nor,
/// crucially, because one had been *spawned* but was still starting. A
/// process exists from the moment it is spawned but binds its command
/// mailbox only once its program runs, a gap of whole seconds while a
/// bundle loads, and a press landing in that gap used to vanish: the send
/// was refused and nothing remembered it. Holding the refused gesture as
/// the pending open closes that window without a retry loop, because the
/// instance's own first publish — which it can only make once it is up —
/// carries it through.
pub fn open_tray(
    pending_open: &mut Option<CommandSection>,
    section: CommandSection,
    live: Option<u64>,
    mailbox: &mut dyn SwitchboardMailbox,
    revive: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    if let Some(pid) = live {
        if mailbox.send(pid, SwitchboardCommand::OpenPanel { section }) {
            return None;
        }
        // The instance is alive but not listening yet. Hold the gesture
        // rather than reviving: a second instance would not be the one the
        // launch table names, and this one is about to publish.
        *pending_open = Some(section);
        return None;
    }
    *pending_open = Some(section);
    revive()
}

/// Deliver the pending open, if any, to `pid`'s mailbox on a successful
/// publish — one command per publish, and the pending open is cleared so
/// it is never re-sent on a later publish from the same or any other
/// instance.
///
/// A publish proves the instance is up and attested, so a send refused
/// here can only be back-pressure from a mailbox it has not drained. The
/// gesture is put back rather than dropped: the next publish delivers it,
/// which is one attempt per publish the instance itself paces, never a
/// retry loop of the desktop's own.
pub fn deliver_pending_open(
    pending_open: &mut Option<CommandSection>,
    pid: u64,
    mailbox: &mut dyn SwitchboardMailbox,
) {
    if let Some(section) = pending_open.take() {
        if !mailbox.send(pid, SwitchboardCommand::OpenPanel { section }) {
            *pending_open = Some(section);
        }
    }
}

/// Relay a **confirmed** power transition to the live Switchboard instance
/// named by `live` — the one process that requested the authority to
/// perform it. The desktop holds no power capability of its own, so it
/// asks the holder rather than acting.
///
/// A declined prompt relays nothing, so the send can only ever follow a
/// deliberate confirmation. With no live instance there is no holder to
/// ask: nothing is sent and the caller is handed the reason to state.
///
/// Returns the `stderr` diagnosis line — without the leading `desktop: `
/// prefix or trailing newline the caller adds in its own house style —
/// when a confirmed transition could not be relayed, and `None` when the
/// command was sent or the user declined.
///
/// A confirmed shutdown that silently went nowhere is the worst possible
/// outcome of this prompt, so a refused send is reported exactly like an
/// absent holder rather than passing for success.
#[must_use]
pub fn relay_power(
    answer: Answer,
    live: Option<u64>,
    mailbox: &mut dyn SwitchboardMailbox,
) -> Option<&'static str> {
    let Answer::Confirmed(action) = answer else {
        return None;
    };
    let Some(pid) = live else {
        return Some("system service is not running; nothing was done");
    };
    if mailbox.send(pid, SwitchboardCommand::Power { action }) {
        return None;
    }
    Some("system service did not accept the request; nothing was done")
}

/// Fold one responsiveness-tracker change into a seat-report send: sends
/// only when `changed` is true (the tracker's own change latch) and a
/// Switchboard instance is live — never per frame, never polled — and
/// always carries the truthful `total` even when `owners` names fewer than
/// that (bounded to [`SEAT_REPORT_OWNERS_MAX`]).
///
/// A `total`/`owners` pair the wire [`SeatReport`] itself would refuse (a
/// contradictory count, a duplicate or reserved owner id) is a session-side
/// invariant violation rather than a caller mistake to recover from: the
/// send is dropped rather than shipping a report this session cannot
/// vouch for.
///
/// Unlike a user's gesture, a refused report is genuinely nothing to hold:
/// it is an observation of a state the tracker still holds, so the next
/// change re-sends a *fresher* one. Re-delivering this stale one later
/// would be worse than dropping it.
pub fn maybe_send_seat_report(
    changed: bool,
    live: Option<u64>,
    total: u16,
    owners: &[u64],
    mailbox: &mut dyn SwitchboardMailbox,
) {
    if !changed {
        return;
    }
    let Some(pid) = live else {
        return;
    };
    let bounded = &owners[..owners.len().min(SEAT_REPORT_OWNERS_MAX)];
    if let Ok(report) = SeatReport::new(total, bounded) {
        let _ = mailbox.send(pid, SwitchboardCommand::SeatReport { report });
    }
}
