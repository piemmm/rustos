//! RustOS **filesystem browser** — the default graphical file manager
//! (`userland/apps/`, `PLAN.md` Stage 7).
//!
//! The browser navigates the filesystem layout — the four top-level
//! directories `System`, `Users`, `Apps`, `Storage` and everything beneath
//! them — and renders the current directory through the shared desktop theme.
//! It is a graphical app, so it consumes the same `lib/*` building blocks the
//! taskbar does (`lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`) and
//! never depends on the window manager.
//!
//! # What this crate is
//!
//! A navigation **model** plus a **renderer**, both driven by an injected
//! [`DirectorySource`]:
//!
//! * [`Browser`] holds the current directory's path and entries and a
//!   selection cursor. Descending, climbing to the parent, and refreshing are
//!   transactional and fail closed: the new directory is listed *before* any
//!   state changes, so a refused or failing read leaves the browser exactly
//!   where it was.
//! * [`render()`] paints the path bar and the (scrolling) entry list into a
//!   `lib/raster` [`Surface`](rustos_raster::Surface) using the active theme's
//!   palette and the shared `lib/font` face — the same surface the compositor
//!   places and rounds.
//!
//! # No `/proc`, no fabrication
//!
//! RustOS has no `/proc` and no `/sys`. The browser shows
//! exactly the entries its [`DirectorySource`] returns — it never injects a
//! synthetic entry — and it makes no permission decision of its own: the
//! check and the path policy live in the VFS behind the source. A
//! directory the caller may not read surfaces a [`BrowseError`] rather than a
//! partial or guessed listing.
//!
//! The binary that ships as the file manager wires the real VFS-backed
//! [`DirectorySource`]; tests wire an in-memory tree, so the navigation and
//! rendering logic is exhaustively testable without a kernel.
//!
//! # Module map
//!
//! * [`entry`] — the [`Entry`]/[`EntryKind`] listing vocabulary.
//! * [`error`] — [`BrowseError`], the fail-closed navigation outcomes.
//! * [`source`] — the [`DirectorySource`] seam.
//! * [`browser`] — the [`Browser`] navigation model.
//! * [`render`](mod@render) — painting the current directory into a `Surface`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the
//! audited `lib/abi` ABI crate and the shared `lib/*` desktop libraries, so
//! this app never links a kernel, driver, or window-manager crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod browser;
pub mod entry;
pub mod error;
pub mod render;
pub mod source;
pub mod vfs;

pub use browser::Browser;
pub use entry::{Entry, EntryKind};
pub use error::BrowseError;
pub use render::render;
pub use source::DirectorySource;
pub use vfs::VfsDirectorySource;

#[cfg(test)]
mod tests;
