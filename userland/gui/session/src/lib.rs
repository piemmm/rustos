//! RustOS desktop **session glue** (`userland/gui/session`).
//!
//! The taskbar models the desktop's controls but, by design, owns no theme
//! registry and no spawn capability: activating its start-menu entries only
//! *reports* an abstract [`MenuAction`](rustos_taskbar::MenuAction) (e.g. the
//! light/dark [`ToggleAppearance`](rustos_taskbar::MenuAction::ToggleAppearance)),
//! leaving the actual work to the session glue (`AGENTS.md` §10, §16.5). This
//! crate is that glue.
//!
//! [`DesktopSession`] owns the one shared [`ThemeRegistry`] and the
//! [`Taskbar`] model. It [`resolve`](DesktopSession::resolve)s a
//! [`TaskbarResponse`] into a [`SessionEvent`]: the appearance toggle is the
//! one action it performs itself — switching the registry and re-theming the
//! taskbar in place — and it reports the now-active [`ThemeId`] so the
//! embedder relays the new theme to the window manager and apps. Every other
//! response is [`SessionEvent::Forward`]ed unchanged for the embedder (which
//! holds the window-manager and process capabilities) to act on.
//!
//! Switching the theme — whether by [`toggle_appearance`] or by
//! [`set_theme`] — re-themes the taskbar through one private apply path, so
//! the relay logic is never duplicated (`AGENTS.md` §2.2). Setting an
//! unknown theme fails closed without disturbing the active theme
//! (`AGENTS.md` §5.4 / §2.9).
//!
//! # Presenting the taskbar through the window manager
//!
//! [`TaskbarPresenter`] (the [`presenter`] module) is the session's glue to
//! the compositor: it paints the taskbar's bar — and, while open, its
//! start-menu popup — with the taskbar's own
//! [`TaskbarRenderer`](rustos_taskbar::TaskbarRenderer) and presents each as a
//! window in the [`Compositor`](rustos_wm::Compositor), placed at its computed
//! origin and rounded through the compositor's single anti-aliased
//! rounded-corner path (`AGENTS.md` §2.2). Composing the taskbar and window
//! manager is the permitted `userland/gui/*` edge (§17.4).
//!
//! # Routing live input to the taskbar and window manager
//!
//! A real input source produces one stream of pointer events, but the desktop
//! has two routers — the window manager's
//! [`InputRouter`](rustos_wm::InputRouter) and the taskbar's
//! [`TaskbarInput`](rustos_taskbar::TaskbarInput). [`SessionInputRouter`] (the
//! [`input`] module) is the glue that fans that one stream to the right one:
//! the taskbar claims a press over the bar or while its menu is open, the
//! window manager handles everything else, motion is fanned to both so their
//! pointers stay in step, and a release ends a window move-grab. Composing the
//! two GUI crates this way is the permitted `userland/gui/*` edge (§17.4).
//!
//! # On-disk graphics assets
//!
//! The session also loads the desktop's SVG graphics assets from
//! `/System/Graphics` (`AGENTS.md` §10 / §16.2): the [`assets`] module's
//! [`GraphicsAssetReader`] seam reads the bytes (a filesystem capability the
//! `no_std` `lib/cursor` / `lib/icon` crates must not hold, §17.4 / §19.5),
//! and [`DesktopSession::load_cursors`] / [`DesktopSession::load_icons`]
//! assemble a [`CursorTheme`](rustos_cursor::CursorTheme) /
//! [`IconSet`](rustos_icon::IconSet), failing closed per kind to the built-in
//! artwork (§2.9).
//!
//! # Where it sits
//!
//! As a `userland/gui/*` crate it composes the other GUI crates and `lib/*`
//! only (`AGENTS.md` §17.4): it owns the [`Taskbar`] and reads the shared
//! [`rustos_theme`] definition. Nothing outside `userland/gui/*` depends on
//! it (§17.3) — the desktop is an optional, one-way-dependent frontend.
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
//! spawn capabilities) to act on (`AGENTS.md` §10, §16.5).
//!
//! The live backing for that [`InputSource`] is [`DeviceInputSource`] (the
//! [`device`] module): it reads framed
//! [`PointerInput`](rustos_abi::input::PointerInput) records from an injected
//! [`PointerInputChannel`] (the kernel input channel) and decodes each into
//! the `lib/input` [`InputEvent`](rustos_wm::InputEvent) the shell routes,
//! failing closed on a malformed record (`AGENTS.md` §5.4 / §19.5).
//!
//! # Running-task list ↔ window stack
//!
//! The taskbar models a running-task list but owns no window manager, and the
//! window manager owns no task list (`AGENTS.md` §17.4). [`TaskBridge`] (the
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
//! [`toggle_appearance`]: DesktopSession::toggle_appearance
//! [`set_theme`]: DesktopSession::set_theme
//! [`ThemeRegistry`]: rustos_theme::ThemeRegistry
//! [`ThemeId`]: rustos_theme::ThemeId
//! [`Taskbar`]: rustos_taskbar::Taskbar
//! [`TaskbarResponse`]: rustos_taskbar::TaskbarResponse

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod assets;
pub mod device;
pub mod input;
pub mod presenter;
pub mod session;
pub mod shell;
pub mod tasks;

#[cfg(test)]
mod tests;

pub use assets::{load_cursor_theme, load_icon_set, GraphicsAssetReader, GRAPHICS_DIR};
pub use device::{DeviceInputSource, PointerInputChannel};
pub use input::{SessionInputResponse, SessionInputRouter};
pub use presenter::TaskbarPresenter;
pub use session::{DesktopSession, SessionEvent};
pub use shell::{DesktopShell, InputSource, ShellOutcome};
pub use tasks::TaskBridge;
