//! TAIRiX traditional desktop **taskbar** (`userland/gui/taskbar`).
//!
//! The taskbar is the GNOME/Windows-style bar pinned to a configured screen
//! edge (`PLAN.md` Stage 7, `plans/NEW-TASKBAR.md`). Along its main axis it
//! is laid out as:
//!
//! - **Leading end** — two permanent launcher buttons, never reordered and
//!   never removable: **Library**, which opens the program-library popup
//!   ([`LibraryPopup`] — the folder-organised application launcher fed from
//!   the merged `lib/proglib` catalog), and **Files**, which opens the file
//!   manager. Both are shared `lib/controls` icon buttons drawn with
//!   `lib/icon` glyphs.
//! - **Pin strip** — a [`PinStrip`] of user-pinned application shortcuts
//!   (the per-user `lib/taskpins` store, resolved by the session into
//!   [`PinView`]s): icon-only slots that launch when idle and follow the
//!   click-to-activate rule when their application is running, with a
//!   right-click [`BarMenu`] offering *Open* and *Unpin*.
//! - **Middle** — a [`TaskList`]: one entry per top-level window, with
//!   click-to-activate and minimise/restore.
//! - **Trailing end** — a clock anchored to the very end, with a
//!   [`NotificationArea`] of status icons immediately before it.
//!
//! The taskbar holds no authority and performs no I/O: pressing Files or
//! choosing a library entry only *reports* a typed [`TaskbarResponse`]
//! ([`OpenFiles`](TaskbarResponse::OpenFiles) /
//! [`LibraryLaunch`](TaskbarResponse::LibraryLaunch)); the session glue —
//! which reads the catalog stores and holds the spawn capability — resolves
//! and performs the action. The popup never touches the VFS: the session
//! hands it the already-merged [`Catalog`](tairix_proglib::Catalog) as a
//! typed view model.
//!
//! # What this crate delivers
//!
//! The taskbar **layout, model, and rendering**: the geometry of every
//! region ([`BarLayout`]), pointer [`hit_test`](BarLayout::hit_test)ing for
//! input routing, the library-popup / task-list / notification-area state
//! machines, and the [`TaskbarRenderer`] that paints those regions into a
//! themed pixel [`Surface`](tairix_raster::Surface). The bar owns a copy of
//! the active theme ([`Taskbar::theme`]) so layout, hit-testing, and painting
//! read one definition; the window manager applies
//! [`BarLayout::corner_radius`] through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows — the taskbar never
//! rounds its own corners.
//!
//! # Where it sits
//!
//! As a `userland/gui/*` crate it depends only on `lib/*` — the shared
//! [`tairix_geometry`] coordinate types, the shared [`tairix_raster`]
//! surface, the shared control vocabulary ([`tairix_controls`]), the
//! program-library catalog engine ([`tairix_proglib`]), and the shared
//! [`tairix_theme`] definition — never on the window manager or any sibling
//! userland crate. Nothing depends on it in turn: the desktop is an
//! optional, one-way-dependent frontend.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod clock;
pub mod edge;
pub mod input;
pub mod layout;
pub mod library;
pub mod menu;
pub mod notifications;
pub mod pins;
pub mod render;
pub mod taskbar;
pub mod tasks;

#[cfg(test)]
mod tests;

pub use clock::Clock;
pub use edge::{Edge, Orientation};
pub use input::{TaskbarInput, TaskbarResponse};
pub use layout::{BarLayout, Hit};
pub use library::{folder_label, LibraryFocus, LibraryLayout, LibraryPopup, LibraryRow};
pub use menu::{BarMenu, MenuLayout, MenuSubject};
pub use notifications::{IconId, NotificationArea, NotificationIcon};
pub use pins::{PinStrip, PinView};
pub use render::TaskbarRenderer;
pub use taskbar::{Taskbar, TaskbarConfig};
pub use tasks::{ActivateOutcome, TaskEntry, TaskId, TaskList};
