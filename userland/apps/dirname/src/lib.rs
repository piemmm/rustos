//! RustOS `dirname` — strip the last component from names
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `dirname`: print each path spelling with its last
//! component removed. The surgery is purely **lexical** — no path is
//! resolved, normalised, or touched on disk — exactly as under POSIX.
//!
//! # Divergence from GNU `dirname`
//!
//! RustOS paths may be anchored to a named alias root (`Home:/tools`,
//! `plans/DRIVES.md`) as well as to the view root `/`. An alias root is
//! treated the way POSIX treats `/`: it is never stripped into, so
//! `dirname Home:/tools` is `Home:/` exactly as `dirname /tools` is `/`.
//! Where the root prefix ends is decided by the shared path grammar's own
//! rule (`rustos_path::alias_root_len`) — this tool carries no second
//! path parser. Documented in the tool's `Help/` documents.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names, with the GNU
//!   option surface (`-z`/`--zero`, `NAME...`).
//! * [`dirname_of`] — the POSIX directory-name algorithm over one
//!   spelling.
//! * [`output`] — the rendered result block for a parsed command.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — `lib/path` and
//! the `lib/help` engine used by the `Run` binary — never a kernel or
//! driver crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths; nothing writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use rustos_path::alias_root_len;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `dirname`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: dirname [-z] name...";

/// One thing the `dirname` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Print the directory name of each spelling in `names`.
    Emit {
        /// The path spellings to take the directory name of, in
        /// command-line order. Never empty.
        names: Vec<String>,
        /// Terminate each result with NUL instead of newline (`-z`).
        zero: bool,
    },
    /// Render `dirname`'s own short help (`-h`/`-?`/`--help`): the
    /// `NAME`, `SYNOPSIS`, and compact `OPTIONS` of its Help document,
    /// through the same engine as any other command's short help
    /// (plans/APPS.md §4).
    Help,
}

/// The failures [`parse`] reports, mirroring the GNU tool's diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirnameError {
    /// An unrecognised option.
    Usage,
    /// No `name` operand was given.
    MissingOperand,
}

impl fmt::Display for DirnameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirnameError::Usage => f.write_str("unrecognized option"),
            DirnameError::MissingOperand => f.write_str("missing operand"),
        }
    }
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `dirname` surface, `dirname [-z] name...`:
///
/// * `-z` / `--zero` — terminate each result with NUL instead of newline.
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * short options bundle (`-zz` is `-z -z`); a lone `-` is an operand.
/// * at least one `name` operand is required; any number is accepted.
///
/// Options and operands may be interleaved (the GNU permutation).
///
/// # Errors
///
/// * [`DirnameError::Usage`] — an unrecognised option.
/// * [`DirnameError::MissingOperand`] — no `name` was given.
pub fn parse(args: &[&str]) -> Result<Command, DirnameError> {
    let mut zero = false;
    let mut names: Vec<String> = Vec::new();
    let mut options_done = false;

    for &arg in args {
        if options_done {
            names.push(String::from(arg));
            continue;
        }
        match arg {
            "--" => options_done = true,
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "--zero" => zero = true,
            _ if arg.starts_with("--") => return Err(DirnameError::Usage),
            _ if arg.len() > 1 && arg.starts_with('-') => {
                for flag in arg[1..].chars() {
                    match flag {
                        'z' => zero = true,
                        _ => return Err(DirnameError::Usage),
                    }
                }
            }
            _ => names.push(String::from(arg)),
        }
    }

    if names.is_empty() {
        return Err(DirnameError::MissingOperand);
    }
    Ok(Command::Emit { names, zero })
}

/// The POSIX directory name of one path spelling.
///
/// The classic algorithm — strip trailing slashes, remove the last
/// component, strip the slashes before it; a spelling with no remaining
/// `/` has the parent `.`, and a parent that empties out is the root —
/// with one RustOS extension: a `Name:/` alias root (decided by the
/// shared grammar's [`alias_root_len`]) plays the role POSIX gives `/`,
/// so `dirname_of("Home:/tools")` is `Home:/` exactly as
/// `dirname_of("/tools")` is `/`. The surgery is purely lexical; nothing
/// is resolved or normalised.
#[must_use]
pub fn dirname_of(name: &str) -> &str {
    let root_len = alias_root_len(name).unwrap_or(0);
    let rest = &name[root_len..];

    let trimmed = rest.trim_end_matches('/');
    if trimmed.is_empty() {
        // Only a root (or nothing) remains: the alias root itself, `/`
        // for an all-slash spelling, `.` for the empty one.
        if root_len > 0 {
            return &name[..root_len];
        }
        return if rest.is_empty() { "." } else { "/" };
    }

    let Some(slash) = trimmed.rfind('/') else {
        // A single component: its parent is the root, or `.` for a
        // relative spelling.
        return if root_len > 0 { &name[..root_len] } else { "." };
    };
    let parent = trimmed[..slash].trim_end_matches('/');
    if parent.is_empty() {
        // The slashes before the last component reach the root.
        return if root_len > 0 {
            &name[..root_len]
        } else {
            &name[..1]
        };
    }
    &name[..root_len + parent.len()]
}

/// The rendered output for a [`Command::Emit`]: each name's directory
/// name followed by the terminator `-z` selects. The block is bounded by
/// the argument vector that produced it.
#[must_use]
pub fn output(names: &[String], zero: bool) -> Vec<u8> {
    let terminator = if zero { b'\0' } else { b'\n' };
    let mut out = Vec::new();
    for name in names {
        out.extend_from_slice(dirname_of(name).as_bytes());
        out.push(terminator);
    }
    out
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::String;
    use alloc::vec;

    use super::{dirname_of, output, parse, Command, DirnameError};

    fn emit(names: &[&str], zero: bool) -> Command {
        Command::Emit {
            names: names.iter().map(|&s| String::from(s)).collect(),
            zero,
        }
    }

    #[test]
    fn operands_are_collected() {
        assert_eq!(parse(&["/a/b"]), Ok(emit(&["/a/b"], false)));
        assert_eq!(parse(&["a", "b", "c"]), Ok(emit(&["a", "b", "c"], false)));
        // A lone `-` is an operand.
        assert_eq!(parse(&["-"]), Ok(emit(&["-"], false)));
    }

    #[test]
    fn missing_operand() {
        assert_eq!(parse(&[]), Err(DirnameError::MissingOperand));
        assert_eq!(parse(&["-z"]), Err(DirnameError::MissingOperand));
    }

    #[test]
    fn zero_terminates_with_nul() {
        assert_eq!(parse(&["-z", "a"]), Ok(emit(&["a"], true)));
        assert_eq!(parse(&["--zero", "a"]), Ok(emit(&["a"], true)));
        // The GNU permutation: options are recognised after operands too.
        assert_eq!(parse(&["a", "-z"]), Ok(emit(&["a"], true)));
        // A bundle of known flags is accepted.
        assert_eq!(parse(&["-zz", "a"]), Ok(emit(&["a"], true)));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse(&["--", "-z"]), Ok(emit(&["-z"], false)));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "a"]), Err(DirnameError::Usage));
        assert_eq!(parse(&["--frob", "a"]), Err(DirnameError::Usage));
        // A bundle with one bad flag is refused whole.
        assert_eq!(parse(&["-zx", "a"]), Err(DirnameError::Usage));
    }

    #[test]
    fn posix_directory_names() {
        assert_eq!(dirname_of("/usr/lib"), "/usr");
        assert_eq!(dirname_of("dir/file.txt"), "dir");
        // No remaining slash: the parent is `.`.
        assert_eq!(dirname_of("file"), ".");
        assert_eq!(dirname_of("dir/"), ".");
        // The parent empties out to the root.
        assert_eq!(dirname_of("/file"), "/");
        assert_eq!(dirname_of("/"), "/");
        assert_eq!(dirname_of("///"), "/");
        assert_eq!(dirname_of("//a"), "/");
        // Slash runs collapse while trimming.
        assert_eq!(dirname_of("a//b"), "a");
        assert_eq!(dirname_of("a/b//"), "a");
        // The empty spelling has the parent `.`.
        assert_eq!(dirname_of(""), ".");
    }

    #[test]
    fn alias_roots_behave_like_the_view_root() {
        // The RustOS extension: `Name:/` plays the role of `/`.
        assert_eq!(dirname_of("Home:/tools/x"), "Home:/tools");
        assert_eq!(dirname_of("Home:/tools"), "Home:/");
        assert_eq!(dirname_of("Home:/tools/"), "Home:/");
        assert_eq!(dirname_of("Home:/"), "Home:/");
        assert_eq!(dirname_of("Home://a"), "Home:/");
        // A resource reference is not an alias root: plain lexical text.
        assert_eq!(dirname_of("sys:random"), ".");
        // A `:` after a `/` is an ordinary character.
        assert_eq!(dirname_of("a/b:c"), "a");
    }

    #[test]
    fn output_renders_each_name_with_its_terminator() {
        let names = vec![String::from("/a/x"), String::from("b/y")];
        assert_eq!(output(&names, false), b"/a\nb\n");
        assert_eq!(output(&names, true), b"/a\0b\0");
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = rustos_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/dirname.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-z, --zero`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/dirname.md must document {switch}"
                );
            }
        }
    }
}
