//! Turning a line of text into a flat token stream.
//!
//! The lexer is the only place that reasons about quoting and escaping. It
//! emits [`Token`]s: control [operators](Token) (`|`, `&&`, `||`, `;`, `&`),
//! fully-decoded [redirection operators](RedirOp) (the `<`/`>` family, `&>`,
//! duplication, and close), and [`Token::Word`]s.
//!
//! A word is not a flat string but a sequence of [`Segment`]s that record,
//! per run of characters, whether the text is subject to variable expansion.
//! Quoting and escaping are resolved *here* so that the one later phase that
//! cares — [`env`](crate::env) expansion — only has to ask "is this run
//! expandable?" and never re-examines quotes. A `$` that
//! was backslash-escaped or single-quoted becomes [`Segment::Literal`] and so
//! is never mistaken for an expansion.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::ParseError;
use crate::parser::OpenMode;

/// One run of characters within a word.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Segment {
    /// Verbatim text — never expanded. Produced by single quotes and by
    /// backslash escapes (including `\$`).
    Literal(String),
    /// Unquoted text, subject to `$` variable expansion.
    Expandable(String),
    /// The body of double quotes: subject to `$` variable expansion like
    /// [`Segment::Expandable`], but *quoted* — which matters where quoting
    /// changes meaning, e.g. a here-document delimiter with any quoted part
    /// takes a literal (unexpanded) body.
    QuotedExpandable(String),
}

/// A lexed word: an ordered (possibly empty, e.g. `""`) list of [`Segment`]s.
pub type Word = Vec<Segment>;

/// The descriptor a redirection acts on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FdSpec {
    /// A fixed descriptor number — an explicit IO number (`2>`) or the
    /// operator's default.
    Fd(u32),
    /// A `{var}` dynamic descriptor. For the opening and duplicating forms
    /// the interpreter allocates a fresh descriptor (≥ 10, never a standard
    /// stream) and binds its number to the shell parameter `var`; for the
    /// closing form it reads the previously bound number back from `var`.
    Var(String),
}

/// A fully-decoded redirection operator, before its target word (if any) is
/// attached by the [`parser`](crate::parser).
///
/// The lexer resolves everything lexical about a redirection here — the
/// descriptor it acts on (the explicit IO number or the operator's default),
/// whether it opens/appends/duplicates/closes, and clobber-override — so the
/// parser only has to attach the target word the file-opening forms need.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirOp {
    /// Open a target file on `fd` with `mode` (`<`, `>`, `>>`, `<>`, and the
    /// clobber-override spellings). Needs a target word.
    File {
        /// The descriptor to bind.
        fd: FdSpec,
        /// How the target is opened.
        mode: OpenMode,
    },
    /// Redirect both stdout (fd 1) and stderr (fd 2) to one file (`&>`, `>&`
    /// file form, and their append/clobber spellings). Needs a target word.
    Combined {
        /// `true` for the append spellings.
        append: bool,
        /// `true` for the clobber-override spellings.
        clobber: bool,
    },
    /// Duplicate descriptor `source` onto `fd` (`n>&m`, `n<&m`, `2>&1`).
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
    /// target word supplies the content; the interpreter appends the trailing
    /// newline a here-string carries.
    HereString {
        /// The descriptor the here-string feeds (the operator's explicit or
        /// default fd).
        fd: FdSpec,
    },
    /// Feed a multi-line here-document (`<< delim`, `<<- delim`) as the input
    /// of `fd` (default 0). The target word is the *delimiter*; the body is
    /// collected from the following input lines, up to a line holding only the
    /// delimiter.
    HereDoc {
        /// The descriptor the here-document feeds (the operator's explicit or
        /// default fd).
        fd: FdSpec,
        /// `true` for `<<-`: leading tabs are stripped from every body line
        /// and from the terminating delimiter line.
        strip_tabs: bool,
    },
}

/// A single lexical unit of a command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Token {
    /// A word (a command name, an argument, or a redirection target).
    Word(Word),
    /// `|` — pipe.
    Pipe,
    /// `|&` — pipe both stdout and stderr (shorthand for `2>&1 |`).
    PipeBoth,
    /// `&&` — run the right side only if the left side succeeded.
    AndIf,
    /// `||` — run the right side only if the left side failed.
    OrIf,
    /// `;` — unconditional sequence.
    Semicolon,
    /// `&` — run the preceding pipeline in the background.
    Ampersand,
    /// `!` — negate the following pipeline's exit status (a bare `!` word at
    /// a command position).
    Bang,
    /// A redirection operator, fully decoded (see [`RedirOp`]).
    Redirect(RedirOp),
}

/// Lex `line` into a token stream.
///
/// A `#` that begins a word starts a comment running to the end of the line;
/// mid-word it is an ordinary character.
///
/// # Errors
///
/// Returns [`ParseError::UnterminatedQuote`] for an unclosed quote,
/// [`ParseError::DanglingEscape`] for a line ending on a lone `\`, and
/// [`ParseError::AmbiguousRedirection`] for a malformed duplication.
pub fn tokenize(line: &str) -> Result<Vec<Token>, ParseError> {
    Ok(tokenize_with_spans(line)?
        .into_iter()
        .map(|(token, _)| token)
        .collect())
}

/// Lex `line` into a token stream, pairing each token with its `[start, end)`
/// span in **character** indices of `line`.
///
/// The spans let the completion engine locate the word under the cursor with
/// the shell's own quoting-aware lexer — never a second, completion-only
/// tokeniser. [`tokenize`] is this function with the spans discarded, so the
/// two views can never disagree.
///
/// # Errors
///
/// Exactly the [`tokenize`] error set.
pub fn tokenize_with_spans(
    line: &str,
) -> Result<Vec<(Token, core::ops::Range<usize>)>, ParseError> {
    Lexer::new(line).run()
}

/// Accumulates a word's [`Segment`]s, coalescing adjacent runs of the same
/// kind so `abc` is one segment, not three.
struct WordBuilder {
    segments: Vec<Segment>,
}

impl WordBuilder {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    fn push_expandable(&mut self, c: char) {
        match self.segments.last_mut() {
            Some(Segment::Expandable(s)) => s.push(c),
            _ => self.segments.push(Segment::Expandable(String::from(c))),
        }
    }

    fn push_literal(&mut self, c: char) {
        match self.segments.last_mut() {
            Some(Segment::Literal(s)) => s.push(c),
            _ => self.segments.push(Segment::Literal(String::from(c))),
        }
    }

    fn push_quoted_expandable(&mut self, c: char) {
        match self.segments.last_mut() {
            Some(Segment::QuotedExpandable(s)) => s.push(c),
            _ => self
                .segments
                .push(Segment::QuotedExpandable(String::from(c))),
        }
    }

    fn finish(self) -> Word {
        self.segments
    }
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

/// The result of looking ahead over an optional leading IO number (see
/// [`Lexer::scan_leading_fd`]).
struct ScanFd {
    /// The parsed descriptor, if any digit was present.
    fd: Option<u32>,
    /// Index of the first non-digit character — where a `<`/`>` would sit.
    op_index: usize,
    /// `true` if the digit run overflowed a `u32`.
    overflow: bool,
}

impl Lexer {
    fn new(line: &str) -> Self {
        Self {
            chars: line.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn run(mut self) -> Result<Vec<(Token, core::ops::Range<usize>)>, ParseError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_spaces();
            let start = self.pos;
            let Some(c) = self.peek() else {
                return Ok(tokens);
            };
            if c == '#' {
                return Ok(tokens);
            }
            // `( list )` subshells are not supported yet. Fail closed: an
            // unquoted paren must never be read as an ordinary word character,
            // or `(ls)` would try to run a program literally named "(ls)".
            if c == '(' || c == ')' {
                return Err(ParseError::UnsupportedCompound);
            }
            // `=(cmd)` process substitution is permanently unsupported (no
            // scratch filesystem to back it); recognised here so the
            // parenthesised command is never misread as a filename.
            if c == '=' && self.peek2() == Some('(') {
                return Err(ParseError::UnsupportedProcessSubstitution);
            }
            // A bare `{` or `}` token is a brace group, which is not supported
            // yet. Fail closed: reading `{` as an ordinary word would run a
            // program literally named "{". A `{name}` glued to `<`/`>` is the
            // dynamic-descriptor prefix instead, and any other `{...}` text
            // stays word characters (`${NAME}` rides through unharmed).
            if (c == '{' || c == '}') && self.brace_is_bare() {
                return Err(ParseError::UnsupportedCompound);
            }
            // A bare `!` token negates the pipeline that follows it. `!` glued
            // to other characters stays an ordinary word character.
            if c == '!' && matches!(self.peek2(), None | Some(' ' | '\t')) {
                self.pos += 1;
                tokens.push((Token::Bang, start..self.pos));
                continue;
            }
            let token = if let Some(op) = self.lex_dynamic_fd_redirect()? {
                op
            } else if let Some(op) = self.lex_redirect()? {
                op
            } else if let Some(op) = self.lex_operator() {
                op
            } else {
                Token::Word(self.lex_word()?)
            };
            tokens.push((token, start..self.pos));
        }
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    /// With the cursor on `{` or `}`: `true` when the brace is a *bare* token
    /// (immediately followed by a token boundary), i.e. the reserved-word
    /// spelling of a brace group rather than part of a word or a `{name}<`
    /// dynamic-descriptor prefix.
    fn brace_is_bare(&self) -> bool {
        matches!(self.peek2(), None | Some(' ' | '\t' | ';' | '|' | '&'))
    }

    /// Recognise a `{name}` dynamic-descriptor prefix glued directly to a
    /// `<`/`>` redirection operator (`{fd}>out`, `{fd}>&-`). Leaves the cursor
    /// untouched — and lexes nothing — unless the whole shape matches, so
    /// `{a,b}` and `${NAME}` stay ordinary word text.
    fn lex_dynamic_fd_redirect(&mut self) -> Result<Option<Token>, ParseError> {
        if self.peek() != Some('{') {
            return Ok(None);
        }
        let mut i = self.pos + 1;
        let mut name = String::new();
        while let Some(&c) = self.chars.get(i) {
            if c == '}' {
                break;
            }
            name.push(c);
            i += 1;
        }
        if self.chars.get(i) != Some(&'}') || !crate::env::is_valid_name(&name) {
            return Ok(None);
        }
        let spec = Some(FdSpec::Var(name));
        match self.chars.get(i + 1) {
            Some('<') => {
                self.pos = i + 1;
                self.lex_input(spec).map(Some)
            }
            Some('>') => {
                self.pos = i + 1;
                self.lex_output(spec).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Recognise a control operator (`|`, `||`, `&&`, `&`, `;`) at the cursor.
    ///
    /// The redirection spellings (`<`/`>` forms and `&>`) are claimed earlier
    /// by [`Lexer::lex_redirect`], so a bare `&` here is always the background
    /// operator and never the start of `&>`.
    fn lex_operator(&mut self) -> Option<Token> {
        let c = self.peek()?;
        let two = self.peek2();
        let (token, width) = match (c, two) {
            ('|', Some('|')) => (Token::OrIf, 2),
            ('|', Some('&')) => (Token::PipeBoth, 2),
            ('&', Some('&')) => (Token::AndIf, 2),
            ('|', _) => (Token::Pipe, 1),
            ('&', _) => (Token::Ampersand, 1),
            (';', _) => (Token::Semicolon, 1),
            _ => return None,
        };
        self.pos += width;
        Some(token)
    }

    /// Recognise a redirection operator at the cursor, if any.
    ///
    /// This runs *before* [`Lexer::lex_operator`] so the redirection spellings
    /// that begin with a control character claim their meaning first: `&>`
    /// (before bare `&`) and any `<`/`>` form (optionally with a glued leading
    /// IO number). A bare numeric word (`echo 2`) is left untouched — a leading
    /// number is an IO number only when it is immediately followed by `<`/`>`.
    ///
    /// # Errors
    ///
    /// Fails closed for a [malformed duplication](ParseError::AmbiguousRedirection).
    fn lex_redirect(&mut self) -> Result<Option<Token>, ParseError> {
        // Combined stdout+stderr via a leading `&`: `&>`, `&>>`, `&>|`, `&>!`.
        if self.peek() == Some('&') && self.peek2() == Some('>') {
            self.pos += 2; // consume "&>"
            let append = self.peek() == Some('>');
            if append {
                self.pos += 1;
            }
            let clobber = self.take_clobber_flag();
            return Ok(Some(Token::Redirect(RedirOp::Combined { append, clobber })));
        }

        // An optional leading IO number, but only when it is glued directly to
        // a `<`/`>`. `scan_leading_fd` does not move the cursor, so a plain
        // numeric word is left for `lex_word`.
        let scan = self.scan_leading_fd();
        match self.chars.get(scan.op_index).copied() {
            Some('<') => {
                if scan.overflow {
                    return Err(ParseError::AmbiguousRedirection);
                }
                self.pos = scan.op_index;
                self.lex_input(scan.fd.map(FdSpec::Fd)).map(Some)
            }
            Some('>') => {
                if scan.overflow {
                    return Err(ParseError::AmbiguousRedirection);
                }
                self.pos = scan.op_index;
                self.lex_output(scan.fd.map(FdSpec::Fd)).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Lex an input redirection whose leading `<` is at the cursor. `fd` is the
    /// explicit IO number if one was written, else the operator's default.
    fn lex_input(&mut self, fd: Option<FdSpec>) -> Result<Token, ParseError> {
        self.pos += 1; // consume '<'
        let fd = fd.unwrap_or(FdSpec::Fd(0));
        let op = match self.peek() {
            // `<(cmd)` — process substitution, not yet supported. Fail closed
            // so the parenthesised command is never misread as a filename.
            Some('(') => return Err(ParseError::UnsupportedProcessSubstitution),
            // A second `<`: the here-string `<<<` or a here-document `<<` /
            // `<<-` (whose optional `-` selects leading-tab stripping).
            Some('<') => {
                self.pos += 1; // consume the second '<'
                if self.peek() == Some('<') {
                    self.pos += 1; // consume the third '<' — here-string
                    RedirOp::HereString { fd }
                } else {
                    let strip_tabs = self.peek() == Some('-');
                    if strip_tabs {
                        self.pos += 1; // consume the '-'
                    }
                    RedirOp::HereDoc { fd, strip_tabs }
                }
            }
            // `<>` — open for reading and writing.
            Some('>') => {
                self.pos += 1;
                RedirOp::File {
                    fd,
                    mode: OpenMode::ReadWrite,
                }
            }
            // `<&m` / `<&-` — duplicate or close. `<&` has no combined form.
            Some('&') => self.lex_dup_or_close(fd, false)?,
            _ => RedirOp::File {
                fd,
                mode: OpenMode::Read,
            },
        };
        Ok(Token::Redirect(op))
    }

    /// Lex an output redirection whose leading `>` is at the cursor. `fd` is the
    /// explicit IO number if one was written, else the operator's default.
    fn lex_output(&mut self, fd: Option<FdSpec>) -> Result<Token, ParseError> {
        self.pos += 1; // consume '>'
        let explicit_fd = fd.is_some();
        let fd = fd.unwrap_or(FdSpec::Fd(1));
        let op = match self.peek() {
            // `>(cmd)` — process substitution, not yet supported. Fail closed
            // so the parenthesised command is never misread as a filename.
            Some('(') => return Err(ParseError::UnsupportedProcessSubstitution),
            Some('>') => {
                self.pos += 1; // consume the second '>'
                if self.peek() == Some('&') {
                    // `>>&file` — combined append (file form). A leading fd
                    // would make it ambiguous with a duplication.
                    if explicit_fd {
                        return Err(ParseError::AmbiguousRedirection);
                    }
                    self.pos += 1; // consume '&'
                    RedirOp::Combined {
                        append: true,
                        clobber: false,
                    }
                } else {
                    RedirOp::File {
                        fd,
                        mode: OpenMode::Append {
                            clobber: self.take_clobber_flag(),
                        },
                    }
                }
            }
            // `>&m` / `>&-` — duplicate or close; `>&file` — combined (file form).
            Some('&') => self.lex_dup_or_close(fd, !explicit_fd)?,
            _ => RedirOp::File {
                fd,
                mode: OpenMode::Write {
                    clobber: self.take_clobber_flag(),
                },
            },
        };
        Ok(Token::Redirect(op))
    }

    /// Lex the `&`-suffixed forms after the `&` has been peeked (but not yet
    /// consumed): `&m` duplicates descriptor `m` onto `fd`, `&-` closes `fd`.
    /// When `combined_ok` (an unnumbered `>&`), a following filename means the
    /// csh-style "redirect both stdout and stderr" form; otherwise a following
    /// non-descriptor is [ambiguous](ParseError::AmbiguousRedirection).
    fn lex_dup_or_close(&mut self, fd: FdSpec, combined_ok: bool) -> Result<RedirOp, ParseError> {
        self.pos += 1; // consume '&'
        match self.peek() {
            Some('-') => {
                self.pos += 1;
                Ok(RedirOp::Close { fd })
            }
            Some(c) if c.is_ascii_digit() => {
                let source = self.take_fd_number()?;
                Ok(RedirOp::Dup { fd, source })
            }
            _ if combined_ok => Ok(RedirOp::Combined {
                append: false,
                clobber: false,
            }),
            _ => Err(ParseError::AmbiguousRedirection),
        }
    }

    /// Consume a single clobber-override suffix (`|` or `!`) if present.
    fn take_clobber_flag(&mut self) -> bool {
        if matches!(self.peek(), Some('|' | '!')) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume a run of decimal digits at the cursor as a descriptor number.
    ///
    /// # Errors
    ///
    /// [`AmbiguousRedirection`](ParseError::AmbiguousRedirection) if no digit
    /// is present or the value does not fit a `u32`.
    fn take_fd_number(&mut self) -> Result<u32, ParseError> {
        let mut value: u32 = 0;
        let mut any = false;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(d))
                .ok_or(ParseError::AmbiguousRedirection)?;
            any = true;
            self.pos += 1;
        }
        if any {
            Ok(value)
        } else {
            Err(ParseError::AmbiguousRedirection)
        }
    }

    /// Look ahead over an optional run of decimal digits at the cursor without
    /// moving it. Returns the parsed fd (if any digit is present), the index of
    /// the first non-digit character (where a `<`/`>` operator would sit), and
    /// whether the value overflowed a `u32`.
    fn scan_leading_fd(&self) -> ScanFd {
        let mut i = self.pos;
        let mut value: u32 = 0;
        let mut any = false;
        let mut overflow = false;
        while let Some(d) = self.chars.get(i).copied().and_then(|c| c.to_digit(10)) {
            match value.checked_mul(10).and_then(|v| v.checked_add(d)) {
                Some(v) => value = v,
                None => overflow = true,
            }
            any = true;
            i += 1;
        }
        ScanFd {
            fd: any.then_some(value),
            op_index: i,
            overflow: any && overflow,
        }
    }

    /// `#` is deliberately absent: it only begins a comment at a token start
    /// (handled in [`Lexer::run`]); mid-word it is an ordinary character.
    fn at_word_boundary(&self) -> bool {
        matches!(
            self.peek(),
            None | Some(' ' | '\t' | '|' | '&' | ';' | '<' | '>')
        )
    }

    fn lex_word(&mut self) -> Result<Word, ParseError> {
        let mut word = WordBuilder::new();
        while !self.at_word_boundary() {
            match self.peek() {
                Some('\'') => {
                    self.bump();
                    self.lex_single_quoted(&mut word)?;
                }
                Some('"') => {
                    self.bump();
                    self.lex_double_quoted(&mut word)?;
                }
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        None => return Err(ParseError::DanglingEscape),
                        Some(c) => word.push_literal(c),
                    }
                }
                Some(c) => {
                    self.bump();
                    word.push_expandable(c);
                }
                None => break,
            }
        }
        Ok(word.finish())
    }

    fn lex_single_quoted(&mut self, word: &mut WordBuilder) -> Result<(), ParseError> {
        loop {
            match self.bump() {
                None => return Err(ParseError::UnterminatedQuote),
                Some('\'') => return Ok(()),
                Some(c) => word.push_literal(c),
            }
        }
    }

    fn lex_double_quoted(&mut self, word: &mut WordBuilder) -> Result<(), ParseError> {
        loop {
            match self.bump() {
                None => return Err(ParseError::UnterminatedQuote),
                Some('"') => return Ok(()),
                Some('\\') => match self.bump() {
                    None => return Err(ParseError::DanglingEscape),
                    // Inside double quotes only these three escapes are
                    // active; the escaped character is literal, so an escaped
                    // `$` cannot trigger expansion.
                    Some(escaped @ ('"' | '\\' | '$')) => word.push_literal(escaped),
                    Some(other) => {
                        word.push_literal('\\');
                        word.push_literal(other);
                    }
                },
                Some(c) => word.push_quoted_expandable(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tokenize, tokenize_with_spans, FdSpec, RedirOp, Segment, Token};
    use crate::error::ParseError;
    use crate::parser::OpenMode;
    use alloc::string::ToString;
    use alloc::vec;

    fn expandable(s: &str) -> Token {
        Token::Word(vec![Segment::Expandable(s.to_string())])
    }

    /// Spans cover each token's exact character range: words, operators, and
    /// redirections, with inter-token whitespace excluded.
    #[test]
    fn spans_locate_each_token() {
        let line = "echo  hi >out";
        let spanned = tokenize_with_spans(line).expect("lexes");
        let spans: vec::Vec<_> = spanned.iter().map(|(_, span)| span.clone()).collect();
        // The redirection operator and its target word are separate tokens
        // (the parser attaches the target), each with its own span.
        assert_eq!(spans, [0..4, 6..8, 9..10, 10..13]);
        assert_eq!(spanned[0].0, expandable("echo"));
        assert!(matches!(spanned[2].0, Token::Redirect(_)));
        assert_eq!(spanned[3].0, expandable("out"));
        // The span slices back to the original text (char == byte here).
        assert_eq!(&line[6..8], "hi");
    }

    /// Spans are in character indices, not bytes: a multi-byte character
    /// counts once.
    #[test]
    fn spans_count_characters_not_bytes() {
        let spanned = tokenize_with_spans("café x").expect("lexes");
        let spans: vec::Vec<_> = spanned.iter().map(|(_, span)| span.clone()).collect();
        assert_eq!(spans, [0..4, 5..6]);
    }

    /// The span-free view is the spanned view with the spans discarded.
    #[test]
    fn tokenize_agrees_with_the_spanned_view() {
        let line = "a | b >c && ! d";
        let plain = tokenize(line).expect("lexes");
        let spanned: vec::Vec<_> = tokenize_with_spans(line)
            .expect("lexes")
            .into_iter()
            .map(|(token, _)| token)
            .collect();
        assert_eq!(plain, spanned);
    }

    #[test]
    fn splits_words_on_whitespace() {
        assert_eq!(
            tokenize("ls  -l   /Apps").unwrap(),
            vec![expandable("ls"), expandable("-l"), expandable("/Apps")]
        );
    }

    #[test]
    fn recognises_all_operators() {
        assert_eq!(
            tokenize("a | b && c || d ; e &").unwrap(),
            vec![
                expandable("a"),
                Token::Pipe,
                expandable("b"),
                Token::AndIf,
                expandable("c"),
                Token::OrIf,
                expandable("d"),
                Token::Semicolon,
                expandable("e"),
                Token::Ampersand,
            ]
        );
    }

    #[test]
    fn operators_need_no_surrounding_spaces() {
        assert_eq!(
            tokenize("a>>b<c|d").unwrap(),
            vec![
                expandable("a"),
                Token::Redirect(RedirOp::File {
                    fd: FdSpec::Fd(1),
                    mode: OpenMode::Append { clobber: false },
                }),
                expandable("b"),
                Token::Redirect(RedirOp::File {
                    fd: FdSpec::Fd(0),
                    mode: OpenMode::Read,
                }),
                expandable("c"),
                Token::Pipe,
                expandable("d"),
            ]
        );
    }

    #[test]
    fn numbered_fd_glues_to_the_operator_but_a_bare_number_is_a_word() {
        assert_eq!(
            tokenize("2>&1").unwrap(),
            vec![Token::Redirect(RedirOp::Dup {
                fd: FdSpec::Fd(2),
                source: 1
            })]
        );
        // A digit run not glued to `<`/`>` stays an ordinary word.
        assert_eq!(
            tokenize("echo 22").unwrap(),
            vec![expandable("echo"), expandable("22"),]
        );
    }

    #[test]
    fn ampersand_redirect_beats_the_background_operator() {
        // `&>` is a combined redirection, not the `&` background operator.
        assert_eq!(
            tokenize("&>log").unwrap(),
            vec![
                Token::Redirect(RedirOp::Combined {
                    append: false,
                    clobber: false,
                }),
                expandable("log"),
            ]
        );
        // A lone `&` is still the background operator.
        assert_eq!(
            tokenize("a &").unwrap(),
            vec![expandable("a"), Token::Ampersand]
        );
    }

    #[test]
    fn here_document_lexes_to_its_operator_and_delimiter() {
        // `<<` is the here-document operator (default fd 0); its delimiter is
        // an ordinary following word, glued or spaced.
        assert_eq!(
            tokenize("<<EOF").unwrap(),
            vec![
                Token::Redirect(RedirOp::HereDoc {
                    fd: FdSpec::Fd(0),
                    strip_tabs: false,
                }),
                expandable("EOF"),
            ]
        );
        // `<<-` selects leading-tab stripping.
        assert_eq!(
            tokenize("<<- END").unwrap(),
            vec![
                Token::Redirect(RedirOp::HereDoc {
                    fd: FdSpec::Fd(0),
                    strip_tabs: true,
                }),
                expandable("END"),
            ]
        );
        // An explicit IO number binds the here-document's descriptor.
        assert_eq!(
            tokenize("4<<EOF").unwrap(),
            vec![
                Token::Redirect(RedirOp::HereDoc {
                    fd: FdSpec::Fd(4),
                    strip_tabs: false,
                }),
                expandable("EOF"),
            ]
        );
    }

    #[test]
    fn here_string_lexes_to_its_operator_and_target() {
        // `<<<` is the here-string operator (default fd 0), and its body is an
        // ordinary following word, glued or spaced.
        assert_eq!(
            tokenize("<<<word").unwrap(),
            vec![
                Token::Redirect(RedirOp::HereString { fd: FdSpec::Fd(0) }),
                expandable("word"),
            ]
        );
        assert_eq!(
            tokenize("<<< word").unwrap(),
            vec![
                Token::Redirect(RedirOp::HereString { fd: FdSpec::Fd(0) }),
                expandable("word"),
            ]
        );
        // An explicit IO number binds the here-string's descriptor.
        assert_eq!(
            tokenize("4<<<x").unwrap(),
            vec![
                Token::Redirect(RedirOp::HereString { fd: FdSpec::Fd(4) }),
                expandable("x"),
            ]
        );
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            tokenize("'a $b | c'").unwrap(),
            vec![Token::Word(vec![Segment::Literal("a $b | c".to_string())])]
        );
    }

    #[test]
    fn double_quotes_keep_spaces_and_split_escapes() {
        // A double-quoted body is expandable but remembered as quoted, so
        // `$c` still expands while a here-doc delimiter would count as quoted.
        assert_eq!(
            tokenize(r#""a $c""#).unwrap(),
            vec![Token::Word(vec![Segment::QuotedExpandable(
                "a $c".to_string()
            )])]
        );
    }

    #[test]
    fn escaped_dollar_is_literal() {
        assert_eq!(
            tokenize(r"\$HOME").unwrap(),
            vec![Token::Word(vec![
                Segment::Literal("$".to_string()),
                Segment::Expandable("HOME".to_string()),
            ])]
        );
    }

    #[test]
    fn adjacent_segments_join_per_kind() {
        assert_eq!(
            tokenize(r#"a'b'"c"d"#).unwrap(),
            vec![Token::Word(vec![
                Segment::Expandable("a".to_string()),
                Segment::Literal("b".to_string()),
                Segment::QuotedExpandable("c".to_string()),
                Segment::Expandable("d".to_string()),
            ])]
        );
    }

    #[test]
    fn empty_double_quotes_make_an_empty_word() {
        assert_eq!(tokenize(r#""""#).unwrap(), vec![Token::Word(vec![])]);
    }

    #[test]
    fn backslash_escapes_an_operator() {
        assert_eq!(
            tokenize(r"a\|b").unwrap(),
            vec![Token::Word(vec![
                Segment::Expandable("a".to_string()),
                Segment::Literal("|".to_string()),
                Segment::Expandable("b".to_string()),
            ])]
        );
    }

    #[test]
    fn hash_starts_a_comment() {
        assert_eq!(tokenize("ls # list").unwrap(), vec![expandable("ls")]);
        assert!(tokenize("# whole line").unwrap().is_empty());
    }

    #[test]
    fn hash_inside_a_word_is_literal() {
        assert_eq!(tokenize("a#b").unwrap(), vec![expandable("a#b")]);
    }

    #[test]
    fn pipe_both_lexes_as_its_own_operator() {
        assert_eq!(
            tokenize("a |& b").unwrap(),
            vec![expandable("a"), Token::PipeBoth, expandable("b")]
        );
    }

    #[test]
    fn bare_bang_is_the_negation_token() {
        assert_eq!(
            tokenize("! cmd").unwrap(),
            vec![Token::Bang, expandable("cmd")]
        );
        assert_eq!(tokenize("!").unwrap(), vec![Token::Bang]);
        // Glued to other characters it stays an ordinary word character
        // (zsh history expansion is not implemented; the spelling is inert).
        assert_eq!(tokenize("!x").unwrap(), vec![expandable("!x")]);
    }

    #[test]
    fn subshell_parens_fail_closed() {
        // `(ls)` must never lex as a word: running a program literally named
        // "(ls)" would be a different command than the user wrote.
        assert_eq!(tokenize("(ls)"), Err(ParseError::UnsupportedCompound));
        assert_eq!(tokenize("( ls )"), Err(ParseError::UnsupportedCompound));
        // Quoted or escaped parens stay ordinary word text.
        assert_eq!(
            tokenize("'(ls)'").unwrap(),
            vec![Token::Word(vec![Segment::Literal("(ls)".to_string())])]
        );
    }

    #[test]
    fn brace_groups_fail_closed_but_brace_words_survive() {
        assert_eq!(tokenize("{ ls; }"), Err(ParseError::UnsupportedCompound));
        assert_eq!(tokenize("ls; }"), Err(ParseError::UnsupportedCompound));
        // `{...}` glued into a word is plain word text (no brace expansion).
        assert_eq!(tokenize("a{b,c}d").unwrap(), vec![expandable("a{b,c}d")]);
        assert_eq!(tokenize("${NAME}").unwrap(), vec![expandable("${NAME}")]);
    }

    #[test]
    fn process_substitution_fails_closed() {
        for line in ["diff <(a) <(b)", "cmd > >(consumer)", "cmd >(c)"] {
            assert_eq!(
                tokenize(line),
                Err(ParseError::UnsupportedProcessSubstitution),
                "line: {line}"
            );
        }
        assert_eq!(
            tokenize("cmd =(a)"),
            Err(ParseError::UnsupportedProcessSubstitution)
        );
    }

    #[test]
    fn dynamic_fd_prefix_lexes_to_a_var_descriptor() {
        assert_eq!(
            tokenize("{fd}>out").unwrap(),
            vec![
                Token::Redirect(RedirOp::File {
                    fd: FdSpec::Var("fd".to_string()),
                    mode: OpenMode::Write { clobber: false },
                }),
                expandable("out"),
            ]
        );
        assert_eq!(
            tokenize("{fd}<in").unwrap(),
            vec![
                Token::Redirect(RedirOp::File {
                    fd: FdSpec::Var("fd".to_string()),
                    mode: OpenMode::Read,
                }),
                expandable("in"),
            ]
        );
        assert_eq!(
            tokenize("{fd}>&-").unwrap(),
            vec![Token::Redirect(RedirOp::Close {
                fd: FdSpec::Var("fd".to_string()),
            })]
        );
        assert_eq!(
            tokenize("{fd}>&1").unwrap(),
            vec![Token::Redirect(RedirOp::Dup {
                fd: FdSpec::Var("fd".to_string()),
                source: 1,
            })]
        );
    }

    #[test]
    fn a_non_name_brace_prefix_is_not_a_dynamic_fd() {
        // `{2}` is not a valid variable name, so the braces stay word text
        // and the `>` is an ordinary stdout redirection.
        assert_eq!(
            tokenize("{2}>out").unwrap(),
            vec![
                expandable("{2}"),
                Token::Redirect(RedirOp::File {
                    fd: FdSpec::Fd(1),
                    mode: OpenMode::Write { clobber: false },
                }),
                expandable("out"),
            ]
        );
        // `{fd}` not glued to `<`/`>` is a plain word.
        assert_eq!(tokenize("{fd}").unwrap(), vec![expandable("{fd}")]);
    }

    #[test]
    fn unterminated_quote_is_rejected() {
        assert_eq!(tokenize("'oops"), Err(ParseError::UnterminatedQuote));
        assert_eq!(tokenize(r#""oops"#), Err(ParseError::UnterminatedQuote));
    }

    #[test]
    fn dangling_escape_is_rejected() {
        assert_eq!(tokenize(r"oops\"), Err(ParseError::DanglingEscape));
    }
}
