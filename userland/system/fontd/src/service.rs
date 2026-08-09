//! The font-service dispatcher: the host-testable core that turns one
//! decoded [`FontRequest`] into a framed `font-v1` reply.
//!
//! [`FontService`] owns the discovered families (built by
//! [`crate::discovery::discover`]), each holding its faces behind a face
//! cache that reads and parses a face's bytes on first use and
//! retains the parsed instances for the service's life. [`FontService::handle`]
//! is the whole request pipeline: decode, resolve, rasterise (or serve from
//! cache), and emit a reply — always producing bytes, an error frame on any
//! failure, so a caller never blocks on a dropped reply (fail closed).
//!
//! # Resolution
//!
//! A scalar resolves within the requested family's own faces, in manifest
//! order; if none maps it, within the family's declared fallback family's
//! faces, in the same order; if still nothing maps it, U+FFFD is rendered
//! from the requested family's primary face. Every glyph in one family's run
//! shares the geometry (pixels-per-em, baseline, box height) the requested
//! family's primary face defines at the requested pixel height, even when
//! the glyph itself came from a fallback face — so mixing scripts never
//! shifts the baseline or the line box mid-run.
//!
//! # Weights
//!
//! A face that declares a `wght` axis is instanced at the exact requested
//! weight, whose advance genuinely differs from another weight's; the
//! instanced [`Face`] is parsed once per distinct weight actually requested
//! and cached. A face with no such axis keeps its one default instance and
//! is thickened afterwards by the synthetic `embolden` transform, which
//! leaves its advance untouched.
//!
//! # The cache a hostile caller cannot grow
//!
//! The pixel height is caller-supplied, so the size of what a request makes
//! this service retain is caller-influenced. The cache is therefore bounded
//! in **bytes**, by a budget derived from the machine's own RAM, through the
//! shared reclaimable-memory model — the same [`tairix_reclaim::ReclaimCache`]
//! the render-path client on the other side of this endpoint uses, declared
//! once in [`tairix_font::glyph_cache`]. The key names the requesting family,
//! the family whose face actually supplied the glyph, that face's index,
//! the glyph id, the pixel height, and the weight, so two families can never
//! collide on the same slot even when they share a fallback face.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::font_ipc::{
    encode_families_reply, encode_glyph_error_reply, encode_glyph_reply, encode_metrics_reply,
    FamilyEntry, FamilyKey, FamilyKind, FontMetrics, FontRequest, FontWeight, FONT_FAMILY_KEY_LEN,
    FONT_METRICS_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_font::{glyph_cache_budget, glyph_cache_candidate, CachedGlyph};
use tairix_fontface::{lineart, AxisSetting, CellGeometry, Face};
use tairix_log::Sink;
use tairix_reclaim::{PressureGauge, ReclaimCache, ReclaimOwner};
use tairix_vt::char_width;

use crate::discovery::FaceLoad;
use crate::embolden::{embolden, stroke_subpixels, SUBPIXEL};

/// The service's glyph-cache key: everything the served bitmap depends on.
///
/// The requesting and resolved families are both part of the key because two
/// families sharing the same fallback face can legitimately compute
/// different geometry for the very same physical glyph — their own primary
/// faces differ — so the same glyph can rasterise to two different bitmaps;
/// keying by the resolved face alone would risk serving one family's raster
/// to the other.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub struct GlyphKey {
    /// The requesting family, which drives the shared geometry a run renders
    /// at.
    requested: [u8; FONT_FAMILY_KEY_LEN],
    /// The family whose faces actually supplied the glyph: the requested
    /// family itself, or its declared fallback.
    resolved: [u8; FONT_FAMILY_KEY_LEN],
    /// The face's index within the resolved family.
    face: u32,
    /// The glyph id within that face.
    glyph: u32,
    /// The requested pixel height.
    pixel_height: u32,
    /// The grid cells the scalar is drawn across, or `0` where the family is
    /// proportional and the bitmap is tight to the ink instead.
    ///
    /// A face is free to map two scalars of different display widths onto one
    /// glyph, so the cell count is not implied by the glyph id: without it a
    /// wide scalar's two-cell bitmap could be served for a narrow one.
    cells: u32,
    /// The requested weight, as its wire value.
    weight: u16,
}

/// The service's rasterised-glyph cache: the shared bounded, classified,
/// pressure-governed cache holding [`CachedGlyph`] coverage under a
/// [`GlyphKey`].
///
/// The generation token is `()` because nothing invalidates a raster while
/// the service lives: a face's bytes never change once read, so the same
/// glyph at the same height and weight rasterises to the same bytes every
/// time. Entries leave by eviction, by memory pressure, or with the service
/// itself — the owner-teardown invalidation the shared classification
/// declares.
pub type GlyphCache = ReclaimCache<GlyphKey, CachedGlyph, ()>;

/// The audit label the service's cache is named by in reclaim records.
const CACHE_LABEL: &str = "fontd.glyphs";

/// The owner the service's cache charges its bytes to: this service process,
/// named directly, since a userland service has no numeric task id to quote.
const CACHE_OWNER: &str = "fontd";

/// Build the service's glyph cache, budgeted from the machine's total usable
/// physical RAM.
///
/// The `Run` binary reads `total_ram_bytes` from the System Information
/// service and passes the process's own pressure gauge and audit sink, so the
/// cache shrinks on the same bands as every other cache on the machine. A
/// zero reading — no service, a refused or malformed reply — yields a zero
/// budget, which admits nothing: every glyph is then rasterised on demand,
/// correct and merely slower, never a hand-picked ceiling standing in for a
/// figure the machine did not supply.
#[must_use]
pub fn glyph_cache(
    total_ram_bytes: u64,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> GlyphCache {
    let cache = ReclaimCache::new(
        CACHE_LABEL,
        glyph_cache_candidate(ReclaimOwner::UserlandProcess(CACHE_OWNER)),
        glyph_cache_budget(total_ram_bytes),
        pressure,
        sink,
    );
    // This is the one cache every GUI client's own glyph-atlas cache is
    // ultimately backed by, so it is the most important row the desktop's
    // cache monitor can show; only the freestanding service binary links
    // the reporter (the host build and the dispatcher-only library consumer
    // never do).
    #[cfg(all(freestanding, feature = "program"))]
    if let Some(ledger) = cache.ledger() {
        tairix_rt::cachereport::register(ledger);
    }
    cache
}

/// One face's lazily-read bytes and its cached parsed instances.
///
/// The face's bytes are read on first use ([`FaceLoad::load`]) and retained
/// for the service's life. The default (unvaried) instance is parsed once
/// and used for every codepoint lookup and as the geometry source, since a
/// face's `cmap` and vertical metrics never change with variation in this
/// engine. A face declaring a `wght` axis additionally caches one instanced
/// [`Face`] per distinct [`FontWeight`] actually requested; a face with no
/// such axis reuses the one default instance for every weight and relies on
/// synthetic emboldening instead.
pub(crate) struct FaceCache<'a> {
    loader: Box<dyn FaceLoad<'a> + 'a>,
    bytes: Option<&'a [u8]>,
    default: Option<Face<'a>>,
    has_wght: bool,
    weighted: Vec<(FontWeight, Face<'a>)>,
}

impl<'a> FaceCache<'a> {
    /// A face whose bytes will be obtained from `loader` on first use.
    pub(crate) fn new(loader: Box<dyn FaceLoad<'a> + 'a>) -> Self {
        Self {
            loader,
            bytes: None,
            default: None,
            has_wght: false,
            weighted: Vec::new(),
        }
    }

    /// This face's bytes, reading them on the first call and retaining them
    /// for the service's life.
    ///
    /// # Errors
    ///
    /// Whatever [`FaceLoad::load`] raises when the face cannot be read.
    fn face_bytes(&mut self) -> Result<&'a [u8], Errno> {
        if self.bytes.is_none() {
            self.bytes = Some(self.loader.load()?);
        }
        self.bytes.ok_or(Errno::BadMagic)
    }

    /// Read this face's bytes and parse its default instance, if either has
    /// not already happened.
    fn ensure_default(&mut self) -> Result<(), Errno> {
        if self.default.is_some() {
            return Ok(());
        }
        let bytes = self.face_bytes()?;
        let face = Face::parse(bytes).map_err(|_| Errno::BadMagic)?;
        self.has_wght = face.axes().iter().any(|axis| axis.tag == *b"wght");
        self.default = Some(face);
        Ok(())
    }

    /// The default (unvaried) instance: the source of `cmap` lookups and of
    /// the family's shared geometry.
    fn default_face(&mut self) -> Result<&Face<'a>, Errno> {
        self.ensure_default()?;
        self.default.as_ref().ok_or(Errno::BadMagic)
    }

    /// Whether this face declares a `wght` axis, so a heavier weight is a
    /// real instance rather than synthetic emboldening.
    fn has_wght(&mut self) -> Result<bool, Errno> {
        self.ensure_default()?;
        Ok(self.has_wght)
    }

    /// The instance to rasterise `weight` from: the cached `wght`-instanced
    /// face when the face declares that axis, else the one default instance
    /// (synthetic emboldening applies the weight afterwards, on the coverage
    /// this instance rasterises).
    fn instance_for(&mut self, weight: FontWeight) -> Result<&Face<'a>, Errno> {
        self.ensure_default()?;
        if !self.has_wght {
            return self.default_face();
        }
        if !self.weighted.iter().any(|&(w, _)| w == weight) {
            let bytes = self.face_bytes()?;
            let settings = [AxisSetting {
                tag: *b"wght",
                value: f32::from(weight.axis_value()),
            }];
            let face = Face::parse_instance(bytes, &settings).map_err(|_| Errno::BadMagic)?;
            self.weighted.push((weight, face));
        }
        self.weighted
            .iter()
            .find_map(|(w, face)| (*w == weight).then_some(face))
            .ok_or(Errno::BadMagic)
    }
}

/// One discovered family: its manifest facts plus its lazily-loaded faces.
pub(crate) struct FamilyRuntime<'a> {
    key: FamilyKey,
    label: String,
    /// How the family lays text out, or `None` for a fallback-role family a
    /// user never selects directly.
    kind: Option<FamilyKind>,
    /// The family's own faces, in manifest (resolution) order; index `0` is
    /// always the primary face.
    faces: Vec<FaceCache<'a>>,
    /// The family whose faces extend this one's coverage, if any.
    fallback: Option<FamilyKey>,
}

impl<'a> FamilyRuntime<'a> {
    pub(crate) fn new(
        key: FamilyKey,
        label: String,
        kind: Option<FamilyKind>,
        faces: Vec<FaceCache<'a>>,
        fallback: Option<FamilyKey>,
    ) -> Self {
        Self {
            key,
            label,
            kind,
            faces,
            fallback,
        }
    }
}

/// Where one resolved glyph came from: the family whose face set supplied
/// it (the requested family itself, or its fallback), that family's face
/// index, and the glyph id within that face.
struct GlyphSource {
    resolved_family_index: usize,
    resolved_family_key: FamilyKey,
    face_index: usize,
    glyph: u16,
}

/// A family's shared line geometry at one pixel height: the pixels-per-em
/// every resolved face in a run rasterises at, the baseline row, the box
/// height (the requested pixel height, echoed), the line height, and — when
/// the family is monospace and its primary face really is uniform — the
/// one advance every glyph shares.
struct FamilyGeometry {
    px_per_em: f64,
    baseline: u32,
    height: u32,
    line_height: u32,
    monospace_advance: u32,
}

impl FamilyGeometry {
    /// The character cell `scalar` is drawn into, or `None` where the family
    /// is proportional and text is laid out by per-glyph advance instead.
    fn cell(&self, scalar: char) -> Option<Cell> {
        (self.monospace_advance != 0).then(|| Cell {
            width: self.monospace_advance,
            cells: u32::from(char_width(scalar)),
        })
    }
}

/// The character cell a monospace family draws one scalar into: the shared
/// advance one cell measures, and the cells the scalar occupies.
///
/// Drawing into the cell rather than tight to the ink is what a character
/// grid means. The glyph is grid-fitted against the cell it will be blitted
/// into, so its stems land on whole pixels and its advance lands on the
/// column the client steps by — the same treatment the compiled-in console
/// atlas gets, instead of ink positioned by a left bearing the grid then
/// rounds away.
#[derive(Clone, Copy)]
struct Cell {
    width: u32,
    cells: u32,
}

impl Cell {
    /// The pen advance for the scalar: the cells it occupies.
    fn advance(self) -> u32 {
        self.width.saturating_mul(self.cells)
    }

    /// The coverage for a scalar the grid draws as geometry rather than from
    /// a face — a border rule, a block — or `None` where the face supplies
    /// the glyph.
    ///
    /// These characters exist to tile, which an outline only does where its
    /// hairlines happen to land on pixel boundaries. Substituting the shared
    /// geometry is what every serious terminal does, and it is the same
    /// [`lineart`] the console atlas is built from, so a border drawn on the
    /// framebuffer console and one drawn in a terminal window are the same
    /// picture. The geometry is defined in one cell, so a scalar the width
    /// table calls double-width is left to the face.
    fn line_art(self, scalar: char, height: u32) -> Option<Vec<u8>> {
        (self.cells == 1)
            .then(|| lineart::coverage(u32::from(scalar), self.width, height))
            .flatten()
    }
}

/// Scale a non-negative font-unit `value` to whole pixels at `pixel_height`
/// over vertical-metric denominator `denom` (`ascent + descent`), rounding
/// up.
///
/// Ceiling (rather than round-to-nearest) matches the atlas generator's own
/// vertical-metric derivation: a baseline or line-gap row that would
/// otherwise clip its ink by rounding down instead grows by at most one
/// pixel.
fn scale_up_px(value: i32, pixel_height: u32, denom: i64) -> Result<u32, Errno> {
    if denom <= 0 || value < 0 {
        return Err(Errno::BadMagic);
    }
    let px = (i64::from(value) * i64::from(pixel_height) + denom - 1) / denom;
    u32::try_from(px).map_err(|_| Errno::BadMagic)
}

/// Scale a non-negative font-unit `value` to whole pixels at `pixel_height`
/// over vertical-metric denominator `denom`, rounding to the nearest pixel.
///
/// Used for the monospace advance report, where a rounded width (rather than
/// a ceiling) is what a character grid should be built from — exactly the
/// convention `lib/fontface`'s own cell-width derivation uses.
fn round_px(value: i64, pixel_height: u32, denom: i64) -> Result<u32, Errno> {
    if denom <= 0 || value < 0 {
        return Err(Errno::BadMagic);
    }
    let px = (value * i64::from(pixel_height) + denom / 2) / denom;
    u32::try_from(px).map_err(|_| Errno::BadMagic)
}

/// Round a non-negative pixel measurement to the nearest whole pixel.
///
/// The saturating float-to-integer cast (guaranteed since Rust 1.45) makes
/// this total: a non-finite or negative input yields `0` rather than an
/// undefined bit pattern, and an absurdly large advance clamps to `u32::MAX`
/// rather than wrapping.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the finite/non-negative guard above and the saturating cast \
              together bound the result to 0..=u32::MAX, so neither \
              truncation nor sign loss the lints warn about is a defect here"
)]
fn round_pixel_measurement(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else {
        (value + 0.5) as u32
    }
}

/// The sandboxed font service's rasterising core.
///
/// Built by [`crate::discovery::discover`], which discovers the families and
/// injects the byte-budgeted glyph cache: sizing that cache needs the
/// machine's RAM figure, and governing it needs the process's own pressure
/// gauge and audit sink — none of which this host-testable core may reach
/// for itself.
pub struct FontService<'a> {
    families: Vec<FamilyRuntime<'a>>,
    index: Vec<(FamilyKey, usize)>,
    cache: GlyphCache,
}

impl<'a> FontService<'a> {
    /// Build a service directly from already-discovered `families`.
    ///
    /// Only [`crate::discovery::discover`] calls this: it is the one place
    /// that has already validated there is at least one usable family and
    /// built the index this type serves lookups from.
    pub(crate) fn from_families(families: Vec<FamilyRuntime<'a>>, cache: GlyphCache) -> Self {
        let index = families
            .iter()
            .enumerate()
            .map(|(position, family)| (family.key, position))
            .collect();
        Self {
            families,
            index,
            cache,
        }
    }

    /// The number of discovered families (selectable and fallback-role
    /// alike).
    #[cfg(test)]
    pub(crate) fn family_count(&self) -> usize {
        self.families.len()
    }

    /// The discovered families' labels, in discovery order — used by tests
    /// to check that discovery order does not depend on the store's own
    /// listing order.
    #[cfg(test)]
    pub(crate) fn family_labels(&self) -> Vec<String> {
        self.families
            .iter()
            .map(|family| family.label.clone())
            .collect()
    }

    /// Release whatever the live memory-pressure band no longer permits the
    /// glyph cache to hold, returning the bytes given back.
    ///
    /// The serve loop calls this when the kernel wakes it to say the band
    /// moved, so the service gives rasters back as the machine tightens
    /// instead of holding them until something else is starved. A band that
    /// permits what is already held releases nothing.
    pub fn trim_cache(&mut self) -> usize {
        self.cache.enforce_pressure()
    }

    /// The position of family `key` in [`Self::families`], if discovered.
    fn index_of(&self, key: FamilyKey) -> Option<usize> {
        self.index
            .iter()
            .find_map(|&(k, position)| (k == key).then_some(position))
    }

    /// The first face in `family_index`'s own faces whose `cmap` maps
    /// `code`, as `(face index, glyph)`.
    fn resolve_within(&mut self, family_index: usize, code: u32) -> Option<(usize, u16)> {
        let family = self.families.get_mut(family_index)?;
        for (face_index, face) in family.faces.iter_mut().enumerate() {
            if let Ok(default_face) = face.default_face() {
                if let Some(glyph) = default_face.glyph_for(code) {
                    return Some((face_index, glyph));
                }
            }
        }
        None
    }

    /// Resolve `scalar` for a [`FontRequest::Glyph`] naming `family_index`:
    /// the family's own faces, then its fallback family's faces, then
    /// U+FFFD from the family's primary face.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] only in the structurally-impossible case that even
    /// the primary face cannot yield U+FFFD — every shipped face maps it, so
    /// this is a defensive fail-closed rather than an expected outcome.
    fn resolve(&mut self, family_index: usize, scalar: char) -> Result<GlyphSource, Errno> {
        let code = u32::from(scalar);
        if let Some((face_index, glyph)) = self.resolve_within(family_index, code) {
            let key = self.families.get(family_index).ok_or(Errno::NotFound)?.key;
            return Ok(GlyphSource {
                resolved_family_index: family_index,
                resolved_family_key: key,
                face_index,
                glyph,
            });
        }
        let fallback_key = self
            .families
            .get(family_index)
            .ok_or(Errno::NotFound)?
            .fallback;
        if let Some(fallback_key) = fallback_key {
            if let Some(fallback_index) = self.index_of(fallback_key) {
                if let Some((face_index, glyph)) = self.resolve_within(fallback_index, code) {
                    return Ok(GlyphSource {
                        resolved_family_index: fallback_index,
                        resolved_family_key: fallback_key,
                        face_index,
                        glyph,
                    });
                }
            }
        }
        // Neither the family nor its fallback covers this scalar: fall back
        // to the replacement glyph from the requested family's own primary
        // face, never refusing the request for lack of coverage.
        let replacement = u32::from(char::REPLACEMENT_CHARACTER);
        let family = self.families.get_mut(family_index).ok_or(Errno::NotFound)?;
        let primary = family.faces.first_mut().ok_or(Errno::NotFound)?;
        let glyph = primary
            .default_face()
            .ok()
            .and_then(|face| face.glyph_for(replacement))
            .ok_or(Errno::NotFound)?;
        let key = family.key;
        Ok(GlyphSource {
            resolved_family_index: family_index,
            resolved_family_key: key,
            face_index: 0,
            glyph,
        })
    }

    /// The shared line geometry `family_index`'s primary face defines at
    /// `pixel_height`.
    fn primary_geometry(
        &mut self,
        family_index: usize,
        pixel_height: u32,
    ) -> Result<FamilyGeometry, Errno> {
        let is_monospace = self
            .families
            .get(family_index)
            .and_then(|family| family.kind)
            == Some(FamilyKind::Monospace);
        let family = self.families.get_mut(family_index).ok_or(Errno::NotFound)?;
        let primary = family.faces.first_mut().ok_or(Errno::NotFound)?;
        let face = primary.default_face()?;
        let ascent = face.ascent();
        let descent = face.descent();
        let denom = i64::from(ascent) + i64::from(descent);
        if denom <= 0 {
            return Err(Errno::BadMagic);
        }
        let baseline = scale_up_px(ascent, pixel_height, denom)?.min(pixel_height);
        let line_gap_rows = scale_up_px(face.line_gap().max(0), pixel_height, denom)?;
        // Built from the same lossless `i32 -> f64` widenings as `denom`
        // rather than converting `denom` itself, which — being `i64` — has
        // no lossless `f64` conversion clippy can see is safe here.
        let denom_f64 = f64::from(ascent) + f64::from(descent);
        let px_per_em = f64::from(pixel_height) * f64::from(face.units_per_em()) / denom_f64;
        let monospace_advance = if is_monospace {
            match face.uniform_advance() {
                Ok(units) => round_px(i64::from(units), pixel_height, denom)?,
                Err(_) => 0,
            }
        } else {
            0
        };
        Ok(FamilyGeometry {
            px_per_em,
            baseline,
            height: pixel_height,
            line_height: pixel_height.saturating_add(line_gap_rows),
            monospace_advance,
        })
    }

    /// `family`'s [`FontMetrics`] at `pixel_height`.
    ///
    /// The requested `weight` does not currently change the result: this
    /// engine derives ascent, descent, line gap, and the monospace advance
    /// from a face's static tables, none of which this format varies by
    /// weight axis. The parameter is accepted (and validated by the wire
    /// decode) so the protocol stays ready for a face whose vertical metrics
    /// genuinely do vary once that is modelled.
    fn metrics_for(
        &mut self,
        family: FamilyKey,
        pixel_height: u32,
        _weight: FontWeight,
    ) -> Result<FontMetrics, Errno> {
        let family_index = self.index_of(family).ok_or(Errno::NotFound)?;
        let geometry = self.primary_geometry(family_index, pixel_height)?;
        Ok(FontMetrics {
            pixel_height: geometry.height,
            baseline: geometry.baseline,
            line_height: geometry.line_height,
            monospace_advance: geometry.monospace_advance,
        })
    }

    /// The installed selectable families — never a fallback-role family —
    /// in discovery order, framed as a [`FontRequest::Families`] reply.
    pub(crate) fn families_reply(&self, reply: &mut [u8]) -> Result<usize, Errno> {
        let mut entries: Vec<FamilyEntry> = Vec::new();
        for family in &self.families {
            if let Some(kind) = family.kind {
                entries.push(FamilyEntry::new(family.key, &family.label, kind)?);
            }
        }
        encode_families_reply(reply, Ok(&entries))
    }

    /// Resolve, rasterise (or fetch cached), and frame `scalar` from
    /// `family` at `pixel_height` in `weight` as a successful glyph reply.
    ///
    /// A scalar the grid draws as geometry is computed here and served
    /// without touching the cache: it is arithmetic over one cell, not a
    /// rasterisation, and retaining it would evict a real glyph to hold
    /// something cheaper to recompute than to look up.
    fn glyph_reply(
        &mut self,
        family: FamilyKey,
        scalar: char,
        pixel_height: u32,
        weight: FontWeight,
        reply: &mut [u8],
    ) -> Result<usize, Errno> {
        let family_index = self.index_of(family).ok_or(Errno::NotFound)?;
        let geometry = self.primary_geometry(family_index, pixel_height)?;
        let cell = geometry.cell(scalar);
        if let Some(cell) = cell {
            if let Some(drawn) = cell.line_art(scalar, geometry.height) {
                return encode_glyph_reply(
                    reply,
                    &tairix_abi::font_ipc::GlyphCoverage {
                        width: cell.width,
                        height: geometry.height,
                        advance: cell.advance(),
                        left: 0,
                        coverage: &samples(&drawn),
                    },
                );
            }
        }
        let source = self.resolve(family_index, scalar)?;
        let key = GlyphKey {
            requested: family.to_wire(),
            resolved: source.resolved_family_key.to_wire(),
            face: u32::try_from(source.face_index).unwrap_or(u32::MAX),
            glyph: u32::from(source.glyph),
            pixel_height,
            cells: cell.map_or(0, |cell| cell.cells),
            weight: weight.to_wire(),
        };
        let Self {
            families, cache, ..
        } = self;
        let served = cache
            .get_or_build(&(), key, || {
                build_glyph(families, &source, &geometry, cell, weight)
            })
            .ok_or(Errno::NotFound)?;
        encode_glyph_reply(
            reply,
            &tairix_abi::font_ipc::GlyphCoverage {
                width: served.width,
                height: served.height,
                advance: served.advance,
                left: served.left,
                coverage: &served.data,
            },
        )
    }

    /// Handle one request frame, writing the reply into `reply` and
    /// returning its length.
    ///
    /// Always produces a reply: a malformed request or a resolution/
    /// rasterisation failure becomes a status-word error frame (which every
    /// kind of client decodes as the carried [`Errno`]), never a dropped
    /// reply. A `0` return means even the error frame did not fit
    /// (structurally impossible for a correctly sized buffer) and the caller
    /// drops the reply, so the client fails closed on decode.
    pub fn handle(&mut self, request: &[u8], reply: &mut [u8]) -> usize {
        match FontRequest::from_bytes(request) {
            Ok(FontRequest::Glyph {
                family,
                scalar,
                pixel_height,
                weight,
            }) => match self.glyph_reply(family, scalar, pixel_height, weight, reply) {
                Ok(len) => len,
                Err(err) => error_frame(reply, err),
            },
            Ok(FontRequest::Metrics {
                family,
                pixel_height,
                weight,
            }) => {
                let result = self.metrics_for(family, pixel_height, weight);
                let bytes = encode_metrics_reply(result);
                if reply.len() < FONT_METRICS_REPLY_LEN {
                    return 0;
                }
                reply[..FONT_METRICS_REPLY_LEN].copy_from_slice(&bytes);
                FONT_METRICS_REPLY_LEN
            }
            Ok(FontRequest::Families) => match self.families_reply(reply) {
                Ok(len) => len,
                Err(err) => error_frame(reply, err),
            },
            Err(err) => error_frame(reply, err),
        }
    }
}

/// Rasterise the glyph `source` resolved to, at the requesting family's
/// shared `geometry`, in `weight` — yielding the [`CachedGlyph`] the reply is
/// served from.
///
/// A `cell` renders the glyph into a character cell: fitted to the grid the
/// client steps by, so the stems of a column of text line up and the pen
/// never accumulates a rounding error across a row. Without one the glyph is
/// tight to its own ink and positioned by its left bearing, which is what
/// proportional text is laid out from.
///
/// `None` when the resolved face's bytes cannot be read or parsed, or the
/// outline cannot be rasterised — the caller turns that into a refused
/// request rather than an empty bitmap.
fn build_glyph(
    families: &mut [FamilyRuntime<'_>],
    source: &GlyphSource,
    geometry: &FamilyGeometry,
    cell: Option<Cell>,
    weight: FontWeight,
) -> Option<CachedGlyph> {
    let px_per_em = geometry.px_per_em;
    let face_cache = families
        .get_mut(source.resolved_family_index)?
        .faces
        .get_mut(source.face_index)?;
    let has_wght = face_cache.has_wght().ok()?;
    let face = face_cache.instance_for(weight).ok()?;
    let drawn = match cell {
        Some(cell) => cell_glyph(face, source.glyph, geometry, cell)?,
        None => proportional_glyph(face, source.glyph, geometry)?,
    };
    let mut coverage = samples(&drawn.coverage);
    if !has_wght {
        let em_subpixels = round_pixel_measurement(px_per_em * f64::from(SUBPIXEL));
        let stroke = stroke_subpixels(em_subpixels, weight);
        embolden(
            &mut coverage,
            usize::try_from(drawn.width).unwrap_or(0),
            stroke,
        );
    }
    Some(CachedGlyph::new(
        drawn.width,
        geometry.height,
        drawn.advance,
        drawn.left,
        coverage,
    ))
}

/// One rasterised glyph before its coverage is widened and emboldened: the
/// bitmap's width, the pen advance, the left bearing, and 4-bit coverage.
struct Drawn {
    width: u32,
    advance: u32,
    left: i32,
    coverage: Vec<u8>,
}

/// Draw `glyph` into its character `cell`.
///
/// The bitmap is exactly the cells the scalar occupies, so the client blits
/// it at the cell origin with no bearing to apply, and the engine fits the
/// outline to that cell as it rasterises.
fn cell_glyph(face: &Face<'_>, glyph: u16, geometry: &FamilyGeometry, cell: Cell) -> Option<Drawn> {
    let box_geometry = CellGeometry {
        width: cell.width,
        height: geometry.height,
        baseline: geometry.baseline,
    };
    let width = cell.advance();
    let coverage = face
        .rasterise_glyph(glyph, &box_geometry, geometry.px_per_em, width)
        .ok()?;
    Some(Drawn {
        width,
        advance: width,
        left: 0,
        coverage,
    })
}

/// Draw `glyph` tight to its own ink, advanced and positioned by the face's
/// own metrics.
fn proportional_glyph(face: &Face<'_>, glyph: u16, geometry: &FamilyGeometry) -> Option<Drawn> {
    let px_per_em = geometry.px_per_em;
    let raster = face
        .rasterise_proportional(glyph, px_per_em, geometry.baseline, geometry.height)
        .ok()?;
    let advance_units = f64::from(face.advance(glyph).ok()?).max(0.0);
    let units_per_em = face.units_per_em();
    if units_per_em <= 0 {
        return None;
    }
    Some(Drawn {
        width: raster.width,
        advance: round_pixel_measurement(advance_units * px_per_em / f64::from(units_per_em)),
        left: raster.left,
        coverage: raster.coverage,
    })
}

/// Widen 4-bit engine coverage (`0..=15`) into the protocol's 8-bit samples,
/// `15` reaching a fully opaque `255`.
fn samples(coverage: &[u8]) -> Box<[u8]> {
    coverage
        .iter()
        .map(|&nibble| nibble.saturating_mul(17))
        .collect()
}

/// Frame a status-word error reply into `reply`, returning its length (`0`
/// only if the buffer cannot hold even the 4-byte status word).
fn error_frame(reply: &mut [u8], err: Errno) -> usize {
    encode_glyph_error_reply(reply, err).unwrap_or(0)
}

#[cfg(test)]
mod tests;
