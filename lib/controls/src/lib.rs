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

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod scroll;

pub use scroll::{
    ScrollGeometry, ScrollModel, ScrollOrientation, ScrollRange, ThumbSpan, TrackHit,
};

#[cfg(test)]
mod tests;
