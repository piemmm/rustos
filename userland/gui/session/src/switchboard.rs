//! Serving the desktop-session half of the Switchboard command channel
//! (`plans/NEW-TASKBAR.md` T11): the two owner-directed requests the
//! monitor's panel raises over other processes' windows
//! ([`SwitchboardRequest::ActivateOwner`] /
//! [`SwitchboardRequest::RestartOwner`]), and the reverse direction — the
//! tray-icon press that opens the panel, the seat's unresponsive-owner
//! report, and what the last composited frame cost — that the session sends
//! on the service's own command mailbox.
//!
//! Every side effect (raising a window, relaunching a bundle, sending on
//! the mailbox) is an injected seam ([`OwnerWindow`], the `relaunch`
//! closure, [`SwitchboardMailbox`]), so the decisions here — which owner is
//! valid, where an open with no live service is remembered, when a report
//! is worth sending — are pure and host-tested without a running kernel.

use tairix_abi::switchboard_ipc::{
    CommandSection, FrameReport, SeatReport, SwitchboardCommand, SwitchboardRequest,
    SEAT_REPORT_OWNERS_MAX,
};
use tairix_abi::{Errno, ProcId};
use tairix_log::EventId;
use tairix_wm::{Compositor, WindowId};

use crate::config::SWITCHBOARD_RUN_PATH;
use crate::confirm::Answer;
use crate::launch::LaunchTable;
use crate::shell::DesktopShell;
use crate::switchuser::park_within;

/// What a successfully served request answers with, beyond the shared
/// status word: nothing (every operation but a publish), or — for a
/// publish — the two identities the session and the publisher then hold
/// of each other.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardOutcome {
    /// The plain status frame suffices.
    Plain,
    /// A successful publish.
    Published {
        /// This session's own kernel-attested identity, answered so the
        /// publisher can authenticate the commands the session later
        /// sends it.
        session: ProcId,
        /// The attested caller that made the publish, which is by
        /// definition the instance the session can talk to: it is
        /// running, it holds the session's endpoint, and it just proved
        /// both. The session tracks it from here rather than deriving a
        /// live instance from the launch table.
        publisher: u64,
    },
}

/// A served request refused, and why — carrying both the wire [`Errno`]
/// the caller receives and the `stderr` line the session states in its
/// own house style, so the one refusal decision names both without the
/// pure serving path performing any I/O itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardRefusal {
    /// The caller holds no launch record of this session's own for the
    /// Switchboard bundle.
    Unattested,
    /// The request frame itself failed to decode.
    Malformed(Errno),
    /// An owner-directed operation named an owner this session cannot act
    /// on: [`SwitchboardRequest::ActivateOwner`] naming an owner with no
    /// live window, or [`SwitchboardRequest::RestartOwner`] naming an
    /// owner with no recorded launch.
    UnknownOwner,
}

/// A Switchboard request was refused. An access decision on the session's
/// own command channel, so it is stated on the audit trail as well as on
/// `stderr`: a desktop-launched service has no terminal a user reads, and
/// a refusal no one can see is how a vanishing panel stayed a mystery.
pub const SWITCHBOARD_CALL_REFUSED: EventId = EventId(20_002);

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

/// Attest the caller against its own Switchboard launch record, decode
/// the request fail-closed, and serve it.
///
/// Only a Switchboard child this session spawned (`caller_pid`, the
/// kernel-attested `call_peer_origin` pid, never a wire claim) may call:
/// anything else is [`SwitchboardRefusal::Unattested`] and never mutates
/// the model. The caller is attested by *its own* launch record naming
/// the Switchboard bundle, so no sibling, leftover, or not-yet-reaped
/// entry can answer for a live instance or lock it out. `PublishSummary`
/// relays the summary to the taskbar capsule;
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
    if !attested_switchboard(launched, caller_pid) {
        return Err(SwitchboardRefusal::Unattested);
    }
    let request = SwitchboardRequest::from_bytes(request).map_err(SwitchboardRefusal::Malformed)?;
    match request {
        SwitchboardRequest::PublishSummary { summary } => {
            shell.set_tray_summary(compositor, Some(summary));
            Ok(SwitchboardOutcome::Published {
                session: self_proc_id,
                publisher: caller_pid,
            })
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

/// Whether `pid` is a Switchboard instance this session itself launched:
/// it has a launch record of its own, and that record names the
/// Switchboard bundle.
///
/// The caller's own record is the authority, never the table's first
/// entry for the bundle path: a session may hold more than one record for
/// it at once — an instance that exited but has not yet been reaped, a
/// replacement started over it — and a live instance must not be locked
/// out by one of them.
fn attested_switchboard(launched: &LaunchTable, pid: u64) -> bool {
    launched
        .get(pid)
        .is_some_and(|app| app.run_path == SWITCHBOARD_RUN_PATH)
}

/// The session's one Switchboard instance: the one already recorded, or
/// the one `spawn` starts and records now.
///
/// One monitor per session is enforced here, at the spawn, so a second is
/// never started rather than started and then refused. `spawn` answers
/// with the pid it recorded, or `None` when the launch was refused — in
/// which case the desktop simply runs without its monitor.
pub fn ensure_switchboard(
    launched: &mut LaunchTable,
    spawn: impl FnOnce(&mut LaunchTable) -> Option<u64>,
) -> Option<u64> {
    match launched.running_from(SWITCHBOARD_RUN_PATH) {
        Some(live) => Some(live),
        None => spawn(launched),
    }
}

/// Whether a refused send of `command` is worth a `stderr` line.
///
/// A dropped frame reading is not news. The readings are best-effort
/// telemetry the monitor may ignore entirely, they are re-sent on the very
/// next frame that differs, and the mailbox fills precisely when the
/// desktop is busy — the condition the reading describes. Stating each one
/// would put a synchronous console write on every frame of exactly the
/// load being measured, which on a serial console costs far more than the
/// reading is worth and slows the desktop it is reporting on.
///
/// Every other command has a consequence a user can see — a panel that
/// does not open, a seat warning that does not arrive, a confirmed
/// shutdown that goes nowhere — so a drop is stated.
#[must_use]
pub fn drop_is_noteworthy(command: SwitchboardCommand) -> bool {
    !matches!(command, SwitchboardCommand::FrameReport { .. })
}

/// One non-blocking send of a command to a live Switchboard instance's own
/// mailbox: `ipc_send` in production, a recording fake under test. The
/// desktop never spins or blocks waiting for the panel to catch up, so a
/// refused send (a full mailbox, or an instance that has started but not
/// yet bound one) is the implementation's own `stderr` diagnosis to make,
/// never a reason for the caller to retry in a loop — and only for the
/// commands [`drop_is_noteworthy`] admits.
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

/// What served-window content landed since the last frame-report decision.
///
/// A monitor must not measure its own act of displaying: when the only
/// content that arrived is the Switchboard's, the frame is the cost of
/// drawing the number, not the desktop's work, and must not be reported.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FrameContent {
    /// No served window presented — chrome, animation, or an idle settle.
    None,
    /// At least one non-Switchboard window presented.
    Foreign,
    /// Only the live Switchboard's own window(s) presented.
    SwitchboardOnly,
}

/// Owners of windows that presented content since the last report decision.
///
/// Fold each successful present in, then read [`PresentedOwners::content`]
/// once at the frame-report site so the gate stays a pure function of the
/// owners that actually arrived.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PresentedOwners {
    any: bool,
    foreign: bool,
}

impl PresentedOwners {
    /// Record one successful present by its kernel-attested owner pid.
    ///
    /// An owner that cannot be resolved, or that is not the live Switchboard,
    /// counts as foreign: suppressing a real desktop frame is worse than
    /// reporting one extra Switchboard paint.
    pub fn note(&mut self, owner_pid: Option<u64>, switchboard_pid: Option<u64>) {
        self.any = true;
        let switchboard_only = matches!(
            (owner_pid, switchboard_pid),
            (Some(owner), Some(switchboard)) if owner == switchboard
        );
        if !switchboard_only {
            self.foreign = true;
        }
    }

    /// Classify the accumulated presents for the frame-report gate.
    #[must_use]
    pub const fn content(self) -> FrameContent {
        if !self.any {
            FrameContent::None
        } else if self.foreign {
            FrameContent::Foreign
        } else {
            FrameContent::SwitchboardOnly
        }
    }
}

/// Minimum time between two frame-report send attempts.
///
/// The counts a frame report carries move on every composited frame — a
/// pointer crossing bare wallpaper redamages two cursor rectangles that
/// overlap by a different amount each time, so `damaged_px` and
/// `dirty_rects` differ from the previous frame even though nothing the
/// desktop *did* changed. Change detection alone therefore cannot quieten a
/// motion storm: without this limit the session sends one command per frame,
/// and the monitor rebuilds its whole overview model for each one.
///
/// 250 ms is chosen against the reader, not the writer: the Resources page
/// is read at human speed, so four readings a second is already more than
/// anyone can follow, while it caps a continuous redraw storm at a handful
/// of sends. It is deliberately the same order as the runtime's cache-report
/// limit (`tairix_rt::cachereport`, 250 ms) because both answer the same
/// question — how often is it worth telling a live monitor something moved —
/// but the two are separate policies over separate channels, each documented
/// where its own gate lives.
pub const MIN_FRAME_REPORT_INTERVAL_NS: u64 = 250_000_000;

/// The session's frame-report gate: what it last told the monitor a frame
/// cost, when it last tried, and whether a change is being held back.
///
/// One per session, driven from the run loop's frame path. It exists because
/// the frame report is the one Switchboard command whose *content* churns at
/// frame rate: [`maybe_send_seat_report`] needs no such state because the
/// responsiveness tracker latches its own changes upstream, so a quiet seat
/// produces no candidate send at all.
///
/// The gate is the whole rate-limit and change-detection design, and it
/// holds no dirty flag: comparing a freshly read [`FrameReport`] against the
/// one already accepted *is* the change detection, which costs one struct
/// comparison on a frame path. [`FrameReportGate::park_deadline_ns`] is what
/// keeps a held-back change from needing a poll to escape.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameReportGate {
    /// The report the last *accepted* send carried, never one merely
    /// attempted: a refused report is not what the monitor holds.
    last_sent: Option<FrameReport>,
    /// When the last send was attempted, successful or not. `None` before
    /// the first attempt, so that attempt is never held back.
    last_attempt_ns: Option<u64>,
    /// Whether a change is currently held back — suppressed by the limit, or
    /// attempted and refused. Drives [`FrameReportGate::park_deadline_ns`].
    pending: bool,
}

impl FrameReportGate {
    /// A session that has reported nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_sent: None,
            last_attempt_ns: None,
            pending: false,
        }
    }

    /// Report what the frame `compositor` has just published cost, when a
    /// Switchboard instance is live, the counts differ from the ones this
    /// gate last had accepted, the frame's served content was not only the
    /// Switchboard's own paint, and the minimum interval has elapsed since
    /// the last attempt.
    ///
    /// Change detection is what makes this a change report rather than a
    /// per-frame send: a desktop redrawing the same rectangles sends
    /// nothing. The interval is what makes it a *report* rather than a
    /// stream: a desktop whose counts move every frame still sends a handful
    /// a second. `content` is what stops the monitor measuring itself: a
    /// frame whose only served present came from the live Switchboard is
    /// dropped even when the counters moved, so the panel's rebuild cannot
    /// re-excite another report. Nothing here waits on the panel, so a frame
    /// path pays one comparison and at most one non-blocking send.
    ///
    /// A refused send is dropped, exactly as a refused seat report is: the
    /// count it carried describes a frame already on screen, and the next
    /// frame that differs from the panel's view re-sends a fresher one. A
    /// refusal is never retried in place — that would turn a busy or absent
    /// monitor into the spin this design exists to avoid; it leaves the
    /// change pending and advances the limiter exactly as a suppressed send
    /// does, so the retry rides the loop's ordinary next pass.
    pub fn maybe_send(
        &mut self,
        compositor: &Compositor,
        live: Option<u64>,
        content: FrameContent,
        now_ns: u64,
        mailbox: &mut dyn SwitchboardMailbox,
    ) {
        if matches!(content, FrameContent::SwitchboardOnly) {
            return;
        }
        let Some(pid) = live else {
            return;
        };
        let mode = compositor.mode();
        let stats = compositor.frame_stats();
        let report = FrameReport {
            screen_px: u64::from(mode.width_px).saturating_mul(u64::from(mode.height_px)),
            damaged_px: stats.damaged_px,
            blended_px: stats.blended_px,
            opaque_px: stats.opaque_px,
            dirty_rects: stats.dirty_rects,
            present_calls: stats.present_calls,
            chrome_hits: stats.chrome_hits,
            chrome_misses: stats.chrome_misses,
        };
        if self.last_sent == Some(report) {
            self.pending = false;
            return;
        }
        if self
            .last_attempt_ns
            .is_some_and(|last| now_ns.saturating_sub(last) < MIN_FRAME_REPORT_INTERVAL_NS)
        {
            self.pending = true;
            return;
        }
        self.last_attempt_ns = Some(now_ns);
        if mailbox.send(pid, SwitchboardCommand::FrameReport { report }) {
            self.last_sent = Some(report);
            self.pending = false;
        } else {
            self.pending = true;
        }
    }

    /// `park_ns` shortened to the moment a held-back change may be sent, or
    /// left exactly as it is when nothing is held back.
    ///
    /// This is what keeps the rate limit from costing the desktop its last
    /// report: a pointer that stops mid-motion leaves one change suppressed,
    /// and without a deadline it would sit unsent until some unrelated wake
    /// — the Resources page reading a frame the user has moved on from. With
    /// it the session wakes once, presents the (undamaged, hence idle)
    /// frame, and sends that; the next pass compares equal, clears the flag,
    /// and the park folds back to indefinite. An idle desktop therefore arms
    /// no timer at all, and a busy one arms exactly one.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(
            park_ns,
            self.pending.then(|| {
                // `pending` is only ever set alongside a recorded attempt on
                // the suppressed and refused paths above. Failing closed to
                // "due now" if it somehow is not costs one extra pass of a
                // loop that is already awake, never a missed report.
                let elapsed = self
                    .last_attempt_ns
                    .map_or(MIN_FRAME_REPORT_INTERVAL_NS, |last| {
                        now_ns.saturating_sub(last)
                    });
                MIN_FRAME_REPORT_INTERVAL_NS.saturating_sub(elapsed)
            }),
        )
    }
}
