//! RustOS compositing window manager (`userland/gui/wm`).
//!
//! This crate is the user-space compositor for the RustOS desktop. It composes per-window [`Surface`]s into a single
//! scan-out frame and presents it through a capability-gated
//! [`Display`](rustos_abi::driver::display::Display) driver; the kernel
//! never composites (the desktop is an
//! optional, one-way-dependent userland frontend).
//!
//! # What this increment delivers
//!
//! The Stage 7 *compositor core*:
//!
//! - **Premultiplied-alpha pixels** ([`color`]) with the Porter–Duff
//!   *over* operator, so per-surface and per-region transparency blend
//!   correctly.
//! - **Surfaces** ([`surface`]): dense premultiplied pixel buffers, the
//!   rendered content of a window.
//! - **Anti-aliased rounded corners** ([`corner`]) via deterministic
//!   supersampling, with a square-corner opt-out — the single
//!   rounded-corner path the taskbar reuses.
//! - **Damage tracking** ([`damage`]): only changed pixels are
//!   recomposited.
//! - **The [`Compositor`]**: a z-ordered [`Window`] stack composited
//!   over an opaque background into a [`DisplayMode`]-shaped byte frame.
//! - **Input routing** ([`input`]): the [`InputRouter`] tracks the
//!   pointer and the focused window, raises and focuses the window
//!   under a primary press (*click-to-activate*), and drives
//!   interactive window move-grabs.
//! - **Pointer cursor overlay** ([`cursor`]): a scalable, colourful,
//!   replaceable [`CursorImage`](rustos_cursor::CursorImage) from
//!   `lib/cursor`, composited as the top-most layer so its hotspot
//!   tracks the pointer.
//! - **Cursor selection** ([`select`]): the [`CursorController`]
//!   chooses the [`CursorKind`](rustos_theme::CursorKind) from live
//!   interaction state (move-grab, the window under the pointer, the
//!   desktop) and installs the matching artwork.
//!
//! GPU acceleration, theming, and the taskbar build on this core in
//! later Stage 7 increments.
//!
//! [`InputRouter`]: input::InputRouter
//!
//! [`Surface`]: surface::Surface
//! [`Window`]: window::Window
//! [`DisplayMode`]: rustos_abi::driver::display::DisplayMode

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod color;
pub mod compositor;
pub mod corner;
pub mod cursor;
pub mod damage;
pub mod geometry;
pub mod input;
pub mod select;
pub mod surface;
pub mod window;

#[cfg(test)]
mod tests;

pub use color::{Color, Pixel};
pub use compositor::Compositor;
pub use corner::Corners;
pub use cursor::CursorLayer;
pub use damage::DamageRegion;
pub use geometry::{Point, Rect, Scale};
pub use input::{InputEvent, InputResponse, InputRouter, Key, Modifiers, NamedKey, PointerButton};
pub use select::{desired_cursor, CursorController};
pub use surface::Surface;
pub use window::{Window, WindowId};
