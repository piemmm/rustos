//! The shapes of a parsed `tee` command line, and their parser.
//!
//! The grammar is the GNU `tee` surface: `-a`/`--append` (append to the
//! file operands instead of overwriting them), `-p` (the pipe-tolerant
//! diagnose mode, equivalent to `--output-error=warn-nopipe`),
//! `--output-error[=MODE]` (matched like GNU `argmatch`: an exact name or
//! an unambiguous prefix of `warn`, `warn-nopipe`, `exit`, `exit-nopipe`;
//! the value arrives only attached with `=`, and a bare `--output-error`
//! selects `warn-nopipe`, exactly as GNU's optional argument does), the
//! reserved `-h`/`-?`/`--help` short-help switches, and `--`
//! end-of-options. GNU `tee` does not treat a `-` operand specially: it
//! names a file called `-`.
//!
//! GNU `tee -i`/`--ignore-interrupts` is deliberately absent: TAIRiX has
//! no per-process signal disposition to set (the `signal` syscall only
//! delivers a signal to a spawned child), so there is nothing the switch
//! could honestly do. It is staged behind that kernel work — never
//! stubbed — the `mkdir -m` precedent (plans/APPS.md §12.1).

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::TeeError;

/// How a failed output is treated (`--output-error`, GNU coreutils).
///
/// TAIRiX has no `SIGPIPE`: a program's consumer going away surfaces as a
/// *write error on standard output*, never a signal. The GNU modes key on
/// `EPIPE`, and the one place that condition arises here is the standard
/// output stream, so the "pipe" class maps to exactly that output: the
/// `-nopipe` modes tolerate a failed standard output silently and the
/// others treat it like any failed file. This confines the divergence to
/// the concept that genuinely differs (`AGENTS.md` §16.7) and is
/// documented in the tool's Help.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputError {
    /// No `--output-error`/`-p` was given: a failed standard output stops
    /// the run (the analogue of GNU dying of `SIGPIPE`, stated on the
    /// diagnostic stream — a TAIRiX process never ends silently); a failed
    /// file is diagnosed, dropped, and the run continues.
    #[default]
    Default,
    /// `warn`: diagnose an error writing to any output, drop that output,
    /// and continue.
    Warn,
    /// `warn-nopipe`: as `warn`, but a failed standard output (the pipe
    /// class) is dropped silently and does not affect the exit status.
    WarnNopipe,
    /// `exit`: diagnose an error writing to any output and stop.
    Exit,
    /// `exit-nopipe`: as `exit`, but a failed standard output (the pipe
    /// class) is dropped silently and does not affect the exit status.
    ExitNopipe,
}

impl OutputError {
    /// Match `text` against the mode names the way GNU `argmatch` does: an
    /// exact name or an unambiguous non-empty prefix (`warn` is exact even
    /// though it also prefixes `warn-nopipe`; `w` is ambiguous between the
    /// two and is refused; `warn-` unambiguously names `warn-nopipe`).
    fn parse(text: &str) -> Result<Self, TeeError> {
        const MODES: [(&str, OutputError); 4] = [
            ("warn", OutputError::Warn),
            ("warn-nopipe", OutputError::WarnNopipe),
            ("exit", OutputError::Exit),
            ("exit-nopipe", OutputError::ExitNopipe),
        ];
        if text.is_empty() {
            return Err(TeeError::InvalidMode(String::new()));
        }
        let mut matched: Option<OutputError> = None;
        for (name, mode) in MODES {
            if *name == *text {
                return Ok(mode);
            }
            if name.starts_with(text) {
                if matched.is_some() {
                    return Err(TeeError::InvalidMode(String::from(text)));
                }
                matched = Some(mode);
            }
        }
        matched.ok_or_else(|| TeeError::InvalidMode(String::from(text)))
    }
}

/// A parsed, runnable `tee` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    /// `-a`: append to the file operands instead of overwriting them.
    pub append: bool,
    /// How a failed output is treated.
    pub on_error: OutputError,
    /// The file operands, in command-line order. Standard output is always
    /// copied to as well; it is not an operand.
    pub files: Vec<String>,
}

/// One thing the `tee` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Copy standard input to standard output and each file operand.
    Tee(Job),
    /// Render `tee`'s own short help (`-h`/`-?`/`--help`) through the same
    /// engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// Options and operands may be interleaved (the GNU permutation); `--`
/// ends option parsing; a lone `-` is an operand naming a file called `-`
/// (GNU `tee` does not treat it specially). When `-p` and
/// `--output-error` are both given, the last occurrence wins, exactly as
/// repeated GNU options do.
///
/// # Errors
///
/// The [`TeeError`] usage variants, mirroring the GNU diagnostics.
pub fn parse(args: &[&str]) -> Result<Command, TeeError> {
    let mut append = false;
    let mut on_error = OutputError::default();
    let mut files: Vec<String> = Vec::new();
    let mut options_done = false;

    for &arg in args {
        if options_done || arg == "-" || !arg.starts_with('-') {
            files.push(String::from(arg));
            continue;
        }
        match arg {
            "--" => options_done = true,
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "--append" => append = true,
            // GNU's optional argument: a bare `--output-error` selects
            // `warn-nopipe`; a value arrives only attached with `=` (a
            // following word is an operand, never consumed as the value).
            "--output-error" => on_error = OutputError::WarnNopipe,
            _ if arg.starts_with("--output-error=") => {
                on_error = OutputError::parse(&arg["--output-error=".len()..])?;
            }
            _ if arg.starts_with("--") => return Err(TeeError::UnknownLong(String::from(arg))),
            _ => {
                for flag in arg[1..].chars() {
                    match flag {
                        'a' => append = true,
                        'p' => on_error = OutputError::WarnNopipe,
                        '?' => return Ok(Command::Help),
                        _ => return Err(TeeError::UnknownShort(flag)),
                    }
                }
            }
        }
    }

    Ok(Command::Tee(Job {
        append,
        on_error,
        files,
    }))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::{parse, Command, Job, OutputError};
    use crate::error::TeeError;

    fn job(append: bool, on_error: OutputError, files: &[&str]) -> Command {
        Command::Tee(Job {
            append,
            on_error,
            files: files.iter().map(|&f| String::from(f)).collect(),
        })
    }

    #[test]
    fn defaults_to_overwrite_and_the_default_error_mode() {
        assert_eq!(parse(&[]), Ok(job(false, OutputError::Default, &[])));
        assert_eq!(
            parse(&["log", "copy"]),
            Ok(job(false, OutputError::Default, &["log", "copy"]))
        );
    }

    #[test]
    fn append_by_short_long_and_bundle() {
        assert_eq!(
            parse(&["-a", "f"]),
            Ok(job(true, OutputError::Default, &["f"]))
        );
        assert_eq!(
            parse(&["--append", "f"]),
            Ok(job(true, OutputError::Default, &["f"]))
        );
        assert_eq!(
            parse(&["-ap", "f"]),
            Ok(job(true, OutputError::WarnNopipe, &["f"]))
        );
    }

    #[test]
    fn p_selects_warn_nopipe() {
        assert_eq!(parse(&["-p"]), Ok(job(false, OutputError::WarnNopipe, &[])));
    }

    #[test]
    fn bare_output_error_selects_warn_nopipe_and_never_consumes_a_word() {
        // GNU's optional argument: the following word is an operand.
        assert_eq!(
            parse(&["--output-error", "warn"]),
            Ok(job(false, OutputError::WarnNopipe, &["warn"]))
        );
    }

    #[test]
    fn output_error_modes_parse_like_argmatch() {
        assert_eq!(
            parse(&["--output-error=warn"]),
            Ok(job(false, OutputError::Warn, &[]))
        );
        assert_eq!(
            parse(&["--output-error=warn-nopipe"]),
            Ok(job(false, OutputError::WarnNopipe, &[]))
        );
        assert_eq!(
            parse(&["--output-error=exit"]),
            Ok(job(false, OutputError::Exit, &[]))
        );
        assert_eq!(
            parse(&["--output-error=exit-nopipe"]),
            Ok(job(false, OutputError::ExitNopipe, &[]))
        );
        // An unambiguous prefix is accepted ...
        assert_eq!(
            parse(&["--output-error=warn-"]),
            Ok(job(false, OutputError::WarnNopipe, &[]))
        );
        assert_eq!(
            parse(&["--output-error=exit-"]),
            Ok(job(false, OutputError::ExitNopipe, &[]))
        );
        // ... an exact name wins over the longer mode it prefixes ...
        assert_eq!(
            parse(&["--output-error=exit"]),
            Ok(job(false, OutputError::Exit, &[]))
        );
        // ... and an ambiguous or unknown one is refused.
        assert_eq!(
            parse(&["--output-error=w"]),
            Err(TeeError::InvalidMode(String::from("w")))
        );
        assert_eq!(
            parse(&["--output-error=e"]),
            Err(TeeError::InvalidMode(String::from("e")))
        );
        assert_eq!(
            parse(&["--output-error=abort"]),
            Err(TeeError::InvalidMode(String::from("abort")))
        );
        assert_eq!(
            parse(&["--output-error="]),
            Err(TeeError::InvalidMode(String::new()))
        );
    }

    #[test]
    fn the_last_error_mode_wins() {
        assert_eq!(
            parse(&["-p", "--output-error=exit"]),
            Ok(job(false, OutputError::Exit, &[]))
        );
        assert_eq!(
            parse(&["--output-error=exit", "-p"]),
            Ok(job(false, OutputError::WarnNopipe, &[]))
        );
    }

    #[test]
    fn dash_is_an_operand_naming_a_file() {
        // GNU `tee` does not treat `-` specially: it is a file called `-`.
        assert_eq!(parse(&["-"]), Ok(job(false, OutputError::Default, &["-"])));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse(&["--", "-a"]),
            Ok(job(false, OutputError::Default, &["-a"]))
        );
    }

    #[test]
    fn options_and_operands_interleave() {
        assert_eq!(
            parse(&["f", "-a"]),
            Ok(job(true, OutputError::Default, &["f"]))
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-a?"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        assert_eq!(
            parse(&["--frob"]),
            Err(TeeError::UnknownLong(String::from("--frob")))
        );
        assert_eq!(parse(&["-x"]), Err(TeeError::UnknownShort('x')));
        // `-i` is deliberately staged behind signal-disposition kernel
        // work, never stubbed: today it is an unrecognised option.
        assert_eq!(parse(&["-i"]), Err(TeeError::UnknownShort('i')));
        assert_eq!(
            parse(&["--ignore-interrupts"]),
            Err(TeeError::UnknownLong(String::from("--ignore-interrupts")))
        );
    }

    #[test]
    fn duplicate_operands_are_preserved_in_order() {
        let parsed = parse(&["log", "log"]).expect("parses");
        let Command::Tee(job) = parsed else {
            panic!("expected a tee job");
        };
        assert_eq!(job.files, vec![String::from("log"), String::from("log")]);
    }
}
