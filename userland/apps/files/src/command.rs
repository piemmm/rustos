//! The file manager's command line: the starting location a caller may name,
//! and the reserved short-help switches.
//!
//! # Why this is its own module
//!
//! The `Run` binary around it is a freestanding program — it only exists when
//! the crate is built for a bare-metal target — so nothing inside it can be
//! reached by a host test. Deciding what a command line means is worth
//! testing: which spellings open a folder, which are turned down and with what
//! reason, and which are a usage error the program refuses outright. All of
//! that is a pure function of the argument vector, so it lives here, compiles
//! on the host, and is covered by the tests beside it.
//!
//! # The grammar
//!
//! `files [--desktop] [directory]`, with the argument conventions every other
//! command app follows: `-h`/`-?`/`--help` win immediately, `--` ends option
//! parsing, and at most one operand is accepted.
//!
//! # The two roles
//!
//! The switch selects which of two things this program is ([`Role`]). Without
//! it — from a shell, or when the desktop opens a folder — it is the ordinary
//! file manager: one window, the shared icon-bar menu convention, and the
//! process ends when that window closes. With it, the desktop session is
//! starting the program as a **component of itself**: a permanent icon-bar
//! slot offering the user's places and whatever is mounted, no window until
//! one is asked for, and no way to quit. Only the session passes the switch,
//! so a second component can never appear; and because a component opens no
//! window at start, naming a starting location alongside it is a command line
//! this program cannot act on rather than an argument silently ignored.
//!
//! # Untrusted input, and the two different refusals
//!
//! The operand comes from whoever launched the program — the desktop opening
//! a folder, a shell, or something hostile — so it is validated before it is
//! used, through the one shared path grammar rather than a second parser here.
//! There are deliberately two outcomes:
//!
//! * A **malformed or out-of-bounds location** ([`Start::refused`]) is a
//!   refusal the program states and recovers from: the window still opens, at
//!   the launching user's home. A bad argument never leaves the user with no
//!   window.
//! * A **command line the program cannot act on at all** ([`UsageError`]) — an
//!   unrecognised option, or a second operand — is refused outright, as in
//!   every other command app: guessing which of two operands was meant would
//!   be worse than saying so.
//!
//! Nothing here opens, lists, or writes anything: whether the accepted
//! location can actually be listed is the filesystem's decision, made under
//! the launching user's own identity by the program around this module.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::fs::FS_PATH_MAX;
use tairix_abi::Errno;
use tairix_browse::vfs::{absolute_path, components_from_absolute_path};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `files`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: files [--desktop] [directory] [-h | -?]";

/// Which of the two things a `files` process is.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Role {
    /// The ordinary file manager: one window at the starting location, the
    /// shared icon-bar menu convention on its slot, and the process ends when
    /// the window closes.
    #[default]
    Window,
    /// A component of the desktop, as the session's own bring-up asks for
    /// with [`tairix_window::DESKTOP_ROLE_SWITCH`]: a permanent icon-bar slot
    /// whose menu is the user's places and the mounted volumes, no window
    /// until one is asked for, and no *Quit*.
    Desktop,
}

/// What a `files` command line asks the program to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the file manager, as [`Start`] describes.
    Open(Start),
    /// Render `files`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help.
    Help,
}

/// What the program is, where its window opens, and what the command line
/// asked for that could not be honoured.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Start {
    /// Which of the two things this process is.
    pub role: Role,
    /// The accepted starting location as root-first path components, or
    /// `None` to open at the launching user's home — either because the
    /// command line named no location, or because the one it named was
    /// refused. Always `None` in the [`Role::Desktop`] role, which opens no
    /// window until one is asked for.
    pub location: Option<Vec<String>>,
    /// The reason a named location was turned down, ready to be stated on the
    /// error stream, or `None` when nothing was refused.
    ///
    /// Carried out rather than written here so the program states it through
    /// the single fail-loud reporting path it already uses for every other
    /// refusal, and so a test can read exactly what a user would be told.
    pub refused: Option<String>,
}

/// A command line the program cannot act on, and so refuses outright.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageError {
    /// An unrecognised option; carries the offending argument.
    UnknownOption(String),
    /// A second operand; carries it. The window shows one directory, so two
    /// locations have no meaning and the second is never silently dropped.
    ExtraOperand(String),
    /// A starting location alongside the desktop-role switch; carries it. A
    /// component opens no window until one is asked for, so there is nothing
    /// for the location to name — refused rather than quietly dropped.
    LocationInDesktopRole(String),
    /// The argument vector was not valid UTF-8 and so could not be read at
    /// all.
    NotUtf8,
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageError::UnknownOption(arg) => write!(f, "unrecognized option {arg:?}"),
            UsageError::ExtraOperand(arg) => write!(f, "extra operand {arg:?}"),
            UsageError::LocationInDesktopRole(arg) => write!(
                f,
                "starting location {arg:?} has no meaning with {}",
                tairix_window::DESKTOP_ROLE_SWITCH
            ),
            UsageError::NotUtf8 => f.write_str("argument vector is not valid UTF-8"),
        }
    }
}

/// Parse `args` (the program's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `files [--desktop] [directory]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches; they win
///   immediately, wherever they appear.
/// * `--desktop` — the desktop session asking for [`Role::Desktop`].
/// * `--` — end of options: the next argument is the directory operand even
///   when it starts with a dash.
/// * one optional operand — the directory to open at. It is validated here
///   and, when it does not survive, reported through [`Start::refused`] with
///   the window still opening at the launching user's home.
///
/// # Errors
///
/// [`UsageError::UnknownOption`] for an option this program does not know,
/// [`UsageError::ExtraOperand`] for a second operand, and
/// [`UsageError::LocationInDesktopRole`] for a location named alongside the
/// desktop-role switch.
pub fn parse(args: &[&str]) -> Result<Command, UsageError> {
    let mut operand: Option<&str> = None;
    let mut options_done = false;
    let mut role = Role::Window;
    for &arg in args {
        if !options_done {
            match arg {
                "--" => {
                    options_done = true;
                    continue;
                }
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                _ if arg == tairix_window::DESKTOP_ROLE_SWITCH => {
                    role = Role::Desktop;
                    continue;
                }
                _ if arg.starts_with('-') && arg.len() > 1 => {
                    return Err(UsageError::UnknownOption(String::from(arg)));
                }
                _ => {}
            }
        }
        if operand.is_some() {
            return Err(UsageError::ExtraOperand(String::from(arg)));
        }
        operand = Some(arg);
    }
    if role == Role::Desktop {
        if let Some(spelling) = operand {
            return Err(UsageError::LocationInDesktopRole(String::from(spelling)));
        }
        return Ok(Command::Open(Start {
            role,
            ..Start::default()
        }));
    }
    Ok(Command::Open(start_at(operand)))
}

/// The starting location `operand` names, or the home directory with the
/// reason it was refused.
///
/// Separate from [`parse`] so the validation of the location — the part that
/// treats the argument as hostile — is one function with one job.
fn start_at(operand: Option<&str>) -> Start {
    let Some(spelling) = operand else {
        return Start::default();
    };
    match location_components(spelling) {
        Ok(location) => Start {
            role: Role::Window,
            location: Some(location),
            refused: None,
        },
        Err(refused) => Start {
            role: Role::Window,
            location: None,
            refused: Some(refused),
        },
    }
}

/// Validate one starting-location spelling into root-first path components.
///
/// Fail closed, and bounded before anything is allocated from it: the raw
/// argument must be within the kernel's own path bound, must be an absolute
/// view path (an alias-rooted or relative spelling names a place this window
/// cannot address), and every component must survive the one shared filename
/// rule — which is what rejects `.`, `..`, a control character, and an
/// over-long name.
///
/// # Errors
///
/// The reason to state on the error stream, phrased for a user and naming the
/// spelling only when it is short enough to be worth echoing.
fn location_components(spelling: &str) -> Result<Vec<String>, String> {
    if spelling.len() > FS_PATH_MAX {
        return Err(format!(
            "starting location refused (longer than {FS_PATH_MAX} bytes); opening the home directory instead"
        ));
    }
    if !spelling.starts_with('/') {
        return Err(refused(spelling, "not an absolute path"));
    }
    components_from_absolute_path(spelling).map_err(|errno| match errno {
        Errno::LengthOutOfRange => refused(spelling, "path exceeds the maximum length"),
        _ => refused(spelling, &malformed_detail(spelling)),
    })
}

/// Which rule the spelling broke, taken from the one shared filename rule so
/// this module never carries a second copy of it.
///
/// Walked only on the refusal path, and only to phrase the diagnosis: the
/// acceptance decision was already made by the shared path parser.
fn malformed_detail(spelling: &str) -> String {
    for segment in spelling.split('/').filter(|segment| !segment.is_empty()) {
        if let Err(err) = tairix_path::validate_file_name(segment) {
            return format!("{segment:?}: {err}");
        }
    }
    String::from("not a valid path")
}

/// One refusal sentence: what was named, why it was turned down, and where the
/// window opens instead.
///
/// The spelling is written through its debug form so a control character or an
/// escape sequence in a hostile argument is escaped rather than replayed at
/// whatever terminal reads the error stream.
fn refused(spelling: &str, detail: &str) -> String {
    format!("starting location {spelling:?} refused ({detail}); opening the home directory instead")
}

/// The sentence to state when an accepted starting location turns out not to
/// be listable — a directory that does not exist, is not a directory, or that
/// the launching user may not read.
///
/// It lives here, with the rest of the command line's vocabulary, so the
/// wording of every "we are opening somewhere else" message is decided in one
/// place and can be read by a test.
#[must_use]
pub fn unlistable_reason(location: &[String]) -> String {
    let spelling = absolute_path(location).unwrap_or_else(|_| String::from("/"));
    format!("could not list {spelling}; opening the home directory instead")
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
