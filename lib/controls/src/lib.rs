//! Shared **Reactive Alloy** GUI control behaviour for the TAIRiX desktop
//! (`lib/controls` — `plans/GUI-CONTROLS-DESIGN.md`).
//!
//! Reactive Alloy is TAIRiX's GUI control design language. Its controls are
//! typed Rust state resolved against the shared theme and drawn through the
//! shared raster/compositor path; nothing about a control's *behaviour* is
//! duplicated per application. This crate is the shared home for that
//! behaviour, living in `lib/*` because its consumers — the compositing
//! window manager (`userland/gui/wm`), the taskbar (`userland/gui/taskbar`),
//! and the default graphical apps — may not depend on one another
//! (the layering rule), exactly as `lib/geometry` owns the shared coordinate
//! types and `lib/theme` owns the shared design tokens.
//!
//! # Scope
//!
//! The first module is the **scroll geometry engine** ([`scroll`]): the one
//! orientation-independent definition of how a viewport's content extent,
//! viewport extent, and offset map to a draggable thumb, and how pointer,
//! wheel, and keyboard input map back to a clamped offset. The design
//! language mandates a single scrollbar behaviour shared by the
//! window-manager root viewport and by nested application content, over one
//! range validation, thumb math, and input model rather than separate
//! vertical, horizontal, window-manager, and application recipes. This
//! module is that single definition.
//!
//! The engine is pure integer arithmetic with no rendering: it computes a
//! one-dimensional thumb *span* along an abstract track and the offset a
//! pointer position implies. The owning viewport maps that span onto a
//! `tairix_geometry::Rect` for its chosen [`ScrollOrientation`] at the
//! edge, so the same math serves both axes.
//!
//! Every input is validated and every result is clamped: an empty,
//! overflowing, or non-scrollable range yields a zero-offset, non-draggable
//! scrollbar rather than out-of-bounds geometry.
//!
//! The [`state`] module is the typed control-state vocabulary: [`ControlKind`]
//! and [`ControlRole`], the composed [`ControlState`] (focus/pointer/selection/
//! validation/authority/activity/pressure/recovery), the derived
//! [`ControlDisposition`] taxonomy that keeps an authority denial distinct from
//! a plain disabled control, and the window-furniture states. Controls are
//! *composed* from these small typed fields, never one giant enum.
//!
//! The [`button`] module is the first drawn control family — [`Button`],
//! [`IconButton`], and [`SplitButton`]. They resolve every visible property
//! from the active `tairix_theme::Theme` and `tairix_geometry::Scale`, round
//! their plates through the shared `tairix_raster` rounded-rect fill (never a
//! second rounding path), draw their labels/icons through `tairix_font`/
//! `tairix_icon`, and consume the shared `tairix_input` pointer/keyboard
//! vocabulary. A control renders state and emits a typed action; it performs
//! no privileged work — the owning service enforces authority.
//!
//! The [`selector`] module is the boolean-selector family — [`Toggle`],
//! [`Checkbox`], and [`Radio`]. Each is a labelled boolean control that reads
//! by *shape* as well as colour (a toggle's thumb slides to the active side, a
//! checkbox draws a filled square when on and a horizontal bar when mixed, a
//! radio draws a centre bead when selected), so its state is legible without
//! relying on hue. They share the button family's shared plate helpers and
//! interaction model, resolve every visible property from the active
//! theme, and — like every control — emit a typed [`SelectorAction`] rather
//! than performing the change themselves; a denied selector keeps its value
//! and shows an Authority Mark.
//!
//! The [`value`] module is the value-control family — [`Slider`] and
//! [`Progress`]. Both are measured controls whose value is a validated permille
//! fraction. A [`Slider`] is interactive (a rail, a value track that fills to a
//! draggable thumb, drag and keyboard stepping, an optional bounded-cap marker)
//! and emits a typed [`SliderAction`] that the owner commits; a [`Progress`] is
//! a read-only instrument trace of known, working, indeterminate, complete, or
//! failed work, driven only by the state its owner sets — it runs no idle loop
//! and renders an indeterminate trace statically under reduced motion.
//!
//! The [`text`] module is the text-entry family — [`TextField`] and
//! [`SearchField`]. Both are single-line entries on a quiet Alloy Plate with a
//! caret, selection, and horizontally-scrolled clipped text; a [`SearchField`]
//! adds a leading magnifier that reads as active when a query is present. A
//! read-only field stays legible and selectable but refuses edits, distinct
//! from a disabled field (muted) and a denied field (Authority Mark); both emit
//! a typed [`TextAction`] the owner validates and commits.
//!
//! The [`menu`] module is the menu command surface — [`Menu`] and
//! [`MenuItem`]. A menu is an elevated command plate carrying a column of row
//! controls (label, optional icon, shortcut, submenu chevron, and a disabled
//! row's reason); it owns keyboard navigation, pointer hover/click, the §13
//! authority rendering (a denied row keeps its slot and shows an Authority
//! Mark), and a destructive row's danger rail, emitting a typed [`MenuAction`].
//!
//! The [`toolbar`] module is the toolbar / toolstrip — [`Toolbar`]. It is a
//! horizontal container of [`IconButton`] / [`SplitButton`] tools grouped with
//! quiet gutters, marks the active tool with a persistent lower accent seam,
//! and routes pointer and keyboard input to the tools it owns, emitting a typed
//! [`ToolbarAction`].
//!
//! The [`tabs`] module is the tab strip — [`Tabs`] and [`Tab`]. Tabs select one
//! of several views: the selected tab carries a strong lower seam, a loading
//! tab a Heat Seam, and a modified or error tab a shape-coded Signal Bead, all
//! legible without colour; it emits a typed [`TabsAction`].
//!
//! The [`combo`] module is the choice-entry control — [`ComboBox`]. It composes
//! the text-field focus model and the [`Menu`] model rather than re-deriving
//! either: the popup *is* a [`Menu`] built from the choices, and selecting one
//! emits a typed [`ComboAction`].

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod button;
pub mod combo;
pub mod menu;
mod paint;
pub mod scroll;
pub mod selector;
pub mod state;
pub mod tabs;
pub mod text;
pub mod toolbar;
pub mod value;

pub use button::{Button, ButtonAction, ButtonContent, IconButton, SplitAction, SplitButton};
pub use combo::{ComboAction, ComboBox};
pub use menu::{Menu, MenuAction, MenuItem};
pub use scroll::{
    ScrollGeometry, ScrollModel, ScrollOrientation, ScrollRange, ThumbSpan, TrackHit,
};
pub use selector::{Checkbox, Radio, SelectorAction, Toggle};
pub use state::{
    ActivityState, AuthorityState, ControlDisposition, ControlKind, ControlRole, ControlState,
    FocusState, PointerState, PressureKind, PressureState, ProgressValue, RecoveryState,
    SelectionState, SizeAction, ValidationState, WindowActivationState, WindowControlKind,
    WindowFurnitureState, WindowSizeState,
};
pub use tabs::{Tab, Tabs, TabsAction};
pub use text::{SearchField, TextAction, TextField};
pub use toolbar::{ToolActivation, Toolbar, ToolbarAction};
pub use value::{Progress, Slider, SliderAction};

#[cfg(test)]
mod button_tests;
#[cfg(test)]
mod combo_tests;
#[cfg(test)]
mod menu_tests;
#[cfg(test)]
mod selector_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tabs_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod text_tests;
#[cfg(test)]
mod toolbar_tests;
#[cfg(test)]
mod value_tests;
