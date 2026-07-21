//! On-demand glyph rasterisation from the outline faces, and its
//! process-global cache.
//!
//! The generated atlas ([`crate::atlas`]) is authored at one native cell size
//! and is what the text console draws. The desktop, however, asks for text at
//! *other* sizes (a comfortable physical pixel size derived from the theme,
//! or a large heading). Rather than resample the fixed native bitmap — which
//! smears when enlarged and drops detail when shrunk — a glyph is rasterised
//! **directly from the TrueType outline** at the requested cell size through
//! the shared `lib/fontface` engine, exactly as the atlas generator rasterises
//! the native size. The result is crisp at any size, small or large, because
//! the curve is sampled at the target resolution rather than stretched.
//!
//! Rasterising an outline is not free, and the desktop redraws the same glyphs
//! at the same size every frame, so each result is memoised in a bounded,
//! process-global [`SpinLock`]-guarded cache keyed by
//! `(face index, glyph id, cell height)`. A hit copies the stored coverage
//! into the caller's reusable buffer with no rasterisation; a miss rasterises
//! once, inserts (evicting the oldest entry when the cache is full so memory
//! stays bounded), and copies out.
//!
//! The four committed faces are embedded and parsed once, lazily, into a
//! shared [`FontFamily`]; a scalar resolves to the same face the atlas would
//! pick, so resized text keeps the same glyph coverage the console shows. If
//! the (trusted, committed) faces ever fail to parse, rasterisation fails
//! closed to a blank glyph rather than panicking.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use tairix_fontface::{CellGeometry, FontFamily, Repertoire, ATLAS_EM_PX};
use tairix_sync::{Once, SpinLock};

use crate::atlas;

/// The committed system faces, embedded so a resized glyph is rasterised from
/// the same outlines the atlas is generated from (SIL OFL 1.1; see
/// `assets/OFL.txt`, `D2Coding-OFL.txt`, `NotoSansHebrew-OFL.txt`).
static PRIMARY_FACE: &[u8] = include_bytes!("../assets/Inconsolata-EX.ttf");
static JAPANESE_FACE: &[u8] = include_bytes!("../assets/MPLUS1Code-Regular.ttf");
static KOREAN_FACE: &[u8] = include_bytes!("../assets/D2Coding-Regular.ttf");
static HEBREW_FACE: &[u8] = include_bytes!("../assets/NotoSansHebrew-ExtraCondensed.ttf");

/// The shared merged family, parsed once on first resized draw.
static FAMILY: Once<FontFamily<'static>> = Once::new();

/// The parsed family, or `None` if the committed faces failed to parse.
///
/// The faces are trusted repository data, so this succeeds in practice; a
/// (structurally impossible) parse failure fails closed to `None`, and the
/// caller renders nothing rather than panicking.
fn family() -> Option<&'static FontFamily<'static>> {
    FAMILY
        .call_once(|| {
            FontFamily::parse(&[
                (PRIMARY_FACE, Repertoire::Full),
                (JAPANESE_FACE, Repertoire::Full),
                (KOREAN_FACE, Repertoire::Korean),
                (HEBREW_FACE, Repertoire::Full),
            ])
        })
        .ok()
}

/// The largest number of distinct `(face, glyph, size)` bitmaps the cache
/// retains.
///
/// The desktop uses a small number of sizes and a small visible glyph
/// repertoire, so this comfortably holds a steady-state working set while
/// capping the entry count: a pathological caller that rasterises at ever more
/// sizes evicts the oldest entries rather than growing without bound.
const MAX_ENTRIES: usize = 1024;

/// The cache key: the resolved face index and glyph id, and the cell height
/// the glyph was rasterised at (the cell width and baseline are a fixed
/// function of the height, so the height alone keys the geometry).
type Key = (u32, u32, u32);

/// A bounded FIFO map from [`Key`] to a rasterised coverage buffer.
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

/// Fill `out` with the `2 * geometry.width * geometry.height` coverage bytes of
/// `ch` rasterised at `geometry` (two cells wide, so a full-width glyph reaches
/// its continuation cell), resizing `out` to that length.
///
/// The bitmap is two cells wide regardless of the scalar's own width; the
/// blitter clips a narrow glyph to its single cell, exactly as the native
/// blitter clips to `CELL_WIDTH` of `GLYPH_WIDTH`. A scalar the face does not
/// cover resolves to the U+FFFD replacement glyph, matching the atlas. A cache
/// hit copies; a miss rasterises once (outside the lock), memoises, and copies.
/// If the faces are unavailable the buffer is cleared to transparent (fail
/// closed).
pub(crate) fn scaled_coverage(
    ch: char,
    geometry: &CellGeometry,
    px_per_em: f64,
    out: &mut Vec<u8>,
) {
    let bitmap_width = geometry.width.saturating_mul(2);
    let len = (bitmap_width as usize).saturating_mul(geometry.height as usize);
    out.clear();
    out.resize(len, 0);

    let Some(family) = family() else {
        return;
    };
    let code = u32::from(ch);
    let Some((face, glyph)) = family
        .resolve(code)
        .or_else(|| family.resolve(u32::from(char::REPLACEMENT_CHARACTER)))
    else {
        return;
    };
    // The family holds a handful of faces, so the index always fits a `u32`;
    // a structurally impossible overflow keys a distinct-but-harmless slot.
    let key: Key = (
        u32::try_from(face).unwrap_or(u32::MAX),
        u32::from(glyph),
        geometry.height,
    );

    if let Some(coverage) = CACHE.lock().entries.get(&key) {
        if coverage.len() == len {
            out.copy_from_slice(coverage);
        }
        return;
    }

    let Ok(coverage) = family.rasterise(face, glyph, geometry, px_per_em, bitmap_width) else {
        return;
    };
    if coverage.len() == len {
        out.copy_from_slice(&coverage);
    }
    CACHE.lock().insert(key, coverage.into_boxed_slice());
}

/// The pixels-per-em to rasterise a glyph at so a cell `height` pixels tall is
/// proportional to the native atlas cell — the reference size scaled linearly.
#[must_use]
pub(crate) fn px_per_em(height: u32) -> f64 {
    f64::from(ATLAS_EM_PX) * f64::from(height) / f64::from(atlas::CELL_HEIGHT)
}
