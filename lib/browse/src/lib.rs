//! TAIRiX **shared directory-browser engine** (`plans/APPWIN.md` AW5).
//!
//! The one navigation **model** plus themed **renderer** every directory
//! browser in the system composes: the `files.app` bundle's windowed file
//! manager and the desktop session's **trusted file picker**
//! (`plans/CAPABILITY_USE.md` CU6) both drive exactly this engine, so the
//! two views can never diverge in navigation semantics, listing policy, or
//! look. Both are driven through an injected [`DirectorySource`]:
//!
//! * [`Browser`] holds the current directory's path and entries and a
//!   selection cursor. Descending, climbing to the parent, and refreshing
//!   are transactional and fail closed: the new directory is listed
//!   *before* any state changes, so a refused or failing read leaves the
//!   browser exactly where it was.
//! * [`render()`] paints the path bar and the (scrolling) entry list into
//!   a `lib/raster` [`Surface`](tairix_raster::Surface) using the active
//!   theme's palette and the shared `lib/font` face — the same surface the
//!   compositor places and rounds.
//!
//! # No `/proc`, no fabrication
//!
//! TAIRiX has no `/proc` and no `/sys`. The browser shows exactly the
//! entries its [`DirectorySource`] returns — it never injects a synthetic
//! entry — and it makes no permission decision of its own: the check and
//! the path policy live in the VFS behind the source, under the identity
//! of whichever process composes the engine (the files app's own, the
//! session's for the picker). A directory the caller may not read surfaces
//! a [`BrowseError`] rather than a partial or guessed listing.
//!
//! # Module map
//!
//! * [`entry`] — the [`Entry`]/[`EntryKind`] listing vocabulary.
//! * [`error`] — [`BrowseError`], the fail-closed navigation outcomes.
//! * [`source`] — the [`DirectorySource`] seam.
//! * [`browser`] — the [`Browser`] navigation model.
//! * [`render`](mod@render) — painting the current directory into a
//!   `Surface`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited
//! `lib/abi` ABI crate and the shared `lib/*` desktop libraries, so this
//! engine never links a kernel, driver, or window-manager crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

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

/// Window content width of a browser view, in pixels — the one definition
/// the files app's `Run` binary and the session's trusted picker size
/// their windows with, and the QEMU vertical's host-side scan-out
/// assertion measures against (`plans/APPWIN.md` AW3/AW5).
pub const WIN_WIDTH: u32 = 480;

/// Window content height of a browser view, in pixels (see
/// [`WIN_WIDTH`]).
pub const WIN_HEIGHT: u32 = 320;

#[cfg(test)]
mod tests;
