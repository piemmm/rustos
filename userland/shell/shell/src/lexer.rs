//! Turning a line of text into a flat token stream.
//!
//! The lexer is the only place that reasons about quoting and escaping. It
//! emits [`Token`]s: control [operators](Token) (`|`, `&&`, `||`, `;`, `&`,
//! `<`, `>`, `>>`) and [`Token::Word`]s.
//!
//! A word is not a flat string but a sequence of [`Segment`]s that record,
//! per run of characters, whether the text is subject to variable expansion.
//! Quoting and escaping are resolved *here* so that the one later phase that
//! cares — [`env`](crate::env) expansion — only has to ask "is this run
//! expandable?" and never re-examines quotes (`AGENTS.md` §2.2). A `$` that
//! was backslash-escaped or single-quoted becomes [`Segment::Literal`] and so
//! is never mistaken for an expansion.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::ParseError;

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
    /// `<` — redirect standard input from a file.
    Less,
    /// `>` — redirect standard output to a file, truncating it.
    Great,
    /// `>>` — redirect standard output to a file, appending.
    DoubleGreat,
}

/// Lex `line` into a token stream.
///
/// A `#` that begins a word starts a comment running to the end of the line;
/// mid-word it is an ordinary character.
///
/// # Errors
///
/// Returns [`ParseError::UnterminatedQuote`] for an unclosed quote and
/// [`ParseError::DanglingEscape`] for a line ending on a lone `\`.
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
            if let Some(op) = self.lex_operator() {
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

    fn lex_operator(&mut self) -> Option<Token> {
        let c = self.peek()?;
        let two = self.peek2();
        let (token, width) = match (c, two) {
            ('|', Some('|')) => (Token::OrIf, 2),
            ('&', Some('&')) => (Token::AndIf, 2),
            ('>', Some('>')) => (Token::DoubleGreat, 2),
            ('|', _) => (Token::Pipe, 1),
            ('&', _) => (Token::Ampersand, 1),
            (';', _) => (Token::Semicolon, 1),
            ('<', _) => (Token::Less, 1),
            ('>', _) => (Token::Great, 1),
            _ => return None,
        };
        self.pos += width;
        Some(token)
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
    use super::{tokenize, Segment, Token};
    use crate::error::ParseError;
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
                Token::DoubleGreat,
                expandable("b"),
                Token::Less,
                expandable("c"),
                Token::Pipe,
                expandable("d"),
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
