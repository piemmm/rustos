//! TAIRiX `true` — do nothing, successfully (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `true`: it ignores its arguments and exits `0`, so a
//! shell script can use it wherever a command that always succeeds is
//! needed. The single deliberate surface beyond that is the reserved
//! short-help convention (`plans/APPS.md` §4): a **first** argument of
//! `-h`, `-?`, or `--help` renders the tool's own Help document — exactly
//! the position GNU `true` honours `--help` in, so `true --help x` and
//! `true x --help` both simply succeed, as they do under coreutils.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool: [`parse`] — the [`Command`] a
//! command line names. Parsing is deliberately **infallible**: GNU `true`
//! has no usage error, and neither does this one.
//!
//! # Layering & safety
//!
//! `no_std`, no `alloc`, no dependencies. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths; nothing writes to fd 3
//! (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The usage banner the short-help switches fall back to when the tool's
/// own Help tree is unavailable.
pub const USAGE: &str = "usage: true [ignored arguments]";

/// One thing the `true` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Do nothing and exit `0`.
    Succeed,
    /// Render `true`'s own short help (a first argument of `-h`/`-?`/
    /// `--help`): the `NAME`, `SYNOPSIS`, and compact `OPTIONS` of its Help
    /// document, through the same engine as any other command's short help
    /// (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `true` surface: every argument is ignored and
/// the command succeeds, except a **first** argument of `-h` / `-?` /
/// `--help`, which renders the short help (the position GNU honours
/// `--help` in). There is no usage error: this parse cannot fail.
#[must_use]
pub fn parse(args: &[&str]) -> Command {
    match args.first() {
        Some(&"-h" | &"-?" | &"--help") => Command::Help,
        _ => Command::Succeed,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{parse, Command};

    #[test]
    fn no_arguments_succeed() {
        assert_eq!(parse(&[]), Command::Succeed);
    }

    #[test]
    fn any_arguments_are_ignored() {
        // GNU `true` ignores everything, including tokens that look like
        // (unknown or malformed) options.
        assert_eq!(parse(&["x"]), Command::Succeed);
        assert_eq!(parse(&["--bogus"]), Command::Succeed);
        assert_eq!(parse(&["-", "--", "-n"]), Command::Succeed);
    }

    #[test]
    fn first_argument_help_flags_win() {
        assert_eq!(parse(&["-h"]), Command::Help);
        assert_eq!(parse(&["-?"]), Command::Help);
        assert_eq!(parse(&["--help"]), Command::Help);
        assert_eq!(parse(&["--help", "x"]), Command::Help);
    }

    #[test]
    fn help_after_another_argument_is_ignored() {
        // The GNU position rule: `true x --help` simply succeeds.
        assert_eq!(parse(&["x", "--help"]), Command::Succeed);
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/true.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/true.md must document {switch}"
            );
        }
    }
}
