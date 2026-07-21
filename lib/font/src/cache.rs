//! Downscaled-glyph rasterisation and its process-global cache.
//!
//! The atlas ([`crate::atlas`]) is authored at one native cell size; the
//! desktop renders its text at a smaller size (a comfortable *physical* pixel
//! size derived from the theme). Rather than ship a second atlas, a glyph is
//! resampled from the native 4-bit coverage bitmap down to the requested cell
//! size with an **area-averaging box filter**: every destination pixel is the
//! coverage-weighted average of the source pixels it overlaps, so an
//! anti-aliased edge stays smooth instead of the jagged holes a
//! nearest-neighbour pick would leave. The filter is exact — it apportions
//! each source pixel by the fractional area it contributes — and produces the
//! same 4-bit coverage the blitter already blends.
//!
//! Resampling a glyph is not free, and the desktop redraws the same glyphs at
//! the same size every frame, so the result is memoised in a bounded,
//! process-global [`SpinLock`]-guarded cache keyed by
//! `(atlas cell index, destination width, destination height)`. A hit copies
//! the stored coverage into the caller's stack buffer with no allocation; a
//! miss resamples once, inserts (evicting the oldest entry when the cache is
//! full so memory stays bounded), and copies out. The key is an integer triple
//! and the value a compact one-byte-per-pixel coverage buffer, so neither
//! whole bitmaps nor scalars are stored.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;

use tairix_sync::SpinLock;

use crate::atlas;
use crate::glyph::glyph_at;

/// The largest number of distinct `(glyph, size)` bitmaps the cache retains.
///
/// The desktop uses a small number of sizes and the visible glyph repertoire
/// is small, so this comfortably holds a steady-state working set while
/// capping worst-case memory: a pathological caller that resamples at ever
/// more sizes evicts the oldest entries rather than growing without bound.
const MAX_ENTRIES: usize = 1024;

/// The cache key: atlas cell index and the destination cell dimensions the
/// glyph was resampled to.
type Key = (u32, u32, u32);

/// A bounded FIFO map from [`Key`] to a resampled coverage buffer.
struct GlyphCache {
    entries: BTreeMap<Key, Box<[u8]>>,
    order: VecDeque<Key>,
}

impl GlyphCache {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Insert `coverage` for `key`, evicting the oldest entry first when the
    /// cache is full so its footprint stays bounded.
    fn insert(&mut self, key: Key, coverage: Box<[u8]>) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.order.len() >= MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(key);
        self.entries.insert(key, coverage);
    }
}

/// The one process-global glyph cache.
static CACHE: SpinLock<GlyphCache> = SpinLock::new(GlyphCache::new());

/// Fill `out[..dst_w * dst_h]` with the `dst_w * dst_h` coverage bytes of atlas
/// cell `index` resampled to that size, returning the number of bytes written.
///
/// `out` must be at least `dst_w * dst_h` bytes; the scaled renderer sizes it
/// to the native glyph bound so it always is. A cache hit copies; a miss
/// resamples once (outside the lock), memoises the result, and copies. The
/// result is deterministic in the key, so a concurrent miss on the same key
/// merely recomputes the identical bytes.
pub(crate) fn scaled_coverage(index: u32, dst_w: u32, dst_h: u32, out: &mut [u8]) -> usize {
    let len = (dst_w as usize) * (dst_h as usize);
    let out = &mut out[..len];
    let key: Key = (index, dst_w, dst_h);

    if let Some(coverage) = CACHE.lock().entries.get(&key) {
        out.copy_from_slice(&coverage[..]);
        return len;
    }

    let coverage = resample(index, dst_w, dst_h);
    out.copy_from_slice(&coverage);
    CACHE.lock().insert(key, coverage);
    len
}

/// Resample atlas cell `index` from the native glyph bitmap down to
/// `dst_w * dst_h` coverage bytes with an exact area-averaging box filter.
fn resample(index: u32, dst_w: u32, dst_h: u32) -> Box<[u8]> {
    let glyph = glyph_at(index);
    let src_w = atlas::GLYPH_WIDTH;
    let src_h = atlas::CELL_HEIGHT;
    // Total area a single destination pixel spans in the shared axis units
    // where a source pixel is `dst` wide and a destination pixel is `src`
    // wide: the per-pixel weights sum to exactly this.
    let total = u64::from(src_w) * u64::from(src_h);

    let mut coverage = vec![0u8; (dst_w as usize) * (dst_h as usize)];
    for dy in 0..dst_h {
        let (sy0, sy1) = source_span(dy, src_h, dst_h);
        for dx in 0..dst_w {
            let (sx0, sx1) = source_span(dx, src_w, dst_w);
            let mut accum = 0u64;
            for sy in sy0..sy1 {
                let oy = overlap(dy, src_h, sy, dst_h);
                if oy == 0 {
                    continue;
                }
                for sx in sx0..sx1 {
                    let cov = u64::from(glyph.coverage(sx, sy));
                    if cov == 0 {
                        continue;
                    }
                    let ox = overlap(dx, src_w, sx, dst_w);
                    accum += cov * ox * oy;
                }
            }
            // Round to nearest 4-bit coverage level. The weighted mean of
            // values in 0..=15 is itself in 0..=15; the fallback caps at full
            // coverage so the value can never exceed a 4-bit level.
            let value = (accum + total / 2) / total;
            coverage[(dy as usize) * (dst_w as usize) + dx as usize] =
                u8::try_from(value).unwrap_or(15);
        }
    }
    coverage.into_boxed_slice()
}

/// The half-open range of source rows/columns that a destination pixel
/// overlaps: `dst_index`'s span is `[dst_index * src, (dst_index + 1) * src)`
/// and a source pixel `s`'s span is `[s * dst, (s + 1) * dst)`, both in the
/// shared `src * dst` axis. Returns `[floor, ceil)` clamped to `src`.
fn source_span(dst_index: u32, src: u32, dst: u32) -> (u32, u32) {
    let lo = dst_index * src / dst;
    let hi = ((dst_index + 1) * src).div_ceil(dst).min(src);
    (lo, hi)
}

/// The length of the overlap between destination pixel `dst_index` (span
/// `[dst_index * src, (dst_index + 1) * src)`) and source pixel `s` (span
/// `[s * dst, (s + 1) * dst)`) in the shared `src * dst` axis, clamped at zero.
fn overlap(dst_index: u32, src: u32, s: u32, dst: u32) -> u64 {
    let d_lo = u64::from(dst_index) * u64::from(src);
    let d_hi = d_lo + u64::from(src);
    let s_lo = u64::from(s) * u64::from(dst);
    let s_hi = s_lo + u64::from(dst);
    d_hi.min(s_hi).saturating_sub(d_lo.max(s_lo))
}
