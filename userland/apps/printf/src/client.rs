//! The `printf` run loop: format reuse, diagnostics, and exit status.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_help::{own_short_help, HelpSource};

use crate::command::Command;
use crate::error::PrintfError;
use crate::template::{render_pass, Severity};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `printf`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: printf format [argument...]";

/// `printf`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "printf";

/// One of the tool's output streams. A failed write is fatal.
pub trait Output {
    /// Write all of `bytes`, or report that the stream no longer accepts
    /// output.
    ///
    /// # Errors
    ///
    /// [`PrintfError::Output`] when the stream stopped accepting bytes.
    fn write_all(&self, bytes: &[u8]) -> Result<(), PrintfError>;
}

/// Run one [`Command`], writing formatted data to `out` and diagnostics
/// to `err`. `locale` is the user's `LANG` preference, if set; `help` is
/// the tool's own `Help/` tree, read by the short-help switches.
///
/// Returns the exit status: `0`, or `1` when a conversion diagnostic was
/// reported (the GNU model — the run completes either way).
///
/// # Errors
///
/// The fatal [`PrintfError`]s: an invalid conversion specification, a
/// malformed escape, or a dead output stream. Output rendered before the
/// failure has already been written, exactly as GNU's exit flushes its
/// stream.
pub fn run(
    command: Command,
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<u8, PrintfError> {
    let (format, arguments) = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes)?;
            return Ok(0);
        }
        Command::Print { format, arguments } => (format, arguments),
    };
    let argument_slices: Vec<&str> = arguments.iter().map(String::as_str).collect();

    let mut status = 0_u8;
    let mut used = 0_usize;
    loop {
        let mut buffer = Vec::new();
        let mut diagnostics = Vec::new();
        let pass = render_pass(
            &format,
            &argument_slices[used..],
            &mut buffer,
            &mut diagnostics,
        );
        // Whatever the pass rendered stands, even before a fatal error —
        // GNU's exit flushes its stream the same way.
        out.write_all(&buffer)?;
        for diagnostic in &diagnostics {
            err.write_all(format!("printf: {}\n", diagnostic.message).as_bytes())?;
            if diagnostic.severity == Severity::Error {
                status = 1;
            }
        }
        let pass = pass?;
        used += pass.consumed;
        if pass.halted {
            break;
        }
        if pass.consumed == 0 {
            if used < argument_slices.len() {
                let first = argument_slices[used];
                err.write_all(
                    format!(
                        "printf: warning: ignoring excess arguments, starting with '{first}'\n"
                    )
                    .as_bytes(),
                )?;
            }
            break;
        }
        if used >= argument_slices.len() {
            break;
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{run, Output, USAGE};
    use crate::command::parse;
    use crate::error::PrintfError;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_help::{HelpSource, SourceError};

    /// A recording sink.
    #[derive(Default)]
    struct Sink(RefCell<Vec<u8>>);

    impl Output for Sink {
        fn write_all(&self, bytes: &[u8]) -> Result<(), PrintfError> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("test output is UTF-8")
        }
    }

    /// No bundled help: the short-help switches fall back to the banner.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }
        fn read(&self, _locale: &str, _document: &str) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    /// Parse and run, returning (stdout, stderr, status).
    fn printf(args: &[&str]) -> Result<(String, String, u8), PrintfError> {
        let command = parse(args)?;
        let out = Sink::default();
        let err = Sink::default();
        let status = run(command, None, &NoHelp, &out, &err)?;
        Ok((out.text(), err.text(), status))
    }

    /// Every expectation here is the observed behaviour of GNU coreutils
    /// `printf` for the same command line.
    #[test]
    fn the_format_is_reused_while_arguments_remain() {
        let (out, err, status) = printf(&["%s-", "a", "b", "c"]).expect("runs");
        assert_eq!(out, "a-b-c-");
        assert_eq!(err, "");
        assert_eq!(status, 0);

        let (out, _, status) = printf(&["%d %d ", "1", "2", "3"]).expect("runs");
        assert_eq!(out, "1 2 3 0 ");
        assert_eq!(status, 0);
    }

    #[test]
    fn a_directiveless_format_warns_about_excess_arguments() {
        let (out, err, status) = printf(&["x", "a", "b"]).expect("runs");
        assert_eq!(out, "x");
        assert_eq!(
            err,
            "printf: warning: ignoring excess arguments, starting with 'a'\n"
        );
        assert_eq!(status, 0, "the excess-arguments warning does not fail");
    }

    #[test]
    fn conversion_errors_set_the_status_and_continue() {
        let (out, err, status) = printf(&["%d|", "12abc"]).expect("runs");
        assert_eq!(out, "12|");
        assert_eq!(err, "printf: '12abc': value not completely converted\n");
        assert_eq!(status, 1);
    }

    #[test]
    fn character_constant_warnings_do_not_fail() {
        let (out, err, status) = printf(&["%d|", "'ABC"]).expect("runs");
        assert_eq!(out, "65|");
        assert_eq!(
            err,
            "printf: warning: BC: character(s) following character constant have been ignored\n"
        );
        assert_eq!(status, 0);
    }

    #[test]
    fn slash_c_ends_all_output_across_passes() {
        let (out, _, status) = printf(&["%b|", "x\\c y", "never"]).expect("runs");
        assert_eq!(out, "x");
        assert_eq!(status, 0);
    }

    #[test]
    fn fatal_errors_keep_the_output_already_rendered() {
        let command = parse(&["a%r", "x"]).expect("parses");
        let out = Sink::default();
        let err = Sink::default();
        let result = run(command, None, &NoHelp, &out, &err);
        assert_eq!(
            result,
            Err(PrintfError::InvalidConversion(String::from("%r")))
        );
        assert_eq!(out.text(), "a", "the literal before the error stands");
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner() {
        let (out, err, status) = printf(&["-h"]).expect("runs");
        assert_eq!(out, format!("{USAGE}\n"));
        assert_eq!(err, "");
        assert_eq!(status, 0);
    }

    #[test]
    fn missing_operand_is_fatal() {
        assert_eq!(printf(&[]).expect_err("fails"), PrintfError::MissingOperand);
    }

    /// Every locale's `OPTIONS` section documents exactly the switches
    /// this parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the
    /// bundle's own on-disk `Help/` tree — the single source the image
    /// builder plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = rustos_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/printf.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-h, -?`", "`--`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/printf.md must document {switch}"
                );
            }
        }
    }
}
