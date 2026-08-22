//! Routing device input into taskbar actions.
//!
//! The [`TaskbarInput`] router turns a stream of device-level
//! [`InputEvent`]s into actions against a [`Taskbar`]: a primary-button press
//! is hit-tested against the bar's computed [`BarLayout`](crate::BarLayout)
//! and drives the model — opening the program-library popup, or performing
//! a running application's default action.
//! A press on a status signal or on the clock is claimed
//! but inert (both are live readouts, not action targets), and a press on an
//! open notification popover dismisses the card it lands on.
//!
//! It is the taskbar counterpart of the window manager's input router, and it
//! consumes the **same** shared [`tairix_input`] event vocabulary, so the
//! desktop routes one event type to both. Like that
//! router it holds no pixels, tracks the pointer position from motion events,
//! applies presses at that position, and never panics: a press that misses
//! every region changes nothing.
//!
//! # The bar acts on the pointer only while it holds it
//!
//! The bar knows where its own regions are. It cannot know whether anything is
//! *drawn over* them: a window dragged across the bar leaves the clock at the
//! clock's coordinates, and a router that hit-tested that position alone would
//! light up, open popovers, and act under a window the user is working in.
//! Stacking belongs to the desktop's seat, so the seat resolves which surface
//! the pointer rests on and hands the pointer events to that one router —
//! this one receives an event only while the bar holds the pointer, and every
//! event it is handed is therefore its own to act on.
//!
//! [`set_pointer_focus`](TaskbarInput::set_pointer_focus) is the other half of
//! that contract: it is how the seat says the pointer has *left* the bar, which
//! is the only way the hover the bar is drawing can be dropped. It cannot be
//! inferred from a position, because the pointer usually has not moved — a
//! window was raised over the bar, or a drag took the pointer — and testing
//! that unchanged position would answer "still on the clock" and leave a
//! highlighted slot and an open hover popover stranded over someone else's
//! window.
//!
//! While the program-library popup is open the router treats it as modal and
//! consumes the whole event stream — presses, releases, scroll, and keys all
//! route into the popup ([`LibraryPopup`](crate::LibraryPopup)); a press on
//! the Library button toggles the popup shut, and a press outside the panel
//! dismisses it (the standard click-away behaviour) without also acting on
//! what it landed on — one click does one thing. The popup's key model gives
//! every action a keyboard path; the desktop routes key events here only
//! while the popup is open, so the focused window's keys are untouched
//! otherwise.
//!
//! A secondary press opens the bar's one context surface
//! ([`BarMenu`](crate::BarMenu)): on a running application's slot with the
//! popup closed — showing the menu that *application* declared, or nothing
//! at all when it declared none — on the clock, showing the clock's own
//! menu, or on a program-library entry row inside
//! the open popup. While
//! the menu is open it is the top modal layer — the whole stream routes
//! into it first, and a press outside its plate dismisses only the menu
//! (one click does one thing), leaving whatever is beneath for the next
//! click.
//!
//! Hovering a slot whose application owns more than one window opens the
//! [`WindowPicker`](crate::WindowPicker), which is a pointer surface and
//! nothing else: it takes no keyboard, a press on a cell chooses that
//! window, a press on its own plate is claimed and does nothing, and it
//! closes the moment the pointer leaves both it and the slot it hangs from.
//!
//! The Switchboard capsule at the trailing end has its own quiet
//! microinteractions: a primary press and quick release opens Switchboard's
//! running-task section, while a press held past [`LONG_PRESS_AFTER_NS`]
//! opens its Recovery section instead — resolved at whichever event the
//! router next handles once the threshold has elapsed (ordinarily a motion
//! sample taken while the press is still held, or the release itself when
//! none arrives sooner), never by polling or sleeping. A press that drags
//! off the capsule before release fires nothing (fail closed), and a long
//! press that already fired never also fires the quick-click response on
//! release. The open readout's "Open Switchboard" safe action reaches the
//! same task destination. Scrolling over the capsule or its readout cycles
//! the task list, and a middle press over the capsule switches back to the
//! previous task.

use tairix_abi::switchboard_ipc::CommandSection;
use tairix_abi::window_ipc::AppMenuItemId;
use tairix_abi::PowerAction;
use tairix_controls::{damage, TraySignalAction};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, PointerButton, PointerFocus};
use tairix_proglib::EntryId;
use tairix_theme::Appearance;

use crate::clock_menu::ClockAction;
use crate::layout::Hit;
use crate::library::{LibraryRow, PopupOutcome};
use crate::menu::{MenuChoice, MenuOutcome};
use crate::picker::{PickerEntry, PICKER_MIN_WINDOWS};
use crate::repaint::TaskbarRepaint;
use crate::system::{self, SystemAction};
use crate::taskbar::Taskbar;
use crate::tasks::TaskId;

/// How long a primary press on the Switchboard capsule must be held before
/// it resolves as a long press (opening Recovery) rather than a quick click
/// (opening the ordinary running-task section), in monotonic nanoseconds.
///
/// Half a second is long enough that an ordinary click never crosses it by
/// accident, short enough that a deliberate hold reads as immediate. The
/// router never sleeps or polls to detect the crossing: it compares the
/// caller-supplied monotonic time against the press's own start time on
/// whichever event next arrives (a motion, or the eventual release).
pub const LONG_PRESS_AFTER_NS: u64 = 500_000_000;

/// What a [`TaskbarInput`] event did to the taskbar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskbarResponse {
    /// The event changed no state the embedder must act on. Pixel-only
    /// changes (a hover, a popup scroll or edit) latch the taskbar's repaint
    /// flag instead ([`Taskbar::take_repaint`]).
    Ignored,
    /// The Library button opened the program-library popup.
    OpenLibrary,
    /// The program-library popup closed without launching anything — the
    /// Library button toggled it shut, a press outside dismissed it, or
    /// `Escape` was pressed.
    LibraryDismissed,
    /// A bundle was chosen to launch — an entry in the program-library
    /// popup (closing it), or a launch row of the system quick-actions menu.
    /// The embedder resolves the entry's bundle and launches it. Both
    /// origins report the same outcome, so there is exactly one launch path
    /// behind the bar.
    LibraryLaunch {
        /// The catalog identifier of the chosen entry.
        entry: EntryId,
    },
    /// A program-library entry's context menu asked for a **desktop
    /// shortcut** to its bundle (closing the popup). The embedder — which
    /// holds the filesystem capability — creates the link in the user's own
    /// `Desktop` folder under its own identity; the bar writes nothing and
    /// learns nothing about whether it worked.
    CreateDesktopShortcut {
        /// The catalog identifier of the entry to link to.
        entry: EntryId,
    },
    /// A primary click landed on a running application's slot, and that
    /// application declared that it handles the click itself. The embedder
    /// relays it to the application as an icon-bar default action.
    AppDefault {
        /// The application's strip index.
        app: usize,
    },
    /// A primary click landed on a running application's slot that declared
    /// no default action of its own. The embedder raises and focuses that
    /// application's most recently used window, and does nothing at all when
    /// it has none.
    AppRaise {
        /// The application's strip index.
        app: usize,
    },
    /// A row of the menu an application declared was chosen. The embedder
    /// relays the application's own row id back to it; the bar never
    /// interprets one.
    AppMenuChosen {
        /// The application's strip index.
        app: usize,
        /// The id the application gave the chosen row.
        item: AppMenuItemId,
    },
    /// The pointer came to rest on a running application's slot whose
    /// application owns more than one window. The embedder builds one cell
    /// per window — it owns their pixels, so it is what can scale a
    /// thumbnail — and hands them back through
    /// [`Taskbar::show_window_picker`](crate::Taskbar::show_window_picker).
    ShowWindowPicker {
        /// The application's strip index.
        app: usize,
    },
    /// A window was chosen in the hover picker. The embedder raises and
    /// focuses it.
    WindowChosen {
        /// The chosen window.
        id: TaskId,
    },
    /// A raised notification's card was clicked to dismiss it. The embedder
    /// clears the notification identified by `(producer, key)` from the
    /// model — and from the session, which owns the live feed.
    DismissNotification {
        /// The dismissed notification's attested producer.
        producer: u64,
        /// The producer-chosen key naming the dismissed notification.
        key: u32,
    },
    /// The clock menu's *Set Date & Time…* row was chosen. The embedder
    /// authenticates an account that holds `CAP_TIME_SET` through its
    /// console's broker and starts the Date & Time application as that
    /// account; the bar holds no such authority and sets no clock itself.
    SetDateTime,

    /// A gesture on the Switchboard capsule (or the readout's "Open
    /// Switchboard" safe action) asked to open the Switchboard window at a
    /// section. The embedder asks the Switchboard service to open — or, on
    /// a dead service, revive and open — its window there.
    OpenSwitchboard {
        /// Which section the window should open showing.
        section: CommandSection,
    },
    /// An appearance row of the system quick-actions menu was chosen. The
    /// embedder switches the desktop's active theme and repaints.
    SetAppearance {
        /// The appearance to switch to.
        appearance: Appearance,
    },
    /// *Lock Screen* was chosen. The embedder puts its own password prompt
    /// in front of the whole screen and stops routing input anywhere else
    /// until the signed-in user is re-verified; the session and everything
    /// running in it keep running untouched.
    LockSession,
    /// *Switch User…* was chosen. The embedder asks the session authority
    /// to record it as background and, only once that is granted, gives up
    /// the screen so the login screen can come back up; everything in the
    /// session keeps running and is resumed when the user returns. A
    /// refusal leaves the session exactly as it is, and is reported.
    SwitchUser,
    /// *Log Out* was chosen. The embedder ends this desktop session
    /// cleanly; the login supervisor that started it prompts again.
    LogOut,
    /// A power row of the system quick-actions menu was chosen. The embedder
    /// **must** put the choice to the user before anything happens, and only
    /// then relay it to the one process that holds the authority to perform
    /// it — the bar holds none. The variant is named for that obligation so
    /// a caller cannot apply it while believing it had already been
    /// confirmed.
    ConfirmSystemPower {
        /// The transition the user asked for.
        action: PowerAction,
    },
}

/// Routes device input into [`Taskbar`] actions.
///
/// The router's only state is the current pointer position, updated by
/// [`InputEvent::PointerMoved`], and an in-progress Switchboard capsule
/// press; presses act at that position, exactly as a real pointing device
/// reports motion separately from clicks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarInput {
    pointer: Point,
    capsule_press: Option<CapsulePress>,
}

/// An in-progress primary press on the Switchboard capsule, tracked so a
/// hold past [`LONG_PRESS_AFTER_NS`] opens Recovery exactly once, while a
/// quick release opens the ordinary Overview section instead.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct CapsulePress {
    /// The monotonic time the press began, in nanoseconds.
    started_ns: u64,
    /// Whether the long-press response already fired for this press, so
    /// the matching release cannot also fire the quick-click response.
    long_fired: bool,
}

impl TaskbarInput {
    /// Create a router with the pointer at the screen origin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The pointer position this router last held the pointer at, in screen
    /// coordinates.
    ///
    /// The *live* pointer belongs to the desktop's seat, which tracks the
    /// device and decides which surface it rests on; this is the position at
    /// which the bar was last handed it, which is what its own hit tests are
    /// applied at. While the pointer rests elsewhere the two differ, and it is
    /// the seat's that is the pointer.
    #[must_use]
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// Take the seat's answer to "does the pointer rest on one of the bar's
    /// surfaces?", applying what changes when it does.
    ///
    /// * [`Entered`](PointerFocus::Entered) adopts the position the pointer
    ///   arrived at and refreshes the bar's hover feedback there. The pointer
    ///   can arrive without moving — a window above the bar closed, a drag
    ///   ended, a modal surface shut — and no motion event exists for those,
    ///   which is why the position travels with the answer.
    /// * [`Left`](PointerFocus::Left) drops every hover the bar is drawing and
    ///   closes the hover window picker. The picker in particular *must* go:
    ///   it is a surface that exists only because the pointer is resting on a
    ///   slot, so leaving it open once the pointer is elsewhere would float a
    ///   panel of window thumbnails over whatever the user is now working in.
    ///
    /// No gesture is resolved here and nothing is reported to the embedder:
    /// this is the pointer arriving or leaving, not the user asking for
    /// anything. In particular an enter never *opens* a hover surface — a
    /// window closing is not a gesture, and a popover that appeared because
    /// something else vanished is a popover nobody asked for. The next real
    /// motion opens one if the pointer is still there.
    pub fn set_pointer_focus(&mut self, focus: PointerFocus, taskbar: &mut Taskbar, scale: Scale) {
        let mut damage = damage::sink();
        match focus {
            PointerFocus::Entered { at } => {
                self.pointer = at;
                taskbar.track_hover(Some(at), scale, &mut damage);
            }
            PointerFocus::Left => {
                taskbar.track_hover(None, scale, &mut damage);
                taskbar.close_picker();
            }
        }
    }

    /// Process one input `event` against `taskbar`, hit-testing at the
    /// desktop `scale` (the compositor's output density) and resolving any
    /// time-driven Switchboard capsule gesture against the monotonic time
    /// `now_ns`, returning what changed.
    ///
    /// With the popup and menu closed only a primary or secondary press
    /// acts; pointer motion updates the tracked position, the bar's hover
    /// feedback, and the hover window picker, and every other event is
    /// [`TaskbarResponse::Ignored`]. With the context menu open the whole
    /// stream routes into it; with the popup open it routes there (see the
    /// [module docs](self)).
    pub fn handle(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
        now_ns: u64,
    ) -> TaskbarResponse {
        // One sink for the whole round: every control this event reaches
        // reports its own repainted bounds into the same region.
        let mut damage = damage::sink();
        if let InputEvent::PointerMoved { to } = event {
            // A delivered motion is an enter: the seat resolves which surface
            // the pointer rests on before it delivers, so a motion arriving
            // here says the bar holds the pointer and says where.
            self.pointer = to;
            taskbar.track_hover(Some(to), scale, &mut damage);
            if let Some(response) = self.continue_capsule_press(taskbar, scale, now_ns) {
                return response;
            }
            // The hover picker follows the pointer while no modal surface is
            // up: a modal menu or popup owns the whole stream, and opening a
            // picker underneath it would show a surface the user cannot
            // reach.
            if !taskbar.menu().is_open() && !taskbar.library().is_open() {
                if let Some(response) = self.track_picker(taskbar, scale) {
                    return response;
                }
            }
        }
        if taskbar.menu().is_open() {
            return self.route_to_menu(event, taskbar, scale, &mut damage);
        }
        if taskbar.library().is_open() {
            return self.route_to_popup(event, taskbar, scale, &mut damage);
        }
        // The picker is non-modal too, and takes a press that lands on it
        // before the bar beneath does: choosing a window is what a press on
        // a cell means, and a press on the picker's own chrome is claimed so
        // it never falls through to the slot under it.
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            }
        ) {
            if let Some(response) = self.press_picker(taskbar, scale) {
                return response;
            }
        }
        // The notification popover and the Switchboard readout are
        // non-modal: unlike the menu and library popup they do not swallow
        // the whole stream. A primary press or release that lands on the
        // popover dismisses the card it hits (or is claimed harmlessly on
        // the panel's chrome); one that lands inside the open readout
        // drives its "Open Switchboard" safe action. Either way the click
        // neither acts on the bar beneath nor reaches the windows below;
        // every other event routes on as usual.
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            } | InputEvent::PointerReleased {
                button: PointerButton::Primary
            }
        ) {
            if let InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } = event
            {
                if let Some(response) = self.press_notification(taskbar, scale) {
                    return response;
                }
            }
            if self.over_tray_readout(taskbar, scale) {
                // The readout claims this click, so a capsule press the bar
                // re-laid out from under is abandoned here rather than left
                // armed to resolve on some later release.
                self.capsule_press = None;
                return match taskbar.tray_pointer(&event, scale, &mut damage) {
                    Some(TraySignalAction::Activated) => TaskbarResponse::OpenSwitchboard {
                        section: CommandSection::Tasks,
                    },
                    None => TaskbarResponse::Ignored,
                };
            }
        }
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(taskbar, scale, now_ns),
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            } => self.press_secondary(taskbar, scale),
            InputEvent::PointerPressed {
                button: PointerButton::Middle,
            } => self.press_middle(taskbar, scale),
            InputEvent::PointerScrolled { dx, dy } => self.scroll_tasks(taskbar, scale, dx, dy),
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => self.release_primary(now_ns),
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerReleased { .. }
            | InputEvent::KeyPressed { .. }
            | InputEvent::KeyReleased { .. } => TaskbarResponse::Ignored,
        }
    }

    /// While a primary press on the Switchboard capsule is in progress,
    /// check the pointer's latest motion against it. Dragging off the
    /// capsule cancels the gesture (fail closed — it fires nothing, on this
    /// event or the eventual release); motion sampled once
    /// [`LONG_PRESS_AFTER_NS`] has elapsed resolves it to Recovery
    /// immediately, without waiting for release. A press already resolved
    /// this way is left alone until release clears it.
    fn continue_capsule_press(
        &mut self,
        taskbar: &Taskbar,
        scale: Scale,
        now_ns: u64,
    ) -> Option<TaskbarResponse> {
        let press = self.capsule_press?;
        if press.long_fired {
            return None;
        }
        if taskbar.hit_test(self.pointer, scale) != Some(Hit::Switchboard) {
            self.capsule_press = None;
            return None;
        }
        if now_ns.saturating_sub(press.started_ns) < LONG_PRESS_AFTER_NS {
            return None;
        }
        self.capsule_press = Some(CapsulePress {
            long_fired: true,
            ..press
        });
        Some(TaskbarResponse::OpenSwitchboard {
            section: CommandSection::Recovery,
        })
    }

    /// Handle a primary-button press at the current pointer position with
    /// the popup closed, hit-tested at the desktop `scale`.
    ///
    /// A press on the Switchboard capsule begins tracking a tap-or-hold
    /// gesture rather than acting immediately — [`release_primary`] and
    /// [`continue_capsule_press`] resolve it; every other hit acts as usual.
    ///
    /// [`release_primary`]: Self::release_primary
    /// [`continue_capsule_press`]: Self::continue_capsule_press
    fn press_primary(
        &mut self,
        taskbar: &mut Taskbar,
        scale: Scale,
        now_ns: u64,
    ) -> TaskbarResponse {
        let Some(hit) = taskbar.hit_test(self.pointer, scale) else {
            return TaskbarResponse::Ignored;
        };
        match hit {
            Hit::Library => {
                taskbar.open_library();
                TaskbarResponse::OpenLibrary
            }
            Hit::App(index) => Self::activate_app(taskbar, index),
            // A status signal and the clock are live readouts, not action
            // targets: the press is claimed so it never falls through to
            // the window beneath, but it does nothing. The clock's menu is
            // a secondary press's to ask for (`press_secondary`) — a left
            // click that pops a menu up is a menu nobody asked for.
            Hit::Notification(_) | Hit::Clock => TaskbarResponse::Ignored,
            Hit::Switchboard => {
                self.capsule_press = Some(CapsulePress {
                    started_ns: now_ns,
                    long_fired: false,
                });
                TaskbarResponse::Ignored
            }
        }
    }

    /// Resolve a primary release against any in-progress Switchboard
    /// capsule press, at the monotonic time `now_ns`.
    ///
    /// A press that already resolved to Recovery (or that dragged off the
    /// capsule and was cancelled by [`continue_capsule_press`]) fires
    /// nothing on release — one gesture reports exactly one response. A
    /// press still in progress opens the running-task section, unless the
    /// hold has itself crossed the long-press threshold with no intervening
    /// motion to have caught it, in which case release is the first event to
    /// resolve it and it opens Recovery instead. A release with no
    /// in-progress capsule press changes nothing.
    ///
    /// [`continue_capsule_press`]: Self::continue_capsule_press
    fn release_primary(&mut self, now_ns: u64) -> TaskbarResponse {
        let Some(press) = self.capsule_press.take() else {
            return TaskbarResponse::Ignored;
        };
        if press.long_fired {
            return TaskbarResponse::Ignored;
        }
        if now_ns.saturating_sub(press.started_ns) >= LONG_PRESS_AFTER_NS {
            return TaskbarResponse::OpenSwitchboard {
                section: CommandSection::Recovery,
            };
        }
        TaskbarResponse::OpenSwitchboard {
            section: CommandSection::Tasks,
        }
    }

    /// Route a primary press against the open notification popover, if the
    /// press lands within it. Returns `Some` when the popover claims the
    /// press — a [`DismissNotification`](TaskbarResponse::DismissNotification)
    /// for the card it hit, or [`Ignored`](TaskbarResponse::Ignored) for a
    /// press on the panel chrome between cards — and `None` when the press
    /// falls outside the popover and should route to the bar. The popover is
    /// presented above the bar and never overlaps it, so this position test
    /// is unambiguous.
    fn press_notification(&self, taskbar: &Taskbar, scale: Scale) -> Option<TaskbarResponse> {
        let layout = taskbar.notifications_layout(scale)?;
        if !layout.contains(self.pointer) {
            return None;
        }
        if let Some(index) = layout.card_at(self.pointer) {
            if let Some(note) = taskbar.notifications().notification(index) {
                return Some(TaskbarResponse::DismissNotification {
                    producer: note.producer,
                    key: note.key,
                });
            }
        }
        Some(TaskbarResponse::Ignored)
    }

    /// Whether the current pointer position lies inside the open Switchboard
    /// readout panel.
    fn over_tray_readout(&self, taskbar: &Taskbar, scale: Scale) -> bool {
        taskbar
            .tray_readout_layout(scale)
            .is_some_and(|readout| readout.contains(self.pointer))
    }

    /// Handle a middle-button press at the current pointer position: over
    /// the Switchboard capsule it switches to the previous task (the
    /// MRU-of-two the task list remembers); anywhere else it is ignored. No
    /// remembered task, or one that vanished, changes nothing (fail closed).
    fn press_middle(&self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        if taskbar.hit_test(self.pointer, scale) != Some(Hit::Switchboard) {
            return TaskbarResponse::Ignored;
        }
        let Some(id) = taskbar.tasks().previous() else {
            return TaskbarResponse::Ignored;
        };
        Self::focus_task(taskbar, id)
    }

    /// Handle a scroll over the Switchboard capsule (or its open readout):
    /// cycle the task list, focusing the entry after the focused one for a
    /// positive step and the one before it for a negative step, wrapping at
    /// both ends (no focused task starts at the first or last entry). The
    /// vertical delta decides; the horizontal one is the fallback when it is
    /// zero. No tasks, no net direction, or a pointer anywhere else changes
    /// nothing.
    fn scroll_tasks(
        &self,
        taskbar: &mut Taskbar,
        scale: Scale,
        dx: i32,
        dy: i32,
    ) -> TaskbarResponse {
        let over_capsule = taskbar.layout(scale).switchboard.contains(self.pointer);
        if !over_capsule && !self.over_tray_readout(taskbar, scale) {
            return TaskbarResponse::Ignored;
        }
        let step = if dy != 0 { dy } else { dx };
        if step == 0 {
            return TaskbarResponse::Ignored;
        }
        let entries = taskbar.tasks().entries();
        if entries.is_empty() {
            return TaskbarResponse::Ignored;
        }
        let focused = taskbar
            .tasks()
            .focused()
            .and_then(|id| entries.iter().position(|entry| entry.id == id));
        let index = if step > 0 {
            focused.map_or(0, |index| (index + 1) % entries.len())
        } else {
            focused.map_or(entries.len() - 1, |index| {
                (index + entries.len() - 1) % entries.len()
            })
        };
        let Some(id) = entries.get(index).map(|entry| entry.id) else {
            return TaskbarResponse::Ignored;
        };
        Self::focus_task(taskbar, id)
    }

    /// Restore-and-focus the window with `id`, reporting the choice — or
    /// nothing when the window vanished (fail closed).
    fn focus_task(taskbar: &mut Taskbar, id: TaskId) -> TaskbarResponse {
        if taskbar.tasks_mut().set_focused(Some(id)) {
            TaskbarResponse::WindowChosen { id }
        } else {
            TaskbarResponse::Ignored
        }
    }

    /// Handle a secondary-button press at the current pointer position with
    /// the popup and menu closed: a press on a running application's slot
    /// opens the menu that application declared (and nothing at all when it
    /// declared none), a press on the Switchboard capsule opens the
    /// desktop's system quick-actions menu; anywhere else on the bar is
    /// claimed and does nothing.
    ///
    /// Opening a menu acts on nothing by itself — the response is always
    /// `Ignored`, and only choosing a row reports an outcome.
    fn press_secondary(&mut self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        let layout = taskbar.layout(scale);
        match layout.hit_test(self.pointer) {
            Some(Hit::App(index)) => {
                let anchor = layout.apps.get(index).copied().unwrap_or(Rect::EMPTY);
                taskbar.open_app_menu(index, anchor);
            }
            Some(Hit::Switchboard) => taskbar.open_system_menu(layout.switchboard),
            Some(Hit::Clock) => taskbar.open_clock_menu(layout.clock),
            _ => {}
        }
        TaskbarResponse::Ignored
    }

    /// Follow the pointer with the hover window picker.
    ///
    /// A pointer inside the open picker drives its highlight; one over an
    /// application slot whose application owns more than one window asks the
    /// embedder to show the picker there (the embedder owns the windows'
    /// pixels, so it builds the cells); one that has left both closes it.
    /// Returns a response only when the embedder must act.
    fn track_picker(&mut self, taskbar: &mut Taskbar, scale: Scale) -> Option<TaskbarResponse> {
        if let Some(layout) = taskbar.picker_layout(scale) {
            if layout.panel.contains(self.pointer) {
                let cell = taskbar.picker().cell_at(&layout, self.pointer);
                taskbar.track_picker_hover(cell);
                return None;
            }
        }
        let hovered = taskbar.apps().hover();
        match hovered {
            Some(index)
                if taskbar
                    .apps()
                    .get(index)
                    .is_some_and(|app| app.windows().len() >= PICKER_MIN_WINDOWS) =>
            {
                if taskbar.picker().app() == Some(index) {
                    return None;
                }
                Some(TaskbarResponse::ShowWindowPicker { app: index })
            }
            _ => {
                taskbar.close_picker();
                None
            }
        }
    }

    /// Resolve a primary press that landed on the open picker: a press on a
    /// cell chooses that window, one on the picker's own chrome is claimed
    /// and does nothing. `None` when the picker is closed or the press
    /// landed elsewhere.
    fn press_picker(&mut self, taskbar: &mut Taskbar, scale: Scale) -> Option<TaskbarResponse> {
        let layout = taskbar.picker_layout(scale)?;
        if !layout.panel.contains(self.pointer) {
            return None;
        }
        let chosen = taskbar
            .picker()
            .cell_at(&layout, self.pointer)
            .and_then(|cell| {
                taskbar
                    .picker()
                    .entries()
                    .get(cell)
                    .map(PickerEntry::window)
            });
        taskbar.close_picker();
        match chosen {
            Some(id) => Some(Self::focus_task(taskbar, id)),
            None => Some(TaskbarResponse::Ignored),
        }
    }

    /// Apply a primary click to the running application at `index`.
    ///
    /// An application that declared it handles the click gets it; one that
    /// did not has its most recently used window raised instead, and an
    /// application with neither a declaration nor a window does nothing —
    /// the honest outcome, never a guessed one.
    fn activate_app(taskbar: &mut Taskbar, index: usize) -> TaskbarResponse {
        // A click closes the hover picker: the user has decided on the
        // application rather than on one of its windows.
        taskbar.close_picker();
        let Some(app) = taskbar.apps().get(index) else {
            return TaskbarResponse::Ignored;
        };
        if app.handles_default() {
            return TaskbarResponse::AppDefault { app: index };
        }
        if app.windows().is_empty() {
            return TaskbarResponse::Ignored;
        }
        TaskbarResponse::AppRaise { app: index }
    }

    /// Route one event into the open context menu (the top modal layer).
    fn route_to_menu(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
        damage: &mut Region,
    ) -> TaskbarResponse {
        let Some(layout) = taskbar.menu_layout(scale) else {
            // An open menu always lays out; a missing layout means the menu
            // just closed under us — drop the claim rather than guess.
            taskbar.close_menu();
            return TaskbarResponse::Ignored;
        };
        let theme = taskbar.theme().clone();
        let outcome = match event {
            InputEvent::KeyPressed { key, .. } => taskbar
                .menu_routing_mut()
                .route_key(key, &layout, scale, &theme, damage),
            InputEvent::KeyReleased { .. } => MenuOutcome::Ignored,
            ref pointer_event => taskbar.menu_routing_mut().route_pointer(
                pointer_event,
                self.pointer,
                &layout,
                scale,
                &theme,
                damage,
            ),
        };
        match outcome {
            MenuOutcome::Ignored => TaskbarResponse::Ignored,
            MenuOutcome::Changed | MenuOutcome::Dismissed => {
                // The menu is its own overlay surface: a highlight move, a
                // fold, or a dismiss changes only what it draws.
                taskbar.request_repaint(TaskbarRepaint::MENU);
                TaskbarResponse::Ignored
            }
            MenuOutcome::Choose(choice) => {
                taskbar.request_repaint(TaskbarRepaint::MENU);
                Self::apply_choice(taskbar, choice)
            }
        }
    }

    /// Translate a chosen menu row into the typed response the embedder
    /// resolves.
    fn apply_choice(taskbar: &mut Taskbar, choice: MenuChoice) -> TaskbarResponse {
        match choice {
            MenuChoice::AppMenu { index, item } => {
                TaskbarResponse::AppMenuChosen { app: index, item }
            }
            MenuChoice::OpenEntry(entry) => {
                // Launching from the entry menu behaves exactly like
                // launching from the row itself: the popup closes.
                taskbar.close_library();
                TaskbarResponse::LibraryLaunch { entry }
            }
            MenuChoice::ShortcutEntry(entry) => {
                // The shortcut appears on the desktop, and the popup is
                // modal: leaving it up would stand between the user and the
                // icon they just asked for.
                taskbar.close_library();
                TaskbarResponse::CreateDesktopShortcut { entry }
            }
            MenuChoice::System(action) => Self::apply_system_action(action),
            MenuChoice::Clock(action) => match action {
                ClockAction::SetDateTime => TaskbarResponse::SetDateTime,
            },
        }
    }

    /// Translate a chosen system quick action into the typed response the
    /// session applies under its own authority.
    ///
    /// The two inspection rows reuse the capsule's own Switchboard-opening
    /// response and the launch row reuses the bar's one launch response, so
    /// no command here introduces a second path to a destination the bar
    /// already reaches.
    fn apply_system_action(action: SystemAction) -> TaskbarResponse {
        match action {
            SystemAction::About => TaskbarResponse::OpenSwitchboard {
                section: CommandSection::System,
            },
            SystemAction::SystemMonitor => TaskbarResponse::OpenSwitchboard {
                section: CommandSection::Tasks,
            },
            // The row is only actionable when this identifier resolved
            // against the catalog, so a refusal here cannot happen through
            // the menu; reporting nothing rather than a launch that must
            // fail is still the honest answer if it ever did.
            SystemAction::TaskShell => match EntryId::new(system::TASK_SHELL_BUNDLE) {
                Ok(entry) => TaskbarResponse::LibraryLaunch { entry },
                Err(_) => TaskbarResponse::Ignored,
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
        }
    }

    /// Route one event into the open program-library popup.
    ///
    /// A primary press on the Library button toggles the popup shut before
    /// the popup sees the event — the button is the popup's own invoker, so
    /// it is the one bar region a modal popup does not swallow.
    fn route_to_popup(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
        damage: &mut Region,
    ) -> TaskbarResponse {
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            }
        ) && taskbar.hit_test(self.pointer, scale) == Some(Hit::Library)
        {
            taskbar.close_library();
            return TaskbarResponse::LibraryDismissed;
        }
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Secondary
            }
        ) {
            if let Some((entry, anchor)) = Self::entry_row_at(taskbar, self.pointer, scale) {
                taskbar.open_entry_menu(entry, anchor);
                return TaskbarResponse::Ignored;
            }
        }

        let layout = taskbar.library_layout(scale);
        let theme = taskbar.theme().clone();
        let outcome = match event {
            InputEvent::KeyPressed { key, modifiers } => taskbar
                .library_routing_mut()
                .route_key(key, modifiers, &layout, damage),
            InputEvent::KeyReleased { .. } => PopupOutcome::Ignored,
            ref pointer_event => taskbar.library_routing_mut().route_pointer(
                pointer_event,
                self.pointer,
                &layout,
                &theme,
                scale,
                damage,
            ),
        };
        match outcome {
            PopupOutcome::Ignored => TaskbarResponse::Ignored,
            PopupOutcome::Changed => {
                // A scroll, a filter edit, or a fold changes only what the
                // popup itself draws — the bar's Library button is already
                // latched once, by the open that made the popup modal.
                taskbar.request_repaint(TaskbarRepaint::LIBRARY);
                TaskbarResponse::Ignored
            }
            PopupOutcome::Launch(entry) => {
                taskbar.close_library();
                TaskbarResponse::LibraryLaunch { entry }
            }
            PopupOutcome::Dismiss => {
                taskbar.close_library();
                TaskbarResponse::LibraryDismissed
            }
        }
    }

    /// The program-library *entry* row under `point` in the open popup, with
    /// its screen-space rectangle — the anchor for its context menu. Folder
    /// rows and misses return `None`.
    fn entry_row_at(taskbar: &Taskbar, point: Point, scale: Scale) -> Option<(EntryId, Rect)> {
        let layout = taskbar.library_layout(scale);
        let row = layout.row_at(point)?;
        let anchor = layout
            .rows
            .iter()
            .find(|(index, _)| *index == row)
            .map(|(_, rect)| *rect)?;
        match taskbar.library().rows().get(row)? {
            LibraryRow::Entry { id, .. } => Some((id.clone(), anchor)),
            LibraryRow::Folder { .. } => None,
        }
    }
}
