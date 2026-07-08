//! RustOS `seq` — print a sequence of numbers (`plans/APPS.md` §12.1
//! Stage C).
//!
//! The GNU coreutils `seq`: print the numbers from FIRST to LAST in steps
//! of INCREMENT (both defaulting to 1), with the GNU option surface —
//! `-f`/`--format` (a printf-style floating-point format), `-s`/
//! `--separator`, and `-w`/`--equal-width` — and the GNU output rules:
//! the default precision is inferred from the operands' spellings, plain
//! integer runs are generated in exact decimal string arithmetic
//! (arbitrarily large, `inf` as LAST permitted), and the floating-point
//! path prints the value one step past LAST when it renders equal to it.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names, with GNU `seq`'s
//!   option rules: no permutation (the first operand ends option
//!   scanning), a leading negative number is an operand, and the operand
//!   count, `-f` format, and `-f`/`-w` conflict are validated in the GNU
//!   diagnostic order.
//! * [`run`] — the engine: operand scanning ([`scan_arg`] mirrors GNU's
//!   width/precision inference), the exact decimal fast path for integer
//!   runs, the floating-point path with the default-format rules, and
//!   the [`Format`] printf-equivalent renderer (`%e`/`%f`/`%g`/`%a`,
//!   C-locale semantics), all writing through the injected [`Output`].
//!
//! One deliberate platform divergence: GNU `seq`'s floating-point path
//! computes in C `long double`; RustOS computes in `f64`. The exact
//! decimal path (which GNU also prefers) is unaffected, and `f64` *is*
//! the `double` the GNU format contract names — only non-integer
//! sequences whose operands need more than 53 bits of significand can
//! print differently from a glibc build on x86. The visible cosmetic
//! consequence is `%a`'s spelling: this tool prints the C `double` form
//! (`0x1.8p+0`) where glibc's `%La` normalisation spells the same value
//! `0xcp-3`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); its only dependency is the shared `lib/help`
//! engine used for the short-help switches. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths; nothing writes to fd 3
//! (`stdinfo` — a full sequence is never an omission or summary).
//!
//! The bundle's `Help/` documents are **not** embedded in this crate:
//! they are authored once in the bundle's on-disk `Help/` tree, planted
//! onto `/System` by the image builder from that source (`tools/syshelp`),
//! and read back at runtime through the injected [`rustos_help::HelpSource`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod format;
pub mod number;

pub use client::{run, Output, USAGE};
pub use command::{parse, Command, Job};
pub use error::SeqError;
pub use format::{parse_format, Format};
pub use number::{parse_number, scan_arg, Operand};
