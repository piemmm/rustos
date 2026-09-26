//! The `wintersun` command line (`plans/APPS.md`).
//!
//! Closed: the game takes no operands, and its one option asks for the
//! reference scene in place of a new world. Anything outside the grammar is
//! a usage error, never a guess.

/// The usage banner a usage error is reported with, and what the short-help
/// switches print when the bundle's own Help tree cannot be read.
pub const USAGE: &str = "usage: wintersun [-h | -? | --help | --reference-scene]";

/// The option asking for the reference scene.
pub const REFERENCE_SCENE: &str = "--reference-scene";

/// The switches asking for the command's own short help.
pub const HELP_SWITCHES: [&str; 3] = ["-h", "-?", "--help"];

/// What a launch asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Launch {
    /// A new world, generated from the clock and played.
    Play,
    /// The reference scene (`crate::reference`), held still.
    ReferenceScene,
    /// The command's own short help.
    Help,
}

/// The one failure [`parse`] reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliError {
    /// The command line was not understood.
    Usage,
}

/// Parse `args`, the arguments after the program name.
///
/// Read left to right, as every command app reads its options: a short-help
/// switch wins where it is reached, `--reference-scene` may be given more
/// than once, and `--` ends the options. The game takes no operands.
///
/// # Errors
///
/// [`CliError::Usage`] for an option outside the grammar reached before any
/// help switch, or for an operand.
pub fn parse(args: &[&str]) -> Result<Launch, CliError> {
    let mut launch = Launch::Play;
    let mut rest = args.iter();
    while let Some(&arg) = rest.next() {
        if HELP_SWITCHES.contains(&arg) {
            return Ok(Launch::Help);
        }
        match arg {
            REFERENCE_SCENE => launch = Launch::ReferenceScene,
            "--" => {
                return match rest.next() {
                    None => Ok(launch),
                    Some(_) => Err(CliError::Usage),
                }
            }
            _ => return Err(CliError::Usage),
        }
    }
    Ok(launch)
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
