//! Synthetic emboldening: the coverage transform that turns the Regular
//! outline the committed faces provide into the heavier weights a theme names.
//!
//! A face declaring a `wght` axis is instanced at the requested weight and
//! never reaches here. Only a face without one is rasterised from its single
//! outline and then thickened, exactly as a stroke-widening rasteriser would
//! (`FreeType`'s `FT_Outline_Embolden` takes the equivalent approach on the
//! outline). Thickening the 8-bit coverage
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

/// The `wght` axis distance above Regular at which the stroke reaches its
/// full [`BOLD_EM_DIVISOR`] strength.
const BOLD_AXIS_SPAN: u32 = 300;

/// The em fraction a fully bold synthetic stroke is: one twenty-fourth, the
/// strength a stroke-widening rasteriser applies (`FreeType`'s
/// `FT_GlyphSlot_Embolden` uses the same em/24).
const BOLD_EM_DIVISOR: u32 = 24;

/// Fixed-point denominator the em fraction below is carried in, chosen so the
/// ramp resolves every axis step without floating point on a text path.
const STROKE_EM_SCALE: u32 = 1 << 16;

/// The synthetic stroke `weight` adds, as a fraction of the em scaled by
/// [`STROKE_EM_SCALE`].
///
/// The `wght` axis is continuous, so the stroke ramps linearly with the
/// distance above Regular and reaches em/24 at Bold (700). Regular and
/// anything lighter add nothing, which keeps body text byte-for-byte what it
/// was. A weight past Bold goes on thickening, since a face without the axis
/// has nothing else to render it with.
fn stroke_em_numerator(weight: FontWeight) -> u32 {
    let above = u32::from(
        weight
            .axis_value()
            .saturating_sub(FontWeight::REGULAR.axis_value()),
    );
    // `above` is at most 600 and the scale a power of two well under 2^22, so
    // the product cannot overflow.
    above * (STROKE_EM_SCALE / BOLD_EM_DIVISOR) / BOLD_AXIS_SPAN
}

/// The stroke width `weight` adds to a glyph whose em measures `em` — in
/// whatever unit `em` is given in, since the ramp is a pure fraction of the
/// em. The coverage path passes the rendered em in 1/256 px; the outline
/// path passes the protocol's em fraction, so a bold drawn as pixels and one
/// drawn as geometry are the same weight.
///
/// The arithmetic is integer throughout — a rendered em size is an exact
/// rational of the cell height, so there is nothing for floating point to buy
/// on a text path — and rounds to the nearest sub-pixel step, which keeps the
/// thickening a smooth function of the rendered size: a heading and a caption
/// in the same weight look like the same weight rather than one being
/// disproportionately fat. The product is taken in a wider type so even an
/// absurd em size yields a bounded stroke instead of wrapping.
pub(crate) fn stroke_subpixels(em: u32, weight: FontWeight) -> u32 {
    let numerator = stroke_em_numerator(weight);
    if numerator == 0 {
        return 0;
    }
    let scale = u64::from(STROKE_EM_SCALE);
    let rounded = (u64::from(em) * u64::from(numerator) + scale / 2) / scale;
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
