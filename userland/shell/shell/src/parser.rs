//! The grammar: a flat [`Token`] stream becomes a [`CommandList`] tree.
//!
//! The grammar is the familiar POSIX shape, restricted to the constructs a
//! first shell needs:
//!
//! ```text
//! list      := pipeline ( (';' | '&' | '&&' | '||') pipeline )* [';' | '&']
//! pipeline  := command ( '|' command )*
//! command   := ( word | redirection )+        (at least one word)
//! redirection := ('<' | '>' | '>>') word
//! ```
//!
//! Words are still [`Segment`](crate::lexer::Segment) lists at this stage:
//! expansion is deferred to
//! [`env`](crate::env) so the parser never re-examines quoting. The parser
//! **fails closed** ([`ParseError`]): an empty command (a dangling `|`, a
//! leading separator) or a redirection with no target produces no tree, so a
//! malformed line runs nothing.

use alloc::vec::Vec;

use crate::error::ParseError;
use crate::lexer::{tokenize, Token, Word};

/// What a redirection does to a standard stream.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RedirectionKind {
    /// `< file` — connect the file to standard input.
    Input,
    /// `> file` — connect standard output to the file, truncating it.
    OutputTruncate,
    /// `>> file` — connect standard output to the file, appending.
    OutputAppend,
}

/// A single redirection: an operator and its (unexpanded) target word.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redirection {
    /// Which stream is redirected, and how.
    pub kind: RedirectionKind,
    /// The redirection target, still a [`Word`] pending expansion.
    pub target: Word,
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
/// command, or a redirection without a target).
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
                Some(Token::Less) => {
                    redirections.push(self.parse_redirection(RedirectionKind::Input)?);
                }
                Some(Token::Great) => {
                    redirections.push(self.parse_redirection(RedirectionKind::OutputTruncate)?);
                }
                Some(Token::DoubleGreat) => {
                    redirections.push(self.parse_redirection(RedirectionKind::OutputAppend)?);
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

    fn parse_redirection(&mut self, kind: RedirectionKind) -> Result<Redirection, ParseError> {
        self.pos += 1;
        match self.take() {
            Some(Token::Word(target)) => Ok(Redirection { kind, target }),
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
    use super::{parse, RedirectionKind, RunCondition};
    use crate::error::ParseError;
    use crate::lexer::Segment;
    use alloc::string::String;
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

    #[test]
    fn redirections_are_collected() {
        let list = parse("sort < in > out >> log").unwrap();
        let cmd = &list.entries[0].pipeline.commands[0];
        assert_eq!(argv(cmd), ["sort"]);
        assert_eq!(cmd.redirections.len(), 3);
        assert_eq!(cmd.redirections[0].kind, RedirectionKind::Input);
        assert_eq!(flat(&cmd.redirections[0].target), "in");
        assert_eq!(cmd.redirections[1].kind, RedirectionKind::OutputTruncate);
        assert_eq!(flat(&cmd.redirections[1].target), "out");
        assert_eq!(cmd.redirections[2].kind, RedirectionKind::OutputAppend);
        assert_eq!(flat(&cmd.redirections[2].target), "log");
    }

    #[test]
    fn redirections_interleave_with_words() {
        // A redirection may sit between argument words; both are kept.
        let list = parse("echo > out hello").unwrap();
        let cmd = &list.entries[0].pipeline.commands[0];
        assert_eq!(argv(cmd), ["echo", "hello"]);
        assert_eq!(cmd.redirections.len(), 1);
        assert_eq!(flat(&cmd.redirections[0].target), "out");
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
    fn redirection_without_target_fails_closed() {
        assert_eq!(parse("ls >"), Err(ParseError::MissingRedirectionTarget));
        assert_eq!(
            parse("ls > | wc"),
            Err(ParseError::MissingRedirectionTarget)
        );
    }
}
