//! TAIRiX `unlink` — remove one name (`plans/APPS.md` §12.1 Stage E,
//! `plans/SYMLINKS.md`).
//!
//! `unlink` removes exactly one name, through the single `fs_unlink` the
//! GNU tool's one `unlink(2)` call maps onto. It is the minimal tool the
//! POSIX call deserves: no recursion, no prompting, no force, no verbose —
//! precisely one operand and precisely one removal, so a script that must
//! remove one name and nothing else has a tool that cannot do more.
//! `-?`/`--help` render the tool's own short help from its bundled `Help/`
//! tree through the shared `lib/help` engine (plans/APPS.md §4).
//!
//! # The name, never what it names
//!
//! The removal is of the name as typed. A symbolic link is removed itself,
//! never followed to its target, so a planted link can never redirect the
//! removal somewhere else — the kernel's `fs_unlink` keeps the final
//! component, which is what makes that structural rather than a convention
//! this tool observes.
//!
//! A **directory** is refused by the kernel: the empty flag word asks for a
//! non-directory removal, so the refusal is decided in the same locked walk
//! that would have removed the entry and no stat/remove race exists here.
//! `rmdir` is the tool for a directory.
//!
//! # What this crate is
//!
//! A **parse-and-remove engine**. For one parsed [`Command`] it performs the
//! one removal and reports nothing on success. Everything that touches the
//! outside world is an injected seam:
//!
//! * [`Filesystem`] — remove one name.
//! * [`Output`] — write the short help to the terminal.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `unlink` wires the real syscall-backed
//! filesystem and console; tests wire in-memory fixtures. This is the seam
//! discipline of the sibling tools (`rmdir`'s `Filesystem`, `ln`'s
//! `FileSystem`).
//!
//! # Fail closed
//!
//! An unrecognised option, no operand, or more than one operand is
//! [`UnlinkError::Usage`] and nothing is removed — GNU `unlink` takes
//! exactly one, and a second operand is far more likely a mistake than an
//! intention. A refused removal carries the kernel's own [`Errno`]. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `unlink`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: unlink [--] file

  -?, --help  show this message

Exactly one operand. `--` ends option parsing, so a name that begins with
a dash is removable. The name is removed as typed: a symbolic link is
removed itself, never followed. A directory is refused (use rmdir).
";

/// `unlink`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "unlink";

/// Why an `unlink` invocation did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnlinkError {
    /// The command line carried an unrecognised option, no operand, or more
    /// than one. The caller should print [`USAGE`]. Nothing is removed.
    Usage,
    /// Removing `path` failed. Carries the kernel's [`Errno`] — e.g.
    /// [`Errno::IsADirectory`] for a directory operand,
    /// [`Errno::NotFound`], or [`Errno::PermissionDenied`].
    Remove {
        /// The name whose removal was refused.
        path: String,
        /// The kernel's refusal.
        errno: Errno,
    },
    /// Writing the short help failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for UnlinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Remove { path, errno } => write!(f, "cannot unlink '{path}': {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// One parsed `unlink` command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// A requested short help (`-?`, `--help`).
    Help,
    /// Remove the one named entry.
    Remove(String),
}

/// Parse an argument vector (without the program name).
///
/// # Errors
///
/// [`UnlinkError::Usage`] on an unrecognised option, no operand, or a second
/// operand.
pub fn parse(args: &[&str]) -> Result<Command, UnlinkError> {
    let mut operand: Option<&str> = None;
    let mut options_ended = false;
    for &arg in args {
        if !options_ended {
            if arg == "--" {
                options_ended = true;
                continue;
            }
            // The bare `-` is a name, not an option cluster.
            if arg.len() > 1 && arg.starts_with('-') {
                match arg {
                    "-?" | "--help" => return Ok(Command::Help),
                    _ => return Err(UnlinkError::Usage),
                }
            }
        }
        if operand.replace(arg).is_some() {
            return Err(UnlinkError::Usage);
        }
    }
    operand
        .map(|path| Command::Remove(String::from(path)))
        .ok_or(UnlinkError::Usage)
}

/// Removes one name.
pub trait Filesystem {
    /// Remove the entry `path` names — the name as typed, never what a link
    /// names, and never a directory.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — e.g. [`Errno::IsADirectory`] for a
    /// directory, [`Errno::NotFound`], or [`Errno::PermissionDenied`].
    fn unlink(&self, path: &str) -> Result<(), Errno>;
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

/// Run one [`Command`] against `fs`. `locale` is the user's `LANG`
/// preference, if set; `help` is the tool's own `Help/` tree, read by the
/// short-help switches.
///
/// # Errors
///
/// * [`UnlinkError::Remove`] — the kernel refused the removal.
/// * [`UnlinkError::Output`] — writing the short help failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Filesystem,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), UnlinkError> {
    match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| String::from(USAGE).into_bytes());
            out.write_all(&bytes).map_err(UnlinkError::Output)
        }
        Command::Remove(path) => fs
            .unlink(&path)
            .map_err(|errno| UnlinkError::Remove { path, errno }),
    }
}

#[cfg(test)]
mod tests;
