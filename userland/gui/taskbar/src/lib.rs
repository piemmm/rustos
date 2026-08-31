//! TAIRiX traditional desktop **taskbar** (`userland/gui/taskbar`).
//!
//! The taskbar is the GNOME/Windows-style bar pinned to a configured screen
//! edge (`PLAN.md` Stage 7, `plans/NEW-TASKBAR.md`). Along its main axis it
//! is laid out as:
//!
//! - **Leading end** — one permanent launcher button, never moved and never
//!   removable: **Library**, which opens the program-library popup
//!   ([`LibraryPopup`] — the folder-organised application launcher fed from
//!   the merged `lib/proglib` catalog). It is a shared `lib/controls` icon
//!   button drawn with a `lib/icon` glyph. The file manager is *not* a
//!   launcher: it is a core desktop component the session autostarts, so it
//!   holds an ordinary application slot in the strip below.
//! - **Middle** — the [`AppStrip`]: one icon-only slot per *running
//!   application*, resolved by the session from the bundle the kernel
//!   attested owns each process. A primary click performs the application's
//!   own default action (or, for an application that declared none, raises
//!   its most recently used window); hovering a slot whose application owns
//!   more than one window opens the [`WindowPicker`] to choose between them;
//!   and a secondary press asks the desktop to open the menu the *application
//!   itself* declared ([`MenuRequest`]). Every such menu reads in one order —
//!   the desktop-drawn *Info* row first, whose child is the application's
//!   information panel drawn from its signed manifest; the application's own
//!   rows next; and *Quit* last. That convention is the applications' to
//!   follow and is written once, in `tairix_window::declaration`, not
//!   restated here: the bar states exactly what it was declared. An
//!   application that declared no menu asks for nothing. The windows themselves
//!   stay in the [`TaskList`], the one window registry the picker and the
//!   Switchboard capsule both read.
//! - **Trailing end** — the [`NotificationArea`]: the persistent status
//!   signals (network, volume, battery) drawn as calm glyphs, plus a card
//!   popover for the transient notifications a producer service raises over
//!   the notification IPC; then the clock; and anchored at the very end —
//!   immovable, outranked only by the leading launcher — the Switchboard
//!   tray capsule ([`SwitchboardTray`]): the desktop's live system readout,
//!   with an instrument readout that opens on hover or keyboard focus,
//!   scroll-to-cycle-tasks, and a
//!   middle-click switch to the previous task.
//!
//! The taskbar holds no authority and performs no I/O: choosing a library
//! entry only *reports* a typed [`TaskbarResponse`]
//! ([`LibraryLaunch`](TaskbarResponse::LibraryLaunch)); the session glue —
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
pub mod clock_menu;
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
pub use clock_menu::{ClockPermits, ClockRow};
pub use edge::{Edge, Orientation};
pub use input::{TaskbarInput, TaskbarResponse};
pub use layout::{BarLayout, Hit, NotificationCard, NotificationsLayout, TrayReadoutLayout};
pub use library::{
    folder_label, LibraryFocus, LibraryIconRequest, LibraryLayout, LibraryPopup, LibraryRow,
};
pub use menu::{EntryRow, MenuRequest, MenuSubject};
pub use notifications::{
    IconId, NotificationArea, NotifySeverity, StatusKind, StatusSignal, TransientNotification,
};
pub use picker::{
    PickerEntry, PickerLayout, WindowPicker, PICKER_CLOSE_GRACE_NS, PICKER_MIN_WINDOWS,
    PICKER_OPEN_DELAY_NS,
};
pub use render::{icon_cache, IconEpoch, TaskbarRenderer};
pub use repaint::TaskbarRepaint;
pub use system::{SystemAction, SystemPermits, SystemRow};
pub use taskbar::{Taskbar, TaskbarConfig};
pub use tasks::{TaskEntry, TaskId, TaskList};
pub use tray::SwitchboardTray;
