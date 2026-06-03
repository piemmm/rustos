//! Integer screen geometry: points and rectangles.
//!
//! The [`Point`] and [`Rect`] types are defined once in the shared
//! [`rustos_geometry`] crate (`lib/geometry`) so the taskbar and the default
//! apps — which may not depend on the window manager (`AGENTS.md` §17.4) —
//! reuse exactly the same coordinate vocabulary without duplication (§2.2,
//! §6). This module re-exports them, and the desktop [`Scale`], so the rest
//! of the compositor keeps referring to `crate::geometry::{Point, Rect,
//! Scale}`.

pub use rustos_geometry::{Point, Rect, Scale};
