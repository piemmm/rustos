//! The search pattern engine: the vim pattern subset `/`, `?`, `n`, `N`,
//! `*`, and `:s` match with.
//!
//! Supported syntax (the magic-mode core):
//!
//! * literal characters (multi-byte text matches per character),
//! * `.` — any single character,
//! * `*` — zero or more of the preceding atom (greedy, backtracking),
//! * `^` at the start / `$` at the end — line anchors,
//! * `[...]` — a character class with ranges and leading-`^` negation,
//! * `\<` / `\>` — word start / word end boundaries,
//! * `\.` `\*` `\[` `\]` `\^` `\$` `\\` — escaped literals.
//!
//! Everything else vim's engine accepts (`\+`, `\(`, alternation, character
//! class names, multi-byte class semantics, offsets) is staged in
//! `plans/VIM.md`. An unsupported or malformed pattern fails closed with a
//! typed error, never a guess.
//!
//! The matcher is a compiled node list walked by a backtracking scanner.
//! Every atom consumes exactly one character, so recursion depth is bounded
//! by the pattern length; nested stars can still multiply backtracking, so
//! every scan runs under the fixed `MATCH_BUDGET` step bound and fails
//! closed (reports no match) when a pathological pattern exhausts it —
//! there is no unbounded blowup on hostile input.

use alloc::string::String;
use alloc::vec::Vec;

/// One matchable atom (consumes exactly one character).
#[derive(Clone, Debug, Eq, PartialEq)]
enum Atom {
    /// A literal character.
    Literal(char),
    /// `.` — any character.
    Any,
    /// `[...]` — a set of characters and ranges, possibly negated.
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

/// One `[...]` member.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ClassItem {
    /// A single character.
    Char(char),
    /// An inclusive `a-z` range.
    Range(char, char),
}

/// One compiled pattern element.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Node {
    /// An atom that must match exactly once.
    One(Atom),
    /// An atom repeated zero or more times (`*`), matched greedily.
    Star(Atom),
    /// `\<` — the position must begin a word.
    WordStart,
    /// `\>` — the position must end a word.
    WordEnd,
}

/// Why a pattern failed to compile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternError {
    /// The pattern text is empty.
    Empty,
    /// A `[` class was never closed.
    UnclosedClass,
    /// A trailing `\` escapes nothing.
    TrailingEscape,
    /// `\x` for an `x` this engine does not support (vim syntax staged in
    /// `plans/VIM.md`).
    UnsupportedEscape(char),
    /// A `*` with no preceding atom to repeat.
    DanglingStar,
}

/// A compiled search pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pattern {
    nodes: Vec<Node>,
    anchor_start: bool,
    anchor_end: bool,
    source: String,
}

/// A word character for the `\<` / `\>` boundaries (vim's `iskeyword`
/// default: letters, digits, underscore).
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The matcher's step bound per line scan. Nested stars backtrack
/// multiplicatively in the worst case; a scan that exhausts this budget
/// reports no match rather than stalling the editor. The bound is a
/// security defence on untrusted input, not a tunable capacity — it is
/// generous for any real pattern over a text line.
const MATCH_BUDGET: usize = 1 << 20;

impl Pattern {
    /// Compile `text` into a pattern, failing closed on syntax this engine
    /// does not support.
    pub fn compile(text: &str) -> Result<Pattern, PatternError> {
        if text.is_empty() {
            return Err(PatternError::Empty);
        }
        let chars: Vec<char> = text.chars().collect();
        let mut nodes: Vec<Node> = Vec::new();
        let mut anchor_start = false;
        let mut anchor_end = false;
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            match ch {
                '^' if i == 0 => anchor_start = true,
                '$' if i + 1 == chars.len() => anchor_end = true,
                '.' => nodes.push(Node::One(Atom::Any)),
                '*' => {
                    // `*` repeats the preceding atom; at the start of a
                    // pattern vim treats it as a literal star.
                    match nodes.pop() {
                        Some(Node::One(atom)) => nodes.push(Node::Star(atom)),
                        Some(node @ (Node::Star(_) | Node::WordStart | Node::WordEnd)) => {
                            nodes.push(node);
                            return Err(PatternError::DanglingStar);
                        }
                        None => nodes.push(Node::One(Atom::Literal('*'))),
                    }
                }
                '[' => {
                    let (atom, next) = compile_class(&chars, i)?;
                    nodes.push(Node::One(atom));
                    i = next;
                    continue;
                }
                '\\' => {
                    let Some(&escaped) = chars.get(i + 1) else {
                        return Err(PatternError::TrailingEscape);
                    };
                    match escaped {
                        '<' => nodes.push(Node::WordStart),
                        '>' => nodes.push(Node::WordEnd),
                        '.' | '*' | '[' | ']' | '^' | '$' | '\\' | '/' => {
                            nodes.push(Node::One(Atom::Literal(escaped)));
                        }
                        other => return Err(PatternError::UnsupportedEscape(other)),
                    }
                    i += 2;
                    continue;
                }
                literal => nodes.push(Node::One(Atom::Literal(literal))),
            }
            i += 1;
        }
        Ok(Pattern {
            nodes,
            anchor_start,
            anchor_end,
            source: String::from(text),
        })
    }

    /// The pattern's source text (echoed by the search prompt and reused by
    /// `n`/`N`).
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The first match in `line` at or after character column `from`.
    /// Returns the matched span as `(start, end)` character columns, `end`
    /// exclusive.
    #[must_use]
    pub fn find_at(&self, line: &str, from: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = line.chars().collect();
        let starts: Vec<usize> = if self.anchor_start {
            if from == 0 {
                alloc::vec![0]
            } else {
                Vec::new()
            }
        } else {
            (from..=chars.len()).collect()
        };
        let mut budget = MATCH_BUDGET;
        for start in starts {
            if let Some(end) = self.match_here(&chars, start, 0, &mut budget) {
                if !self.anchor_end || end == chars.len() {
                    return Some((start, end));
                }
            }
        }
        None
    }

    /// The last match in `line` that starts strictly before character
    /// column `before`.
    #[must_use]
    pub fn rfind_before(&self, line: &str, before: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = line.chars().collect();
        let limit = before.min(chars.len() + 1);
        let mut best = None;
        let mut budget = MATCH_BUDGET;
        for start in 0..limit {
            if self.anchor_start && start != 0 {
                break;
            }
            if let Some(end) = self.match_here(&chars, start, 0, &mut budget) {
                if !self.anchor_end || end == chars.len() {
                    best = Some((start, end));
                }
            }
        }
        best
    }

    /// Match the node tail beginning at `node` against `chars` starting at
    /// character index `at`; returns the end index of a successful match.
    /// Every call spends one unit of `budget`; an exhausted budget fails
    /// the match closed.
    fn match_here(
        &self,
        chars: &[char],
        at: usize,
        node: usize,
        budget: &mut usize,
    ) -> Option<usize> {
        *budget = budget.checked_sub(1)?;
        let Some(current) = self.nodes.get(node) else {
            return Some(at);
        };
        match current {
            Node::One(atom) => {
                let ch = chars.get(at)?;
                if atom_matches(atom, *ch) {
                    self.match_here(chars, at + 1, node + 1, budget)
                } else {
                    None
                }
            }
            Node::Star(atom) => {
                // Greedy: consume the longest run, then back off until the
                // tail matches.
                let mut end = at;
                while end < chars.len() && atom_matches(atom, chars[end]) {
                    end += 1;
                }
                loop {
                    if let Some(done) = self.match_here(chars, end, node + 1, budget) {
                        return Some(done);
                    }
                    if end == at || *budget == 0 {
                        return None;
                    }
                    end -= 1;
                }
            }
            Node::WordStart => {
                let here = chars.get(at).copied().is_some_and(is_word);
                let before = at > 0 && chars.get(at - 1).copied().is_some_and(is_word);
                if here && !before {
                    self.match_here(chars, at, node + 1, budget)
                } else {
                    None
                }
            }
            Node::WordEnd => {
                let before = at > 0 && chars.get(at - 1).copied().is_some_and(is_word);
                let here = chars.get(at).copied().is_some_and(is_word);
                if before && !here {
                    self.match_here(chars, at, node + 1, budget)
                } else {
                    None
                }
            }
        }
    }
}

/// Whether one atom matches one character.
fn atom_matches(atom: &Atom, ch: char) -> bool {
    match atom {
        Atom::Literal(literal) => *literal == ch,
        Atom::Any => true,
        Atom::Class { negated, items } => {
            let hit = items.iter().any(|item| match item {
                ClassItem::Char(single) => *single == ch,
                ClassItem::Range(lo, hi) => (*lo..=*hi).contains(&ch),
            });
            hit != *negated
        }
    }
}

/// Compile a `[...]` class beginning at `chars[open]` (the `[`). Returns
/// the atom and the index just past the closing `]`.
fn compile_class(chars: &[char], open: usize) -> Result<(Atom, usize), PatternError> {
    let mut i = open + 1;
    let negated = chars.get(i) == Some(&'^');
    if negated {
        i += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    // A `]` immediately after the opener (or the negation) is a literal
    // member, as in vim.
    let mut first = true;
    loop {
        let Some(&ch) = chars.get(i) else {
            return Err(PatternError::UnclosedClass);
        };
        if ch == ']' && !first {
            return Ok((Atom::Class { negated, items }, i + 1));
        }
        first = false;
        // A range needs `x-y` with `y` not the closer.
        if chars.get(i + 1) == Some(&'-') {
            if let Some(&hi) = chars.get(i + 2) {
                if hi != ']' {
                    let (lo, hi) = if ch <= hi { (ch, hi) } else { (hi, ch) };
                    items.push(ClassItem::Range(lo, hi));
                    i += 3;
                    continue;
                }
            }
        }
        items.push(ClassItem::Char(ch));
        i += 1;
    }
}

/// Build the `*` / `#`-style whole-word pattern for the word `text` (the
/// `\<word\>` form `*` uses).
#[must_use]
pub fn whole_word_pattern(text: &str) -> String {
    let mut source = String::from("\\<");
    for ch in text.chars() {
        if matches!(ch, '.' | '*' | '[' | ']' | '^' | '$' | '\\' | '/') {
            source.push('\\');
        }
        source.push(ch);
    }
    source.push_str("\\>");
    source
}
