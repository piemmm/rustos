//! RustOS compositing window manager (`userland/gui/wm`).
//!
//! This crate is the user-space compositor for the RustOS desktop
//! (`AGENTS.md` §10). It composes per-window [`Surface`]s into a single
//! scan-out frame and presents it through a capability-gated
//! [`Display`](rustos_abi::driver::display::Display) driver; the kernel
//! never composites (`AGENTS.md` §10, §17.3 — the desktop is an
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
//!   rounded-corner path the taskbar reuses (`AGENTS.md` §2.2).
//! - **Damage tracking** ([`damage`]): only changed pixels are
//!   recomposited.
//! - **The [`Compositor`]**: a z-ordered [`Window`] stack composited
//!   over an opaque background into a [`DisplayMode`]-shaped byte frame.
//!
//! GPU acceleration, input routing, theming, and the taskbar build on
//! this core in later Stage 7 increments.
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
pub mod damage;
pub mod geometry;
pub mod surface;
pub mod window;

#[cfg(test)]
mod tests;

pub use color::{Color, Pixel};
pub use compositor::Compositor;
pub use corner::Corners;
pub use damage::DamageRegion;
pub use geometry::{Point, Rect};
pub use surface::Surface;
pub use window::{Window, WindowId};
