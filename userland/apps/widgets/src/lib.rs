//! The Reactive Alloy **widget gallery** model (`plans/GUI-CONTROLS-DESIGN.md`).
//!
//! This is the host-tested composition the `widgets.app` bundle's `Run`
//! binary drives. It owns no rendering of its own beyond layout: every widget
//! it shows is a shared [`tairix_controls`] control, drawn and driven by that
//! crate, so the gallery adds no second control implementation. The gallery is
//! merely their *owner* — it lays each control out within the window client
//! rectangle, routes a pointer or key event to the control under focus, and
//! reflects that control's typed action straight back into it (a toggle flips,
//! a slider moves, a field edits), so the demo is genuinely interactive
//! without touching any privileged service.
//!
//! The composition presents only client content (the compositor draws the
//! window frame, title bar, and command buttons server-side): a tab strip
//! selecting one control family, and a panel of captioned demo widgets for the
//! selected family. Each family lives on its own tab and shows several
//! variations — different roles, states, and values — so the full behaviour of
//! each control is legible in one place.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod gallery;
mod panels;
mod widget;

pub use gallery::{DemoItem, Gallery, GalleryTab};
pub use widget::DemoWidget;

#[cfg(test)]
mod gallery_tests;
