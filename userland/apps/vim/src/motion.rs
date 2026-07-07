//! Motions and text objects: pure position arithmetic over a [`Buffer`].
//!
//! Every function here is side-effect-free: it takes the buffer and a start
//! [`Position`] and returns where the motion lands (or the span a text
//! object selects). The normal-mode interpreter applies the same motion to
//! move the cursor and to bound an operator, exactly as vim does, so the
//! two can never disagree.
//!
//! Word motions follow vim's three character classes: *word* characters
//! (letters, digits, underscore), *punctuation* (every other non-blank),
//! and *blanks*. `w`/`b`/`e` step between class runs; `W`/`B`/`E` treat
//! every non-blank run as one WORD.

use alloc::string::String;
use alloc::vec::Vec;

use crate::buffer::{Buffer, Position};

/// How an operator treats the span a motion covers, vim's motion "kinds".
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MotionKind {
    /// Up to but excluding the target character (`w`, `0`, `{`, …).
    Exclusive,
    /// Including the target character (`e`, `$`, `f`, `%`, …).
    Inclusive,
    /// Whole lines (`j`, `k`, `G`, `gg`, …).
    Linewise,
}

/// A motion's landing point and how an operator spans to it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MotionTarget {
    /// Where the motion lands.
    pub pos: Position,
    /// The span rule an operator applies over it.
    pub kind: MotionKind,
}

impl MotionTarget {
    /// An exclusive charwise target.
    #[must_use]
    pub const fn exclusive(pos: Position) -> MotionTarget {
        MotionTarget {
            pos,
            kind: MotionKind::Exclusive,
        }
    }

    /// An inclusive charwise target.
    #[must_use]
    pub const fn inclusive(pos: Position) -> MotionTarget {
        MotionTarget {
            pos,
            kind: MotionKind::Inclusive,
        }
    }

    /// A linewise target.
    #[must_use]
    pub const fn linewise(pos: Position) -> MotionTarget {
        MotionTarget {
            pos,
            kind: MotionKind::Linewise,
        }
    }
}

/// vim's character classes for the word motions.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CharClass {
    Blank,
    Word,
    Punct,
}

/// Classify one character: word (alphanumeric or `_`), blank, or
/// punctuation (everything else).
fn class_of(ch: char) -> CharClass {
    if ch == ' ' || ch == '\t' {
        CharClass::Blank
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Classify for the WORD motions: blank or non-blank.
fn big_class_of(ch: char) -> CharClass {
    if ch == ' ' || ch == '\t' {
        CharClass::Blank
    } else {
        CharClass::Word
    }
}

/// The line's characters (positions are character-indexed).
fn chars_of(buffer: &Buffer, line: usize) -> Vec<char> {
    buffer.line(line).chars().collect()
}

/// The column of the first non-blank character of `line` (0 for an
/// all-blank or empty line — vim parks on the last blank there; column 0 is
/// the honest clamp for an empty line).
#[must_use]
pub fn first_non_blank(buffer: &Buffer, line: usize) -> usize {
    let chars = chars_of(buffer, line);
    chars
        .iter()
        .position(|&ch| class_of(ch) != CharClass::Blank)
        .unwrap_or(0)
}

/// Step `count` characters left on the current line (`h`), stopping at
/// column 0.
#[must_use]
pub fn left(pos: Position, count: usize) -> MotionTarget {
    MotionTarget::exclusive(Position::new(pos.line, pos.col.saturating_sub(count)))
}

/// Step `count` characters right on the current line (`l`), stopping on
/// the last character.
#[must_use]
pub fn right(buffer: &Buffer, pos: Position, count: usize) -> MotionTarget {
    let len = buffer.line_len(pos.line);
    let last = len.saturating_sub(1);
    MotionTarget::exclusive(Position::new(pos.line, (pos.col + count).min(last)))
}

/// Step `count` lines up (`k`); the column is the caller's sticky column,
/// clamped by the editor after the move.
#[must_use]
pub fn up(pos: Position, count: usize) -> MotionTarget {
    MotionTarget::linewise(Position::new(pos.line.saturating_sub(count), pos.col))
}

/// Step `count` lines down (`j`), stopping on the last line.
#[must_use]
pub fn down(buffer: &Buffer, pos: Position, count: usize) -> MotionTarget {
    let last = buffer.len_lines() - 1;
    MotionTarget::linewise(Position::new((pos.line + count).min(last), pos.col))
}

/// Column 0 of the current line (`0`).
#[must_use]
pub fn line_start(pos: Position) -> MotionTarget {
    MotionTarget::exclusive(Position::new(pos.line, 0))
}

/// The first non-blank of the current line (`^`).
#[must_use]
pub fn first_non_blank_motion(buffer: &Buffer, pos: Position) -> MotionTarget {
    MotionTarget::exclusive(Position::new(pos.line, first_non_blank(buffer, pos.line)))
}

/// The last character of the line `count - 1` lines down (`$`).
#[must_use]
pub fn line_end(buffer: &Buffer, pos: Position, count: usize) -> MotionTarget {
    let line = (pos.line + count.saturating_sub(1)).min(buffer.len_lines() - 1);
    let col = buffer.line_len(line).saturating_sub(1);
    MotionTarget::inclusive(Position::new(line, col))
}

/// Line `target` (1-based), first non-blank (`G` / `gg` / `:N`).
#[must_use]
pub fn goto_line(buffer: &Buffer, target: usize) -> MotionTarget {
    let line = target.saturating_sub(1).min(buffer.len_lines() - 1);
    MotionTarget::linewise(Position::new(line, first_non_blank(buffer, line)))
}

/// Advance through the buffer one character at a time, crossing line ends
/// (a line end counts as one blank, as vim's word scanner sees it).
/// Returns the next position, or [`None`] at the end of the buffer.
pub(crate) fn step_forward(buffer: &Buffer, pos: Position) -> Option<Position> {
    if pos.col + 1 < buffer.line_len(pos.line).max(1) {
        return Some(Position::new(pos.line, pos.col + 1));
    }
    if pos.col + 1 == buffer.line_len(pos.line) || buffer.line_len(pos.line) == 0 {
        if pos.line + 1 < buffer.len_lines() {
            return Some(Position::new(pos.line + 1, 0));
        }
        return None;
    }
    Some(Position::new(pos.line, pos.col + 1))
}

/// Step back one character, crossing line starts. Returns [`None`] at the
/// start of the buffer.
pub(crate) fn step_back(buffer: &Buffer, pos: Position) -> Option<Position> {
    if pos.col > 0 {
        return Some(Position::new(pos.line, pos.col - 1));
    }
    if pos.line == 0 {
        return None;
    }
    let line = pos.line - 1;
    Some(Position::new(line, buffer.line_len(line).saturating_sub(1)))
}

/// The class of the character at `pos`, blank for a position past the line
/// end (an empty line reads as a blank of its own class).
fn class_at(buffer: &Buffer, pos: Position, big: bool) -> CharClass {
    let chars = chars_of(buffer, pos.line);
    match chars.get(pos.col) {
        Some(&ch) if big => big_class_of(ch),
        Some(&ch) => class_of(ch),
        None => CharClass::Blank,
    }
}

/// Whether `pos` is on an empty line (its own word for `w`/`b`).
fn on_empty_line(buffer: &Buffer, pos: Position) -> bool {
    buffer.line_len(pos.line) == 0
}

/// One `w`/`W` step: to the start of the next word.
fn word_forward_once(buffer: &Buffer, pos: Position, big: bool) -> Position {
    let start_class = class_at(buffer, pos, big);
    let mut at = pos;
    // Leave the current run (an empty line is left immediately).
    while let Some(next) = step_forward(buffer, at) {
        let left_line = next.line != at.line;
        at = next;
        if on_empty_line(buffer, at) {
            return at;
        }
        let class = class_at(buffer, at, big);
        if left_line || class != start_class || start_class == CharClass::Blank {
            if class != CharClass::Blank {
                return at;
            }
            break;
        }
    }
    // Skip blanks to the next word start.
    loop {
        if on_empty_line(buffer, at) {
            return at;
        }
        if class_at(buffer, at, big) != CharClass::Blank {
            return at;
        }
        match step_forward(buffer, at) {
            Some(next) => at = next,
            None => return at,
        }
    }
}

/// `w`/`W`: forward `count` word starts (exclusive).
#[must_use]
pub fn word_forward(buffer: &Buffer, pos: Position, count: usize, big: bool) -> MotionTarget {
    let mut at = pos;
    for _ in 0..count.max(1) {
        at = word_forward_once(buffer, at, big);
    }
    MotionTarget::exclusive(at)
}

/// One `e`/`E` step: to the end of the current-or-next word.
fn word_end_once(buffer: &Buffer, pos: Position, big: bool) -> Position {
    let mut at = pos;
    // Always advance at least one character.
    let Some(mut next) = step_forward(buffer, at) else {
        return at;
    };
    at = next;
    // Skip blanks (and empty lines) to reach a word.
    while class_at(buffer, at, big) == CharClass::Blank {
        match step_forward(buffer, at) {
            Some(step) => at = step,
            None => return at,
        }
    }
    // Walk to the last character of this run.
    let class = class_at(buffer, at, big);
    loop {
        next = match step_forward(buffer, at) {
            Some(step) => step,
            None => return at,
        };
        if next.line != at.line
            || class_at(buffer, next, big) != class
            || class_at(buffer, next, big) == CharClass::Blank
        {
            return at;
        }
        at = next;
    }
}

/// `e`/`E`: forward `count` word ends (inclusive).
#[must_use]
pub fn word_end(buffer: &Buffer, pos: Position, count: usize, big: bool) -> MotionTarget {
    let mut at = pos;
    for _ in 0..count.max(1) {
        at = word_end_once(buffer, at, big);
    }
    MotionTarget::inclusive(at)
}

/// One `b`/`B` step: back to the previous word start.
fn word_back_once(buffer: &Buffer, pos: Position, big: bool) -> Position {
    let Some(mut at) = step_back(buffer, pos) else {
        return pos;
    };
    // Skip blanks backwards (empty lines are their own stop).
    while class_at(buffer, at, big) == CharClass::Blank && !on_empty_line(buffer, at) {
        match step_back(buffer, at) {
            Some(step) => at = step,
            None => return at,
        }
    }
    if on_empty_line(buffer, at) {
        return at;
    }
    // Walk back to the start of this run.
    let class = class_at(buffer, at, big);
    while let Some(prev) = step_back(buffer, at) {
        if prev.line != at.line
            || class_at(buffer, prev, big) != class
            || on_empty_line(buffer, prev)
        {
            break;
        }
        at = prev;
    }
    at
}

/// `b`/`B`: back `count` word starts (exclusive).
#[must_use]
pub fn word_back(buffer: &Buffer, pos: Position, count: usize, big: bool) -> MotionTarget {
    let mut at = pos;
    for _ in 0..count.max(1) {
        at = word_back_once(buffer, at, big);
    }
    MotionTarget::exclusive(at)
}

/// `{` / `}`: back/forward `count` paragraph boundaries (an empty line, or
/// the buffer edge). Exclusive, like vim.
#[must_use]
pub fn paragraph(buffer: &Buffer, pos: Position, count: usize, forward: bool) -> MotionTarget {
    let mut line = pos.line;
    for _ in 0..count.max(1) {
        if forward {
            line += 1;
            while line < buffer.len_lines() && buffer.line_len(line) != 0 {
                line += 1;
            }
            if line >= buffer.len_lines() {
                line = buffer.len_lines() - 1;
                let col = buffer.line_len(line).saturating_sub(1);
                return MotionTarget::exclusive(Position::new(line, col));
            }
        } else {
            if line == 0 {
                return MotionTarget::exclusive(Position::new(0, 0));
            }
            line -= 1;
            while line > 0 && buffer.line_len(line) != 0 {
                line -= 1;
            }
        }
    }
    MotionTarget::exclusive(Position::new(line, 0))
}

/// `f`/`F`/`t`/`T`: the `count`-th occurrence of `target` on the current
/// line. `forward` picks the scan direction; `till` stops one short.
/// Returns [`None`] when the character is not there (the motion fails and
/// an operator over it does nothing, as in vim).
#[must_use]
pub fn find_char(
    buffer: &Buffer,
    pos: Position,
    target: char,
    count: usize,
    forward: bool,
    till: bool,
) -> Option<MotionTarget> {
    let chars = chars_of(buffer, pos.line);
    let mut found = pos.col;
    let mut remaining = count.max(1);
    if forward {
        let mut col = pos.col + 1;
        while col < chars.len() {
            if chars[col] == target {
                remaining -= 1;
                if remaining == 0 {
                    found = col;
                    break;
                }
            }
            col += 1;
        }
        if remaining != 0 {
            return None;
        }
        let landing = if till { found - 1 } else { found };
        Some(MotionTarget::inclusive(Position::new(pos.line, landing)))
    } else {
        let mut col = pos.col;
        while col > 0 {
            col -= 1;
            if chars[col] == target {
                remaining -= 1;
                if remaining == 0 {
                    let landing = if till { col + 1 } else { col };
                    return Some(MotionTarget::exclusive(Position::new(pos.line, landing)));
                }
            }
        }
        None
    }
}

/// `%`: the match of the bracket at (or after) the cursor. Scans the line
/// for the first bracket from the cursor, then walks the buffer balancing
/// nesting. Returns [`None`] with no bracket or no match.
#[must_use]
pub fn match_pair(buffer: &Buffer, pos: Position) -> Option<MotionTarget> {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    let chars = chars_of(buffer, pos.line);
    let (col, open, close, forward) =
        chars
            .iter()
            .enumerate()
            .skip(pos.col)
            .find_map(|(col, &ch)| {
                PAIRS.iter().find_map(|&(open, close)| {
                    if ch == open {
                        Some((col, open, close, true))
                    } else if ch == close {
                        Some((col, open, close, false))
                    } else {
                        None
                    }
                })
            })?;
    let mut depth = 0i64;
    let mut at = Position::new(pos.line, col);
    loop {
        let here: Vec<char> = chars_of(buffer, at.line);
        if let Some(&ch) = here.get(at.col) {
            if ch == open {
                depth += if forward { 1 } else { -1 };
            } else if ch == close {
                depth += if forward { -1 } else { 1 };
            }
            if depth == 0 {
                return Some(MotionTarget::inclusive(at));
            }
        }
        at = if forward {
            step_forward(buffer, at)?
        } else {
            step_back(buffer, at)?
        };
    }
}

/// A text object's span: both ends inclusive, charwise.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ObjectSpan {
    /// First position covered.
    pub start: Position,
    /// Last position covered (inclusive).
    pub end: Position,
}

/// `iw` / `aw`: the word (or blank run) under the cursor; `around` extends
/// over the following blanks (or, with none, the preceding ones).
#[must_use]
pub fn word_object(buffer: &Buffer, pos: Position, around: bool) -> Option<ObjectSpan> {
    let chars = chars_of(buffer, pos.line);
    if chars.is_empty() {
        return None;
    }
    let col = pos.col.min(chars.len() - 1);
    let class = class_of(chars[col]);
    let mut start = col;
    while start > 0 && class_of(chars[start - 1]) == class {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && class_of(chars[end + 1]) == class {
        end += 1;
    }
    if around && class != CharClass::Blank {
        let mut extended = end;
        while extended + 1 < chars.len() && class_of(chars[extended + 1]) == CharClass::Blank {
            extended += 1;
        }
        if extended == end {
            while start > 0 && class_of(chars[start - 1]) == CharClass::Blank {
                start -= 1;
            }
        } else {
            end = extended;
        }
    }
    Some(ObjectSpan {
        start: Position::new(pos.line, start),
        end: Position::new(pos.line, end),
    })
}

/// `i(`/`a(`/`i[`/… : the span inside (or around) the innermost `open` /
/// `close` pair enclosing the cursor. Multi-line, nesting-aware.
#[must_use]
pub fn pair_object(
    buffer: &Buffer,
    pos: Position,
    open: char,
    close: char,
    around: bool,
) -> Option<ObjectSpan> {
    // Walk back to the unbalanced opener, counting closers passed.
    let mut depth = 0i64;
    let mut at = pos;
    let opener = loop {
        let chars = chars_of(buffer, at.line);
        if let Some(&ch) = chars.get(at.col) {
            if ch == close && at != pos {
                depth += 1;
            } else if ch == open {
                if depth == 0 {
                    break at;
                }
                depth -= 1;
            }
        }
        at = step_back(buffer, at)?;
    };
    // Walk forward from the opener to its match.
    depth = 0;
    let mut at = opener;
    let closer = loop {
        let chars = chars_of(buffer, at.line);
        if let Some(&ch) = chars.get(at.col) {
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    break at;
                }
            }
        }
        at = step_forward(buffer, at)?;
    };
    if around {
        return Some(ObjectSpan {
            start: opener,
            end: closer,
        });
    }
    let start = step_forward(buffer, opener)?;
    let end = step_back(buffer, closer)?;
    if start > end {
        return None;
    }
    Some(ObjectSpan { start, end })
}

/// `i"` / `a"` (and `'`): the span inside (or around) the quote pair on the
/// current line whose run covers or follows the cursor.
#[must_use]
pub fn quote_object(
    buffer: &Buffer,
    pos: Position,
    quote: char,
    around: bool,
) -> Option<ObjectSpan> {
    let chars = chars_of(buffer, pos.line);
    let mut positions: Vec<usize> = Vec::new();
    for (col, &ch) in chars.iter().enumerate() {
        if ch == quote {
            positions.push(col);
        }
    }
    // Pair quotes left to right; pick the first pair that ends at or after
    // the cursor.
    let mut it = positions.chunks_exact(2);
    let (open, close) = it.find(|pair| pair[1] >= pos.col).map(|p| (p[0], p[1]))?;
    if around {
        return Some(ObjectSpan {
            start: Position::new(pos.line, open),
            end: Position::new(pos.line, close),
        });
    }
    // An empty quote pair has no inner span.
    if open + 1 == close {
        return None;
    }
    Some(ObjectSpan {
        start: Position::new(pos.line, open + 1),
        end: Position::new(pos.line, close - 1),
    })
}

/// The joined text of a charwise span, used by yanks: whole lines between
/// the ends, partial first and last lines.
#[must_use]
pub fn span_text(buffer: &Buffer, start: Position, end: Position) -> Vec<String> {
    if start.line == end.line {
        let chars = chars_of(buffer, start.line);
        let hi = (end.col + 1).min(chars.len());
        let lo = start.col.min(hi);
        return alloc::vec![chars[lo..hi].iter().collect()];
    }
    let mut parts = Vec::new();
    let first = chars_of(buffer, start.line);
    parts.push(first[start.col.min(first.len())..].iter().collect());
    for line in start.line + 1..end.line {
        parts.push(String::from(buffer.line(line)));
    }
    let last = chars_of(buffer, end.line);
    parts.push(last[..(end.col + 1).min(last.len())].iter().collect());
    parts
}
