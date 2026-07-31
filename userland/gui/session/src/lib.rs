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
//! # Running-task list ↔ window stack
//!
//! The taskbar models a running-task list but owns no window manager, and the
//! window manager owns no task list. [`TaskBridge`] (the
//! [`tasks`] module) is the glue between them: it owns the correspondence
//! between compositor windows and taskbar tasks, [`open`](TaskBridge::open)s
//! and [`close`](TaskBridge::close)s top-level windows as running tasks,
//! applies the bar's click-to-activate / minimise outcome to the compositor
//! ([`activate`](TaskBridge::activate)), and mirrors a window-manager focus
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

pub mod assets;
pub mod cli;
pub mod config;
pub mod device;
pub mod input;
pub mod keyboard;
pub mod launch;
pub mod library;
pub mod picker;
pub mod presenter;
pub mod seat;
pub mod session;
pub mod shell;
pub mod tasks;
pub mod windows;

#[cfg(test)]
mod tests;

pub use assets::{load_cursor_theme, load_icon_set, SessionFileReader, GRAPHICS_DIR};
pub use cli::{parse, CliError, Command, USAGE};
pub use config::{FILES_LABEL, FILES_RUN_PATH};
pub use device::{DeviceInputSource, PointerInputChannel};
pub use input::{SessionInputResponse, SessionInputRouter};
pub use keyboard::{KeyInputChannel, KeyboardInputSource};
pub use launch::{launch_failure_report, reap_launched, LaunchTable, LaunchedApp};
pub use library::{load_library, LoadedLibrary};
pub use picker::{
    ConcludedPick, PickConclusion, PickerSlot, SessionPicker, PICKER_ORIGIN, PICKER_TITLE,
};
pub use presenter::TaskbarPresenter;
pub use seat::{SeatEventReader, SeatInputChannel};
pub use session::DesktopSession;
pub use shell::{DesktopShell, InputSource, ShellOutcome};
pub use tasks::TaskBridge;
pub use windows::{window_control_event, SessionWindows, ShellWindowHost};
