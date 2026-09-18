//! The desktop **Settings** application's host-tested model
//! (`plans/NEW-DESKTOP-SETTINGS.md`).
//!
//! This is the composition the `settings.app` bundle's `Run` binary drives.
//! It configures nothing itself: it holds no capability beyond the console
//! and its own window frame, and every change a pane will make is a request
//! to the process that already owns that domain. The whole surface is
//! generated from one closed registry table ([`CATEGORIES`]), so a
//! category cannot exist without a row and a row cannot exist without a pane.
//!
//! Every pixel is a shared [`tairix_controls`] control; this crate adds no
//! control implementation and no second theming or rasterisation path. What it
//! adds is the navigation — the strip, the search, the location trail, the
//! frame that sheds what a narrow window cannot seat — and the one renderer
//! for a pane that states how the machine actually stands rather than drawing
//! a control that would change nothing.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod appearance;
mod frame;
mod gallery;
mod registry;
mod shell;
mod statement;

pub use appearance::{Composition, Form, FormOutcome, FormPlace, Setting, POINTER_SIZE_LABEL};
pub use frame::{resolve_frame, Overflow, ShellFrame, CONTENT_FLOOR, SIDEBAR_WIDTH};
pub use gallery::{Gallery, GalleryOutcome, PictureWanted, NONE_LABEL};
pub use registry::{
    strip_rows, Category, CategoryRow, Location, Pane, PaneBacking, PaneRow, StripRow, CATEGORIES,
};
pub use shell::{Shell, ShellOutcome};

#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod shell_tests;
