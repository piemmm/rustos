//! Which of the taskbar's rendered surfaces need repainting, and what of each.
//!
//! The taskbar presents up to five independent pixel surfaces at once: the
//! bar strip, the program-library popup, the window picker, the notification
//! popover, and the Switchboard capsule's expanded readout. A hover moving
//! within a small popup changes only that one surface's pixels — the others
//! are untouched. Latching a single flag for all of them (as the bar once
//! did) forces the embedder to re-render and re-composite every surface for
//! every such change, which is measurably wasteful: repainting the bar and
//! the library popup cost more than ten times what repainting a small popover
//! alone costs, so a single flag turns cheap, frequent hovers into the most
//! expensive path on the desktop. [`TaskbarRepaint`] carries one
//! [`Repaint`] account per surface so
//! [`Taskbar::take_repaint`](crate::Taskbar::take_repaint) tells the embedder
//! exactly which of them changed **and which of their pixels**.
//!
//! A whole-surface account is what a change to the model itself owes — a new
//! clock label, a rebuilt application strip, a theme swap — because such a
//! change has no rectangle smaller than the surface. A change a *control*
//! reports owes only that control's own rectangles, which is what keeps a
//! pointer crossing the bar costing the two slots it moved between rather
//! than the whole strip.
//!
//! [`Taskbar`](crate::Taskbar) latches this per-surface rather than as one
//! bit, and its per-site latches are attributed by construction: a state
//! change that touches only what one surface draws owes only that surface,
//! while a change that touches several (opening the library popup also
//! presses the bar's Library button; a theme swap repaints everything) owes
//! all of them, composed with [`BitOr`]/[`BitOrAssign`].
//!
//! A menu is not among them: every menu on the desktop is the seat's one
//! chain, drawn by the session (`plans/NEW-MENUS.md`), so the bar has no menu
//! pixels to latch — though it owes its plates the same per-rectangle account,
//! which is why [`Repaint`] is the shared one.

use core::ops::{BitOr, BitOrAssign};

use tairix_controls::damage::Repaint;

/// Which of the taskbar's rendered surfaces need repainting, and what of each.
///
/// See the [module docs](self) for why this is five accounts rather than one
/// flag, and [`Taskbar::take_repaint`](crate::Taskbar::take_repaint) for the
/// exact contract each promises.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarRepaint {
    /// The bar strip itself.
    pub bar: Repaint,
    /// The program-library popup.
    pub library: Repaint,
    /// The window picker an application slot opens on hover.
    pub picker: Repaint,
    /// The notification popover.
    pub notifications: Repaint,
    /// The Switchboard capsule's expanded instrument readout.
    pub readout: Repaint,
}

impl TaskbarRepaint {
    /// No surface needs repainting.
    pub const NONE: Self = Self {
        bar: Repaint::clean(),
        library: Repaint::clean(),
        picker: Repaint::clean(),
        notifications: Repaint::clean(),
        readout: Repaint::clean(),
    };

    /// The whole bar strip.
    pub const BAR: Self = Self {
        bar: Repaint::Whole,
        library: Repaint::clean(),
        picker: Repaint::clean(),
        notifications: Repaint::clean(),
        readout: Repaint::clean(),
    };

    /// The whole program-library popup.
    pub const LIBRARY: Self = Self {
        bar: Repaint::clean(),
        library: Repaint::Whole,
        picker: Repaint::clean(),
        notifications: Repaint::clean(),
        readout: Repaint::clean(),
    };

    /// The whole window picker.
    pub const PICKER: Self = Self {
        bar: Repaint::clean(),
        library: Repaint::clean(),
        picker: Repaint::Whole,
        notifications: Repaint::clean(),
        readout: Repaint::clean(),
    };

    /// The whole notification popover.
    pub const NOTIFICATIONS: Self = Self {
        bar: Repaint::clean(),
        library: Repaint::clean(),
        picker: Repaint::clean(),
        notifications: Repaint::Whole,
        readout: Repaint::clean(),
    };

    /// The whole expanded readout.
    pub const READOUT: Self = Self {
        bar: Repaint::clean(),
        library: Repaint::clean(),
        picker: Repaint::clean(),
        notifications: Repaint::clean(),
        readout: Repaint::Whole,
    };

    /// Every surface, whole — a theme, scale, or edge change alters the
    /// geometry or palette every one of them draws with.
    pub const ALL: Self = Self {
        bar: Repaint::Whole,
        library: Repaint::Whole,
        picker: Repaint::Whole,
        notifications: Repaint::Whole,
        readout: Repaint::Whole,
    };

    /// Whether any surface owes anything.
    #[must_use]
    pub fn any(&self) -> bool {
        !(self.bar.is_clean()
            && self.library.is_clean()
            && self.picker.is_clean()
            && self.notifications.is_clean()
            && self.readout.is_clean())
    }
}

impl BitOr for TaskbarRepaint {
    type Output = Self;

    fn bitor(mut self, rhs: Self) -> Self {
        self |= rhs;
        self
    }
}

impl BitOrAssign for TaskbarRepaint {
    fn bitor_assign(&mut self, rhs: Self) {
        self.bar.merge(rhs.bar);
        self.library.merge(rhs.library);
        self.picker.merge(rhs.picker);
        self.notifications.merge(rhs.notifications);
        self.readout.merge(rhs.readout);
    }
}
