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
//! - **Middle** — the [`AppStrip`]: one icon-only slot per *running
//!   application*, resolved by the session from the bundle the kernel
//!   attested owns each process. A primary click performs the application's
//!   own default action (or, for an application that declared none, raises
//!   its most recently used window); hovering a slot whose application owns
//!   more than one window opens the [`WindowPicker`] to choose between them;
//!   and a secondary press opens the [`BarMenu`] over the menu the
//!   *application itself* declared — minimally a *Quit* row and an *About*
//!   row whose submenu is the application's information panel, drawn from
//!   its signed manifest. An application that declared no menu opens
//!   nothing. The windows themselves stay in the [`TaskList`], the one
//!   window registry the picker and the Switchboard capsule both read.
//! - **Trailing end** — the [`NotificationArea`]: the persistent status
//!   signals (network, volume, battery) drawn as calm glyphs, plus a card
//!   popover for the transient notifications a producer service raises over
//!   the notification IPC; then the clock; and anchored at the very end —
//!   immovable, outranked only by the leading launchers — the Switchboard
//!   tray capsule ([`SwitchboardTray`]): the desktop's live system readout,
//!   with a hover/pinned instrument readout, scroll-to-cycle-tasks, and a
//!   middle-click switch to the previous task.
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

pub mod apps;
pub mod clock;
pub mod edge;
pub mod input;
pub mod layout;
pub mod library;
pub mod menu;
pub mod notifications;
pub mod picker;
pub mod render;
pub mod repaint;
pub mod system;
pub mod taskbar;
pub mod tasks;
pub mod tray;

#[cfg(test)]
mod tests;

pub use apps::{AppIdentity, AppSlot, AppStrip};
pub use clock::Clock;
pub use edge::{Edge, Orientation};
pub use input::{TaskbarInput, TaskbarResponse};
pub use layout::{BarLayout, Hit, NotificationCard, NotificationsLayout, TrayReadoutLayout};
pub use library::{
    folder_label, LibraryFocus, LibraryIconRequest, LibraryLayout, LibraryPopup, LibraryRow,
};
pub use menu::{BarMenu, MenuLayout, MenuSubject, MENU_OPEN_ROW};
pub use notifications::{
    IconId, NotificationArea, NotifySeverity, StatusKind, StatusSignal, TransientNotification,
};
pub use picker::{PickerEntry, PickerLayout, WindowPicker, PICKER_MIN_WINDOWS};
pub use render::{icon_cache, IconEpoch, TaskbarRenderer};
pub use repaint::TaskbarRepaint;
pub use system::{SystemAction, SystemPermits, SystemRow};
pub use taskbar::{Taskbar, TaskbarConfig};
pub use tasks::{TaskEntry, TaskId, TaskList};
pub use tray::SwitchboardTray;
