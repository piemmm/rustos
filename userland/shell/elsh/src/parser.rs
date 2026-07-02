//! The grammar: a flat [`Token`] stream becomes a [`CommandList`] tree.
//!
//! The grammar is the familiar POSIX shape, restricted to the constructs a
//! first shell needs:
//!
//! ```text
//! list        := pipeline ( (';' | '&' | '&&' | '||') pipeline )* [';' | '&']
//! pipeline    := command ( '|' command )*
//! command     := ( word | redirection )+        (at least one word)
//! redirection := <a redirection operator> [word]
//! ```
//!
//! A redirection operator is already fully decoded by the
//! [`lexer`](crate::lexer) into a [`RedirOp`]: it carries the descriptor it
//! acts on, whether it opens/appends/duplicates/closes, and (for the combined
//! `&>` forms) that it targets both standard output and standard error. The
//! parser only attaches the target [`Word`] the file-opening forms need; the
//! descriptor-duplication (`n>&m`) and close (`n>&-`) forms take no target.
//!
//! Words are still [`Segment`](crate::lexer::Segment) lists at this stage:
//! expansion is deferred to
//! [`env`](crate::env) so the parser never re-examines quoting. The parser
//! **fails closed** ([`ParseError`]): an empty command (a dangling `|`, a
//! leading separator) or a file redirection with no target produces no tree,
//! so a malformed line runs nothing.

use alloc::vec::Vec;

use crate::error::ParseError;
use crate::lexer::{tokenize, RedirOp, Token, Word};

/// How a file-opening redirection opens its target.
///
/// The clobber flag rides on the two writing modes because it only means
/// anything there: it records that a clobber-override operator (`>|`, `>!`,
/// `>>|`, `>>!`) was used, so the open must truncate/create even when the
/// shell's `noclobber` option is set. Reading modes carry no such flag.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OpenMode {
    /// `<` — open the target for reading.
    Read,
    /// `<>` — open the target for reading and writing.
    ReadWrite,
    /// `>` / `>|` / `>!` — open for writing, truncating an existing file.
    Write {
        /// `true` for the clobber-override spellings (`>|`, `>!`).
        clobber: bool,
    },
    /// `>>` / `>>|` / `>>!` — open for writing, appending.
    Append {
        /// `true` for the clobber-override spellings (`>>|`, `>>!`).
        clobber: bool,
    },
}

/// A single redirection with its (still unexpanded) target attached.
///
/// Modelled as an enum so illegal states are unrepresentable: a file open
/// always carries a target [`Word`], while a duplication or close never does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Redirection {
    /// Open `target` on descriptor `fd` with `mode`.
    File {
        /// The descriptor the open binds (the operator's explicit or default fd).
        fd: u32,
        /// How the target is opened.
        mode: OpenMode,
        /// The redirection target, still a [`Word`] pending expansion.
        target: Word,
    },
    /// Redirect *both* standard output (fd 1) and standard error (fd 2) to one
    /// `target` — the `&>` / `>&` (file) family.
    Combined {
        /// `true` for the append spellings (`&>>`, `>>&`).
        append: bool,
        /// `true` for the clobber-override spellings (`&>|`, `&>!`).
        clobber: bool,
        /// The shared target, still a [`Word`] pending expansion.
        target: Word,
    },
    /// Make `fd` a duplicate of the already-open descriptor `source`
    /// (`n>&m`, `n<&m`, `2>&1`).
    Dup {
        /// The descriptor being (re)bound.
        fd: u32,
        /// The descriptor it is made to alias.
        source: u32,
    },
    /// Close `fd` (`n>&-`, `<&-`).
    Close {
        /// The descriptor to close.
        fd: u32,
    },
}

/// One simple command: its argument words and its redirections, in source
/// order. `words` is guaranteed non-empty by the parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    /// The command name (`words[0]`) and its arguments, pending expansion.
    pub words: Vec<Word>,
    /// Redirections applied to the command.
    pub redirections: Vec<Redirection>,
}

/// One or more commands joined by `|`. `commands` is guaranteed non-empty.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    /// The commands of the pipeline, left to right.
    pub commands: Vec<Command>,
}

/// Whether an entry runs, given the previous entry's exit status.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RunCondition {
    /// Run unconditionally — the first entry, or one after `;` or `&`.
    Always,
    /// Run only if the previous pipeline succeeded — after `&&`.
    OnSuccess,
    /// Run only if the previous pipeline failed — after `||`.
    OnFailure,
}

/// One pipeline in a list, with how it relates to its predecessor and
/// whether it is launched in the background.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListEntry {
    /// The pipeline to run.
    pub pipeline: Pipeline,
    /// Whether to run it, given the previous entry's outcome.
    pub run_if: RunCondition,
    /// `true` if the pipeline's own terminator was `&` (run detached).
    pub background: bool,
}

/// A whole parsed command line: a sequence of [`ListEntry`]s.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandList {
    /// The entries, in source order.
    pub entries: Vec<ListEntry>,
}

impl CommandList {
    /// `true` if the line held no commands (blank line or comment-only).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parse a whole line of shell text into a [`CommandList`].
///
/// # Errors
///
/// Returns a [`ParseError`] for any lexical fault (see
/// [`tokenize`]) or grammatical fault (an empty
/// command, or a file redirection without a target).
pub fn parse(line: &str) -> Result<CommandList, ParseError> {
    let tokens = tokenize(line)?;
    Parser { tokens, pos: 0 }.parse_list()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn parse_list(&mut self) -> Result<CommandList, ParseError> {
        let mut entries = Vec::new();
        if self.peek().is_none() {
            return Ok(CommandList { entries });
        }
        let mut run_if = RunCondition::Always;
        loop {
            let pipeline = self.parse_pipeline()?;
            let (background, next_run_if, more) = self.parse_separator();
            entries.push(ListEntry {
                pipeline,
                run_if,
                background,
            });
            if !more {
                return Ok(CommandList { entries });
            }
            run_if = next_run_if;
        }
    }

    /// Consume the separator after a pipeline. Returns
    /// `(this_is_background, next_entry_run_condition, another_pipeline_follows)`.
    fn parse_separator(&mut self) -> (bool, RunCondition, bool) {
        let token = self.peek().cloned();
        match token {
            Some(Token::AndIf) => {
                self.pos += 1;
                (false, RunCondition::OnSuccess, true)
            }
            Some(Token::OrIf) => {
                self.pos += 1;
                (false, RunCondition::OnFailure, true)
            }
            Some(Token::Semicolon) => {
                self.pos += 1;
                (false, RunCondition::Always, self.peek().is_some())
            }
            Some(Token::Ampersand) => {
                self.pos += 1;
                (true, RunCondition::Always, self.peek().is_some())
            }
            _ => (false, RunCondition::Always, false),
        }
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut commands = Vec::new();
        commands.push(self.parse_command()?);
        while matches!(self.peek(), Some(Token::Pipe)) {
            self.pos += 1;
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands })
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        let mut words = Vec::new();
        let mut redirections = Vec::new();
        loop {
            match self.peek() {
                Some(Token::Word(_)) => {
                    let Some(Token::Word(word)) = self.take() else {
                        unreachable!("peeked a word")
                    };
                    words.push(word);
                }
                Some(Token::Redirect(_)) => {
                    let Some(Token::Redirect(op)) = self.take() else {
                        unreachable!("peeked a redirection")
                    };
                    redirections.push(self.attach_target(op)?);
                }
                _ => break,
            }
        }
        if words.is_empty() {
            return Err(ParseError::MissingCommand);
        }
        Ok(Command {
            words,
            redirections,
        })
    }

    /// Turn a lexer [`RedirOp`] into a [`Redirection`], attaching the target
    /// [`Word`] the file-opening forms need and leaving the duplication/close
    /// forms target-less.
    fn attach_target(&mut self, op: RedirOp) -> Result<Redirection, ParseError> {
        match op {
            RedirOp::File { fd, mode } => Ok(Redirection::File {
                fd,
                mode,
                target: self.take_target()?,
            }),
            RedirOp::Combined { append, clobber } => Ok(Redirection::Combined {
                append,
                clobber,
                target: self.take_target()?,
            }),
            RedirOp::Dup { fd, source } => Ok(Redirection::Dup { fd, source }),
            RedirOp::Close { fd } => Ok(Redirection::Close { fd }),
        }
    }

    fn take_target(&mut self) -> Result<Word, ParseError> {
        match self.take() {
            Some(Token::Word(target)) => Ok(target),
            _ => Err(ParseError::MissingRedirectionTarget),
        }
    }

    fn take(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, OpenMode, Redirection, RunCondition};
    use crate::error::ParseError;
    use crate::lexer::Segment;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Flatten a word's segments back to a plain string (tests only — the
    /// real path expands through `env`).
    fn flat(word: &[Segment]) -> String {
        let mut out = String::new();
        for seg in word {
            match seg {
                Segment::Literal(s) | Segment::Expandable(s) => out.push_str(s),
            }
        }
        out
    }

    fn argv(cmd: &super::Command) -> Vec<String> {
        cmd.words.iter().map(|w| flat(w)).collect()
    }

    #[test]
    fn empty_line_is_empty_list() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("   # just a comment").unwrap().is_empty());
    }

    #[test]
    fn single_command_with_args() {
        let list = parse("ls -l /Apps").unwrap();
        assert_eq!(list.entries.len(), 1);
        let entry = &list.entries[0];
        assert_eq!(entry.run_if, RunCondition::Always);
        assert!(!entry.background);
        assert_eq!(entry.pipeline.commands.len(), 1);
        assert_eq!(argv(&entry.pipeline.commands[0]), ["ls", "-l", "/Apps"]);
    }

    #[test]
    fn pipeline_chains_commands() {
        let list = parse("cat f | grep x | wc -l").unwrap();
        let cmds = &list.entries[0].pipeline.commands;
        assert_eq!(cmds.len(), 3);
        assert_eq!(argv(&cmds[0]), ["cat", "f"]);
        assert_eq!(argv(&cmds[2]), ["wc", "-l"]);
    }

    /// The single redirection of a one-command line (tests only).
    fn only_redirection(line: &str) -> Redirection {
        let list = parse(line).unwrap();
        let cmd = &list.entries[0].pipeline.commands[0];
        assert_eq!(cmd.redirections.len(), 1, "expected one redirection");
        cmd.redirections[0].clone()
    }

    #[test]
    fn plain_file_redirections_default_their_fds() {
        let list = parse("sort < in > out >> log").unwrap();
        let cmd = &list.entries[0].pipeline.commands[0];
        assert_eq!(argv(cmd), ["sort"]);
        assert_eq!(
            cmd.redirections,
            [
                Redirection::File {
                    fd: 0,
                    mode: OpenMode::Read,
                    target: vec![Segment::Expandable("in".into())],
                },
                Redirection::File {
                    fd: 1,
                    mode: OpenMode::Write { clobber: false },
                    target: vec![Segment::Expandable("out".into())],
                },
                Redirection::File {
                    fd: 1,
                    mode: OpenMode::Append { clobber: false },
                    target: vec![Segment::Expandable("log".into())],
                },
            ]
        );
    }

    #[test]
    fn numbered_fd_prefix_binds_the_named_descriptor() {
        assert_eq!(
            only_redirection("cmd 2>errors"),
            Redirection::File {
                fd: 2,
                mode: OpenMode::Write { clobber: false },
                target: vec![Segment::Expandable("errors".into())],
            }
        );
        assert_eq!(
            only_redirection("cmd 3>>info.jsonl"),
            Redirection::File {
                fd: 3,
                mode: OpenMode::Append { clobber: false },
                target: vec![Segment::Expandable("info.jsonl".into())],
            }
        );
    }

    #[test]
    fn a_leading_digit_word_is_not_an_fd_prefix() {
        // `2` is a plain argument here — an IO number binds only when glued to
        // a `<`/`>`. `>bar` still defaults to fd 1.
        let list = parse("echo 2 >bar").unwrap();
        let cmd = &list.entries[0].pipeline.commands[0];
        assert_eq!(argv(cmd), ["echo", "2"]);
        assert_eq!(
            cmd.redirections,
            [Redirection::File {
                fd: 1,
                mode: OpenMode::Write { clobber: false },
                target: vec![Segment::Expandable("bar".into())],
            }]
        );
    }

    #[test]
    fn clobber_override_operators_set_the_flag() {
        assert_eq!(
            only_redirection("cmd >|out"),
            Redirection::File {
                fd: 1,
                mode: OpenMode::Write { clobber: true },
                target: vec![Segment::Expandable("out".into())],
            }
        );
        assert_eq!(
            only_redirection("cmd >!out"),
            Redirection::File {
                fd: 1,
                mode: OpenMode::Write { clobber: true },
                target: vec![Segment::Expandable("out".into())],
            }
        );
        assert!(matches!(
            only_redirection("cmd >>|log"),
            Redirection::File {
                mode: OpenMode::Append { clobber: true },
                ..
            }
        ));
        assert!(matches!(
            only_redirection("cmd >>!log"),
            Redirection::File {
                mode: OpenMode::Append { clobber: true },
                ..
            }
        ));
    }

    #[test]
    fn read_write_redirection() {
        assert_eq!(
            only_redirection("cmd <>file"),
            Redirection::File {
                fd: 0,
                mode: OpenMode::ReadWrite,
                target: vec![Segment::Expandable("file".into())],
            }
        );
    }

    #[test]
    fn duplication_and_close() {
        assert_eq!(
            only_redirection("cmd 2>&1"),
            Redirection::Dup { fd: 2, source: 1 }
        );
        assert_eq!(
            only_redirection("cmd 1>&2"),
            Redirection::Dup { fd: 1, source: 2 }
        );
        assert_eq!(
            only_redirection("cmd 0<&3"),
            Redirection::Dup { fd: 0, source: 3 }
        );
        assert_eq!(only_redirection("cmd 3>&-"), Redirection::Close { fd: 3 });
        // `>&-` defaults to closing stdout, `<&-` to stdin.
        assert_eq!(only_redirection("cmd >&-"), Redirection::Close { fd: 1 });
        assert_eq!(only_redirection("cmd <&-"), Redirection::Close { fd: 0 });
    }

    #[test]
    fn combined_stdout_stderr_forms() {
        for line in ["cmd &>both", "cmd >&both"] {
            assert_eq!(
                only_redirection(line),
                Redirection::Combined {
                    append: false,
                    clobber: false,
                    target: vec![Segment::Expandable("both".into())],
                },
                "line: {line}"
            );
        }
        for line in ["cmd &>>both", "cmd >>&both"] {
            assert_eq!(
                only_redirection(line),
                Redirection::Combined {
                    append: true,
                    clobber: false,
                    target: vec![Segment::Expandable("both".into())],
                },
                "line: {line}"
            );
        }
        for line in ["cmd &>|both", "cmd &>!both"] {
            assert_eq!(
                only_redirection(line),
                Redirection::Combined {
                    append: false,
                    clobber: true,
                    target: vec![Segment::Expandable("both".into())],
                },
                "line: {line}"
            );
        }
    }

    #[test]
    fn redirections_interleave_with_words() {
        // A redirection may sit between argument words; both are kept.
        let list = parse("echo > out hello").unwrap();
        let cmd = &list.entries[0].pipeline.commands[0];
        assert_eq!(argv(cmd), ["echo", "hello"]);
        assert_eq!(cmd.redirections.len(), 1);
    }

    #[test]
    fn connectors_set_run_conditions_and_background() {
        let list = parse("a & b ; c && d || e").unwrap();
        let conds: Vec<_> = list.entries.iter().map(|e| e.run_if).collect();
        assert_eq!(
            conds,
            [
                RunCondition::Always,    // a
                RunCondition::Always,    // b (after &)
                RunCondition::Always,    // c (after ;)
                RunCondition::OnSuccess, // d (after &&)
                RunCondition::OnFailure, // e (after ||)
            ]
        );
        assert!(list.entries[0].background); // a &
        assert!(!list.entries[1].background);
    }

    #[test]
    fn trailing_separators_do_not_add_empty_entries() {
        assert_eq!(parse("a ;").unwrap().entries.len(), 1);
        assert_eq!(parse("a &").unwrap().entries.len(), 1);
    }

    #[test]
    fn empty_command_fails_closed() {
        assert_eq!(parse("| a"), Err(ParseError::MissingCommand));
        assert_eq!(parse("a |"), Err(ParseError::MissingCommand));
        assert_eq!(parse("; a"), Err(ParseError::MissingCommand));
        assert_eq!(parse("a && "), Err(ParseError::MissingCommand));
    }

    #[test]
    fn file_redirection_without_target_fails_closed() {
        assert_eq!(parse("ls >"), Err(ParseError::MissingRedirectionTarget));
        assert_eq!(
            parse("ls > | wc"),
            Err(ParseError::MissingRedirectionTarget)
        );
        assert_eq!(parse("ls &>"), Err(ParseError::MissingRedirectionTarget));
    }

    #[test]
    fn here_documents_and_strings_fail_closed_as_unsupported() {
        assert_eq!(parse("cmd <<EOF"), Err(ParseError::UnsupportedRedirection));
        assert_eq!(parse("cmd <<-EOF"), Err(ParseError::UnsupportedRedirection));
        assert_eq!(
            parse("cmd <<<word"),
            Err(ParseError::UnsupportedRedirection)
        );
    }

    #[test]
    fn ambiguous_duplications_fail_closed() {
        // `<&` with neither a source fd nor `-`, and a numbered dup-to-file.
        assert_eq!(parse("cmd <&file"), Err(ParseError::AmbiguousRedirection));
        assert_eq!(parse("cmd 2>&file"), Err(ParseError::AmbiguousRedirection));
    }
}
