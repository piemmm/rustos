//! The application strip: one slot per running application.
//!
//! An icon bar lists *applications*, not windows. Each slot is one running
//! application as the desktop session resolved it: its display name and icon
//! from the bundle the kernel attested owns the process, the windows it
//! currently owns, and — when it declared one over the window channel — the
//! menu a secondary press opens and whether it handles the primary click
//! itself.
//!
//! The windows themselves stay in the [`TaskList`](crate::TaskList): that is
//! the one window registry, and a slot names its windows by id rather than
//! holding a second copy of their state. A slot with more than one window
//! opens a [`WindowPicker`](crate::WindowPicker) on hover, which is where a
//! window is chosen.
//!
//! The strip holds no authority: it renders what the session resolved and
//! reports typed outcomes. The declaration's event route stays with the
//! session — the bar never learns where an application's mailbox is.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::window_ipc::AppMenu;
use tairix_controls::{ControlState, PointerState, TaskbarItem};
use tairix_icon::IconKind;
use tairix_raster::Surface;

use crate::tasks::TaskId;

/// The identity an application's information panel states, read from the
/// bundle's **signed** manifest.
///
/// The panel is drawn by the session in system chrome, so its text comes from
/// what the bundle's signer said rather than from anything the running
/// process claims: an application cannot state an identity that is not its
/// own. A field the manifest omits is simply absent from the panel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AppIdentity {
    /// The bundle's human-readable name.
    pub name: String,
    /// The bundle's version string.
    pub version: String,
    /// The bundle's one-line purpose, when it states one.
    pub purpose: Option<String>,
    /// The bundle's author attribution, when it names one.
    pub author: Option<String>,
}

/// One running application, as the session resolved it for the bar.
#[derive(Clone, Debug)]
pub struct AppSlot {
    label: String,
    icon: IconKind,
    artwork: Option<Surface>,
    windows: Vec<TaskId>,
    menu: AppMenu,
    handles_default: bool,
    identity: AppIdentity,
}

impl AppSlot {
    /// A slot for an application with the given display label and class
    /// glyph: no windows, no declaration, and the label as its whole
    /// identity.
    #[must_use]
    pub fn new(label: impl Into<String>, icon: IconKind) -> Self {
        let label = label.into();
        Self {
            identity: AppIdentity {
                name: label.clone(),
                ..AppIdentity::default()
            },
            label,
            icon,
            artwork: None,
            windows: Vec::new(),
            menu: AppMenu::EMPTY,
            handles_default: false,
        }
    }

    /// This slot with the application's own rasterised icon artwork.
    #[must_use]
    pub fn with_artwork(mut self, artwork: Surface) -> Self {
        self.artwork = Some(artwork);
        self
    }

    /// This slot owning `windows`, in the order they opened.
    #[must_use]
    pub fn with_windows(mut self, windows: Vec<TaskId>) -> Self {
        self.windows = windows;
        self
    }

    /// This slot carrying the application's icon-bar declaration: the menu a
    /// secondary press opens, and whether the application handles the
    /// primary click itself.
    #[must_use]
    pub fn with_declaration(mut self, menu: AppMenu, handles_default: bool) -> Self {
        self.menu = menu;
        self.handles_default = handles_default;
        self
    }

    /// This slot with the identity its information panel states.
    #[must_use]
    pub fn with_identity(mut self, identity: AppIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// The application's display label (read by context surfaces, never
    /// drawn on the slot itself).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The application's class glyph, drawn when no artwork is available.
    #[must_use]
    pub fn icon(&self) -> IconKind {
        self.icon
    }

    /// The application's rasterised icon artwork, if the session loaded one.
    #[must_use]
    pub fn artwork(&self) -> Option<&Surface> {
        self.artwork.as_ref()
    }

    /// The windows this application owns, in the order they opened.
    #[must_use]
    pub fn windows(&self) -> &[TaskId] {
        &self.windows
    }

    /// The menu a secondary press on the slot opens. Empty when the
    /// application declared none, which the bar honours by opening nothing.
    #[must_use]
    pub fn menu(&self) -> &AppMenu {
        &self.menu
    }

    /// Whether a primary click on the slot is the application's to handle.
    ///
    /// `false` leaves the click to the session's own default — raise the
    /// application's most recently used window, and do nothing at all when
    /// it has none.
    #[must_use]
    pub fn handles_default(&self) -> bool {
        self.handles_default
    }

    /// The identity the application's information panel states.
    #[must_use]
    pub fn identity(&self) -> &AppIdentity {
        &self.identity
    }
}

/// The application strip: the resolved slots in display order plus hover
/// state.
#[derive(Clone, Debug, Default)]
pub struct AppStrip {
    apps: Vec<AppSlot>,
    hover: Option<usize>,
}

impl AppStrip {
    /// An empty strip.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the strip's slots with the session's freshly resolved ones.
    ///
    /// A hover that no longer names a slot is dropped, so the strip can
    /// never highlight a slot that is gone.
    pub fn set_apps(&mut self, apps: Vec<AppSlot>) {
        self.hover = self.hover.filter(|&index| index < apps.len());
        self.apps = apps;
    }

    /// The resolved slots, in display order.
    #[must_use]
    pub fn apps(&self) -> &[AppSlot] {
        &self.apps
    }

    /// The slot at `index`, if any.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&AppSlot> {
        self.apps.get(index)
    }

    /// The number of running applications on the strip.
    #[must_use]
    pub fn len(&self) -> usize {
        self.apps.len()
    }

    /// Whether no application is on the strip.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// The hovered slot index, if the pointer rests on one.
    #[must_use]
    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Track the hovered slot, reporting whether the visual state changed.
    pub(crate) fn set_hover(&mut self, hover: Option<usize>) -> bool {
        let hover = hover.filter(|&index| index < self.apps.len());
        if self.hover == hover {
            return false;
        }
        self.hover = hover;
        true
    }

    /// The shared control for the slot at `index`, ready to paint: an
    /// icon-only [`TaskbarItem`] carrying the application's identity and the
    /// strip's hover state.
    #[must_use]
    pub(crate) fn item(&self, index: usize) -> Option<TaskbarItem> {
        self.apps.get(index)?;
        let pointer = if self.hover == Some(index) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        Some(
            TaskbarItem::new(self.apps[index].icon)
                .with_state(ControlState::idle().with_pointer(pointer)),
        )
    }
}
