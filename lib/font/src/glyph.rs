//! Glyph lookup over the generated Inconsolata EX + M PLUS 1 Code atlas.
//!
//! [`lookup`] maps any `char` to its atlas cell by binary search over the
//! generated codepoint ranges ([`crate::atlas::RANGES`]); a scalar the face
//! does not cover falls back to the U+FFFD replacement glyph
//! ([`Glyph::fallback`]) so unsupported text is visibly wrong rather than
//! silently dropped. A [`Glyph`] hands out per-pixel 4-bit coverage values
//! decoded from the packed payload; every access is bounds-checked, so
//! malformed coordinates read as transparent rather than out of bounds.

use crate::atlas;

/// One glyph bitmap: up to [`atlas::GLYPH_WIDTH`] × [`atlas::CELL_HEIGHT`]
/// pixels of 4-bit coverage. Narrow glyphs leave the second cell transparent;
/// full-width glyphs may cover it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Glyph {
    /// The glyph's [`atlas::BYTES_PER_GLYPH`] packed bytes: row major, two
    /// pixels per byte, left pixel in the high nibble.
    data: &'static [u8],
}

/// Packed bytes per glyph row (two 4-bit pixels per byte).
const BYTES_PER_ROW: usize = (atlas::GLYPH_WIDTH as usize).div_ceil(2);

impl Glyph {
    /// The atlas cell at `index`, or `None` when the index (or the payload
    /// itself) is short — fail closed, never out of bounds.
    fn at(index: u32) -> Option<Self> {
        let start = index as usize * atlas::BYTES_PER_GLYPH;
        let data = atlas::COVERAGE.get(start..start + atlas::BYTES_PER_GLYPH)?;
        Some(Self { data })
    }

    /// The U+FFFD replacement glyph: the fallback for a scalar the face does
    /// not cover. The generator guarantees the fallback cell exists; a
    /// (structurally impossible) miss yields a blank glyph rather than a
    /// panic.
    #[must_use]
    pub fn fallback() -> Self {
        Self::at(atlas::FALLBACK_INDEX).unwrap_or(Self { data: &[] })
    }

    /// The coverage of pixel `(x, y)`: `0` (transparent) through `15` (fully
    /// covered). Out-of-glyph coordinates are transparent.
    #[must_use]
    pub fn coverage(&self, x: u32, y: u32) -> u8 {
        if x >= atlas::GLYPH_WIDTH || y >= atlas::CELL_HEIGHT {
            return 0;
        }
        let byte = self
            .data
            .get(y as usize * BYTES_PER_ROW + x as usize / 2)
            .copied()
            .unwrap_or(0);
        if x % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0F
        }
    }

    /// Whether every pixel of the cell is transparent.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.data.iter().all(|&b| b == 0)
    }
}

/// The atlas glyph for `ch`, or `None` when the face does not cover it.
///
/// Binary search over the sorted, non-overlapping generated ranges: `O(log r)`
/// with no allocation, cheap enough for a per-character hot path.
#[must_use]
pub fn lookup(ch: char) -> Option<Glyph> {
    let code = u32::from(ch);
    let ranges = atlas::RANGES;
    let index = ranges
        .binary_search_by(|&(first, len, _)| {
            if code < first {
                core::cmp::Ordering::Greater
            } else if code >= first + len {
                core::cmp::Ordering::Less
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .ok()?;
    let (first, _, base) = ranges[index];
    Glyph::at(base + (code - first))
}

/// The atlas glyph for `ch`, falling back to the U+FFFD replacement glyph
/// for a scalar the face does not cover.
#[must_use]
pub fn lookup_or_fallback(ch: char) -> Glyph {
    lookup(ch).unwrap_or_else(Glyph::fallback)
}
