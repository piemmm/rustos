//! Which of the taskbar's rendered surfaces need repainting.
//!
//! The taskbar presents up to five independent pixel surfaces at once: the
//! bar strip, the program-library popup, the context menu, the notification
//! popover, and the Switchboard capsule's expanded readout. A hover moving
//! within a small popup, or a highlight moving inside an open menu, changes
//! only that one surface's pixels — the other four are untouched. Latching a
//! single flag for all five (as the bar once did) forces the embedder to
//! re-render and re-composite every surface for every such change, which is
//! measurably wasteful: repainting the bar and the library popup cost more
//! than ten times what repainting the context menu alone costs, so a single
//! flag turns cheap, frequent hovers into the most expensive path on the
//! desktop. [`TaskbarRepaint`] names each surface separately so
//! [`Taskbar::take_repaint`](crate::Taskbar::take_repaint) tells the embedder
//! exactly which of the five actually changed.
//!
//! [`Taskbar`](crate::Taskbar) latches this per-part rather than as one bit,
//! and its per-site latches are attributed by construction: a state change
//! that touches only what one surface draws sets only that surface's flag,
//! while a change that touches several (opening the library popup also
//! presses the bar's Library button; a theme swap repaints everything) sets
//! all of them, composed with [`BitOr`]/[`BitOrAssign`].

use core::ops::{BitOr, BitOrAssign};

/// Which of the taskbar's rendered surfaces need repainting.
///
/// See the [module docs](self) for why this is five flags rather than one,
/// and [`Taskbar::take_repaint`](crate::Taskbar::take_repaint) for the exact
/// contract each flag promises.
// The five surfaces are independent yes/no facts about one frame, not a state
// machine: every combination is legal and is produced in practice (a menu
// hover latches one, opening the library popup latches two, a theme swap
// latches all five). Folding them into an enum or sub-structs would hide
// which surface a latch site actually dirties, which is the whole point of
// the type.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarRepaint {
    /// The bar strip itself.
    pub bar: bool,
    /// The program-library popup.
    pub library: bool,
    /// The bar's context menu.
    pub menu: bool,
    /// The notification popover.
    pub notifications: bool,
    /// The Switchboard capsule's expanded instrument readout.
    pub readout: bool,
}

impl TaskbarRepaint {
    /// No surface needs repainting.
    pub const NONE: Self = Self {
        bar: false,
        library: false,
        menu: false,
        notifications: false,
        readout: false,
    };

    /// Only the bar strip.
    pub const BAR: Self = Self {
        bar: true,
        ..Self::NONE
    };

    /// Only the program-library popup.
    pub const LIBRARY: Self = Self {
        library: true,
        ..Self::NONE
    };

    /// Only the bar's context menu.
    pub const MENU: Self = Self {
        menu: true,
        ..Self::NONE
    };

    /// Only the notification popover.
    pub const NOTIFICATIONS: Self = Self {
        notifications: true,
        ..Self::NONE
    };

    /// Only the Switchboard capsule's expanded readout.
    pub const READOUT: Self = Self {
        readout: true,
        ..Self::NONE
    };

    /// Every surface — a theme, scale, or edge change alters the geometry or
    /// palette every one of the five draws with.
    pub const ALL: Self = Self {
        bar: true,
        library: true,
        menu: true,
        notifications: true,
        readout: true,
    };

    /// Whether any surface is latched.
    #[must_use]
    pub const fn any(self) -> bool {
        self.bar || self.library || self.menu || self.notifications || self.readout
    }
}

impl BitOr for TaskbarRepaint {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self {
            bar: self.bar || rhs.bar,
            library: self.library || rhs.library,
            menu: self.menu || rhs.menu,
            notifications: self.notifications || rhs.notifications,
            readout: self.readout || rhs.readout,
        }
    }
}

impl BitOrAssign for TaskbarRepaint {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}
