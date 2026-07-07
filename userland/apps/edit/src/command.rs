//! The parsed shape of an `edit` command line.

use alloc::string::String;

use crate::error::EditError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `edit`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: edit [file] [-h | -?]";

/// One thing the `edit` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the full-screen editor, on `path` when one was named (a file
    /// that does not exist yet starts as an empty buffer to be created on
    /// the first save) or on an unnamed new buffer otherwise.
    Run {
        /// The file operand, when the command line named one.
        path: Option<String>,
    },
    /// Render `edit`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `edit [file] [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end of options; the next argument is the file operand even
///   when it starts with a dash.
/// * one optional operand — the file to edit. Everything the editor does
///   beyond that is keys pressed inside the session, so a second operand
///   or any other option is refused.
///
/// # Errors
///
/// [`EditError::Usage`] on an unrecognised option or a second operand.
pub fn parse(args: &[&str]) -> Result<Command, EditError> {
    let mut path: Option<String> = None;
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            match arg {
                "--" => {
                    options_done = true;
                    continue;
                }
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                _ if arg.starts_with('-') && arg.len() > 1 => return Err(EditError::Usage),
                _ => {}
            }
        }
        if path.is_some() {
            // A second operand: the editor holds exactly one buffer.
            return Err(EditError::Usage);
        }
        path = Some(String::from(arg));
    }
    Ok(Command::Run { path })
}
