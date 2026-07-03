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
//! Words are still [`Segment`] lists at this stage:
//! expansion is deferred to
//! [`env`](crate::env) so the parser never re-examines quoting. The parser
//! **fails closed** ([`ParseError`]): an empty command (a dangling `|`, a
//! leading separator) or a file redirection with no target produces no tree,
//! so a malformed line runs nothing.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::ParseError;
use crate::lexer::{tokenize, FdSpec, RedirOp, Segment, Token, Word};

/// Maximum accumulated size of one here-document body, in bytes.
///
/// A fixed security bound, not a growable capacity: a here-document is
/// untrusted input collected line by line, and without a cap a hostile or
/// runaway input stream could drive unbounded heap growth. A body that would
/// exceed the bound is discarded and the line fails closed with
/// [`ParseError::HereDocTooLarge`] — collection still runs to the terminator
/// so the remaining body lines are never misread as commands.
pub const MAX_HERE_DOC_BYTES: usize = 65_536;

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
        fd: FdSpec,
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
        fd: FdSpec,
        /// The descriptor it is made to alias.
        source: u32,
    },
    /// Close `fd` (`n>&-`, `<&-`).
    Close {
        /// The descriptor to close.
        fd: FdSpec,
    },
    /// Feed a here-string (`<<< word`) as the input of `fd` (default 0). The
    /// `content` word supplies the here-string body, still pending expansion;
    /// the interpreter appends the trailing newline.
    HereString {
        /// The descriptor the here-string feeds.
        fd: FdSpec,
        /// The here-string body, still a [`Word`] pending expansion.
        content: Word,
    },
    /// Feed a multi-line here-document (`<< delim`, `<<- delim`) as the input
    /// of its descriptor. The body is not part of the command line: it is
    /// collected afterwards through
    /// [`CommandList::feed_here_doc_line`] until the delimiter line, and a
    /// list whose here-documents are not all complete runs nothing.
    HereDoc(HereDoc),
}

/// A multi-line here-document: its delimiter, collection state, and body.
///
/// The parser creates it *pending*: the command line only names the delimiter,
/// so the body must be collected from the following input lines (in source
/// order when a line has several here-documents) before the list can run.
/// Collection is bounded by [`MAX_HERE_DOC_BYTES`] and fails closed: an
/// over-large body is discarded (but still collected to its terminator, so
/// the remaining body lines are never misread as commands), and an
/// uncollected body aborts the line rather than running with empty input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HereDoc {
    fd: FdSpec,
    strip_tabs: bool,
    delimiter: String,
    quoted: bool,
    body: BodyState,
}

/// The collection state of a here-document body — a small state machine, so
/// an impossible combination (a body both complete and still filling) is
/// unrepresentable.
#[derive(Clone, Debug, Eq, PartialEq)]
enum BodyState {
    /// Still collecting; holds the body accumulated so far.
    Filling(String),
    /// Still collecting, but the body was discarded (it grew past
    /// [`MAX_HERE_DOC_BYTES`] or lost an input line); only the terminator is
    /// still looked for, so later lines are never misread as commands.
    FillingDiscarded,
    /// The terminator was consumed; holds the valid body.
    Complete(String),
    /// The terminator was consumed, but the body had been discarded.
    CompleteDiscarded,
}

impl HereDoc {
    /// Build a pending here-document from its operator and delimiter word.
    ///
    /// The delimiter undergoes quote removal but never expansion; the body is
    /// later expanded only when no part of the delimiter was quoted, exactly
    /// as POSIX specifies. An empty delimiter can only be written quoted
    /// (`<<""`), so an empty word counts as quoted.
    fn new(fd: FdSpec, strip_tabs: bool, delimiter_word: &Word) -> Self {
        let quoted = delimiter_word.is_empty()
            || delimiter_word.iter().any(|segment| {
                matches!(segment, Segment::Literal(_) | Segment::QuotedExpandable(_))
            });
        let mut delimiter = String::new();
        for segment in delimiter_word {
            match segment {
                Segment::Literal(s) | Segment::Expandable(s) | Segment::QuotedExpandable(s) => {
                    delimiter.push_str(s);
                }
            }
        }
        Self {
            fd,
            strip_tabs,
            delimiter,
            quoted,
            body: BodyState::Filling(String::new()),
        }
    }

    /// The descriptor the here-document feeds.
    #[must_use]
    pub fn fd(&self) -> &FdSpec {
        &self.fd
    }

    /// The delimiter that terminates the body (after quote removal).
    #[must_use]
    pub fn delimiter(&self) -> &str {
        &self.delimiter
    }

    /// `true` once the terminating delimiter line has been consumed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(
            self.body,
            BodyState::Complete(_) | BodyState::CompleteDiscarded
        )
    }

    /// `true` when any part of the delimiter was quoted, which makes the body
    /// literal (no `$` expansion).
    #[must_use]
    pub fn is_quoted(&self) -> bool {
        self.quoted
    }

    /// The collected body (each line newline-terminated), pending expansion.
    ///
    /// # Errors
    ///
    /// Fails closed with [`ParseError::UnterminatedHereDoc`] when the
    /// terminator was never reached and [`ParseError::HereDocTooLarge`] when
    /// the body exceeded [`MAX_HERE_DOC_BYTES`], so an incomplete or discarded
    /// body can never run as empty input.
    pub fn body(&self) -> Result<&str, ParseError> {
        match &self.body {
            BodyState::Complete(body) => Ok(body),
            BodyState::CompleteDiscarded => Err(ParseError::HereDocTooLarge),
            BodyState::Filling(_) | BodyState::FillingDiscarded => {
                Err(ParseError::UnterminatedHereDoc)
            }
        }
    }

    /// Consume one body line (newline already stripped by the reader).
    ///
    /// For `<<-`, leading tabs are stripped from body and terminator lines
    /// alike. A line equal to the delimiter completes the document; any other
    /// line is appended to the body with its newline restored. A line that
    /// would push the body past [`MAX_HERE_DOC_BYTES`] discards the body and
    /// marks it over-length, but collection continues to the terminator.
    fn feed_line(&mut self, line: &str) {
        let stripped = if self.strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if stripped == self.delimiter {
            self.body = match core::mem::replace(&mut self.body, BodyState::CompleteDiscarded) {
                BodyState::Filling(body) => BodyState::Complete(body),
                BodyState::FillingDiscarded | BodyState::CompleteDiscarded => {
                    BodyState::CompleteDiscarded
                }
                complete @ BodyState::Complete(_) => complete,
            };
            return;
        }
        if let BodyState::Filling(body) = &mut self.body {
            if body.len() + stripped.len() >= MAX_HERE_DOC_BYTES {
                self.mark_over_length();
                return;
            }
            body.push_str(stripped);
            body.push('\n');
        }
    }

    /// Discard the body and record that it grew too large. Collection still
    /// runs to the terminator so later lines are not misread as commands.
    fn mark_over_length(&mut self) {
        if matches!(self.body, BodyState::Filling(_)) {
            self.body = BodyState::FillingDiscarded;
        }
    }
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

/// One or more commands joined by `|` / `|&`. `commands` is guaranteed
/// non-empty. A `|&` join is lowered here, once, to its POSIX meaning — a
/// `2>&1` duplication appended to the left-hand command — so the interpreter
/// and host never re-derive it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    /// The commands of the pipeline, left to right.
    pub commands: Vec<Command>,
    /// `true` when the pipeline was prefixed with `!`: its exit status is
    /// negated (0 becomes 1, anything else becomes 0).
    pub negated: bool,
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

    /// The first here-document (in source order) still awaiting its
    /// terminator, or `None` when the list is ready to run.
    #[must_use]
    pub fn pending_here_doc(&self) -> Option<&HereDoc> {
        self.here_docs().find(|doc| !doc.is_complete())
    }

    /// Feed one input line to the first pending here-document (bodies are
    /// collected in source order, as POSIX specifies). A no-op when none is
    /// pending.
    pub fn feed_here_doc_line(&mut self, line: &str) {
        if let Some(doc) = self.here_docs_mut().find(|doc| !doc.is_complete()) {
            doc.feed_line(line);
        }
    }

    /// Discard the pending here-document's body as over-length (used when a
    /// body line itself had to be dropped, so the body is unusable). The
    /// document keeps consuming lines to its terminator, and the line then
    /// fails closed with [`ParseError::HereDocTooLarge`]. A no-op when no
    /// here-document is pending.
    pub fn poison_pending_here_doc(&mut self) {
        if let Some(doc) = self.here_docs_mut().find(|doc| !doc.is_complete()) {
            doc.mark_over_length();
        }
    }

    fn here_docs(&self) -> impl Iterator<Item = &HereDoc> {
        self.entries
            .iter()
            .flat_map(|entry| entry.pipeline.commands.iter())
            .flat_map(|command| command.redirections.iter())
            .filter_map(|redirection| match redirection {
                Redirection::HereDoc(doc) => Some(doc),
                _ => None,
            })
    }

    fn here_docs_mut(&mut self) -> impl Iterator<Item = &mut HereDoc> {
        self.entries
            .iter_mut()
            .flat_map(|entry| entry.pipeline.commands.iter_mut())
            .flat_map(|command| command.redirections.iter_mut())
            .filter_map(|redirection| match redirection {
                Redirection::HereDoc(doc) => Some(doc),
                _ => None,
            })
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
                // Anything left over is a token the grammar gave no meaning
                // (e.g. a `!` after the pipeline began); dropping it silently
                // would run a different line than the user wrote.
                if self.peek().is_some() {
                    return Err(ParseError::UnexpectedToken);
                }
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
        // `!` before the first command negates the pipeline's status; a
        // second `!` negates it again, as in zsh.
        let mut negated = false;
        while matches!(self.peek(), Some(Token::Bang)) {
            self.pos += 1;
            negated = !negated;
        }
        let mut commands = Vec::new();
        commands.push(self.parse_command()?);
        while let Some(join @ (Token::Pipe | Token::PipeBoth)) = self.peek() {
            let pipe_both = matches!(join, Token::PipeBoth);
            self.pos += 1;
            if pipe_both {
                // `a |& b` means `a 2>&1 | b`: one definition of the combined
                // pipe, lowered here as a trailing duplication on `a`.
                if let Some(left) = commands.last_mut() {
                    left.redirections.push(Redirection::Dup {
                        fd: FdSpec::Fd(2),
                        source: 1,
                    });
                }
            }
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands, negated })
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
            RedirOp::HereString { fd } => Ok(Redirection::HereString {
                fd,
                content: self.take_target()?,
            }),
            RedirOp::HereDoc { fd, strip_tabs } => Ok(Redirection::HereDoc(HereDoc::new(
                fd,
                strip_tabs,
                &self.take_target()?,
            ))),
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
    use crate::lexer::{FdSpec, Segment};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A fixed-descriptor [`FdSpec`] (tests only — shorthand for literals).
    fn fd(n: u32) -> FdSpec {
        FdSpec::Fd(n)
    }

    /// Flatten a word's segments back to a plain string (tests only — the
    /// real path expands through `env`).
    fn flat(word: &[Segment]) -> String {
        let mut out = String::new();
        for seg in word {
            match seg {
                Segment::Literal(s) | Segment::Expandable(s) | Segment::QuotedExpandable(s) => {
                    out.push_str(s);
                }
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
                    fd: fd(0),
                    mode: OpenMode::Read,
                    target: vec![Segment::Expandable("in".into())],
                },
                Redirection::File {
                    fd: fd(1),
                    mode: OpenMode::Write { clobber: false },
                    target: vec![Segment::Expandable("out".into())],
                },
                Redirection::File {
                    fd: fd(1),
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
                fd: fd(2),
                mode: OpenMode::Write { clobber: false },
                target: vec![Segment::Expandable("errors".into())],
            }
        );
        assert_eq!(
            only_redirection("cmd 3>>info.jsonl"),
            Redirection::File {
                fd: fd(3),
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
                fd: fd(1),
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
                fd: fd(1),
                mode: OpenMode::Write { clobber: true },
                target: vec![Segment::Expandable("out".into())],
            }
        );
        assert_eq!(
            only_redirection("cmd >!out"),
            Redirection::File {
                fd: fd(1),
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
                fd: fd(0),
                mode: OpenMode::ReadWrite,
                target: vec![Segment::Expandable("file".into())],
            }
        );
    }

    #[test]
    fn duplication_and_close() {
        assert_eq!(
            only_redirection("cmd 2>&1"),
            Redirection::Dup {
                fd: fd(2),
                source: 1
            }
        );
        assert_eq!(
            only_redirection("cmd 1>&2"),
            Redirection::Dup {
                fd: fd(1),
                source: 2
            }
        );
        assert_eq!(
            only_redirection("cmd 0<&3"),
            Redirection::Dup {
                fd: fd(0),
                source: 3
            }
        );
        assert_eq!(
            only_redirection("cmd 3>&-"),
            Redirection::Close { fd: fd(3) }
        );
        // `>&-` defaults to closing stdout, `<&-` to stdin.
        assert_eq!(
            only_redirection("cmd >&-"),
            Redirection::Close { fd: fd(1) }
        );
        assert_eq!(
            only_redirection("cmd <&-"),
            Redirection::Close { fd: fd(0) }
        );
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
    fn here_document_parses_pending_and_collects_its_body() {
        let mut list = parse("cmd <<EOF").unwrap();
        {
            let doc = list.pending_here_doc().expect("pending here-doc");
            assert_eq!(doc.fd(), &fd(0));
            assert_eq!(doc.delimiter(), "EOF");
            assert!(!doc.is_quoted());
            // An unterminated body fails closed rather than running empty.
            assert_eq!(doc.body(), Err(ParseError::UnterminatedHereDoc));
        }
        list.feed_here_doc_line("first");
        list.feed_here_doc_line("second");
        list.feed_here_doc_line("EOF");
        assert!(list.pending_here_doc().is_none());
        let cmd = &list.entries[0].pipeline.commands[0];
        let Redirection::HereDoc(doc) = &cmd.redirections[0] else {
            panic!("expected a here-doc redirection");
        };
        assert_eq!(doc.body(), Ok("first\nsecond\n"));
    }

    #[test]
    fn here_document_with_dash_strips_leading_tabs() {
        let mut list = parse("cmd <<-END").unwrap();
        list.feed_here_doc_line("\t\tindented");
        list.feed_here_doc_line("plain");
        // The terminator itself may be tab-indented under `<<-`.
        list.feed_here_doc_line("\tEND");
        assert!(list.pending_here_doc().is_none());
        let cmd = &list.entries[0].pipeline.commands[0];
        let Redirection::HereDoc(doc) = &cmd.redirections[0] else {
            panic!("expected a here-doc redirection");
        };
        assert_eq!(doc.body(), Ok("indented\nplain\n"));
    }

    #[test]
    fn plain_here_document_keeps_tabs_and_needs_an_exact_terminator() {
        let mut list = parse("cmd <<EOF").unwrap();
        // Without `-`, a tab-indented delimiter line is body, not terminator.
        list.feed_here_doc_line("\tEOF");
        assert!(list.pending_here_doc().is_some());
        list.feed_here_doc_line("EOF");
        assert!(list.pending_here_doc().is_none());
        let cmd = &list.entries[0].pipeline.commands[0];
        let Redirection::HereDoc(doc) = &cmd.redirections[0] else {
            panic!("expected a here-doc redirection");
        };
        assert_eq!(doc.body(), Ok("\tEOF\n"));
    }

    #[test]
    fn quoted_delimiter_marks_the_body_literal() {
        for line in ["cmd <<'EOF'", "cmd <<\"EOF\"", "cmd <<E\\OF", "cmd <<''"] {
            let list = parse(line).unwrap();
            let doc = list.pending_here_doc().expect("pending here-doc");
            assert!(doc.is_quoted(), "line: {line}");
        }
        let list = parse("cmd <<EOF").unwrap();
        assert!(!list.pending_here_doc().unwrap().is_quoted());
    }

    #[test]
    fn here_document_delimiter_is_never_expanded() {
        let list = parse("cmd <<$X").unwrap();
        assert_eq!(list.pending_here_doc().unwrap().delimiter(), "$X");
    }

    #[test]
    fn multiple_here_documents_fill_in_source_order() {
        let mut list = parse("a <<ONE | b <<TWO").unwrap();
        assert_eq!(list.pending_here_doc().unwrap().delimiter(), "ONE");
        list.feed_here_doc_line("body one");
        list.feed_here_doc_line("ONE");
        assert_eq!(list.pending_here_doc().unwrap().delimiter(), "TWO");
        list.feed_here_doc_line("body two");
        list.feed_here_doc_line("TWO");
        assert!(list.pending_here_doc().is_none());
    }

    #[test]
    fn over_large_here_document_fails_closed_but_still_terminates() {
        use super::MAX_HERE_DOC_BYTES;
        use alloc::string::ToString;

        let mut list = parse("cmd <<EOF").unwrap();
        let chunk = "x".repeat(4096);
        for _ in 0..=(MAX_HERE_DOC_BYTES / chunk.len()) {
            list.feed_here_doc_line(&chunk);
        }
        // Still pending: an over-length body keeps consuming to its
        // terminator so later lines are never misread as commands.
        assert!(list.pending_here_doc().is_some());
        list.feed_here_doc_line("EOF");
        assert!(list.pending_here_doc().is_none());
        let cmd = &list.entries[0].pipeline.commands[0];
        let Redirection::HereDoc(doc) = &cmd.redirections[0] else {
            panic!("expected a here-doc redirection");
        };
        assert_eq!(doc.body(), Err(ParseError::HereDocTooLarge));
        // The literal delimiter is unaffected by the discarded body.
        assert_eq!(doc.delimiter(), "EOF".to_string());
    }

    #[test]
    fn poisoned_here_document_fails_closed_but_still_terminates() {
        let mut list = parse("cmd <<EOF").unwrap();
        list.feed_here_doc_line("kept so far");
        // A dropped input line poisons the body: it can no longer be trusted.
        list.poison_pending_here_doc();
        list.feed_here_doc_line("still consumed");
        list.feed_here_doc_line("EOF");
        assert!(list.pending_here_doc().is_none());
        let cmd = &list.entries[0].pipeline.commands[0];
        let Redirection::HereDoc(doc) = &cmd.redirections[0] else {
            panic!("expected a here-doc redirection");
        };
        assert_eq!(doc.body(), Err(ParseError::HereDocTooLarge));
    }

    #[test]
    fn here_document_without_a_delimiter_word_fails_closed() {
        assert_eq!(parse("cmd <<"), Err(ParseError::MissingRedirectionTarget));
        assert_eq!(parse("cmd <<-"), Err(ParseError::MissingRedirectionTarget));
    }

    #[test]
    fn here_string_attaches_its_content() {
        assert_eq!(
            only_redirection("cmd <<<word"),
            Redirection::HereString {
                fd: fd(0),
                content: vec![Segment::Expandable("word".into())],
            }
        );
        // An explicit IO number binds the here-string's descriptor.
        assert_eq!(
            only_redirection("cmd 4<<< body"),
            Redirection::HereString {
                fd: fd(4),
                content: vec![Segment::Expandable("body".into())],
            }
        );
        // A here-string with no following word fails closed like any other
        // target-taking redirection.
        assert_eq!(parse("cmd <<<"), Err(ParseError::MissingRedirectionTarget));
    }

    #[test]
    fn pipe_both_lowers_to_a_stderr_duplication() {
        let list = parse("a |& b").unwrap();
        let cmds = &list.entries[0].pipeline.commands;
        assert_eq!(cmds.len(), 2);
        assert_eq!(
            cmds[0].redirections,
            [Redirection::Dup {
                fd: fd(2),
                source: 1
            }]
        );
        assert!(cmds[1].redirections.is_empty());
    }

    #[test]
    fn bang_negates_a_pipeline() {
        assert!(parse("! cmd").unwrap().entries[0].pipeline.negated);
        // A second `!` negates again, and an unprefixed pipeline is not
        // negated.
        assert!(!parse("! ! cmd").unwrap().entries[0].pipeline.negated);
        let list = parse("a && ! b").unwrap();
        assert!(!list.entries[0].pipeline.negated);
        assert!(list.entries[1].pipeline.negated);
    }

    #[test]
    fn a_misplaced_bang_fails_closed() {
        // `!` is only the negation prefix; after the pipeline has begun the
        // grammar gives it no meaning, and dropping it silently would run a
        // different line than the user wrote.
        assert_eq!(parse("test ! -f x"), Err(ParseError::UnexpectedToken));
    }

    #[test]
    fn ambiguous_duplications_fail_closed() {
        // `<&` with neither a source fd nor `-`, and a numbered dup-to-file.
        assert_eq!(parse("cmd <&file"), Err(ParseError::AmbiguousRedirection));
        assert_eq!(parse("cmd 2>&file"), Err(ParseError::AmbiguousRedirection));
    }
}
