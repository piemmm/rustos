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
use core::ops::Range;

use tairix_abi::font_ipc::{
    encode_batch_error_reply, encode_families_reply, encode_metrics_reply, ContourSource,
    FamilyEntry, FamilyKey, FamilyKind, FontMetrics, FontRequest, FontStretch, FontStyle,
    FontUnits, FontWeight, GlyphBatchWriter, GlyphCoverage, GlyphRun, GlyphSegment,
    OutlineBatchWriter, OutlineSource, Synthesis, FONT_FAMILY_KEY_LEN, FONT_MAX_SYNTH_BOLD,
    FONT_METRICS_REPLY_LEN, FONT_STRETCH_SCALE, FONT_SYNTH_BOLD_SCALE,
};
use tairix_abi::Errno;
use tairix_font::{glyph_cache_budget, glyph_cache_candidate, CachedGlyph};
use tairix_fontface::{
    lineart, AxisSetting, CellGeometry, Contour, Face, GenericFamily, OutlineSegment,
};
use tairix_hash::{BuildSipHash13, HashSeed};
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
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
pub type GlyphCache = ReclaimCache<GlyphKey, CachedGlyph, (), BuildSipHash13>;

/// The audit label the service's cache is named by in reclaim records.
const CACHE_LABEL: &str = "fontd.glyphs";

/// The owner the service's cache charges its bytes to: this service process,
/// named directly, since a userland service has no numeric task id to quote.
const CACHE_OWNER: &str = "fontd";

/// A fixed key for the crate's own tests, so a run's cache layout is
/// reproducible.
#[cfg(test)]
pub(crate) const TEST_HASH_KEY: HashSeed =
    HashSeed::from_words(0x464F_4E54_4400_0001, 0x464F_4E54_4400_0002);

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
///
/// `key` is this service's hash key. Every client on the machine chooses the
/// characters, sizes, and weights this index is keyed by, so an unpredictable
/// key is what stops one of them crowding a bucket and slowing every other
/// client's glyphs; a service that could draw none is given a zero budget by
/// its caller and retains nothing instead.
#[must_use]
pub fn glyph_cache(
    total_ram_bytes: u64,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
    key: HashSeed,
) -> GlyphCache {
    let cache = ReclaimCache::new(
        CACHE_LABEL,
        glyph_cache_candidate(ReclaimOwner::UserlandProcess(CACHE_OWNER)),
        glyph_cache_budget(total_ram_bytes),
        pressure,
        sink,
        BuildSipHash13::with_seed(key),
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

/// The design-axis coordinate one request asks a face to be instanced at.
///
/// One key for every axis the protocol exposes, so a face is parsed once per
/// *distinct* instance actually requested rather than once per axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FaceInstance {
    pub(crate) weight: FontWeight,
    pub(crate) style: FontStyle,
    pub(crate) stretch: FontStretch,
}

impl FaceInstance {
    /// The instance a coverage request asks for: the desktop draws upright
    /// text at its own width, so only the weight varies there.
    const fn upright(weight: FontWeight) -> Self {
        Self {
            weight,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        }
    }

    /// Whether this instance leaves every axis at the face's own default, so
    /// the default parse already is it.
    fn is_default(self) -> bool {
        self.weight == FontWeight::REGULAR
            && self.style == FontStyle::Normal
            && self.stretch == FontStretch::NORMAL
    }
}

/// Which variation axes a face declares, read once with its default parse.
///
/// A bit per axis rather than a field per axis: what the code asks is
/// whether the face carries one, and the set grows with the protocol's axes
/// rather than with a struct's field count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FaceAxes(u8);

impl FaceAxes {
    /// The weight axis.
    const WGHT: u8 = 1 << 0;
    /// The width axis.
    const WDTH: u8 = 1 << 1;
    /// The designer's own italic axis.
    const ITAL: u8 = 1 << 2;
    /// The slant axis, in degrees counter-clockwise.
    const SLNT: u8 = 1 << 3;

    /// Whether the face declares the axis `bit` names.
    const fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    /// Whether the face can render a weight by itself.
    const fn wght(self) -> bool {
        self.has(Self::WGHT)
    }

    /// Whether the face can lean by itself, through either axis that leans.
    const fn can_slant(self) -> bool {
        self.has(Self::ITAL) || self.has(Self::SLNT)
    }
}

/// The oblique lean a synthetic slant applies, in degrees — CSS's own
/// default for `font-style: oblique`, which is what a document asking for a
/// posture the face cannot furnish means.
const SYNTHETIC_OBLIQUE_DEGREES: f32 = 14.0;

/// The same lean as the horizontal shear a caller applies, in the protocol's
/// 2.14 fixed point: `tan(14°)`.
///
/// A constant rather than a computed `tan` because the angle is fixed policy
/// and the service has no business carrying trigonometry for one number.
const SYNTHETIC_OBLIQUE_SHEAR: i16 = 4085;

/// One face's lazily-read bytes and its cached parsed instances.
///
/// The face's bytes are read on first use ([`FaceLoad::load`]) and retained
/// for the service's life. The default (unvaried) instance is parsed once
/// and used for every codepoint lookup and as the geometry source, since a
/// face's `cmap` and vertical metrics never change with variation in this
/// engine. A face declaring an axis a request moves additionally caches one
/// instanced [`Face`] per distinct [`FaceInstance`] actually asked for; a
/// face declaring none of them reuses the one default instance and is
/// completed by the caller-side synthesis the reply reports.
pub(crate) struct FaceCache<'a> {
    loader: Box<dyn FaceLoad<'a> + 'a>,
    bytes: Option<&'a [u8]>,
    default: Option<Face<'a>>,
    axes: FaceAxes,
    instances: Vec<(FaceInstance, Face<'a>)>,
}

impl<'a> FaceCache<'a> {
    /// A face whose bytes will be obtained from `loader` on first use.
    pub(crate) fn new(loader: Box<dyn FaceLoad<'a> + 'a>) -> Self {
        Self {
            loader,
            bytes: None,
            default: None,
            axes: FaceAxes::default(),
            instances: Vec::new(),
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
        let declared = |tag: &[u8; 4], bit: u8| {
            u8::from(face.axes().iter().any(|axis| axis.tag == *tag)) * bit
        };
        self.axes = FaceAxes(
            declared(b"wght", FaceAxes::WGHT)
                | declared(b"wdth", FaceAxes::WDTH)
                | declared(b"ital", FaceAxes::ITAL)
                | declared(b"slnt", FaceAxes::SLNT),
        );
        self.default = Some(face);
        Ok(())
    }

    /// The default (unvaried) instance: the source of `cmap` lookups and of
    /// the family's shared geometry.
    fn default_face(&mut self) -> Result<&Face<'a>, Errno> {
        self.ensure_default()?;
        self.default.as_ref().ok_or(Errno::BadMagic)
    }

    /// Which variation axes this face declares.
    fn axes(&mut self) -> Result<FaceAxes, Errno> {
        self.ensure_default()?;
        Ok(self.axes)
    }

    /// What `instance` asks for that this face cannot furnish, and the
    /// caller must therefore complete on the geometry it is handed.
    ///
    /// A face declaring the axis renders the real thing and reports nothing,
    /// even where the request lands outside the axis's own range: the
    /// designer's widest weight is a better bold than a stroke over it.
    /// Width is never synthesised at all — stretching letterforms is a
    /// distortion, not a width — so an absent `wdth` axis is simply the
    /// face's own width, which is what CSS says a UA must do.
    fn synthesis_for(&mut self, instance: FaceInstance) -> Result<Synthesis, Errno> {
        let axes = self.axes()?;
        let bold = if axes.wght() {
            0
        } else {
            synthetic_bold_em(instance.weight)
        };
        let shear = if instance.style == FontStyle::Normal || axes.can_slant() {
            0
        } else {
            SYNTHETIC_OBLIQUE_SHEAR
        };
        Synthesis::new(bold, shear)
    }

    /// The instance to draw `instance` from: the cached instanced face when
    /// this face declares any axis the request moves, else the one default
    /// instance, which the reported [`Synthesis`] completes.
    fn instance_for(&mut self, instance: FaceInstance) -> Result<&Face<'a>, Errno> {
        let axes = self.ensure_default().and(Ok(self.axes))?;
        let settings = instance_settings(instance, axes);
        if settings.is_empty() {
            return self.default_face();
        }
        if !self.instances.iter().any(|&(held, _)| held == instance) {
            let bytes = self.face_bytes()?;
            let face = Face::parse_instance(bytes, &settings).map_err(|_| Errno::BadMagic)?;
            self.instances.push((instance, face));
        }
        self.instances
            .iter()
            .find_map(|(held, face)| (*held == instance).then_some(face))
            .ok_or(Errno::BadMagic)
    }
}

/// The axis settings `instance` moves that `axes` actually declares.
///
/// Empty when the face can furnish none of them, or the request leaves every
/// axis at its default — either way the default parse already *is* the
/// instance, so nothing is parsed twice.
fn instance_settings(instance: FaceInstance, axes: FaceAxes) -> Vec<AxisSetting> {
    let mut settings = Vec::new();
    if instance.is_default() {
        return settings;
    }
    if axes.has(FaceAxes::WGHT) {
        settings.push(AxisSetting {
            tag: *b"wght",
            value: f32::from(instance.weight.axis_value()),
        });
    }
    if axes.has(FaceAxes::WDTH) {
        settings.push(AxisSetting {
            tag: *b"wdth",
            value: f32::from(instance.stretch.hundredths()) / f32::from(FONT_STRETCH_SCALE),
        });
    }
    if instance.style != FontStyle::Normal {
        // A designer's own italic is the better answer where the face has
        // one; `slnt` leans the upright letterforms, which is what oblique
        // means and the nearest thing to an italic a face without `ital` can
        // offer. `slnt` counts counter-clockwise, so a forward lean is
        // negative.
        if axes.has(FaceAxes::ITAL) && instance.style == FontStyle::Italic {
            settings.push(AxisSetting {
                tag: *b"ital",
                value: 1.0,
            });
        } else if axes.has(FaceAxes::SLNT) {
            settings.push(AxisSetting {
                tag: *b"slnt",
                value: -SYNTHETIC_OBLIQUE_DEGREES,
            });
        }
    }
    settings
}

/// The synthetic bold stroke `weight` asks for, as a fraction of the em in
/// the protocol's own units.
///
/// The one ramp both sides of the service use: the coverage path applies it
/// to the raster it already holds, and the outline path reports it for the
/// caller to stroke with, so a bold drawn as pixels and one drawn as
/// geometry are the same weight.
fn synthetic_bold_em(weight: FontWeight) -> u16 {
    let stroke = stroke_subpixels(FONT_SYNTH_BOLD_SCALE, weight);
    u16::try_from(stroke)
        .unwrap_or(u16::MAX)
        .min(FONT_MAX_SYNTH_BOLD)
}

/// One discovered family: its manifest facts plus its lazily-loaded faces.
pub(crate) struct FamilyRuntime<'a> {
    key: FamilyKey,
    label: String,
    /// How the family lays text out, or `None` for a fallback-role family a
    /// user never selects directly.
    kind: Option<FamilyKind>,
    /// The CSS generic this family is the store's answer for, if it claims
    /// one in its manifest.
    generic: Option<GenericFamily>,
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
        generic: Option<GenericFamily>,
        faces: Vec<FaceCache<'a>>,
        fallback: Option<FamilyKey>,
    ) -> Self {
        Self {
            key,
            label,
            kind,
            generic,
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

    /// The family a request naming `key` is served from.
    ///
    /// An installed family of that exact key always wins, so a store may
    /// name a family `serif` and mean it. Otherwise a key spelling one of
    /// CSS's generic families walks the ladder: the first discovered family
    /// claiming that generic in its own manifest, then the first claiming
    /// `sans-serif` (the generic a desktop always has), then the first
    /// selectable family at all — and, with a store of nothing but
    /// coverage-only families, fails closed.
    ///
    /// A key that is neither installed nor generic is **not** substituted:
    /// a document naming `Helvetica` is told the store does not hold it, so
    /// it can try the next family it named rather than being served
    /// something it did not ask for.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when no family answers.
    fn resolve_family(&self, key: FamilyKey) -> Result<usize, Errno> {
        if let Some(position) = self.index_of(key) {
            return Ok(position);
        }
        let generic = GenericFamily::from_key(key).ok_or(Errno::NotFound)?;
        self.claiming(generic)
            .or_else(|| self.claiming(GenericFamily::SansSerif))
            .or_else(|| {
                self.families
                    .iter()
                    .position(|family| family.kind.is_some())
            })
            .ok_or(Errno::NotFound)
    }

    /// The label of the family a key resolves to, for a test asserting
    /// which rung of the ladder answered.
    #[cfg(test)]
    pub(crate) fn family_for_key(&self, key: FamilyKey) -> Option<&str> {
        let index = self.resolve_family(key).ok()?;
        self.families.get(index).map(|family| family.label.as_str())
    }

    /// The first discovered family claiming `generic`.
    ///
    /// Discovery order is sorted by key, so two families claiming the same
    /// generic resolve deterministically rather than by scan order.
    fn claiming(&self, generic: GenericFamily) -> Option<usize> {
        self.families
            .iter()
            .position(|family| family.generic == Some(generic))
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

    /// Resolve `scalar` for a [`FontRequest::Glyphs`] naming `family_index`:
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
        let family_index = self.resolve_family(family)?;
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

    /// Resolve, rasterise (or fetch cached), and frame as many of `run` from
    /// `family` at `pixel_height` in `weight` as the reply frame holds.
    ///
    /// The batch answers a prefix and says how long it is, so the client asks
    /// again for the remainder. It stops at the first scalar the frame cannot
    /// hold or the faces cannot yield — a truncated batch is the client's cue
    /// to come back, and only a failure on the *first* scalar has nothing to
    /// report and is refused outright.
    fn glyphs_reply(
        &mut self,
        family: FamilyKey,
        run: &GlyphRun,
        pixel_height: u32,
        weight: FontWeight,
        reply: &mut [u8],
    ) -> Result<usize, Errno> {
        let family_index = self.resolve_family(family)?;
        // Resolved once for the whole run: the family and its line geometry
        // are what the run shares, so a per-scalar re-derivation would be
        // paid for nothing.
        let geometry = self.primary_geometry(family_index, pixel_height)?;
        let mut writer = GlyphBatchWriter::new(reply)?;
        for &scalar in run.scalars() {
            let pushed =
                self.push_glyph(family, family_index, &geometry, scalar, weight, &mut writer);
            let fitted = match pushed {
                Ok(fitted) => fitted,
                Err(err) if writer.count() == 0 => return Err(err),
                Err(_) => false,
            };
            if !fitted {
                break;
            }
        }
        writer.finish()
    }

    /// Append `scalar`'s record to `writer`, reporting whether it fitted.
    ///
    /// A scalar the grid draws as geometry is computed here and served
    /// without touching the cache: it is arithmetic over one cell, not a
    /// rasterisation, and retaining it would evict a real glyph to hold
    /// something cheaper to recompute than to look up.
    fn push_glyph(
        &mut self,
        family: FamilyKey,
        family_index: usize,
        geometry: &FamilyGeometry,
        scalar: char,
        weight: FontWeight,
        writer: &mut GlyphBatchWriter<'_>,
    ) -> Result<bool, Errno> {
        let cell = geometry.cell(scalar);
        if let Some(cell) = cell {
            if let Some(drawn) = cell.line_art(scalar, geometry.height) {
                return writer.push(&GlyphCoverage {
                    width: cell.width,
                    height: geometry.height,
                    advance: cell.advance(),
                    left: 0,
                    coverage: &samples(&drawn),
                });
            }
        }
        let source = self.resolve(family_index, scalar)?;
        let key = GlyphKey {
            requested: family.to_wire(),
            resolved: source.resolved_family_key.to_wire(),
            face: u32::try_from(source.face_index).unwrap_or(u32::MAX),
            glyph: u32::from(source.glyph),
            pixel_height: geometry.height,
            cells: cell.map_or(0, |cell| cell.cells),
            weight: weight.to_wire(),
        };
        let Self {
            families, cache, ..
        } = self;
        let served = cache
            .get_or_build(&(), key, || {
                build_glyph(families, &source, geometry, cell, weight)
            })
            .ok_or(Errno::NotFound)?;
        writer.push(&GlyphCoverage {
            width: served.width,
            height: served.height,
            advance: served.advance,
            left: served.left,
            coverage: &served.data,
        })
    }

    /// The requested family's primary-face geometry in that face's own font
    /// units — the frame of reference a whole run is laid out in, whichever
    /// faces its individual scalars resolve to.
    ///
    /// Distinct from [`primary_geometry`](Self::primary_geometry), which
    /// resolves the same face against a *pixel* height: an outline reply has
    /// no resolution, so there is nothing to resolve it against.
    fn primary_face_units(&mut self, family_index: usize) -> Result<FaceUnits, Errno> {
        let family = self.families.get_mut(family_index).ok_or(Errno::NotFound)?;
        let primary = family.faces.first_mut().ok_or(Errno::NotFound)?;
        let face = primary.default_face()?;
        let units_per_em = u32::try_from(face.units_per_em()).map_err(|_| Errno::BadMagic)?;
        Ok(FaceUnits {
            units_per_em,
            ascent: face.ascent(),
            descent: face.descent(),
            line_gap: face.line_gap(),
        })
    }

    /// Resolve and outline as many of `run` from `family` as the reply frame
    /// holds, instanced at the requested axes.
    ///
    /// The batch answers a prefix and says how long it is, exactly as the
    /// coverage reply does, so a client asks again for the remainder. It
    /// stops at the first scalar the frame cannot hold or the faces cannot
    /// yield; only a failure on the *first* scalar has nothing to report and
    /// is refused outright.
    fn outlines_reply(
        &mut self,
        family: FamilyKey,
        run: &GlyphRun,
        instance: FaceInstance,
        reply: &mut [u8],
    ) -> Result<usize, Errno> {
        let family_index = self.resolve_family(family)?;
        let units = self.primary_face_units(family_index)?;
        let mut writer = OutlineBatchWriter::new(
            reply,
            units.units_per_em,
            units.ascent,
            units.descent,
            units.line_gap,
        )?;
        for &scalar in run.scalars() {
            let pushed = self.push_outline(family_index, scalar, instance, &mut writer);
            let fitted = match pushed {
                Ok(fitted) => fitted,
                Err(err) if writer.count() == 0 => return Err(err),
                Err(_) => false,
            };
            if !fitted {
                break;
            }
        }
        writer.finish()
    }

    /// Append `scalar`'s outline record to `writer`, reporting whether it
    /// fitted.
    fn push_outline(
        &mut self,
        family_index: usize,
        scalar: char,
        instance: FaceInstance,
        writer: &mut OutlineBatchWriter<'_>,
    ) -> Result<bool, Errno> {
        let source = self.resolve(family_index, scalar)?;
        let face_cache = self
            .families
            .get_mut(source.resolved_family_index)
            .and_then(|family| family.faces.get_mut(source.face_index))
            .ok_or(Errno::NotFound)?;
        let synth = face_cache.synthesis_for(instance)?;
        let face = face_cache.instance_for(instance)?;
        let units_per_em = u32::try_from(face.units_per_em()).map_err(|_| Errno::BadMagic)?;
        let advance = FontUnits::from_f64(f64::from(face.advance(source.glyph).unwrap_or(0)))?;
        let outline = face
            .glyph_outline(source.glyph)
            .map_err(|_| Errno::BadMagic)?;
        let (contours, segments) = wire_contours(&outline)?;
        let borrowed: Vec<ContourSource<'_>> = contours
            .iter()
            .map(|(start, range)| ContourSource {
                start: *start,
                segments: &segments[range.clone()],
            })
            .collect();
        writer.push(&OutlineSource {
            units_per_em,
            advance,
            synth,
            contours: &borrowed,
        })
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
            Ok(FontRequest::Glyphs {
                family,
                scalars,
                pixel_height,
                weight,
            }) => match self.glyphs_reply(family, &scalars, pixel_height, weight, reply) {
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
            Ok(FontRequest::Outlines {
                family,
                scalars,
                weight,
                style,
                stretch,
            }) => {
                let instance = FaceInstance {
                    weight,
                    style,
                    stretch,
                };
                match self.outlines_reply(family, &scalars, instance, reply) {
                    Ok(len) => len,
                    Err(err) => error_frame(reply, err),
                }
            }
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
    let has_wght = face_cache.axes().ok()?.wght();
    let face = face_cache
        .instance_for(FaceInstance::upright(weight))
        .ok()?;
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

/// One face's own vertical geometry, in its own font units.
struct FaceUnits {
    units_per_em: u32,
    ascent: i32,
    descent: i32,
    line_gap: i32,
}

/// Convert an engine outline into the protocol's fixed-point segments.
///
/// The segments of every contour are collected into one run, each contour
/// naming its own slice of it, so the wire form borrows rather than
/// allocating a vector per contour. A coordinate the fixed-point form cannot
/// represent refuses the glyph, which is how a corrupt outline fails closed
/// instead of being saturated into a shape the face does not state.
type WireContours = (
    Vec<((FontUnits, FontUnits), Range<usize>)>,
    Vec<GlyphSegment>,
);

fn wire_contours(outline: &[Contour]) -> Result<WireContours, Errno> {
    let mut segments: Vec<GlyphSegment> = Vec::new();
    let mut contours = Vec::with_capacity(outline.len());
    for contour in outline {
        let from = segments.len();
        for segment in &contour.segments {
            segments.push(match *segment {
                OutlineSegment::Line { to } => GlyphSegment::Line {
                    to: wire_point(to)?,
                },
                OutlineSegment::Quadratic { control, to } => GlyphSegment::Quadratic {
                    control: wire_point(control)?,
                    to: wire_point(to)?,
                },
            });
        }
        contours.push((wire_point(contour.start)?, from..segments.len()));
    }
    Ok((contours, segments))
}

/// One outline point in the protocol's fixed-point font units.
fn wire_point(point: (f64, f64)) -> Result<(FontUnits, FontUnits), Errno> {
    Ok((FontUnits::from_f64(point.0)?, FontUnits::from_f64(point.1)?))
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
    encode_batch_error_reply(reply, err).unwrap_or(0)
}

#[cfg(test)]
mod tests;
