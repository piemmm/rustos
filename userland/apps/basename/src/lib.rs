//! RustOS `basename` — strip directory and suffix from names
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `basename`: print the final component of each path
//! spelling, optionally with a trailing suffix removed. The surgery is
//! purely **lexical** — no path is resolved, normalised, or touched on
//! disk — exactly as under POSIX.
//!
//! # Divergence from GNU `basename`
//!
//! RustOS paths may be anchored to a named alias root (`Home:/tools`,
//! `plans/DRIVES.md`) as well as to the view root `/`. An alias root is
//! treated the way POSIX treats `/`: it is never stripped into, so
//! `basename Home:/` is `Home:/`, exactly as `basename /` is `/`. Where
//! the root prefix ends is decided by the shared path grammar's own rule
//! (`rustos_path::alias_root_len`) — this tool carries no second path
//! parser. Documented in the tool's `Help/` documents.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names, with the GNU
//!   option surface (`-a`/`--multiple`, `-s`/`--suffix`, `-z`/`--zero`,
//!   the `NAME [SUFFIX]` operand form).
//! * [`basename_of`] — the POSIX base-name algorithm over one spelling.
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
/// short-help switches print when `basename`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: basename name [suffix] | basename [-az] [-s suffix] name...";

/// One thing the `basename` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Print the base name of each spelling in `names`.
    Emit {
        /// The path spellings to take the base name of, in command-line
        /// order. Never empty.
        names: Vec<String>,
        /// The suffix to strip from each base name (`-s`, or the second
        /// operand of the two-operand form).
        suffix: Option<String>,
        /// Terminate each result with NUL instead of newline (`-z`).
        zero: bool,
    },
    /// Render `basename`'s own short help (`-h`/`-?`/`--help`): the
    /// `NAME`, `SYNOPSIS`, and compact `OPTIONS` of its Help document,
    /// through the same engine as any other command's short help
    /// (plans/APPS.md §4).
    Help,
}

/// The failures [`parse`] reports, mirroring the GNU tool's diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BasenameError {
    /// An unrecognised option.
    Usage,
    /// No `name` operand was given.
    MissingOperand,
    /// A third operand in the `NAME [SUFFIX]` form; carries the operand.
    ExtraOperand(String),
    /// `-s`/`--suffix` was given without its suffix value.
    MissingSuffix,
}

impl fmt::Display for BasenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BasenameError::Usage => f.write_str("unrecognized option"),
            BasenameError::MissingOperand => f.write_str("missing operand"),
            BasenameError::ExtraOperand(operand) => write!(f, "extra operand '{operand}'"),
            BasenameError::MissingSuffix => f.write_str("option requires an argument -- 's'"),
        }
    }
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `basename` surface:
///
/// * `-a` / `--multiple` — every operand is a `name`.
/// * `-s <suffix>` / `--suffix=<suffix>` / `--suffix <suffix>` — strip a
///   trailing `suffix` from each base name; implies `-a`.
/// * `-z` / `--zero` — terminate each result with NUL instead of newline.
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * short options bundle (`-az`); a bundled `-s` takes the rest of its
///   token as the suffix (`-s.c`) or the next argument.
/// * without `-a`/`-s`, the operands are `NAME [SUFFIX]`: one or two, a
///   third is an error.
///
/// Options and operands may be interleaved (the GNU permutation).
///
/// # Errors
///
/// * [`BasenameError::Usage`] — an unrecognised option.
/// * [`BasenameError::MissingOperand`] — no `name` was given.
/// * [`BasenameError::ExtraOperand`] — a third operand without `-a`/`-s`.
/// * [`BasenameError::MissingSuffix`] — `-s` without its value.
pub fn parse(args: &[&str]) -> Result<Command, BasenameError> {
    let mut multiple = false;
    let mut zero = false;
    let mut suffix: Option<String> = None;
    let mut names: Vec<String> = Vec::new();
    let mut options_done = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if options_done {
            names.push(String::from(arg));
        } else {
            match arg {
                "--" => options_done = true,
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                "--multiple" => multiple = true,
                "--zero" => zero = true,
                "--suffix" => {
                    index += 1;
                    let Some(&value) = args.get(index) else {
                        return Err(BasenameError::MissingSuffix);
                    };
                    suffix = Some(String::from(value));
                }
                _ if arg.starts_with("--suffix=") => {
                    suffix = arg.get("--suffix=".len()..).map(String::from);
                }
                _ if arg.starts_with("--") => return Err(BasenameError::Usage),
                _ if arg.len() > 1 && arg.starts_with('-') => {
                    for (offset, flag) in arg[1..].char_indices() {
                        match flag {
                            'a' => multiple = true,
                            'z' => zero = true,
                            's' => {
                                // The rest of the token is the suffix
                                // (`-s.c`), or the next argument is.
                                let rest = &arg[1 + offset + flag.len_utf8()..];
                                if rest.is_empty() {
                                    index += 1;
                                    let Some(&value) = args.get(index) else {
                                        return Err(BasenameError::MissingSuffix);
                                    };
                                    suffix = Some(String::from(value));
                                } else {
                                    suffix = Some(String::from(rest));
                                }
                                break;
                            }
                            _ => return Err(BasenameError::Usage),
                        }
                    }
                }
                _ => names.push(String::from(arg)),
            }
        }
        index += 1;
    }

    // `-s` implies `-a`, as in the GNU tool.
    if suffix.is_some() {
        multiple = true;
    }

    if !multiple {
        match names.len() {
            0 => return Err(BasenameError::MissingOperand),
            1 => {}
            2 => {
                suffix = names.pop();
            }
            _ => return Err(BasenameError::ExtraOperand(names.swap_remove(2))),
        }
    }
    if names.is_empty() {
        return Err(BasenameError::MissingOperand);
    }
    Ok(Command::Emit {
        names,
        suffix,
        zero,
    })
}

/// The POSIX base name of one path spelling, with an optional trailing
/// `suffix` removed.
///
/// The classic algorithm — strip trailing slashes, take the text after the
/// last remaining `/`, then strip a trailing `suffix` that is neither the
/// whole result nor empty — with one RustOS extension: a `Name:/` alias
/// root (decided by the shared grammar's [`alias_root_len`]) plays the
/// role POSIX gives `/`, so `basename_of("Home:/", None)` is `Home:/`
/// exactly as `basename_of("/", None)` is `/`. The surgery is purely
/// lexical; nothing is resolved or normalised.
#[must_use]
pub fn basename_of<'a>(name: &'a str, suffix: Option<&str>) -> &'a str {
    if name.is_empty() {
        return "";
    }
    let root_len = alias_root_len(name).unwrap_or(0);
    let rest = &name[root_len..];

    let trimmed = rest.trim_end_matches('/');
    if trimmed.is_empty() {
        // Nothing but the root: the alias root itself, `/` for an
        // all-slash spelling, or the empty remainder of a bare root.
        return if root_len > 0 { &name[..root_len] } else { "/" };
    }

    let base = match trimmed.rfind('/') {
        Some(slash) => &trimmed[slash + 1..],
        None => trimmed,
    };
    match suffix {
        Some(suffix) if !suffix.is_empty() && base.len() > suffix.len() => {
            base.strip_suffix(suffix).unwrap_or(base)
        }
        _ => base,
    }
}

/// The rendered output for an [`Command::Emit`]: each name's base name
/// followed by the terminator `-z` selects. The block is bounded by the
/// argument vector that produced it.
#[must_use]
pub fn output(names: &[String], suffix: Option<&str>, zero: bool) -> Vec<u8> {
    let terminator = if zero { b'\0' } else { b'\n' };
    let mut out = Vec::new();
    for name in names {
        out.extend_from_slice(basename_of(name, suffix).as_bytes());
        out.push(terminator);
    }
    out
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::String;
    use alloc::vec;

    use super::{basename_of, output, parse, BasenameError, Command};

    fn emit(names: &[&str], suffix: Option<&str>, zero: bool) -> Command {
        Command::Emit {
            names: names.iter().map(|&s| String::from(s)).collect(),
            suffix: suffix.map(String::from),
            zero,
        }
    }

    #[test]
    fn single_operand() {
        assert_eq!(parse(&["/a/b"]), Ok(emit(&["/a/b"], None, false)));
    }

    #[test]
    fn two_operands_name_suffix_form() {
        assert_eq!(
            parse(&["src/lib.rs", ".rs"]),
            Ok(emit(&["src/lib.rs"], Some(".rs"), false))
        );
    }

    #[test]
    fn three_operands_are_an_error() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Err(BasenameError::ExtraOperand(String::from("c")))
        );
    }

    #[test]
    fn missing_operand() {
        assert_eq!(parse(&[]), Err(BasenameError::MissingOperand));
        assert_eq!(parse(&["-a"]), Err(BasenameError::MissingOperand));
        assert_eq!(parse(&["-z"]), Err(BasenameError::MissingOperand));
    }

    #[test]
    fn multiple_takes_every_operand_as_a_name() {
        assert_eq!(
            parse(&["-a", "a/x", "b/y", "c/z"]),
            Ok(emit(&["a/x", "b/y", "c/z"], None, false))
        );
        assert_eq!(
            parse(&["--multiple", "a", "b"]),
            Ok(emit(&["a", "b"], None, false))
        );
    }

    #[test]
    fn suffix_implies_multiple() {
        // With `-s`, both operands are names, not `NAME SUFFIX`.
        assert_eq!(
            parse(&["-s", ".rs", "a.rs", "b.rs"]),
            Ok(emit(&["a.rs", "b.rs"], Some(".rs"), false))
        );
        assert_eq!(
            parse(&["--suffix=.rs", "a.rs"]),
            Ok(emit(&["a.rs"], Some(".rs"), false))
        );
        assert_eq!(
            parse(&["--suffix", ".rs", "a.rs"]),
            Ok(emit(&["a.rs"], Some(".rs"), false))
        );
    }

    #[test]
    fn bundled_short_options() {
        assert_eq!(parse(&["-az", "a"]), Ok(emit(&["a"], None, true)));
        // A bundled `-s` takes the rest of its token as the suffix.
        assert_eq!(
            parse(&["-zs.c", "a.c"]),
            Ok(emit(&["a.c"], Some(".c"), true))
        );
    }

    #[test]
    fn suffix_value_may_be_missing() {
        assert_eq!(parse(&["-s"]), Err(BasenameError::MissingSuffix));
        assert_eq!(parse(&["a", "-s"]), Err(BasenameError::MissingSuffix));
    }

    #[test]
    fn options_and_operands_interleave() {
        // The GNU permutation: options are recognised after operands too.
        assert_eq!(parse(&["a", "-z"]), Ok(emit(&["a"], None, true)));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse(&["--", "-a"]), Ok(emit(&["-a"], None, false)));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "a"]), Err(BasenameError::Usage));
        assert_eq!(parse(&["--frob", "a"]), Err(BasenameError::Usage));
    }

    #[test]
    fn posix_base_names() {
        assert_eq!(basename_of("/usr/lib", None), "lib");
        assert_eq!(basename_of("dir/file.txt", None), "file.txt");
        assert_eq!(basename_of("file", None), "file");
        // Trailing slashes are stripped first.
        assert_eq!(basename_of("a/b//", None), "b");
        // An all-slash spelling is the root.
        assert_eq!(basename_of("/", None), "/");
        assert_eq!(basename_of("///", None), "/");
        // The empty spelling stays empty.
        assert_eq!(basename_of("", None), "");
    }

    #[test]
    fn suffix_stripping() {
        assert_eq!(basename_of("src/lib.rs", Some(".rs")), "lib");
        // A base that *is* the suffix keeps it.
        assert_eq!(basename_of("dir/.rs", Some(".rs")), ".rs");
        // A base that does not end with the suffix is unchanged.
        assert_eq!(basename_of("dir/lib.rs", Some(".c")), "lib.rs");
        // An empty suffix strips nothing.
        assert_eq!(basename_of("dir/lib.rs", Some("")), "lib.rs");
    }

    #[test]
    fn alias_roots_behave_like_the_view_root() {
        // The RustOS extension: `Name:/` plays the role of `/`.
        assert_eq!(basename_of("Home:/tools/x", None), "x");
        assert_eq!(basename_of("Home:/tools/", None), "tools");
        assert_eq!(basename_of("Home:/", None), "Home:/");
        assert_eq!(basename_of("Home://", None), "Home:/");
        // A resource reference is not an alias root: plain lexical text.
        assert_eq!(basename_of("sys:random", None), "sys:random");
        // A `:` after a `/` is an ordinary character.
        assert_eq!(basename_of("a/b:c", None), "b:c");
    }

    #[test]
    fn output_renders_each_name_with_its_terminator() {
        let names = vec![String::from("/a/x"), String::from("b/y.c")];
        assert_eq!(output(&names, Some(".c"), false), b"x\ny\n");
        assert_eq!(output(&names, None, true), b"x\0y.c\0");
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
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/basename.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-a, --multiple`",
                "`-s, --suffix <suffix>`",
                "`-z, --zero`",
                "`-h, -?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/basename.md must document {switch}"
                );
            }
        }
    }
}
