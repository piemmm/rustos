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

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod button;
mod paint;
pub mod scroll;
pub mod selector;
pub mod state;
pub mod value;

pub use button::{Button, ButtonAction, ButtonContent, IconButton, SplitAction, SplitButton};
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
pub use value::{Progress, Slider, SliderAction};

#[cfg(test)]
mod button_tests;
#[cfg(test)]
mod selector_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod value_tests;
