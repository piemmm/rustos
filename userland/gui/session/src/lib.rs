//! TAIRiX desktop **session glue** (`userland/gui/session`).
//!
//! The taskbar models the desktop's controls but, by design, owns no theme
//! registry, no filesystem reach, and no spawn capability: its buttons and
//! its program-library popup only *report* typed
//! [`TaskbarResponse`](tairix_taskbar::TaskbarResponse)s, leaving the actual
//! work to the session glue. This crate is that glue.
//!
//! [`DesktopSession`] owns the one shared [`ThemeRegistry`] and the
//! [`Taskbar`] model; [`DesktopSession::set_theme`] switches the registry
//! and re-themes the taskbar in place (its interactive home is the
//! Switchboard's System menu, `plans/NEW-TASKBAR.md` T13). Taskbar responses
//! flow to the embedder — which holds the window-manager, filesystem, and
//! process capabilities — as [`ShellOutcome::Taskbar`] values.
//!
//! # The program library
//!
//! The popup lists the resolved program-library catalog
//! (`plans/NEW-TASKBAR.md`): the [`library`] module's [`load_library`] reads
//! the machine store and the logged-in user's overlay through the
//! [`SessionFileReader`] seam, merges them with `tairix_proglib::merge`, and
//! reports any unusable store as a ready-to-print warning — the desktop
//! degrades to a calm empty library and says why, never guessing at a
//! half-parsed store. The embedder hands the merged catalog to the popup
//! with [`DesktopShell::set_library`] and resolves its
//! [`LibraryLaunch`](tairix_taskbar::TaskbarResponse::LibraryLaunch)
//! responses back through the catalog to spawn the chosen bundle.
//!
//! # The icon bar
//!
//! The bar's middle is one slot per *running application*
//! (`plans/NEW-TASKBAR.md`): the [`apps`] module's [`AppBarService`] holds
//! every application's icon-bar declaration as the window engine attested
//! it, groups each live served window under the process that owns it, and
//! resolves the label, icon, and information-panel identity of each slot
//! from the **signed** `AppInfo` of the bundle the desktop launched that
//! process from — so an application cannot state an identity that is not
//! its own inside system-drawn chrome. A declaring application keeps its
//! slot for the life of its process; one that declared nothing but owns a
//! window gets a slot with no menu, so no window is unreachable. Hovering a
//! slot whose application owns more than one window opens the bar's window
//! picker, whose cells are the session's own copies of each window's last
//! presented frame scaled through [`thumbnail`].
//!
//! # Presenting the taskbar through the window manager
//!
//! [`TaskbarPresenter`] (the [`presenter`] module) is the session's glue to
//! the compositor: it paints the taskbar's bar — and, while open, its
//! program-library popup — with the taskbar's own
//! [`TaskbarRenderer`](tairix_taskbar::TaskbarRenderer) and presents each as a
//! window in the [`Compositor`](tairix_wm::Compositor), placed at its computed
//! origin and rounded through the compositor's single anti-aliased
//! rounded-corner path. Composing the taskbar and window
//! manager is the permitted `userland/gui/*` edge.
//!
//! # Fading the desktop in and out
//!
//! The login screen fades to black before it exits, so a session starts on a
//! dark screen. [`ScreenFade`] (the [`fade`] module) fades the desktop up
//! over it through the compositor's screen reveal, over the theme's own
//! session-fade span, and folds its next frame into the embedder's park —
//! settled, it asks for no wake at all. Logging out, stepping aside for
//! another account, and being resumed all run that same fade, so a session
//! dissolves into the black the login screen appears out of instead of
//! cutting to it. Reaching full strength is announced once as
//! [`DESKTOP_REVEALED`], the witness that the desktop is visible rather than
//! merely presented.
//!
//! # Routing live input to the taskbar and window manager
//!
//! A real input source produces one stream of pointer events, but the desktop
//! has two routers — the window manager's
//! [`InputRouter`](tairix_wm::InputRouter) and the taskbar's
//! [`TaskbarInput`](tairix_taskbar::TaskbarInput). [`SessionInputRouter`] (the
//! [`input`] module) is the glue that fans that one stream to the right one:
//! while the program-library popup is open it is modal and the whole stream
//! (presses, releases, scroll, keys) routes to the taskbar; otherwise the
//! taskbar claims a primary press over the bar, the window manager handles
//! everything else, motion is fanned to both so their pointers stay in step,
//! and a release ends a window move-grab. Composing the
//! two GUI crates this way is the permitted `userland/gui/*` edge.
//!
//! # On-disk graphics assets
//!
//! The session also loads the desktop's SVG graphics assets from
//! `/System/Graphics`: the [`assets`] module's
//! [`SessionFileReader`] seam reads the bytes (a filesystem capability the
//! `no_std` `lib/cursor` / `lib/icon` crates must not hold),
//! and [`DesktopSession::load_cursors`] / [`DesktopSession::load_icons`]
//! assemble a [`CursorTheme`](tairix_cursor::CursorTheme) /
//! [`IconSet`](tairix_icon::IconSet), failing closed per kind to the built-in
//! artwork. The same seam feeds the program-library loader above — one
//! file-reading seam, one production implementation.
//!
//! # Where it sits
//!
//! As a `userland/gui/*` crate it composes the other GUI crates and `lib/*`
//! only: it owns the [`Taskbar`] and reads the shared
//! [`tairix_theme`] definition. Nothing outside `userland/gui/*` depends on
//! it — the desktop is an optional, one-way-dependent frontend.
//!
//! # Driving the desktop from a live input stream
//!
//! [`DesktopShell`] (the [`shell`] module) composes all of the above — the
//! session, the input router, the taskbar presenter, and the taskbar renderer
//! — into one event-driven frontend. It [`pump`](DesktopShell::pump)s the
//! pending pointer events from an injected [`InputSource`] seam (a real
//! device channel on a running system, an in-memory queue in tests), routes
//! each to the window manager or taskbar, applies the light/dark toggle
//! itself, re-presents the bar, and surfaces every other effect as a
//! [`ShellOutcome`] for the embedder (which holds the framebuffer, power, and
//! spawn capabilities) to act on.
//!
//! The live backing for that [`InputSource`] is [`DeviceInputSource`] (the
//! [`device`] module): it reads framed
//! [`PointerInput`](tairix_abi::input::PointerInput) records from an injected
//! [`PointerInputChannel`] (the kernel input channel) and decodes each into
//! the `lib/input` [`InputEvent`](tairix_wm::InputEvent) the shell routes,
//! failing closed on a malformed record. The
//! keyboard's live backing is its counterpart [`KeyboardInputSource`] (the
//! [`keyboard`] module): it decodes framed
//! [`KeyInput`](tairix_abi::input::KeyInput) records from an injected
//! [`KeyInputChannel`] into the same [`InputEvent`](tairix_wm::InputEvent)
//! stream, which the window manager delivers to the focused window.
//!
//! Both of those channels are, in turn, backed by the kernel seat registry:
//! [`SeatInputChannel`] (the [`seat`] module) drains each fixed-width input
//! record from the per-seat, owner-gated channel the kernel routed the
//! desktop's input to, through an injected [`SeatEventReader`] seam (the
//! seat-addressed [`POINTER_READ`](tairix_abi::SyscallNumber::POINTER_READ) /
//! [`KEYBOARD_READ`](tairix_abi::SyscallNumber::KEYBOARD_READ) syscalls on a
//! running system, an in-memory queue in tests). Only the task holding the
//! seat lease may drain — the kernel owner-gates every read — and the
//! channel implements both [`PointerInputChannel`] and [`KeyInputChannel`]
//! through one shared, fail-closed validation path: a drain of anything
//! other than exactly one whole record surfaces an error, so a truncated
//! read can never be decoded as a spurious event.
//!
//! # Window-owner responsiveness ("not responding")
//!
//! The session's app-ward window events go out as non-blocking mailbox
//! sends, so an app that stops draining its mailbox surfaces as refused
//! deliveries. [`HangTracker`] (the [`vigil`] module) folds those
//! per-delivery outcomes into per-owner verdicts — backpressure refusals
//! spanning [`UNRESPONSIVE_AFTER_NS`] flag the owner unresponsive, one
//! accepted delivery clears it — and the count feeds the taskbar's
//! Switchboard tray capsule (`plans/NEW-TASKBAR.md` T9/T10) so a wedged
//! app is visible without any fabricated heartbeat.
//!
//! # Window registry ↔ window stack
//!
//! The taskbar models the window registry its picker and Switchboard capsule
//! read, but owns no window manager, and the window manager owns no registry.
//! [`TaskBridge`] (the
//! [`tasks`] module) is the glue between them: it owns the correspondence
//! between compositor windows and registry entries, [`open`](TaskBridge::open)s
//! and [`close`](TaskBridge::close)s top-level windows, raises a chosen one
//! ([`raise`](TaskBridge::raise)), and mirrors a window-manager focus
//! change back into the bar's highlight ([`sync_focus`](TaskBridge::sync_focus)).
//! [`DesktopShell`] drives it: [`open_window`](DesktopShell::open_window) /
//! [`close_window`](DesktopShell::close_window) manage the lifecycle, and
//! [`pump`](DesktopShell::pump) keeps the bar and the window stack in step as
//! input arrives.
//!
//! [`ThemeRegistry`]: tairix_theme::ThemeRegistry
//! [`Taskbar`]: tairix_taskbar::Taskbar

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod apps;
pub mod artwork;
pub mod assets;
pub mod cli;
pub mod config;
pub mod confirm;
pub mod desktop;
pub mod device;
pub mod fade;
pub mod holdback;
pub mod input;
pub mod keyboard;
pub mod launch;
pub mod library;
pub mod listing;
pub mod lock;
pub mod picker;
pub mod pinboard;
pub mod presenter;
pub mod seat;
pub mod session;
pub mod settings;
pub mod shell;
pub mod switchboard;
pub mod switchuser;
pub mod tasks;
pub mod vigil;
pub mod wallpaper;
pub mod windows;

#[cfg(test)]
mod desktop_tests;
#[cfg(test)]
mod holdback_tests;
#[cfg(test)]
mod switchuser_tests;
#[cfg(test)]
mod tests;

pub use apps::{
    picker_cells, prefetch_bar_icons, resolve_library_icons, thumbnail, AppBarBridge,
    AppBarService, AppGroup, ArtworkFileReader, ArtworkSandbox, Declaration, IconRasteriser,
    BUNDLE_RUN_SUFFIX, MAX_BAR_APPS,
};
pub use artwork::{ArtworkDesk, ArtworkJob};
pub use assets::{load_cursor_theme, load_icon_set, SessionFileReader, SessionFileWriter};
pub use cli::{parse, CliError, Command, USAGE};
pub use config::{
    FILES_LABEL, FILES_RUN_PATH, SWITCHBOARD_LABEL, SWITCHBOARD_RUN_PATH, WALLPAPER_LABEL,
    WALLPAPER_RUN_PATH,
};
pub use confirm::{Answer, ConfirmPrompt, CONFIRM_ORIGIN};
pub use desktop::{
    Desktop, DesktopAction, DesktopActivation, DesktopOutcome, PinboardChange, DESKTOP_MARGIN,
    RELIST_MIN_INTERVAL_NS,
};
pub use device::{DeviceInputSource, PointerInputChannel};
pub use fade::{
    ScreenFade, DESKTOP_REVEALED, DESKTOP_REVEALED_MESSAGE, DESKTOP_SESSION_RANGE_END,
    DESKTOP_SESSION_RANGE_START,
};
pub use holdback::{Delivery, Flushed, HoldBack, HOLD_BACK_CAPACITY};
pub use input::{SessionInputResponse, SessionInputRouter};
pub use keyboard::{KeyInputChannel, KeyboardInputSource};
pub use launch::{admitted_pid, launch_failure_report, reap_launched, LaunchTable, LaunchedApp};
pub use library::{catalogued, load_library, LoadedLibrary};
pub use listing::{ListingClient, ListingDesk};
pub use lock::{LockOutcome, LockedDrain, ScreenLock};
pub use picker::{
    ConcludedPick, PickConclusion, PickerSlot, SessionPicker, PICKER_ORIGIN, PICKER_TITLE,
};
pub use pinboard::{PinboardCommand, PinboardMenu, PinboardMenuOutcome};
pub use presenter::TaskbarPresenter;
pub use seat::{SeatEventReader, SeatInputChannel};
pub use session::DesktopSession;
pub use settings::{
    serve_pinboard_apply, LoadedPinboard, PinboardApplyRefusal, PinboardStore, PinboardStoreError,
};
pub use shell::{DesktopShell, InputSource, ShellOutcome};
pub use switchboard::{
    deliver_pending_open, drop_is_noteworthy, ensure_switchboard, maybe_send_frame_report,
    maybe_send_seat_report, open_tray, relay_power, serve_switchboard_request, FrameContent,
    OwnerWindow, PresentedOwners, SwitchboardMailbox, SwitchboardOutcome, SwitchboardRefusal,
    SwitchboardServe, SWITCHBOARD_CALL_REFUSED,
};
pub use switchuser::{
    ResumeFailure, SeatPresentation, SessionAuthority, SwitchRefusal, SwitchUser, WakeRefusal,
    NO_DEADLINE_NS,
};
pub use tasks::TaskBridge;
pub use vigil::{HangTracker, UNRESPONSIVE_AFTER_NS};
pub use wallpaper::{Prepared, WallpaperDesk, WallpaperSource};
pub use windows::{
    desktop_info, resolve_window_identities, window_control_alternate_event, window_control_event,
    SessionWindows, ShellWindowHost, WINDOW_SHOWN, WINDOW_SHOWN_MESSAGE,
};
