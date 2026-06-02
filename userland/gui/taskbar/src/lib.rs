//! RustOS traditional desktop **taskbar** (`userland/gui/taskbar`).
//!
//! The taskbar is the GNOME/Windows-style bar pinned to a configured screen
//! edge (`AGENTS.md` §10, `PLAN.md` Stage 7). Along its main axis it is laid
//! out as:
//!
//! - **Leading end** — a [`StartMenu`] button. The menu is *not* an
//!   application launcher; at this stage it holds only session controls
//!   (log out, lock, shut down, restart). It is built so launcher entries
//!   can be added later (a new [`MenuAction`] variant) without changing the
//!   list/activate interface.
//! - **Middle** — a [`TaskList`]: one entry per top-level window, with
//!   click-to-activate and minimise/restore.
//! - **Trailing end** — a clock anchored to the very end, with a
//!   [`NotificationArea`] of status icons immediately before it.
//!
//! # What this increment delivers
//!
//! The Stage 7 taskbar **layout, model, and rendering**: the geometry of
//! every region ([`BarLayout`]), pointer [`hit_test`](BarLayout::hit_test)ing
//! for input routing, the start-menu / task-list / notification-area state
//! machines, and [`render`]ing those regions into a themed pixel
//! [`Surface`](rustos_raster::Surface). It sources its corner radius and
//! colours from the active theme ([`rustos_theme`]) and exposes the radius
//! through [`BarLayout::corner_radius`]; the taskbar never rounds its own
//! corners — the window manager applies that radius through its single
//! anti-aliased rounded-corner path (`AGENTS.md` §2.2), exactly as it rounds
//! windows.
//!
//! Glyph rendering (clock and task-title text), notification-icon artwork,
//! and the live IPC wiring to the window manager build on this model in
//! later Stage 7 increments.
//!
//! [`render`]: render::render
//!
//! # Where it sits
//!
//! As a `userland/gui/*` crate it depends only on `lib/*` — the shared
//! [`rustos_geometry`] coordinate types, the shared [`rustos_raster`]
//! surface, and the shared [`rustos_theme`] definition — never on the window
//! manager or any sibling userland crate (`AGENTS.md` §17.4). Nothing depends
//! on it in turn (§17.3): the desktop is an optional, one-way-dependent
//! frontend.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod clock;
pub mod edge;
pub mod layout;
pub mod menu;
pub mod notifications;
pub mod render;
pub mod taskbar;
pub mod tasks;

#[cfg(test)]
mod tests;

pub use clock::Clock;
pub use edge::{Edge, Orientation};
pub use layout::{BarLayout, Hit};
pub use menu::{MenuAction, MenuEntry, MenuEntryId, SessionControl, StartMenu};
pub use notifications::{IconId, NotificationArea, NotificationIcon};
pub use render::render;
pub use taskbar::{Taskbar, TaskbarConfig};
pub use tasks::{ActivateOutcome, TaskEntry, TaskId, TaskList};
