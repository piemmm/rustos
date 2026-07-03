//! The parsed shape of a `man` command line.

use alloc::string::String;

use crate::error::ManError;

/// One thing the `man` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `man`'s own short help (`-h`/`-?`): the `NAME`, `SYNOPSIS`, and
    /// compact `OPTIONS` of `man`'s own Help document, through the same
    /// engine as any other command's short help (plans/APPS.md §4).
    ShortHelp,
    /// Render one command's Help document in full.
    Page {
        /// The command word whose owning bundle holds the document (resolved
        /// through the shared store-then-`PATH` policy, plans/APPS.md §8).
        word: String,
        /// An explicit topic within that bundle's `Help/` tree; the command
        /// word itself when absent (plans/APPS.md §7).
        topic: Option<String>,
    },
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `man [-h | -?] <command> [topic]`:
///
/// * `-h` / `-?` / `--help` — render `man`'s own short help (wins
///   immediately).
/// * `--` — end option parsing; later words are operands.
/// * `<command>` — the command whose Help document to render.
/// * `[topic]` — a named topic within that command's bundle.
///
/// It **fails closed**: an unknown option or a third operand is a
/// [`ManError::Usage`] rather than a silently ignored token, and no operand
/// at all is a usage error too (there is nothing to render).
///
/// # Errors
///
/// [`ManError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, ManError> {
    let mut word: Option<&str> = None;
    let mut topic: Option<&str> = None;
    let mut options_done = false;
    for &arg in args {
        if !options_done && arg.starts_with('-') && arg.len() > 1 {
            match arg {
                "--" => options_done = true,
                "-h" | "-?" | "--help" => return Ok(Command::ShortHelp),
                _ => return Err(ManError::Usage),
            }
            continue;
        }
        if word.is_none() {
            word = Some(arg);
        } else if topic.is_none() {
            topic = Some(arg);
        } else {
            return Err(ManError::Usage);
        }
    }
    match word {
        Some(word) => Ok(Command::Page {
            word: String::from(word),
            topic: topic.map(String::from),
        }),
        None => Err(ManError::Usage),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::ManError;

    #[test]
    fn a_bare_word_names_the_command_and_its_own_document() {
        assert_eq!(
            parse(&["ps"]),
            Ok(Command::Page {
                word: "ps".into(),
                topic: None,
            })
        );
    }

    #[test]
    fn a_second_operand_names_a_topic_inside_the_bundle() {
        assert_eq!(
            parse(&["top", "keys"]),
            Ok(Command::Page {
                word: "top".into(),
                topic: Some("keys".into()),
            })
        );
    }

    #[test]
    fn the_reserved_short_help_switches_win_immediately() {
        assert_eq!(parse(&["-h"]), Ok(Command::ShortHelp));
        assert_eq!(parse(&["-?"]), Ok(Command::ShortHelp));
        assert_eq!(parse(&["--help"]), Ok(Command::ShortHelp));
        assert_eq!(parse(&["-h", "ps"]), Ok(Command::ShortHelp));
    }

    #[test]
    fn a_double_dash_ends_option_parsing() {
        // After `--` a dash-leading word is an operand, so a command whose
        // name starts with `-` is still reachable.
        assert_eq!(
            parse(&["--", "-weird"]),
            Ok(Command::Page {
                word: "-weird".into(),
                topic: None,
            })
        );
    }

    #[test]
    fn out_of_grammar_input_fails_closed_as_usage() {
        assert_eq!(parse(&[]), Err(ManError::Usage));
        assert_eq!(parse(&["--verbose", "ps"]), Err(ManError::Usage));
        assert_eq!(parse(&["-x"]), Err(ManError::Usage));
        assert_eq!(parse(&["ps", "topic", "extra"]), Err(ManError::Usage));
        assert_eq!(parse(&["--"]), Err(ManError::Usage));
    }

    #[test]
    fn a_lone_dash_is_an_operand_not_an_option() {
        // `-` conventionally names standard input; `man` has no such source,
        // so it flows through as an (invalid) command word the resolver will
        // refuse — parsing does not invent a meaning for it.
        assert_eq!(
            parse(&["-"]),
            Ok(Command::Page {
                word: "-".into(),
                topic: None,
            })
        );
    }
}
