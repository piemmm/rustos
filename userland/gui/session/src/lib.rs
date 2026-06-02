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
//! # Where it sits
//!
//! As a `userland/gui/*` crate it composes the other GUI crates and `lib/*`
//! only (`AGENTS.md` §17.4): it owns the [`Taskbar`] and reads the shared
//! [`rustos_theme`] definition. Nothing outside `userland/gui/*` depends on
//! it (§17.3) — the desktop is an optional, one-way-dependent frontend.
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

pub mod session;

#[cfg(test)]
mod tests;

pub use session::{DesktopSession, SessionEvent};
