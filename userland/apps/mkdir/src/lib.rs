//! TAIRiX `mkdir` — make directories (`plans/APPS.md` §12.1 Stage C).
//!
//! `mkdir` creates each directory operand, in order. With `-p`/`--parents`
//! it creates every missing ancestor first and tolerates operands that
//! already exist as directories, the GNU semantics. `-v`/`--verbose`
//! reports each directory actually created. `-h`/`-?`/`--help` render the
//! tool's own short help from its bundled `Help/` tree through the shared
//! `lib/help` engine (plans/APPS.md §4).
//!
//! GNU `mkdir`'s `-m`/`--mode` is deliberately absent: the `fs_mkdir`
//! syscall carries no creation mode today, so the switch lands with the
//! mode-set kernel work `chmod` is staged on (`plans/APPS.md` §12.1
//! Stage B) — never as a stub that silently ignores its argument.
//!
//! # What this crate is
//!
//! A **pure creation engine**: for one parsed [`Command`] it decides which
//! paths to create, in which order, and what to report. Everything that
//! touches the outside world is an injected seam:
//!
//! * [`Filesystem`] — create a directory; learn what kind of object a path
//!   is (only ever consulted to tolerate an existing directory under `-p`).
//! * [`Output`] — write `-v` reports to the terminal.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `mkdir` wires the real syscall-backed
//! filesystem and console; tests wire in-memory fixtures. This is the seam
//! discipline of the sibling tools (`rm`'s `Removal`, `cat`'s `FileSource`).
//!
//! The kernel stays the one path validator: an operand is handed to
//! `fs_mkdir` as spelled, and only the `-p` ancestor walk parses it —
//! through the one shared grammar (`tairix_path`, `Path::prefix`) — with an
//! unparseable operand falling back to a single kernel call so no second
//! validator exists.
//!
//! # Fail closed
//!
//! An unknown option is [`MkdirError::Usage`] and nothing is created; the
//! first failing creation stops the run with the kernel's own
//! [`Errno`] in [`MkdirError::Create`]; a failed
//! terminal write is [`MkdirError::Output`]. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::{Errno, FileKind};
use tairix_help::{own_short_help, HelpSource};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `mkdir`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: mkdir [-pv] [--] directory...

  -p, --parents   make missing parent directories; an operand that is
                  already a directory is not an error
  -v, --verbose   report each created directory
  -h, -?, --help  show this message

At least one directory operand is required. `--` ends option parsing:
every later argument is a path.
";

/// `mkdir`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "mkdir";

/// Why a `mkdir` invocation did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MkdirError {
    /// The command line carried an unrecognised option or named no operand.
    /// The caller should print [`USAGE`]. Nothing is created.
    Usage,
    /// Creating `path` failed. Carries the kernel's [`Errno`] — e.g.
    /// [`Errno::AlreadyExists`] for an existing name or
    /// [`Errno::NotADirectory`] when an ancestor is a file.
    Create {
        /// The path whose creation was refused.
        path: String,
        /// The kernel's refusal.
        errno: Errno,
    },
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for MkdirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Create { path, errno } => {
                write!(f, "cannot create directory '{path}': {errno}")
            }
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// The parsed options of a creation run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// `-p`/`--parents`: create missing ancestors; an operand that already
    /// exists as a directory is not an error.
    pub parents: bool,
    /// `-v`/`--verbose`: report each directory actually created.
    pub verbose: bool,
}

/// One parsed `mkdir` command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// A requested short help (`-h`, `-?`, `--help`).
    Help,
    /// Create the named directories.
    Make {
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
/// [`MkdirError::Usage`] on an unrecognised option or when no operand is
/// named.
pub fn parse(args: &[&str]) -> Result<Command, MkdirError> {
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
                    "help" => return Ok(Command::Help),
                    _ => return Err(MkdirError::Usage),
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
                        _ => return Err(MkdirError::Usage),
                    }
                }
                continue;
            }
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() {
        return Err(MkdirError::Usage);
    }
    Ok(Command::Make { options, paths })
}

/// Creates directories and inspects what a path is.
pub trait Filesystem {
    /// Create the directory at `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — e.g. [`Errno::AlreadyExists`] for
    /// an existing name, [`Errno::NotADirectory`] when an ancestor is a
    /// file, or [`Errno::PermissionDenied`].
    fn mkdir(&self, path: &str) -> Result<(), Errno>;

    /// Return the [`FileKind`] of the existing object at `path`.
    ///
    /// Consulted only under `-p`, to decide whether an
    /// [`Errno::AlreadyExists`] refusal named a directory (tolerated) or
    /// something else (a genuine failure).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises while resolving `path`.
    fn kind(&self, path: &str) -> Result<FileKind, Errno>;
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

/// Run one [`Command`], creating its operands through `fs`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// Operands are created in order; the first failure stops the run.
///
/// # Errors
///
/// * [`MkdirError::Create`] — a creation the kernel refused.
/// * [`MkdirError::Output`] — writing the short help or a `-v` report
///   failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Filesystem,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), MkdirError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Make { options, paths } => {
            for path in &paths {
                if options.parents {
                    make_parents(fs, out, options.verbose, path)?;
                } else {
                    make_one(fs, out, options.verbose, path)?;
                }
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
) -> Result<(), MkdirError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(MkdirError::Output)
}

/// Create the single directory at `path`, reporting it under `-v`.
fn make_one(
    fs: &dyn Filesystem,
    out: &dyn Output,
    verbose: bool,
    path: &str,
) -> Result<(), MkdirError> {
    fs.mkdir(path).map_err(|errno| MkdirError::Create {
        path: String::from(path),
        errno,
    })?;
    report(out, verbose, path)
}

/// Create `path` and every missing ancestor, outermost first, tolerating
/// prefixes that already exist as directories (the GNU `-p` semantics).
///
/// The ancestor spellings come from the one shared path grammar
/// (`tairix_path::Path::prefix`); an operand that grammar cannot parse is
/// handed to the kernel whole, so the kernel stays the one validator.
fn make_parents(
    fs: &dyn Filesystem,
    out: &dyn Output,
    verbose: bool,
    operand: &str,
) -> Result<(), MkdirError> {
    let Ok(parsed) = tairix_path::parse(operand) else {
        return make_one(fs, out, verbose, operand);
    };
    let count = parsed.components().len();
    if count == 0 {
        // The operand names a bare root (`/`, `Home:/`, `.`), which always
        // exists; under `-p` an existing directory is not an error.
        return Ok(());
    }
    for len in 1..=count {
        let Some(prefix) = parsed.prefix(len) else {
            // Unreachable by construction (`len <= count`), but fail closed
            // rather than panic.
            return Err(MkdirError::Create {
                path: String::from(operand),
                errno: Errno::OutOfRange,
            });
        };
        match fs.mkdir(&prefix) {
            Ok(()) => report(out, verbose, &prefix)?,
            Err(Errno::AlreadyExists) if fs.kind(&prefix) == Ok(FileKind::Directory) => {}
            Err(errno) => {
                return Err(MkdirError::Create {
                    path: prefix,
                    errno,
                })
            }
        }
    }
    Ok(())
}

/// Write the GNU `-v` report line for a created directory.
fn report(out: &dyn Output, verbose: bool, path: &str) -> Result<(), MkdirError> {
    if !verbose {
        return Ok(());
    }
    out.write_all(format!("mkdir: created directory '{path}'\n").as_bytes())
        .map_err(MkdirError::Output)
}

#[cfg(test)]
mod tests;
