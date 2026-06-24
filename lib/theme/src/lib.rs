//! RustOS shared desktop theme definition (`lib/theme` — `PLAN.md` Stage 7).
//!
//! The charter requires "one shared theme definition" that drives the
//! colours, corner radii, fonts, and cursors of the window manager, the
//! taskbar, and the default apps, with a default dark theme and a light
//! theme switchable at runtime, and where "adding a theme is data, not new
//! code". This crate is that definition.
//!
//! It is pure *data*: a [`Theme`] is a table of [`Rgba`] colour roles
//! ([`Palette`]), geometric [`Metrics`] (the corner radii the compositor's
//! single rounded-corner path consumes), [`Fonts`], and a
//! [`CursorSet`]. None of the rendering or compositing arithmetic lives
//! here — that is the shared rasteriser's job (`lib/raster`) — so nothing
//! is duplicated. A consumer converts a theme [`Rgba`]
//! into the shared render colour at the edge (`From<Rgba> for
//! rustos_raster::Color`).
//!
//! # Where it sits
//!
//! As a `lib/*` crate it has no dependencies and is depended on by the GUI
//! crates and the default apps, never the reverse — the bottom of the
//! layering. Living in `lib/*` (not `userland/gui/*`) is deliberate:
//! sibling userland crates may not depend on one another, so the one shared definition they all read belongs here, exactly
//! as `lib/procinfo` is the shared home for the System Information client
//! helpers.
//!
//! # Switching themes
//!
//! [`ThemeRegistry`] owns the available themes and the active one. It
//! always holds the two built-ins, switches with
//! [`set_active`](ThemeRegistry::set_active), and accepts custom themes with
//! [`register`](ThemeRegistry::register). Both mutators fail closed.
//!
//! ```
//! use rustos_theme::{Appearance, ThemeId, ThemeRegistry};
//!
//! let mut themes = ThemeRegistry::with_builtins();
//! assert_eq!(themes.active().appearance(), Appearance::Dark);
//!
//! themes.set_active(ThemeId::LIGHT).expect("light is built in");
//! assert_eq!(themes.active().appearance(), Appearance::Light);
//! ```

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod color;
pub mod cursor;
pub mod metrics;
pub mod palette;
pub mod registry;
pub mod theme;
pub mod typography;

#[cfg(test)]
mod tests;

pub use color::Rgba;
pub use cursor::{CursorKind, CursorSet};
pub use metrics::Metrics;
pub use palette::Palette;
pub use registry::{ThemeError, ThemeRegistry};
pub use theme::{Appearance, Theme, ThemeId};
pub use typography::{FontSpec, FontWeight, Fonts};
