//! TAIRiX shared filename-glob matcher (`lib/glob`).
//!
//! Several TAIRiX components need to match a name against a shell-style
//! wildcard pattern: the shell's filename generation and its interactive
//! completion first, and later the file browser, `find`-class tooling, and any
//! other place that resolves a wildcard. That matching is *identical* wherever
//! it happens, so it lives here once and every consumer imports it, rather than
//! each growing a private copy.
//!
//! # Glob, not full regular expressions
//!
//! This crate implements POSIX shell **globbing**, deliberately not a general
//! regular-expression engine. Globs are what a shell actually expands, and
//! they can be matched in bounded time with a simple, backtracking-free
//! algorithm. A full regex dialect (alternation, capture groups, unbounded
//! repetition) invites catastrophic backtracking on a hostile pattern, and no
//! current consumer needs it; if one ever does, it is a separate engine, not a
//! feature bolted onto this one.
//!
//! # Pattern syntax
//!
//! A pattern is matched against a whole candidate string (it is anchored: the
//! pattern must match the entire candidate, not a substring).
//!
//! - `*` matches any run of zero or more characters.
//! - `?` matches exactly one character.
//! - `[...]` is a *bracket expression* matching one character:
//!   - `[abc]` matches any of `a`, `b`, `c`.
//!   - `[a-z]` matches any character in the inclusive range `a..=z`.
//!   - `[!abc]` (or `[^abc]`) matches any character that is *not* listed.
//!   - A `]` immediately after `[`, `[!`, or `[^` is a literal `]`.
//!   - A `-` at the start or end of the expression is a literal `-`.
//! - `\` escapes the following character, so `\*` matches a literal `*`.
//!
//! Characters have no special meaning to the matcher beyond the above; in
//! particular the path separator `/` is an ordinary character here. A consumer
//! globbing a multi-segment path splits it on `/` and matches each segment's
//! pattern against each name, so path-separator policy stays with the caller
//! and is not duplicated in the matcher.
//!
//! # Bounds and fail-closed behaviour
//!
//! A pattern is untrusted input (a user or a script supplies it), so every
//! dimension is bounded as a security limit, not a growable capacity:
//! [`MAX_PATTERN_LEN`], [`MAX_TOKENS`], and [`MAX_CLASS_ITEMS`]. A pattern that
//! exceeds a bound, or is malformed (an unterminated bracket expression, a
//! trailing escape, an empty or reversed bracket range), is rejected by
//! [`Pattern::new`] with a typed [`GlobError`] — it is never silently
//! "fixed up" or matched as if it were literal.
//!
//! Compilation is the only fallible step. [`Pattern::matches`] cannot fail and
//! never panics: a compiled pattern always yields a `bool`. Matching runs in
//! `O(tokens * candidate-chars)` time with no recursion and no exponential
//! backtracking, so neither a hostile pattern nor a hostile candidate can
//! trigger runaway work.
//!
//! # Example
//!
//! ```
//! use tairix_glob::Pattern;
//!
//! let pat = Pattern::new("*.rs").expect("valid pattern");
//! assert!(pat.matches("lib.rs"));
//! assert!(!pat.matches("lib.rss"));
//!
//! let pat = Pattern::new("img_[0-9][0-9].???").expect("valid pattern");
//! assert!(pat.matches("img_04.png"));
//! assert!(!pat.matches("img_4.png"));
//!
//! assert!(Pattern::new("[unterminated").is_err());
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// Largest pattern accepted, in bytes of UTF-8.
///
/// A pattern is untrusted input, so its length is a fixed security bound
/// rather than a capacity that grows on demand. A pattern longer than this is
/// rejected with [`GlobError::PatternTooLong`].
pub const MAX_PATTERN_LEN: usize = 4096;

/// Largest number of top-level tokens a pattern may compile to.
///
/// Each literal character, `?`, `*`, and bracket expression is one token. This
/// bounds both the compiled size and the per-match work, and a pattern that
/// needs more tokens is rejected with [`GlobError::TooManyTokens`].
pub const MAX_TOKENS: usize = 1024;

/// Largest number of items (single characters plus ranges) in one bracket
/// expression.
///
/// Exceeding it rejects the pattern with [`GlobError::TooManyTokens`], so a
/// bracket expression cannot be used to smuggle unbounded work past
/// [`MAX_TOKENS`].
pub const MAX_CLASS_ITEMS: usize = 256;

/// Why a pattern was rejected at compile time.
///
/// A malformed or over-large pattern always fails closed with one of these; it
/// is never matched as if it were a literal string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobError {
    /// The pattern was longer than [`MAX_PATTERN_LEN`] bytes.
    PatternTooLong,
    /// The pattern needed more than [`MAX_TOKENS`] tokens, or a bracket
    /// expression more than [`MAX_CLASS_ITEMS`] items.
    TooManyTokens,
    /// A bracket expression was opened with `[` but never closed with `]`.
    UnterminatedClass,
    /// A bracket range `a-b` had its endpoints out of order (`b` below `a`).
    ReversedRange,
    /// The pattern ended with a `\` that escaped nothing.
    DanglingEscape,
}

impl fmt::Display for GlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            GlobError::PatternTooLong => "glob pattern exceeds the maximum length",
            GlobError::TooManyTokens => "glob pattern has too many tokens",
            GlobError::UnterminatedClass => "glob bracket expression is not closed",
            GlobError::ReversedRange => "glob bracket range endpoints are out of order",
            GlobError::DanglingEscape => "glob pattern ends with a dangling escape",
        };
        f.write_str(msg)
    }
}

/// One member of a bracket expression.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClassItem {
    /// A single literal character.
    Char(char),
    /// An inclusive character range `lo..=hi` (with `lo <= hi`).
    Range(char, char),
}

/// A compiled bracket expression (`[...]`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Class {
    /// Whether the sense is inverted (`[!...]` / `[^...]`).
    negated: bool,
    /// The members; a character matches if it is (not, when `negated`) in one.
    items: Vec<ClassItem>,
}

impl Class {
    /// Whether `ch` is accepted by this bracket expression.
    fn matches(&self, ch: char) -> bool {
        let mut hit = false;
        for item in &self.items {
            let member = match *item {
                ClassItem::Char(c) => c == ch,
                ClassItem::Range(lo, hi) => lo <= ch && ch <= hi,
            };
            if member {
                hit = true;
                break;
            }
        }
        hit ^ self.negated
    }
}

/// One unit of a compiled pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// A literal character that must appear verbatim.
    Literal(char),
    /// `?`: any single character.
    AnyOne,
    /// `*`: any run of zero or more characters.
    AnySequence,
    /// `[...]`: one character accepted by the bracket expression.
    Class(Class),
}

/// A compiled glob pattern, ready to match many candidates.
///
/// Build one with [`Pattern::new`]; the fallible compile step validates and
/// bounds the pattern. Matching with [`Pattern::matches`] is then infallible
/// and allocation-light.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    tokens: Vec<Token>,
}

impl Pattern {
    /// Compile `pattern` into a matcher.
    ///
    /// # Errors
    ///
    /// Returns a [`GlobError`] if the pattern exceeds a bound (it is too long
    /// or has too many tokens) or is malformed (an unterminated bracket
    /// expression, a reversed bracket range, or a dangling escape). See the
    /// [`GlobError`] variants for the exact cases.
    pub fn new(pattern: &str) -> Result<Self, GlobError> {
        if pattern.len() > MAX_PATTERN_LEN {
            return Err(GlobError::PatternTooLong);
        }

        let chars: Vec<char> = pattern.chars().collect();
        let mut tokens: Vec<Token> = Vec::new();
        let mut i = 0;

        while i < chars.len() {
            if tokens.len() >= MAX_TOKENS {
                return Err(GlobError::TooManyTokens);
            }
            match chars[i] {
                '*' => {
                    // Collapse a run of `*` to a single token: `**` matches
                    // exactly what `*` does, and one token per run keeps the
                    // match loop's work bounded.
                    if !matches!(tokens.last(), Some(Token::AnySequence)) {
                        tokens.push(Token::AnySequence);
                    }
                    i += 1;
                }
                '?' => {
                    tokens.push(Token::AnyOne);
                    i += 1;
                }
                '\\' => {
                    let Some(&escaped) = chars.get(i + 1) else {
                        return Err(GlobError::DanglingEscape);
                    };
                    tokens.push(Token::Literal(escaped));
                    i += 2;
                }
                '[' => {
                    let (class, consumed) = parse_class(&chars, i)?;
                    tokens.push(Token::Class(class));
                    i += consumed;
                }
                literal => {
                    tokens.push(Token::Literal(literal));
                    i += 1;
                }
            }
        }

        Ok(Self { tokens })
    }

    /// Whether `candidate` matches this pattern in full.
    ///
    /// Anchored: the whole candidate must match, not a prefix or substring.
    /// Never panics and never allocates unboundedly; it runs in
    /// `O(tokens * candidate-chars)` with no backtracking blow-up.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        let text: Vec<char> = candidate.chars().collect();
        matches_tokens(&self.tokens, &text)
    }
}

/// Parse a bracket expression starting at `chars[start] == '['`.
///
/// Returns the compiled [`Class`] and the number of characters consumed
/// (including the opening `[` and closing `]`).
fn parse_class(chars: &[char], start: usize) -> Result<(Class, usize), GlobError> {
    let mut i = start + 1;

    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }

    let mut items: Vec<ClassItem> = Vec::new();
    let mut first = true;

    while i < chars.len() {
        let c = chars[i];

        // A `]` closes the expression unless it is the very first member, in
        // which case it is a literal `]`. Because reaching a non-first
        // position always pushed at least one member, a closed expression is
        // never empty.
        if c == ']' && !first {
            return Ok((Class { negated, items }, i - start + 1));
        }
        first = false;

        if items.len() >= MAX_CLASS_ITEMS {
            return Err(GlobError::TooManyTokens);
        }

        // A range `c-d`, but only when the `-` is followed by a member (a `-`
        // just before the closing `]`, or at the end of input, is a literal
        // `-`).
        if chars.get(i + 1) == Some(&'-') {
            if let Some(&hi) = chars.get(i + 2) {
                if hi != ']' {
                    if hi < c {
                        return Err(GlobError::ReversedRange);
                    }
                    items.push(ClassItem::Range(c, hi));
                    i += 3;
                    continue;
                }
            }
        }

        items.push(ClassItem::Char(c));
        i += 1;
    }

    Err(GlobError::UnterminatedClass)
}

/// Match a compiled token slice against a candidate's characters.
///
/// Uses the classic two-pointer glob algorithm: on a mismatch it backtracks
/// only to the most recent `*`, advancing what that `*` consumed by one. There
/// is no recursion and no re-exploration of already-rejected prefixes, so the
/// worst case is `O(tokens * text)`.
fn matches_tokens(tokens: &[Token], text: &[char]) -> bool {
    let mut ti = 0;
    let mut ci = 0;
    // The most recent `*` token and the candidate position it was anchored at,
    // so a later mismatch can let that `*` swallow one more character.
    let mut star: Option<(usize, usize)> = None;

    while ci < text.len() {
        match tokens.get(ti) {
            Some(Token::AnySequence) => {
                star = Some((ti, ci));
                ti += 1;
                continue;
            }
            Some(Token::AnyOne) => {
                ti += 1;
                ci += 1;
                continue;
            }
            Some(Token::Literal(c)) if *c == text[ci] => {
                ti += 1;
                ci += 1;
                continue;
            }
            Some(Token::Class(class)) if class.matches(text[ci]) => {
                ti += 1;
                ci += 1;
                continue;
            }
            _ => {}
        }

        // Mismatch (or tokens exhausted before the text): resume from the last
        // `*`, letting it consume one more character. With no `*` to fall back
        // on, the candidate cannot match.
        match star {
            Some((star_token, matched_upto)) => {
                ti = star_token + 1;
                ci = matched_upto + 1;
                star = Some((star_token, matched_upto + 1));
            }
            None => return false,
        }
    }

    // The text is consumed; the match succeeds only if the remaining tokens are
    // all `*` (each of which can match the empty run).
    tokens[ti..].iter().all(|t| matches!(t, Token::AnySequence))
}

#[cfg(test)]
mod tests {
    use super::{GlobError, Pattern, MAX_CLASS_ITEMS, MAX_PATTERN_LEN, MAX_TOKENS};
    use alloc::format;
    use alloc::string::String;

    fn matches(pattern: &str, candidate: &str) -> bool {
        Pattern::new(pattern)
            .expect("pattern compiles")
            .matches(candidate)
    }

    #[test]
    fn empty_pattern_matches_only_empty() {
        assert!(matches("", ""));
        assert!(!matches("", "x"));
    }

    #[test]
    fn literal_is_anchored() {
        assert!(matches("readme.md", "readme.md"));
        assert!(!matches("readme.md", "readme.markdown"));
        assert!(!matches("readme.md", "areadme.md"));
    }

    #[test]
    fn question_mark_matches_exactly_one() {
        assert!(matches("f?o", "foo"));
        assert!(matches("f?o", "fxo"));
        assert!(!matches("f?o", "fo"));
        assert!(!matches("f?o", "fooo"));
    }

    #[test]
    fn star_matches_any_run_including_empty() {
        assert!(matches("*", ""));
        assert!(matches("*", "anything at all"));
        assert!(matches("*.rs", "lib.rs"));
        assert!(matches("a*z", "az"));
        assert!(matches("a*z", "abcz"));
        assert!(!matches("a*z", "abc"));
    }

    #[test]
    fn multiple_stars_behave_like_one() {
        assert!(matches("a**b", "ab"));
        assert!(matches("a**b", "axxxb"));
        assert!(matches("***", "whatever"));
    }

    #[test]
    fn star_backtracks_correctly() {
        // The greedy `*` must give characters back so the trailing literal can
        // still match.
        assert!(matches("*.tar.gz", "archive.tar.gz"));
        assert!(matches("*abc*abc*", "xxabcyyabczz"));
        assert!(!matches("*abc*abc*", "xxabcyy"));
    }

    #[test]
    fn bracket_class_members_and_ranges() {
        assert!(matches("[abc]", "b"));
        assert!(!matches("[abc]", "d"));
        assert!(matches("[a-z]", "m"));
        assert!(!matches("[a-z]", "M"));
        assert!(matches("img_[0-9][0-9].png", "img_42.png"));
        assert!(!matches("img_[0-9][0-9].png", "img_4.png"));
    }

    #[test]
    fn bracket_negation() {
        assert!(matches("[!0-9]", "a"));
        assert!(!matches("[!0-9]", "5"));
        assert!(matches("[^abc]", "d"));
        assert!(!matches("[^abc]", "a"));
    }

    #[test]
    fn bracket_literal_close_and_dash() {
        // A `]` right after `[` is a literal `]`.
        assert!(matches("[]]", "]"));
        // A `-` at the end is a literal `-`.
        assert!(matches("[a-]", "-"));
        assert!(matches("[a-]", "a"));
        assert!(!matches("[a-]", "b"));
    }

    #[test]
    fn escaping_metacharacters() {
        assert!(matches(r"\*", "*"));
        assert!(!matches(r"\*", "x"));
        assert!(matches(r"a\?b", "a?b"));
        assert!(!matches(r"a\?b", "axb"));
        assert!(matches(r"\[", "["));
    }

    #[test]
    fn unicode_candidates() {
        assert!(matches("caf?", "café"));
        assert!(matches("*é", "café"));
        assert!(matches("[α-ω]", "β"));
    }

    #[test]
    fn malformed_patterns_fail_closed() {
        assert_eq!(Pattern::new("[abc"), Err(GlobError::UnterminatedClass));
        assert_eq!(Pattern::new("[]"), Err(GlobError::UnterminatedClass));
        assert_eq!(Pattern::new("[!]"), Err(GlobError::UnterminatedClass));
        assert_eq!(Pattern::new("[z-a]"), Err(GlobError::ReversedRange));
        assert_eq!(Pattern::new(r"trailing\"), Err(GlobError::DanglingEscape));
    }

    #[test]
    fn empty_class_after_immediate_close_is_reported() {
        // `[]]` is a class matching `]`, but `[]` alone never closes and is
        // reported as unterminated (the first `]` is a literal member).
        assert!(Pattern::new("[]]").is_ok());
        assert_eq!(Pattern::new("[]"), Err(GlobError::UnterminatedClass));
    }

    #[test]
    fn pattern_length_bound_is_enforced() {
        // A run of `*` collapses to one token, so the length bound — not the
        // token bound — is what this exercises. At the limit it compiles; one
        // byte over is rejected before tokenising.
        let ok: String = "*".repeat(MAX_PATTERN_LEN);
        assert!(Pattern::new(&ok).is_ok());
        let too_long: String = "*".repeat(MAX_PATTERN_LEN + 1);
        assert_eq!(Pattern::new(&too_long), Err(GlobError::PatternTooLong));
    }

    #[test]
    fn token_count_bound_is_enforced() {
        // `?` is one token each; one past the limit is rejected.
        let ok: String = "?".repeat(MAX_TOKENS);
        assert!(Pattern::new(&ok).is_ok());
        let too_many: String = "?".repeat(MAX_TOKENS + 1);
        assert_eq!(Pattern::new(&too_many), Err(GlobError::TooManyTokens));
    }

    #[test]
    fn class_item_bound_is_enforced() {
        let mut body = String::from("[");
        for _ in 0..=MAX_CLASS_ITEMS {
            body.push('a');
        }
        body.push(']');
        assert_eq!(Pattern::new(&body), Err(GlobError::TooManyTokens));
    }

    #[test]
    fn separator_is_an_ordinary_character() {
        // The matcher does not treat `/` specially; segment policy is the
        // caller's job.
        assert!(matches("a/*/c", "a/b/c"));
        assert!(matches("*", "a/b/c"));
    }

    #[test]
    fn error_display_is_human_readable() {
        assert!(!format!("{}", GlobError::UnterminatedClass).is_empty());
    }
}
