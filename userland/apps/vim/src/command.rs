//! The `vim` command line: switches, startup positioning, and file
//! operands.
//!
//! The accepted grammar is the vim core:
//!
//! ```text
//! vim [-R] [+num | + | +/pattern] [--] [file ...]
//! vim -h | -?
//! ```
//!
//! `-R` opens readonly; `+num` starts on line `num`, a bare `+` on the
//! last line, and `+/pattern` on the first match. `--` ends option
//! parsing. The reserved `-h`/`-?` short-help switches render the tool's
//! own Help document (plans/APPS.md). Anything else is a usage error.

use alloc::string::String;
use alloc::vec::Vec;

/// The usage banner (also the `-h` fallback when no help document is
/// available).
pub const USAGE: &str = "usage: vim [-R] [+num | + | +/pattern] [--] [file ...]";

/// Where the editor starts after loading the first file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Start {
    /// `+num` — a 1-based line.
    Line(usize),
    /// `+` — the last line.
    LastLine,
    /// `+/pattern` — the first match of `pattern`.
    Pattern(String),
}

/// A parsed command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Serve the short help (`-h` / `-?`).
    Help,
    /// Run the editor.
    Run {
        /// `-R` — refuse writes.
        readonly: bool,
        /// The startup position, if any.
        start: Option<Start>,
        /// The file operands (the argument list).
        files: Vec<String>,
    },
}

/// A usage error: the offending argument, spelled out on stderr by the
/// caller alongside [`USAGE`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageError {
    /// The argument that could not be understood.
    pub argument: String,
}

/// Parse the argument vector (`arguments[0]` is the command word itself).
pub fn parse(arguments: &[&str]) -> Result<Command, UsageError> {
    let mut readonly = false;
    let mut start: Option<Start> = None;
    let mut files: Vec<String> = Vec::new();
    let mut options_done = false;
    for &argument in arguments.iter().skip(1) {
        if options_done {
            files.push(String::from(argument));
            continue;
        }
        match argument {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "-R" => readonly = true,
            "--" => options_done = true,
            "+" => start = Some(Start::LastLine),
            _ if argument.starts_with("+/") => {
                let pattern = &argument[2..];
                if pattern.is_empty() {
                    return Err(UsageError {
                        argument: String::from(argument),
                    });
                }
                start = Some(Start::Pattern(String::from(pattern)));
            }
            _ if argument.starts_with('+') => match argument[1..].parse::<usize>() {
                Ok(line) if line > 0 => start = Some(Start::Line(line)),
                _ => {
                    return Err(UsageError {
                        argument: String::from(argument),
                    })
                }
            },
            _ if argument.starts_with('-') && argument.len() > 1 => {
                return Err(UsageError {
                    argument: String::from(argument),
                });
            }
            _ => files.push(String::from(argument)),
        }
    }
    Ok(Command::Run {
        readonly,
        start,
        files,
    })
}
