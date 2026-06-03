//! Character display width: how many terminal columns a glyph occupies.
//!
//! A curses application that draws non-ASCII text has to know that some
//! glyphs — CJK ideographs, Hangul syllables, fullwidth forms, most emoji —
//! occupy *two* terminal columns, not one. Getting this wrong shifts every
//! cell after the glyph by a column. This module is the one place that
//! answers the question (`AGENTS.md` §2.2): the [`crate::Window`] writer, the
//! [renderer], and a consumer laying out columns all measure through
//! [`char_width`] / [`str_width`], so they agree on where a glyph ends.
//!
//! The width table is first-party (`AGENTS.md` §2.12): a small, sorted set of
//! the Unicode "East Asian Wide / Fullwidth" ranges plus the common emoji
//! blocks. Every other scalar is one column. Combining marks are *not* given
//! zero width — the cell model stores one glyph per cell, so a lone combining
//! mark occupies its own cell; this is a documented, deliberate simplification
//! rather than a half-built grapheme model.
//!
//! [renderer]: mod@crate::render

/// The placeholder glyph stored in the trailing cell of a double-width glyph.
///
/// A wide glyph is written into its left cell; the cell to its right holds
/// this continuation marker so the grid stays rectangular (one [`rustos_vt::Cell`]
/// per column). The [renderer] never prints a continuation cell — the wide
/// glyph to its left already advances the terminal cursor across it — and a
/// consumer treats it as "covered by the glyph to my left".
///
/// [renderer]: mod@crate::render
pub const CONTINUATION: char = '\u{0}';

/// The sorted, non-overlapping ranges of scalar values that occupy two
/// terminal columns (Unicode East Asian Wide + Fullwidth, plus the common
/// pictographic/emoji blocks that terminals render double-width).
const WIDE_RANGES: &[(u32, u32)] = &[
    (0x1100, 0x115F),   // Hangul Jamo
    (0x2329, 0x232A),   // angle brackets
    (0x2E80, 0x303E),   // CJK radicals, Kangxi, CJK symbols
    (0x3041, 0x33FF),   // Hiragana, Katakana, CJK symbols and punctuation
    (0x3400, 0x4DBF),   // CJK Unified Ideographs Extension A
    (0x4E00, 0x9FFF),   // CJK Unified Ideographs
    (0xA000, 0xA4CF),   // Yi syllables
    (0xAC00, 0xD7A3),   // Hangul syllables
    (0xF900, 0xFAFF),   // CJK Compatibility Ideographs
    (0xFE10, 0xFE19),   // vertical forms
    (0xFE30, 0xFE6F),   // CJK Compatibility Forms, small form variants
    (0xFF00, 0xFF60),   // fullwidth forms
    (0xFFE0, 0xFFE6),   // fullwidth signs
    (0x1F300, 0x1F64F), // Misc Symbols and Pictographs + emoticons
    (0x1F900, 0x1F9FF), // Supplemental Symbols and Pictographs
    (0x20000, 0x3FFFD), // CJK Unified Ideographs Extension B and beyond
];

/// The number of terminal columns `ch` occupies: `2` for a double-width
/// glyph, `1` for everything else.
#[must_use]
pub fn char_width(ch: char) -> u16 {
    if is_wide(ch) {
        2
    } else {
        1
    }
}

/// Whether `ch` is a double-width glyph (East Asian Wide / Fullwidth or a
/// common pictograph).
#[must_use]
pub fn is_wide(ch: char) -> bool {
    let code = ch as u32;
    WIDE_RANGES
        .binary_search_by(|&(lo, hi)| {
            if code < lo {
                core::cmp::Ordering::Greater
            } else if code > hi {
                core::cmp::Ordering::Less
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// The total number of terminal columns `text` occupies, summing
/// [`char_width`] over its characters.
///
/// A consumer uses this to fit text to a column budget without splitting a
/// double-width glyph; see [`truncate_to_width`].
#[must_use]
pub fn str_width(text: &str) -> usize {
    text.chars().map(|ch| usize::from(char_width(ch))).sum()
}

/// The longest prefix of `text` whose [`str_width`] does not exceed `cols`
/// columns.
///
/// A double-width glyph that would straddle the limit is dropped whole rather
/// than half-printed, so the result never exceeds `cols` columns and never
/// ends on a continuation cell.
#[must_use]
pub fn truncate_to_width(text: &str, cols: usize) -> &str {
    let mut used = 0usize;
    for (offset, ch) in text.char_indices() {
        let next = used + usize::from(char_width(ch));
        if next > cols {
            return &text[..offset];
        }
        used = next;
    }
    text
}
