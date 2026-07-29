//! Glyph lookup over the generated Inconsolata EX console atlas.
//!
//! The compiled-in atlas is the primary face's repertoire only (the CJK and
//! Hebrew companions are served at runtime by `fontd`), so a CJK/Hebrew scalar
//! resolves to the U+FFFD fallback here exactly like any other unmapped
//! scalar.
//!
//! [`lookup`] maps any `char` to its atlas cell by binary search over the
//! generated codepoint ranges ([`crate::atlas::RANGES`]); a scalar the face
//! does not cover falls back to the U+FFFD replacement glyph
//! ([`Glyph::fallback`]) so unsupported text is visibly wrong rather than
//! silently dropped. Each lookup decompresses only that glyph's bounded block
//! into the returned [`Glyph`]; no allocation or whole-atlas startup pass is
//! needed. A glyph hands out per-pixel 4-bit coverage values from its decoded
//! bitmap, and every access is bounds-checked, so malformed coordinates read
//! as transparent rather than out of bounds.

use crate::atlas;

/// One decoded glyph bitmap: up to [`atlas::GLYPH_WIDTH`] ×
/// [`atlas::CELL_HEIGHT`] pixels of 4-bit coverage. Narrow glyphs leave the
/// second cell transparent; full-width glyphs may cover it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Glyph {
    /// The glyph's decoded [`atlas::BYTES_PER_GLYPH`] packed bytes: row major,
    /// two pixels per byte, left pixel in the high nibble.
    data: [u8; atlas::BYTES_PER_GLYPH],
}

/// Packed bytes per glyph row (two 4-bit pixels per byte).
const BYTES_PER_ROW: usize = (atlas::GLYPH_WIDTH as usize).div_ceil(2);

impl Glyph {
    /// The atlas cell at `index`, or `None` when its offsets or compressed
    /// block are malformed — fail closed, never out of bounds.
    fn at(index: u32) -> Option<Self> {
        let next = index.checked_add(1)?;
        if next > atlas::CELL_COUNT {
            return None;
        }
        let table_len = (atlas::CELL_COUNT as usize + 1).checked_mul(size_of::<u32>())?;
        let compressed = atlas::COVERAGE.get(table_len..)?;
        let start = read_offset(index)?;
        let end = read_offset(next)?;
        let block = compressed.get(start..end)?;
        decode_glyph(block).map(|data| Self { data })
    }

    /// The U+FFFD replacement glyph: the fallback for a scalar the face does
    /// not cover. The generator guarantees the fallback cell exists; a
    /// (structurally impossible) miss yields a blank glyph rather than a
    /// panic.
    #[must_use]
    pub fn fallback() -> Self {
        Self::at(atlas::FALLBACK_INDEX).unwrap_or(Self {
            data: [0; atlas::BYTES_PER_GLYPH],
        })
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
        if x.is_multiple_of(2) {
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

/// Read one little-endian compressed-block offset from the atlas table.
fn read_offset(index: u32) -> Option<usize> {
    let start = index as usize * size_of::<u32>();
    let bytes = atlas::COVERAGE.get(start..start + size_of::<u32>())?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?) as usize)
}

/// Decode one bounded glyph block into its fixed-size packed bitmap.
fn decode_glyph(mut encoded: &[u8]) -> Option<[u8; atlas::BYTES_PER_GLYPH]> {
    let mut decoded = [0u8; atlas::BYTES_PER_GLYPH];
    let mut written = 0usize;
    while let Some((&token, rest)) = encoded.split_first() {
        encoded = rest;
        if token < 0x80 {
            let length = usize::from(token) + 1;
            let bytes = encoded.get(..length)?;
            let end = written.checked_add(length)?;
            decoded.get_mut(written..end)?.copy_from_slice(bytes);
            written = end;
            encoded = &encoded[length..];
            continue;
        }
        let (&distance_low, rest) = encoded.split_first()?;
        encoded = rest;
        let length = usize::from((token >> 2) & 0x1F) + 3;
        let distance = (usize::from(token & 0x03) << 8) | usize::from(distance_low);
        let distance = distance + 1;
        if distance > written {
            return None;
        }
        let end = written.checked_add(length)?;
        if end > decoded.len() {
            return None;
        }
        for destination in written..end {
            decoded[destination] = decoded[destination - distance];
        }
        written = end;
    }
    (written == decoded.len()).then_some(decoded)
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

#[cfg(test)]
mod codec_tests {
    use super::decode_glyph;
    use crate::atlas;
    use std::vec;

    #[test]
    fn decoder_accepts_literals_and_overlapping_matches() {
        let mut encoded = vec![2, 1, 2, 3];
        for _ in 0..(atlas::BYTES_PER_GLYPH - 3) / 34 {
            encoded.extend_from_slice(&[0xFC, 2]);
        }
        let remainder = (atlas::BYTES_PER_GLYPH - 3) % 34;
        if remainder != 0 {
            let encoded_length = u8::try_from(remainder - 3).expect("match length fits a token");
            encoded.extend_from_slice(&[0x80 | encoded_length << 2, 2]);
        }
        let decoded = decode_glyph(&encoded).expect("valid stream decodes");
        assert!(decoded
            .iter()
            .copied()
            .eq([1, 2, 3].into_iter().cycle().take(decoded.len())));
    }

    #[test]
    fn decoder_rejects_malformed_streams() {
        assert!(decode_glyph(&[]).is_none());
        assert!(decode_glyph(&[3, 1, 2]).is_none());
        assert!(decode_glyph(&[0x80, 0]).is_none());
        assert!(decode_glyph(&[0xF0; atlas::BYTES_PER_GLYPH]).is_none());

        let oversized_literal = [127u8; atlas::BYTES_PER_GLYPH + 4];
        assert!(decode_glyph(&oversized_literal).is_none());
    }
}
