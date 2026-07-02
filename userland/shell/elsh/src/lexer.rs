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
    /// Text subject to `$` variable expansion. Produced by unquoted text and
    /// by the body of double quotes.
    Expandable(String),
}

/// A lexed word: an ordered (possibly empty, e.g. `""`) list of [`Segment`]s.
pub type Word = Vec<Segment>;

/// A fully-decoded redirection operator, before its target word (if any) is
/// attached by the [`parser`](crate::parser).
///
/// The lexer resolves everything lexical about a redirection here — the
/// descriptor it acts on (the explicit IO number or the operator's default),
/// whether it opens/appends/duplicates/closes, and clobber-override — so the
/// parser only has to attach the target word the file-opening forms need.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RedirOp {
    /// Open a target file on `fd` with `mode` (`<`, `>`, `>>`, `<>`, and the
    /// clobber-override spellings). Needs a target word.
    File {
        /// The descriptor to bind.
        fd: u32,
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

/// A single lexical unit of a command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Token {
    /// A word (a command name, an argument, or a redirection target).
    Word(Word),
    /// `|` — pipe.
    Pipe,
    /// `&&` — run the right side only if the left side succeeded.
    AndIf,
    /// `||` — run the right side only if the left side failed.
    OrIf,
    /// `;` — unconditional sequence.
    Semicolon,
    /// `&` — run the preceding pipeline in the background.
    Ampersand,
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
/// [`ParseError::DanglingEscape`] for a line ending on a lone `\`,
/// [`ParseError::UnsupportedRedirection`] for a here-document/here-string, and
/// [`ParseError::AmbiguousRedirection`] for a malformed duplication.
pub fn tokenize(line: &str) -> Result<Vec<Token>, ParseError> {
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

    fn run(mut self) -> Result<Vec<Token>, ParseError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_spaces();
            let Some(c) = self.peek() else {
                return Ok(tokens);
            };
            if c == '#' {
                return Ok(tokens);
            }
            if let Some(op) = self.lex_redirect()? {
                tokens.push(op);
            } else if let Some(op) = self.lex_operator() {
                tokens.push(op);
            } else {
                tokens.push(Token::Word(self.lex_word()?));
            }
        }
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
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
    /// Fails closed for a not-yet-supported operator ([here-documents and
    /// here-strings](ParseError::UnsupportedRedirection)) and for a
    /// [malformed duplication](ParseError::AmbiguousRedirection).
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
                self.lex_input(scan.fd).map(Some)
            }
            Some('>') => {
                if scan.overflow {
                    return Err(ParseError::AmbiguousRedirection);
                }
                self.pos = scan.op_index;
                self.lex_output(scan.fd).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Lex an input redirection whose leading `<` is at the cursor. `fd` is the
    /// explicit IO number if one was written, else the operator's default.
    fn lex_input(&mut self, fd: Option<u32>) -> Result<Token, ParseError> {
        self.pos += 1; // consume '<'
        let op = match self.peek() {
            // `<<`, `<<-`, `<<<` — here-documents/strings, not yet supported.
            Some('<') => return Err(ParseError::UnsupportedRedirection),
            // `<>` — open for reading and writing.
            Some('>') => {
                self.pos += 1;
                RedirOp::File {
                    fd: fd.unwrap_or(0),
                    mode: OpenMode::ReadWrite,
                }
            }
            // `<&m` / `<&-` — duplicate or close. `<&` has no combined form.
            Some('&') => self.lex_dup_or_close(fd.unwrap_or(0), false)?,
            _ => RedirOp::File {
                fd: fd.unwrap_or(0),
                mode: OpenMode::Read,
            },
        };
        Ok(Token::Redirect(op))
    }

    /// Lex an output redirection whose leading `>` is at the cursor. `fd` is the
    /// explicit IO number if one was written, else the operator's default.
    fn lex_output(&mut self, fd: Option<u32>) -> Result<Token, ParseError> {
        self.pos += 1; // consume '>'
        let op = match self.peek() {
            Some('>') => {
                self.pos += 1; // consume the second '>'
                if self.peek() == Some('&') {
                    // `>>&file` — combined append (file form). A leading fd
                    // would make it ambiguous with a duplication.
                    if fd.is_some() {
                        return Err(ParseError::AmbiguousRedirection);
                    }
                    self.pos += 1; // consume '&'
                    RedirOp::Combined {
                        append: true,
                        clobber: false,
                    }
                } else {
                    RedirOp::File {
                        fd: fd.unwrap_or(1),
                        mode: OpenMode::Append {
                            clobber: self.take_clobber_flag(),
                        },
                    }
                }
            }
            // `>&m` / `>&-` — duplicate or close; `>&file` — combined (file form).
            Some('&') => self.lex_dup_or_close(fd.unwrap_or(1), fd.is_none())?,
            _ => RedirOp::File {
                fd: fd.unwrap_or(1),
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
    fn lex_dup_or_close(&mut self, fd: u32, combined_ok: bool) -> Result<RedirOp, ParseError> {
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
                Some(c) => word.push_expandable(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tokenize, RedirOp, Segment, Token};
    use crate::error::ParseError;
    use crate::parser::OpenMode;
    use alloc::string::ToString;
    use alloc::vec;

    fn expandable(s: &str) -> Token {
        Token::Word(vec![Segment::Expandable(s.to_string())])
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
                    fd: 1,
                    mode: OpenMode::Append { clobber: false },
                }),
                expandable("b"),
                Token::Redirect(RedirOp::File {
                    fd: 0,
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
            vec![Token::Redirect(RedirOp::Dup { fd: 2, source: 1 })]
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
    fn here_documents_fail_closed_in_the_lexer() {
        assert_eq!(tokenize("<<EOF"), Err(ParseError::UnsupportedRedirection));
        assert_eq!(tokenize("<<<word"), Err(ParseError::UnsupportedRedirection));
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
        // `a `, `b`, ` ` and `c ` are expandable; the escaped `"` and `$`
        // become literal, so the `$c` stays expandable but `\$` would not.
        assert_eq!(
            tokenize(r#""a $c""#).unwrap(),
            vec![Token::Word(vec![Segment::Expandable("a $c".to_string())])]
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
                Segment::Expandable("cd".to_string()),
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
    fn unterminated_quote_is_rejected() {
        assert_eq!(tokenize("'oops"), Err(ParseError::UnterminatedQuote));
        assert_eq!(tokenize(r#""oops"#), Err(ParseError::UnterminatedQuote));
    }

    #[test]
    fn dangling_escape_is_rejected() {
        assert_eq!(tokenize(r"oops\"), Err(ParseError::DanglingEscape));
    }
}
