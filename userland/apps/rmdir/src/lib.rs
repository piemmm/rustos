//! RustOS `rmdir` — remove empty directories (`plans/APPS.md` §12.1
//! Stage C).
//!
//! `rmdir` removes each directory operand, in order. A removal succeeds
//! only when the operand is an **empty directory**: the kernel decides that
//! atomically, in the same locked walk that removes the entry (the
//! directory-only `fs_unlink`), so a concurrent swap of the directory for a
//! file can never unlink the file. With `-p`/`--parents` each operand's
//! ancestors are removed too, innermost first, the GNU semantics.
//! `--ignore-fail-on-non-empty` tolerates exactly the "directory not empty"
//! refusal and no other. `-v`/`--verbose` reports each attempt.
//! `-h`/`-?`/`--help` render the tool's own short help from its bundled
//! `Help/` tree through the shared `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **pure removal engine**: for one parsed [`Command`] it decides which
//! paths to remove, in which order, and what to report. Everything that
//! touches the outside world is an injected seam:
//!
//! * [`Filesystem`] — remove an (empty) directory.
//! * [`Output`] — write `-v` reports to the terminal.
//! * [`rustos_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `rmdir` wires the real syscall-backed
//! filesystem and console; tests wire in-memory fixtures. This is the seam
//! discipline of the sibling tools (`rm`'s `Removal`, `mkdir`'s
//! `Filesystem`).
//!
//! The kernel stays the one path validator: an operand is handed to the
//! removal seam as spelled, and only the `-p` ancestor walk parses it —
//! through the one shared grammar (`rustos_path`, `Path::prefix`) — with an
//! unparseable operand removed whole so no second validator exists.
//!
//! # Fail closed
//!
//! An unknown option is [`RmdirError::Usage`] and nothing is removed; the
//! first failing removal stops the run with the kernel's own
//! [`Errno`] in [`RmdirError::Remove`] (unless it is the
//! one refusal `--ignore-fail-on-non-empty` tolerates); a failed terminal
//! write is [`RmdirError::Output`]. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use rustos_abi::Errno;
use rustos_help::{own_short_help, HelpSource};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `rmdir`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...

  -p, --parents               remove each operand's ancestors too,
                              innermost first
  -v, --verbose               report each removal attempt
  --ignore-fail-on-non-empty  a directory that is not empty is not an
                              error (with -p the upward walk stops there)
  -h, -?, --help              show this message

At least one directory operand is required. `--` ends option parsing:
every later argument is a path. Only an empty directory is removed; a
file is refused (use rm for files).
";

/// `rmdir`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "rmdir";

/// Why an `rmdir` invocation did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RmdirError {
    /// The command line carried an unrecognised option or named no operand.
    /// The caller should print [`USAGE`]. Nothing is removed.
    Usage,
    /// Removing `path` failed. Carries the kernel's [`Errno`] — e.g.
    /// [`Errno::NotEmpty`] for a populated directory,
    /// [`Errno::NotADirectory`] when the operand is a file, or
    /// [`Errno::NotFound`].
    Remove {
        /// The path whose removal was refused.
        path: String,
        /// The kernel's refusal.
        errno: Errno,
    },
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for RmdirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Remove { path, errno } => {
                write!(f, "failed to remove '{path}': {errno}")
            }
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// The parsed options of a removal run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// `-p`/`--parents`: remove each operand's ancestors too, innermost
    /// first.
    pub parents: bool,
    /// `-v`/`--verbose`: report each removal attempt.
    pub verbose: bool,
    /// `--ignore-fail-on-non-empty`: a [`Errno::NotEmpty`] refusal is not
    /// an error; with `-p` the upward walk stops there.
    pub ignore_non_empty: bool,
}

/// One parsed `rmdir` command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// A requested short help (`-h`, `-?`, `--help`).
    Help,
    /// Remove the named directories.
    Remove {
        /// The parsed switches.
        options: Options,
        /// The directory operands, in command-line order.
        paths: Vec<String>,
    },
}

/// Parse an argument vector (without the program name).
///
/// # Errors
///
/// [`RmdirError::Usage`] on an unrecognised option or when no operand is
/// named.
pub fn parse(args: &[&str]) -> Result<Command, RmdirError> {
    let mut options = Options::default();
    let mut paths = Vec::new();
    let mut options_ended = false;
    for &arg in args {
        if !options_ended {
            if arg == "--" {
                options_ended = true;
                continue;
            }
            if let Some(word) = arg.strip_prefix("--") {
                match word {
                    "parents" => options.parents = true,
                    "verbose" => options.verbose = true,
                    "ignore-fail-on-non-empty" => options.ignore_non_empty = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(RmdirError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least
            // one letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'p' => options.parents = true,
                        'v' => options.verbose = true,
                        'h' | '?' => return Ok(Command::Help),
                        _ => return Err(RmdirError::Usage),
                    }
                }
                continue;
            }
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() {
        return Err(RmdirError::Usage);
    }
    Ok(Command::Remove { options, paths })
}

/// Removes (empty) directories.
pub trait Filesystem {
    /// Remove the (empty) directory at `path` — the directory-only removal:
    /// the filesystem refuses a non-directory atomically.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — e.g. [`Errno::NotEmpty`] for a
    /// populated directory, [`Errno::NotADirectory`] for a file,
    /// [`Errno::NotFound`], or [`Errno::PermissionDenied`].
    fn rmdir(&self, path: &str) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal (fd 1).
pub trait Output {
    /// Write all of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises; a partial write is an error.
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Whether an operand's upward `-p` walk continues after one removal.
enum Walk {
    /// The removal succeeded; keep climbing.
    Continue,
    /// A tolerated non-empty refusal; stop climbing this operand, no error.
    Stop,
}

/// Run one [`Command`], removing its operands through `fs`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// Operands are removed in order; the first failure stops the run (a
/// non-empty refusal under `--ignore-fail-on-non-empty` is not a failure).
///
/// # Errors
///
/// * [`RmdirError::Remove`] — a removal the kernel refused.
/// * [`RmdirError::Output`] — writing the short help or a `-v` report
///   failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Filesystem,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), RmdirError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Remove { options, paths } => {
            for path in &paths {
                remove_chain(fs, out, options, path)?;
            }
            Ok(())
        }
    }
}

/// Render the tool's own short help through the shared engine, falling back
/// to the usage banner when no document can be served.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), RmdirError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(RmdirError::Output)
}

/// Remove `operand` and, under `-p`, each of its ancestors innermost first
/// (the GNU semantics: `rmdir -p a/b/c` removes `a/b/c`, `a/b`, `a`).
///
/// The ancestor spellings come from the one shared path grammar
/// (`rustos_path::Path::prefix`); an operand that grammar cannot parse is
/// removed whole, so the kernel stays the one validator.
fn remove_chain(
    fs: &dyn Filesystem,
    out: &dyn Output,
    options: Options,
    operand: &str,
) -> Result<(), RmdirError> {
    if let Walk::Stop = remove_one(fs, out, options, operand)? {
        return Ok(());
    }
    if !options.parents {
        return Ok(());
    }
    let Ok(parsed) = rustos_path::parse(operand) else {
        return Ok(());
    };
    let count = parsed.components().len();
    // The proper ancestors, innermost first, excluding the bare root
    // (`rmdir -p /a` never asks to remove `/`).
    for len in (1..count).rev() {
        let Some(prefix) = parsed.prefix(len) else {
            // Unreachable by construction (`len < count`), but fail closed
            // rather than panic.
            return Err(RmdirError::Remove {
                path: String::from(operand),
                errno: Errno::OutOfRange,
            });
        };
        if let Walk::Stop = remove_one(fs, out, options, &prefix)? {
            return Ok(());
        }
    }
    Ok(())
}

/// Remove one directory, reporting the attempt under `-v` (before the
/// attempt, the GNU order, so a failed removal is still visible next to its
/// diagnostic).
fn remove_one(
    fs: &dyn Filesystem,
    out: &dyn Output,
    options: Options,
    path: &str,
) -> Result<Walk, RmdirError> {
    if options.verbose {
        out.write_all(format!("rmdir: removing directory, '{path}'\n").as_bytes())
            .map_err(RmdirError::Output)?;
    }
    match fs.rmdir(path) {
        Ok(()) => Ok(Walk::Continue),
        Err(Errno::NotEmpty) if options.ignore_non_empty => Ok(Walk::Stop),
        Err(errno) => Err(RmdirError::Remove {
            path: String::from(path),
            errno,
        }),
    }
}

#[cfg(test)]
mod tests;
