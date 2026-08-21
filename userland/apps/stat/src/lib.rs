//! TAIRiX `stat` — report a file's or a filesystem's status
//! (`plans/APPS.md` §12.1 Stage F).
//!
//! The GNU coreutils `stat`: it renders the fields of one `fs_stat` per
//! operand, either as the default full report or through a `--format` /
//! `--printf` string of GNU's own specifiers. `-f` switches to the
//! filesystem vocabulary — the volume's block and inode counts, taken from
//! the System Information API mount snapshot — and `-t` is the one-line
//! terse form of either.
//!
//! **Without `-L` a symbolic link is described as itself.** That is the
//! whole reason the tool exists beside `ls`: `%N` shows the link and the
//! target it stores, `%F` says `symbolic link`, and the sizes and stamps are
//! the link's own. `-L` resolves the final link and reports what it names.
//!
//! # Where TAIRiX genuinely differs
//!
//! Every specifier is GNU's, and each divergence is confined to the concept
//! that actually differs rather than reshaping a field that could have
//! matched:
//!
//! * `%d`/`%D` — a volume is identified by a 16-byte id, not a device
//!   number, so the pair renders that id (decimal and hex). Comparing two
//!   files' `%d` still answers exactly "are these on one volume?".
//! * `%G` is **refused**: the System Information API publishes a user
//!   directory and no group counterpart, so there is no group name to
//!   print; `%g` (the numeric id) is the honest field. `%U` *is* served,
//!   through the same ungated query `whoami` reads.
//! * `%t`/`%T` in the file vocabulary are **refused**: TAIRiX has no device
//!   special files, so a major/minor device type would be a fabricated
//!   value. The `-t` terse form therefore omits those two columns rather
//!   than printing zeroes.
//! * `%t` in the filesystem vocabulary is **refused**: a TAIRiX volume has
//!   no numeric filesystem-type magic. `%T` names the type the mount
//!   records.
//!
//! A refusal is a usage error naming the specifier and the reason, decided
//! when the format is parsed — before any path is touched — so a format the
//! platform cannot serve never half-renders.
//!
//! # What this crate is
//!
//! A **reporter**: for one parsed [`Command`] it gathers each operand's
//! facts once and renders the format over them. Everything that touches the
//! outside world is an injected seam:
//!
//! * [`Filesystem`] — the node's report, a link's stored target, and the
//!   canonical path `%m` needs.
//! * [`Mounts`] — the mount snapshot behind `%m`, `%o`, and every `-f`
//!   field.
//! * [`Names`] — the uid → account-name lookup behind `%U`.
//! * [`Output`] — the report (fd 1) and the diagnostics (fd 2).
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! # Fail closed
//!
//! An unrecognised option, a missing operand, or an unserviceable specifier
//! is a usage error and nothing is printed. A refused operand is diagnosed
//! and the run continues to the rest, exiting non-zero — the GNU behaviour.
//! A fact the platform cannot supply renders as `?`, never as a plausible
//! substitute. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod client;
mod command;
mod error;
mod io;

pub use client::{run, Reporter, USAGE};
pub use command::{parse, Command, Options, Pad, Piece, Subject, Trailer};
pub use error::StatError;
pub use io::{Filesystem, Mount, Mounts, Names, Output};
