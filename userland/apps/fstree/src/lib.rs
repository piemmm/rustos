//! RustOS `fstree` — the full-screen tree file manager
//! (`.junie/fstree-next-plan.md`).
//!
//! A persistent directory-tree pane plus a file pane over the storage
//! forest, drawn with the OS curses library. This crate delivers the S1
//! model core (the lazily populated tree, pane navigation, sorting, the
//! hidden-entries toggle, the status/message lines, the `?` help overlay),
//! the S2 file operations (copy, move, rename, delete, mkdir, and the
//! permission-bits editor, each planned and validated before any I/O and
//! driven by a resumable executor whose per-file overwrite questions run
//! through the key loop), and the S3 tagging surface: multi-file tags
//! (`t`, tag-by-glob, invert, clear), batch copy/move/delete over the
//! tagged set with a per-file continue-on-error report, the flattened
//! branch view, and disk-usage statistics — both fed by one bounded,
//! cancellable walker — and the S4 search surface: a live filename filter
//! per pane (`f`), a branch-wide filename search (`/`), and a streaming
//! file-content search (`F`) over the tagged set or the focused branch,
//! both searches feeding the same taggable, operable flattened list.
//! The S5 viewers open with Enter on a regular file: a streaming text
//! pager and an offset/hex/ASCII dump, auto-picked from a head sample,
//! each with paging, goto, and background literal search over the same
//! seams. The S9 disassembly viewer opens a recognised executable
//! container (or a standalone signed manifest) into a summary page and
//! per-region paged disassembly, every decode running behind the parser
//! sandbox through the [`view_disasm::Decode`] seam; `o` force-picks a
//! viewer, and an unrecognised file falls back to hex with a notice.
//! The remaining stages are staged in `.junie/fstree-next-plan.md`.
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
//! * [`tag`] — the tag set and the batch-operation driver.
//! * [`walk`] — the bounded branch walker (flattened view, disk usage,
//!   and the searches through its [`walk::Sieve`]).
//! * [`search`] — the streaming file-content scanner behind `F`.
//! * [`view_text`] / [`view_hex`] — the Enter viewers: the streaming
//!   text pager and the hex dump, paging and searching through the same
//!   [`fs::Fs`] seam in bounded windows.
//! * [`view_disasm`] — the disassembly viewer: container summaries and
//!   per-region instruction windows, decoded only by the sandboxed
//!   decode service through the [`view_disasm::Decode`] seam.
//! * [`mod@render`] — the curses frame (panes, status, message, overlays).
//! * [`app`] — the key grammar and the session loop (walk ticks run on a
//!   timed input bound; every wait still parks in the kernel).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — the audited
//! `rustos-abi`, the OS-provided `rustos-curses`/`rustos-termcap`/
//! `rustos-vt`/`rustos-help`, and the sandbox seam `rustos-sandbox` (with
//! `rustos-binfmt` solely for magic-prefix routing) — never a kernel or
//! driver crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths; a refused listing fails closed onto the message
//! line. Nothing writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod fs;
pub mod info;
pub mod model;
pub mod ops;
pub mod render;
pub mod search;
pub mod settings;
pub mod tag;
pub mod view_disasm;
pub mod view_hex;
pub mod view_text;
pub mod walk;

#[cfg(test)]
mod tests;

pub use app::{handle_event, run, FstreeError};
pub use fs::{Fs, FsEntry, RenameOutcome, VolumeInfo, VolumeSpace};
pub use info::{Info, NullInfo};
pub use model::{Model, Pane, SortKey};
pub use render::render;
pub use search::{ContentScan, Needle};
pub use settings::Settings;
pub use tag::{Batch, BatchProgress, TagEntry, TagRange, TagSet};
pub use view_disasm::{Decode, DisasmView};
pub use view_hex::{HexPattern, HexView};
pub use view_text::{JobOutcome, TextView};
pub use walk::{FlatEntry, Sieve, WalkPurpose, WalkState, Walker};
