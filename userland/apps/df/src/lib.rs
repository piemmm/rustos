//! RustOS `df` — report filesystem space usage (Stage 6
//! `userland/apps/`, a `plans/APPS.md` command app).
//!
//! `df` reports, one row per mounted filesystem, the volume's size, the
//! space used, the space available, and the mount point. With `file`
//! operands it reports the filesystem containing each operand. The
//! option surface follows GNU coreutils (`AGENTS.md` §16.7): `-a` shows
//! the pseudo/duplicate mounts the default hides, `-T`/`-t`/`-x` add and
//! filter by filesystem type, `-i` reports inodes, `-P` selects the
//! POSIX portable wording, `--total` appends a summary row, and
//! `-k`/`-h`/`-H`/`--si`/`-B <size>` select the scale. `-?`/`--help`
//! render the tool's own short help from its bundled `Help/` tree
//! through the shared `lib/help` engine (plans/APPS.md §4).
//!
//! # Where the numbers come from
//!
//! Live system state is read exclusively through the System Information
//! API (`AGENTS.md` §16.6): the typed, versioned `sysinfo-v1`
//! `MOUNT_LIST` query served by `/System/Services/sysinfod.app/Run`,
//! whose rows now carry each backing volume's space accounting
//! (`VolumeStats`) as the mounted filesystem driver reports it. There is
//! no `/proc`, no mount-table file, and no second query client: the
//! paging walk is the shared `rustos_procinfo::for_each_mount` the
//! `mount` tool uses.
//!
//! # What this crate is
//!
//! A **selection and rendering engine**, not a data source. For one
//! parsed [`Command`] it fetches the mount table, chooses the rows (all
//! mounts, or each operand's covering mount), applies the type filters,
//! and renders the GNU-shaped table. The operations that touch the
//! outside world are the injected seams:
//!
//! * [`rustos_procinfo::Transport`] — the `MOUNT_LIST` query.
//! * [`PathProbe`] — confirm a `file` operand exists.
//! * [`Output`] — the table on standard output, diagnostics on standard
//!   error, and the omission advisory on fd 3.
//! * [`rustos_help::HelpSource`] — the tool's own `Help/` tree, read by
//!   the short-help switches.
//!
//! # Advisory output
//!
//! When the default view hides capacity-less bindings or further mounts
//! of an already-listed volume, `df` notes the omission on the standard
//! information stream (fd 3) with the `fs.mounts_omitted` record
//! (`AGENTS.md` §20.1) — advisory only, never affecting the table, the
//! ordering, or the exit status.
//!
//! # Fail loud, degrade gracefully
//!
//! A `file` operand that does not exist, is relative (mount points are
//! absolute; `df` never guesses a resolution), or is uncovered is
//! diagnosed on standard error and the report continues (exit `1`). A
//! failed mount-table query and a failed output write are fatal; type
//! filters that leave nothing report the GNU `no file systems
//! processed` error. There is no panic path.
//!
//! # Module map
//!
//! * [`error`] — [`DfError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Options`] shapes and their
//!   [`parse`]r.
//! * [`io`] — the [`PathProbe`] and [`Output`] seams.
//! * [`client`] — the [`run`] entry point and the report engine.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate:
//! they are authored once in the bundle's on-disk `Help/` tree, planted
//! onto `/System` by the image builder from that source
//! (`tools/syshelp`), and read back at runtime through the injected
//! [`rustos_help::HelpSource`] seam. Help is never hardcoded into the
//! program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, the shared `lib/procinfo`
//! sysinfo client, and the shared `lib/util` size vocabulary, so this
//! userland tool never links a kernel or driver crate. No `unsafe`, and
//! no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Options, ParseError};
pub use error::DfError;
pub use io::{Output, PathProbe};
