//! RustOS `fstree` — the full-screen tree file manager
//! (`.junie/fstree-next-plan.md`).
//!
//! A persistent directory-tree pane plus a file pane over the storage
//! forest, drawn with the OS curses library. This crate delivers the S1
//! model core (the lazily populated tree, pane navigation, sorting, the
//! hidden-entries toggle, the status/message lines, the `?` help overlay)
//! and the S2 file operations: copy, move, rename, delete, mkdir, and the
//! permission-bits editor, each planned and validated before any I/O and
//! driven by a resumable executor whose per-file overwrite questions run
//! through the key loop. Tagging, search, and the viewers are staged in
//! `.junie/fstree-next-plan.md` and land stage by stage.
//!
//! # What this crate is
//!
//! A fully host-testable session built from seams:
//!
//! * [`fs::Fs`] — the directory-listing and free-space channel; the `Run`
//!   binary implements it over the kernel-authorised `fs_*` syscalls, the
//!   tests over an in-memory tree.
//! * `Tty` (from `rustos-curses`) — the terminal byte channel; an
//!   in-memory channel makes the whole session drivable without a kernel.
//! * [`model::Model`] — the I/O-free state machine that [`render::render`]
//!   draws and [`app::run`] drives.
//!
//! # Module map
//!
//! * [`fs`] — the [`fs::Fs`] seam and its listing vocabulary.
//! * [`model`] — the tree/pane/sort/prompt state machine.
//! * [`ops`] — the file-operation planner and resumable executor.
//! * [`mod@render`] — the curses frame (panes, status, message, overlays).
//! * [`app`] — the key grammar and the blocking session loop.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — the audited
//! `rustos-abi` and the OS-provided `rustos-curses`/`rustos-termcap`/
//! `rustos-vt`/`rustos-help` — never a kernel or driver crate. No `unsafe`,
//! and no `unwrap`/`expect`/`panic!` in production paths; a refused listing
//! fails closed onto the message line. Nothing writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod fs;
pub mod model;
pub mod ops;
pub mod render;

#[cfg(test)]
mod tests;

pub use app::{handle_event, run, FstreeError};
pub use fs::{Fs, FsEntry, RenameOutcome, VolumeSpace};
pub use model::{Model, Pane, SortKey};
pub use render::render;
