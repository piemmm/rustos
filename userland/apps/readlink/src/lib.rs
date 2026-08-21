//! TAIRiX `readlink` — print a symbolic link's target (`plans/APPS.md`
//! §12.1 Stage E, `plans/SYMLINKS.md`).
//!
//! `readlink` prints the target each operand stores, one per operand.
//! The target is the **stored spelling**, read verbatim: it may be
//! relative, may carry `..`, and may name nothing at all, because a link's
//! target is data rather than a path the kernel walked when the link was
//! made. An operand that is not a symbolic link has no target to print and
//! is refused.
//!
//! `-z` ends each line with NUL instead of newline, `-n` drops the final
//! delimiter, and `-v`/`-q`/`-s` select whether a refusal is diagnosed —
//! quiet is the GNU default. `-?`/`--help` render the tool's own short help
//! from its bundled `Help/` tree through the shared `lib/help` engine
//! (plans/APPS.md §4).
//!
//! # Why `-f`/`-e`/`-m` are refused rather than implemented
//!
//! GNU's three canonicalisation switches resolve *every* component of a
//! path, following each link they meet. That resolution already exists,
//! exactly once, inside the VFS — with its physical `..`, its hop budget,
//! its per-component permission checks, and its rule that a link cannot
//! escape the volume that stores it (`plans/SYMLINKS.md`). Re-deriving it
//! here from `fs_readlink` calls would be a second implementation of a
//! security-relevant algorithm, and a userland copy that disagreed with the
//! kernel by one rule would print a path the kernel resolves differently.
//! So the three switches fail closed with [`ReadlinkError::Unsupported`]
//! until the VFS exposes its own canonicalisation, rather than being
//! stubbed or approximated — the same posture `ln` takes for `-r` and `du`
//! for `-x`.
//!
//! # What this crate is
//!
//! A **parse-and-print engine**. For one parsed [`Command`] it reads each
//! operand's target and renders the lines. Everything that touches the
//! outside world is an injected seam:
//!
//! * [`Filesystem`] — read one link's stored target.
//! * [`Output`] — the printed targets (fd 1) and the diagnostics (fd 2).
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `readlink` wires the real syscall-backed
//! filesystem and standard streams; tests wire in-memory fixtures. This is
//! the seam discipline of the sibling tools (`ln`'s `FileSystem`, `du`'s
//! `Walk`).
//!
//! # Fail closed
//!
//! An unrecognised option or no operand is [`ReadlinkError::Usage`] and
//! nothing is printed. A refused read is reported (under `-v`) and the run
//! continues to the remaining operands, exiting non-zero — the GNU
//! behaviour. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `readlink`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "\
usage: readlink [-nz] [-q | -s | -v] [--] file...

  -n, --no-newline  do not print the delimiter after the last target
  -z, --zero        end each target with NUL instead of newline
  -q, -s            do not diagnose a refused read (the default)
  -v, --verbose     diagnose a refused read on standard error
  -?, --help        show this message

At least one operand is required. `--` ends option parsing. Each target
is printed as stored, never resolved: use ls -l to see a link beside
what it names.
";

/// `readlink`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "readlink";

/// Why a `readlink` invocation did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadlinkError {
    /// The command line carried an unrecognised option or named no operand.
    /// The caller should print [`USAGE`]. Nothing is printed.
    Usage,
    /// A canonicalisation switch (`-f`, `-e`, `-m`) was given. Carries the
    /// switch as spelled. The VFS owns path resolution, so this tool
    /// refuses rather than re-deriving it (see the crate documentation).
    Unsupported(String),
    /// Writing a target or a diagnostic failed. Carries the underlying
    /// [`Errno`]; a stream that stops accepting bytes is fatal.
    Output(Errno),
}

impl fmt::Display for ReadlinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Unsupported(switch) => write!(
                f,
                "{switch} is not available: path canonicalisation is the filesystem's, \
                 and this tool will not re-derive it"
            ),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// The parsed switches of a `readlink` run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// `-n`/`--no-newline`: omit the delimiter after the last target.
    /// Ignored, with a diagnostic, when more than one operand is named —
    /// the GNU behaviour, because the delimiters between targets are what
    /// separate them.
    pub no_newline: bool,
    /// `-z`/`--zero`: end each target with NUL instead of newline.
    pub zero: bool,
    /// `-v`/`--verbose`: diagnose a refused read. Quiet is the default, so
    /// `-q`/`-s` clear this; the last of the three wins.
    pub verbose: bool,
}

impl Options {
    /// The byte that ends one printed target.
    const fn delimiter(self) -> u8 {
        if self.zero {
            b'\0'
        } else {
            b'\n'
        }
    }
}

/// One parsed `readlink` command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// A requested short help (`-?`, `--help`).
    Help,
    /// Print each operand's stored target.
    Print {
        /// The parsed switches.
        options: Options,
        /// The path operands, in command-line order.
        paths: Vec<String>,
    },
}

/// Parse an argument vector (without the program name).
///
/// # Errors
///
/// * [`ReadlinkError::Usage`] on an unrecognised option or no operand.
/// * [`ReadlinkError::Unsupported`] for `-f`/`-e`/`-m`.
pub fn parse(args: &[&str]) -> Result<Command, ReadlinkError> {
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
                    "no-newline" => options.no_newline = true,
                    "zero" => options.zero = true,
                    "verbose" => options.verbose = true,
                    "silent" | "quiet" => options.verbose = false,
                    "canonicalize" | "canonicalize-existing" | "canonicalize-missing" => {
                        return Err(ReadlinkError::Unsupported(String::from(arg)))
                    }
                    "help" => return Ok(Command::Help),
                    _ => return Err(ReadlinkError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least
            // one letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'n' => options.no_newline = true,
                        'z' => options.zero = true,
                        'v' => options.verbose = true,
                        'q' | 's' => options.verbose = false,
                        'f' | 'e' | 'm' => {
                            return Err(ReadlinkError::Unsupported(format!("-{letter}")))
                        }
                        '?' => return Ok(Command::Help),
                        _ => return Err(ReadlinkError::Usage),
                    }
                }
                continue;
            }
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() {
        return Err(ReadlinkError::Usage);
    }
    Ok(Command::Print { options, paths })
}

/// Reads a symbolic link's stored target.
pub trait Filesystem {
    /// The target the link at `path` stores, exactly as stored.
    ///
    /// The final component is never followed: the answer is the *link's*
    /// own target, not a resolution of it.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — [`Errno::OutOfRange`] when `path`
    /// names something that is not a symbolic link (a file, a directory:
    /// neither has a target), [`Errno::NotFound`] for an absent name,
    /// [`Errno::NotSupported`] on a format that stores no links, or a
    /// permission refusal.
    fn read_link(&self, path: &str) -> Result<String, Errno>;
}

/// Writes rendered bytes to one of the tool's output streams.
///
/// The client uses two instances: standard output for the targets, and
/// standard error for the per-operand diagnostics.
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
/// Returns `Ok(true)` when every operand's target was printed, `Ok(false)`
/// when at least one read was refused (the GNU behaviour: continue, exit
/// non-zero).
///
/// # Errors
///
/// [`ReadlinkError::Output`] when a target or a diagnostic cannot be
/// written — the only fatal condition.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Filesystem,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, ReadlinkError> {
    let (options, paths) = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| String::from(USAGE).into_bytes());
            out.write_all(&bytes).map_err(ReadlinkError::Output)?;
            return Ok(true);
        }
        Command::Print { options, paths } => (options, paths),
    };
    // The delimiters between targets are what separate them, so `-n` can
    // only apply to a single operand; GNU says so on standard error rather
    // than silently running them together.
    let mut trailing = !options.no_newline;
    if options.no_newline && paths.len() > 1 {
        err.write_all(b"readlink: ignoring --no-newline with multiple arguments\n")
            .map_err(ReadlinkError::Output)?;
        trailing = true;
    }
    let mut clean = true;
    let last = paths.len() - 1;
    for (index, path) in paths.iter().enumerate() {
        match fs.read_link(path) {
            Ok(target) => {
                out.write_all(target.as_bytes())
                    .map_err(ReadlinkError::Output)?;
                if trailing || index != last {
                    out.write_all(&[options.delimiter()])
                        .map_err(ReadlinkError::Output)?;
                }
            }
            Err(errno) => {
                clean = false;
                if options.verbose {
                    err.write_all(format!("readlink: {path}: {errno}\n").as_bytes())
                        .map_err(ReadlinkError::Output)?;
                }
            }
        }
    }
    Ok(clean)
}

#[cfg(test)]
mod tests;
