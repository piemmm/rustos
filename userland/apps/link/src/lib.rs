//! TAIRiX `link` — give a file a second name (`plans/APPS.md` §12.1
//! Stage E, `plans/SYMLINKS.md` S6).
//!
//! `link EXISTING NEW` creates one hard link, through the single `fs_link`
//! the GNU tool's one `link(2)` call maps onto. It is the minimal tool that
//! call deserves: exactly two operands, exactly one link, and no
//! `-f`/`-i`/`-s`/`-v`/`-t`/`-T` — `ln` is the tool with the option surface
//! and with symbolic links. Keeping them separate is the point: a script
//! that must create one hard link and nothing else gets a tool that
//! *cannot* replace a name, follow a link, or make a symbolic one instead.
//! `-?`/`--help` render the tool's own short help from its bundled `Help/`
//! tree through the shared `lib/help` engine (plans/APPS.md §4).
//!
//! # Neither name is followed
//!
//! The link is made between the two names **as typed**. `fs_link` is called
//! with an empty [`LinkFlags`](tairix_abi::LinkFlags) word, which is POSIX
//! `link()`: the node that gains a name is the one the caller spelled, so a
//! symbolic link planted at `EXISTING` cannot redirect the new name at what
//! it points to. (`ln -L` is the tool for the following posture.) The *new*
//! name is never followed under either posture, because it is a name being
//! created and a create never replaces one — an occupied `NEW` is
//! [`Errno::AlreadyExists`], not a silent overwrite.
//!
//! Every other refusal is the kernel's, and each says something different:
//! [`Errno::IsADirectory`] because a directory has exactly one name
//! everywhere, [`Errno::CrossVolume`] because a node's second name must live
//! on the volume that stores it, [`Errno::TooManyLinks`] when the format's
//! per-node name count would overflow, and [`Errno::NotSupported`] on a
//! format that holds one name per node — a permanent property of the format,
//! never a transient failure.
//!
//! # What this crate is
//!
//! A **parse-and-link engine**. For one parsed [`Command`] it makes the one
//! link and reports nothing on success. Everything that touches the outside
//! world is an injected seam:
//!
//! * [`Filesystem`] — create one hard link.
//! * [`Output`] — write the short help to the terminal.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `link` wires the real syscall-backed filesystem
//! and console; tests wire in-memory fixtures. This is the seam discipline
//! of the sibling tools (`ln`'s `FileSystem`, `unlink`'s `Filesystem`).
//!
//! # Fail closed
//!
//! An unrecognised option, or anything other than exactly two operands, is
//! [`LinkError::Usage`] and no link is made. A refused creation carries the
//! kernel's own [`Errno`]. No `unsafe`, and no `unwrap`/`expect`/`panic!`
//! in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `link`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: link [--] existing new

  -?, --help  show this message

Exactly two operands. `--` ends option parsing, so a name that begins
with a dash may be linked. Neither name is followed: the node given the
second name is the one spelled, and an occupied `new` is refused rather
than replaced. Use ln for symbolic links and for -f/-i/-v/-t/-T.
";

/// `link`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "link";

/// Why a `link` invocation did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkError {
    /// The command line carried an unrecognised option, or anything other
    /// than exactly two operands. The caller should print [`USAGE`]. No link
    /// is made.
    Usage,
    /// The link could not be created. Carries both names the GNU tool's own
    /// diagnostic reports and the kernel's [`Errno`] — e.g.
    /// [`Errno::AlreadyExists`] for an occupied new name,
    /// [`Errno::IsADirectory`] for a directory,
    /// [`Errno::CrossVolume`] across mounts, [`Errno::TooManyLinks`] on a
    /// count overflow, or [`Errno::NotSupported`] on a format that stores
    /// one name per node.
    Create {
        /// The name that already exists.
        existing: String,
        /// The name that was to be created.
        new: String,
        /// The kernel's refusal.
        errno: Errno,
    },
    /// Writing the short help failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Create {
                existing,
                new,
                errno,
            } => write!(f, "cannot create link '{new}' to '{existing}': {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// One parsed `link` command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// A requested short help (`-?`, `--help`).
    Help,
    /// Give the node `existing` names the second name `new`.
    Create {
        /// The name that already exists — the node that gains a name.
        existing: String,
        /// The name to create.
        new: String,
    },
}

/// Parse an argument vector (without the program name).
///
/// # Errors
///
/// [`LinkError::Usage`] on an unrecognised option, or on anything other than
/// exactly two operands.
pub fn parse(args: &[&str]) -> Result<Command, LinkError> {
    let mut operands: Vec<&str> = Vec::new();
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
                    _ => return Err(LinkError::Usage),
                }
            }
        }
        if operands.len() == 2 {
            return Err(LinkError::Usage);
        }
        operands.push(arg);
    }
    match operands[..] {
        [existing, new] => Ok(Command::Create {
            existing: String::from(existing),
            new: String::from(new),
        }),
        _ => Err(LinkError::Usage),
    }
}

/// Creates one hard link.
pub trait Filesystem {
    /// Add `new` as a second directory entry for the node `existing` names.
    ///
    /// Neither name is followed: `existing` is the node as spelled (POSIX
    /// `link()`), and `new` is a name being created, so an occupied one is
    /// refused rather than replaced.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — [`Errno::AlreadyExists`] for an
    /// occupied new name, [`Errno::IsADirectory`] for a directory,
    /// [`Errno::CrossVolume`] when the two names are on different volumes,
    /// [`Errno::TooManyLinks`] on a name-count overflow,
    /// [`Errno::NotSupported`] on a format that stores one name per node, or
    /// a permission refusal.
    fn link(&self, existing: &str, new: &str) -> Result<(), Errno>;
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
/// * [`LinkError::Create`] — the kernel refused the link.
/// * [`LinkError::Output`] — writing the short help failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Filesystem,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LinkError> {
    match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| String::from(USAGE).into_bytes());
            out.write_all(&bytes).map_err(LinkError::Output)
        }
        Command::Create { existing, new } => {
            fs.link(&existing, &new).map_err(|errno| LinkError::Create {
                existing,
                new,
                errno,
            })
        }
    }
}

#[cfg(test)]
mod tests;
