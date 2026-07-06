//! The parsed shape of a `cat` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::CatError;

/// One source `cat` reads from, in command-line order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
    /// Standard input — the `-` operand, and the default when no operand is
    /// given.
    Stdin,
    /// A named file.
    Path(String),
}

/// Which lines receive a number prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Numbering {
    /// No line numbers (the default).
    None,
    /// Number every output line (`-n`).
    All,
    /// Number only non-empty output lines (`-b`; overrides `-n`, as in the
    /// GNU tool).
    NonBlank,
}

/// The output-shaping options of the GNU `cat` surface.
///
/// The flags are the GNU tool's independent boolean switches; a
/// two-variant enum per flag would only restate `bool` with extra noise,
/// so the field count mirrors the command surface deliberately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Render {
    /// Line-number policy (`-n` / `-b`).
    pub numbering: Numbering,
    /// Suppress repeated adjacent blank lines (`-s`).
    pub squeeze_blank: bool,
    /// Print `$` at the end of every line (`-E`).
    pub show_ends: bool,
    /// Print TAB characters as `^I` (`-T`).
    pub show_tabs: bool,
    /// Print other control and non-ASCII bytes in `^`/`M-` notation (`-v`).
    pub show_nonprinting: bool,
}

impl Render {
    /// The plain pass-through rendering: no numbering, no transformation.
    pub const PLAIN: Self = Self {
        numbering: Numbering::None,
        squeeze_blank: false,
        show_ends: false,
        show_tabs: false,
        show_nonprinting: false,
    };
}

/// One thing the `cat` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Concatenate `sources` to the terminal, shaped by `render`.
    Concat {
        /// The output-shaping options.
        render: Render,
        /// The sources to read, in order. Never empty: an empty command line
        /// yields a single [`Source::Stdin`].
        sources: Vec<Source>,
    },
    /// Render `cat`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `cat` surface,
/// `cat [-AbeEnstTuv] [--] [file...]`:
///
/// * `-A` / `--show-all` — equivalent to `-vET`.
/// * `-b` / `--number-nonblank` — number non-empty output lines; overrides
///   `-n`.
/// * `-e` — equivalent to `-vE`.
/// * `-E` / `--show-ends` — print `$` at the end of every line.
/// * `-n` / `--number` — number every output line.
/// * `-s` / `--squeeze-blank` — suppress repeated adjacent blank lines.
/// * `-t` — equivalent to `-vT`.
/// * `-T` / `--show-tabs` — print TAB as `^I`.
/// * `-u` — accepted and ignored (output is already unbuffered).
/// * `-v` / `--show-nonprinting` — `^`/`M-` notation for control and
///   non-ASCII bytes, except LFD and TAB.
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * `-` — standard input.
/// * any other option — a [`CatError::Usage`] error (fail closed; never a
///   silently ignored token).
///
/// Short options bundle exactly as in the GNU tool: `-nE` is `-n -E`.
/// With no path (or `-`) operand the single source is standard input.
///
/// # Errors
///
/// [`CatError::Usage`] for any unrecognised option before `--`.
pub fn parse(args: &[&str]) -> Result<Command, CatError> {
    let mut render = Render::PLAIN;
    let mut number_all = false;
    let mut number_nonblank = false;
    let mut sources = Vec::new();
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            match arg {
                "--" => {
                    options_done = true;
                    continue;
                }
                "--help" => return Ok(Command::Help),
                "--show-all" => {
                    render.show_nonprinting = true;
                    render.show_ends = true;
                    render.show_tabs = true;
                    continue;
                }
                "--number-nonblank" => {
                    number_nonblank = true;
                    continue;
                }
                "--show-ends" => {
                    render.show_ends = true;
                    continue;
                }
                "--number" => {
                    number_all = true;
                    continue;
                }
                "--squeeze-blank" => {
                    render.squeeze_blank = true;
                    continue;
                }
                "--show-tabs" => {
                    render.show_tabs = true;
                    continue;
                }
                "--show-nonprinting" => {
                    render.show_nonprinting = true;
                    continue;
                }
                // An unrecognised long option is refused, never ignored.
                _ if arg.starts_with("--") => return Err(CatError::Usage),
                // A bundle of short flags (`-nE` is `-n -E`); the bare `-`
                // stdin operand is not an option.
                _ if arg.starts_with('-') && arg != "-" => {
                    for flag in arg[1..].chars() {
                        match flag {
                            'A' => {
                                render.show_nonprinting = true;
                                render.show_ends = true;
                                render.show_tabs = true;
                            }
                            'b' => number_nonblank = true,
                            'e' => {
                                render.show_nonprinting = true;
                                render.show_ends = true;
                            }
                            'E' => render.show_ends = true,
                            'n' => number_all = true,
                            's' => render.squeeze_blank = true,
                            't' => {
                                render.show_nonprinting = true;
                                render.show_tabs = true;
                            }
                            'T' => render.show_tabs = true,
                            'u' => {}
                            'v' => render.show_nonprinting = true,
                            'h' | '?' => return Ok(Command::Help),
                            _ => return Err(CatError::Usage),
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        sources.push(if arg == "-" {
            Source::Stdin
        } else {
            Source::Path(String::from(arg))
        });
    }
    render.numbering = if number_nonblank {
        Numbering::NonBlank
    } else if number_all {
        Numbering::All
    } else {
        Numbering::None
    };
    if sources.is_empty() {
        sources.push(Source::Stdin);
    }
    Ok(Command::Concat { render, sources })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Numbering, Render, Source};
    use crate::error::CatError;
    use alloc::string::String;
    use alloc::vec;

    fn path(name: &str) -> Source {
        Source::Path(String::from(name))
    }

    fn numbered(numbering: Numbering) -> Render {
        Render {
            numbering,
            ..Render::PLAIN
        }
    }

    #[test]
    fn no_arguments_reads_stdin() {
        assert_eq!(
            parse(&[]),
            Ok(Command::Concat {
                render: Render::PLAIN,
                sources: vec![Source::Stdin],
            })
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `--help` is recognised even alongside other arguments.
        assert_eq!(parse(&["-n", "file", "--help"]), Ok(Command::Help));
        // A short-help flag wins from inside a bundle too.
        assert_eq!(parse(&["-nh"]), Ok(Command::Help));
    }

    #[test]
    fn number_flag_sets_numbering() {
        assert_eq!(
            parse(&["-n", "a.txt"]),
            Ok(Command::Concat {
                render: numbered(Numbering::All),
                sources: vec![path("a.txt")],
            })
        );
        assert_eq!(
            parse(&["--number"]),
            Ok(Command::Concat {
                render: numbered(Numbering::All),
                sources: vec![Source::Stdin],
            })
        );
    }

    #[test]
    fn number_nonblank_overrides_number() {
        // As in the GNU tool, `-b` wins over `-n` in either order.
        for args in [["-n", "-b"], ["-b", "-n"]] {
            assert_eq!(
                parse(&args),
                Ok(Command::Concat {
                    render: numbered(Numbering::NonBlank),
                    sources: vec![Source::Stdin],
                })
            );
        }
    }

    #[test]
    fn show_all_is_vet() {
        let expected = Render {
            numbering: Numbering::None,
            squeeze_blank: false,
            show_ends: true,
            show_tabs: true,
            show_nonprinting: true,
        };
        for args in [
            vec!["-A"],
            vec!["--show-all"],
            vec!["-v", "-E", "-T"],
            vec!["-vET"],
        ] {
            assert_eq!(
                parse(&args),
                Ok(Command::Concat {
                    render: expected,
                    sources: vec![Source::Stdin],
                })
            );
        }
    }

    #[test]
    fn e_and_t_imply_nonprinting() {
        assert_eq!(
            parse(&["-e"]),
            Ok(Command::Concat {
                render: Render {
                    show_ends: true,
                    show_nonprinting: true,
                    ..Render::PLAIN
                },
                sources: vec![Source::Stdin],
            })
        );
        assert_eq!(
            parse(&["-t"]),
            Ok(Command::Concat {
                render: Render {
                    show_tabs: true,
                    show_nonprinting: true,
                    ..Render::PLAIN
                },
                sources: vec![Source::Stdin],
            })
        );
    }

    #[test]
    fn squeeze_and_marker_long_options_parse() {
        assert_eq!(
            parse(&["--squeeze-blank", "--show-ends", "--show-tabs"]),
            Ok(Command::Concat {
                render: Render {
                    squeeze_blank: true,
                    show_ends: true,
                    show_tabs: true,
                    ..Render::PLAIN
                },
                sources: vec![Source::Stdin],
            })
        );
        assert_eq!(
            parse(&["--show-nonprinting", "--number-nonblank"]),
            Ok(Command::Concat {
                render: Render {
                    numbering: Numbering::NonBlank,
                    show_nonprinting: true,
                    ..Render::PLAIN
                },
                sources: vec![Source::Stdin],
            })
        );
    }

    #[test]
    fn unbuffered_flag_is_accepted_and_ignored() {
        assert_eq!(
            parse(&["-u", "a"]),
            Ok(Command::Concat {
                render: Render::PLAIN,
                sources: vec![path("a")],
            })
        );
    }

    #[test]
    fn bundled_short_flags_expand() {
        assert_eq!(
            parse(&["-bsE", "a"]),
            Ok(Command::Concat {
                render: Render {
                    numbering: Numbering::NonBlank,
                    squeeze_blank: true,
                    show_ends: true,
                    ..Render::PLAIN
                },
                sources: vec![path("a")],
            })
        );
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(Command::Concat {
                render: Render::PLAIN,
                sources: vec![path("a"), path("b"), path("c")],
            })
        );
    }

    #[test]
    fn dash_is_stdin_mixed_with_files() {
        assert_eq!(
            parse(&["a", "-", "b"]),
            Ok(Command::Concat {
                render: Render::PLAIN,
                sources: vec![path("a"), Source::Stdin, path("b")],
            })
        );
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x"]), Err(CatError::Usage));
        assert_eq!(parse(&["--frobnicate"]), Err(CatError::Usage));
        // A bundle with one bad flag is refused whole.
        assert_eq!(parse(&["-nx"]), Err(CatError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(
            parse(&["--", "-n"]),
            Ok(Command::Concat {
                render: Render::PLAIN,
                sources: vec![path("-n")],
            })
        );
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/cat.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-A, --show-all`",
                "`-b, --number-nonblank`",
                "`-e`",
                "`-E, --show-ends`",
                "`-n, --number`",
                "`-s, --squeeze-blank`",
                "`-t`",
                "`-T, --show-tabs`",
                "`-u`",
                "`-v, --show-nonprinting`",
                "`-h, -?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/cat.md must document {switch}"
                );
            }
        }
    }
}
