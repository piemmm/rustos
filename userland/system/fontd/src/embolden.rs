//! Synthetic emboldening: the coverage transform that turns the Regular
//! outline the committed faces provide into the heavier weights a theme names.
//!
//! The four `/System/Fonts` faces ship one weight each, so a heavier run is
//! rasterised from the *same* outline and then thickened here, exactly as a
//! stroke-widening rasteriser would (`FreeType`'s `FT_Outline_Embolden` takes
//! the equivalent approach on the outline). Thickening the 8-bit coverage
//! rather than the outline keeps the whole operation inside the sandbox that
//! already owns the raster, needs no second rasterisation pass, and cannot
//! move a control point.
//!
//! The stroke is *horizontal only*. A vertical smear would push an ascender
//! or a descender out of the cell the client laid out, changing the geometry
//! `FontMetrics` promised; a horizontal one stays inside the two-cell bitmap
//! and leaves the baseline, cell height, and pen advance untouched — so a
//! bold run occupies precisely the cells its regular twin would.

use tairix_abi::font_ipc::FontWeight;

/// Sub-pixel fixed-point unit the stroke width is carried in: 1/256 px.
pub(crate) const SUBPIXEL: u32 = 256;

/// The em divisor each weight's stroke is, or `0` for a weight that adds no
/// stroke at all.
///
/// `Bold` is one twenty-fourth of the em — the strength a stroke-widening
/// rasteriser applies for a synthetic bold (`FreeType`'s
/// `FT_GlyphSlot_Embolden` uses the same em/24) — and `Medium` is half of it,
/// so the three weights read as an even progression rather than "regular and
/// fat". `Regular` adds nothing, which is what keeps body text byte-for-byte
/// what it was before weights existed.
const fn stroke_em_divisor(weight: FontWeight) -> u32 {
    match weight {
        FontWeight::Regular => 0,
        FontWeight::Medium => 48,
        FontWeight::Bold => 24,
    }
}

/// The stroke width, in 1/256 px, that `weight` adds to a glyph whose em is
/// `em_subpixels` (also 1/256 px) tall as rendered.
///
/// The arithmetic is integer throughout — a rendered em size is an exact
/// rational of the cell height, so there is nothing for floating point to buy
/// on a text path — and rounds to the nearest sub-pixel step, which keeps the
/// thickening a smooth function of the rendered size: a heading and a caption
/// in the same weight look like the same weight rather than one being
/// disproportionately fat. The rounding term is added in a wider type so even
/// an absurd em size yields a bounded stroke instead of wrapping.
pub(crate) fn stroke_subpixels(em_subpixels: u32, weight: FontWeight) -> u32 {
    let divisor = stroke_em_divisor(weight);
    if divisor == 0 {
        return 0;
    }
    let rounded = (u64::from(em_subpixels) + u64::from(divisor) / 2) / u64::from(divisor);
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

/// Thicken `coverage` — a `width`-by-`height` row-major 8-bit alpha bitmap —
/// by `stroke` (in 1/256 px) to the right of every inked sample.
///
/// Each output sample is the maximum of the samples the stroke covers, with
/// the fractional tail scaled proportionally, so a partial-pixel stroke
/// darkens an edge instead of jumping a whole pixel. A stroke of zero leaves
/// the bitmap byte-identical, which is what keeps Regular text byte-for-byte
/// what it was before weights existed.
pub(crate) fn embolden(coverage: &mut [u8], width: usize, stroke: u32) {
    if stroke == 0 || width == 0 || coverage.len() < width {
        return;
    }
    let whole = (stroke / SUBPIXEL) as usize;
    let frac = stroke % SUBPIXEL;
    for row in coverage.chunks_exact_mut(width) {
        // Right-to-left in place: an output sample only reads samples to its
        // left, so a reverse walk needs no scratch row copy.
        for x in (0..width).rev() {
            let mut value = row[x];
            for step in 1..=whole.min(x) {
                value = value.max(row[x - step]);
            }
            if frac != 0 && x > whole {
                let tail = u32::from(row[x - whole - 1]) * frac / SUBPIXEL;
                // `tail` is at most the source sample, so the conversion
                // cannot fail; a full sample is the fail-safe if it ever did.
                value = value.max(u8::try_from(tail).unwrap_or(u8::MAX));
            }
            row[x] = value;
        }
    }
}

#[cfg(test)]
mod tests;
