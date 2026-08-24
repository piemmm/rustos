//! The font-service client: the render path's thin, cached front end to the
//! sandboxed OS font service (`fontd`) over the reserved `FONT_ENDPOINT`.
//!
//! `lib/font` parses no TrueType and holds no font outline: the faces live
//! only in `fontd`, which rasterises a scalar from a named family at a
//! chosen pixel height in its own minimum-capability sandbox and returns the
//! small 8-bit coverage bitmap the blitter composites, or reports a family's
//! line metrics. A malformed face can therefore fault only that sandbox,
//! never a compositor or a terminal.
//!
//! # Transport seam
//!
//! Drawing text takes no client handle — [`crate::BitmapFont::draw_text`] is a
//! plain method — so the client is a process-global behind a [`FontTransport`]
//! seam: production installs the `ipc_call`-backed transport, a host test
//! installs a mock. Under the optional `rt` feature the seam defaults lazily
//! to the runtime transport, so a program that links `tairix-rt` needs no
//! setup; without a transport a draw composites nothing, fail closed, rather
//! than reaching for a device.
//!
//! # Two locks, and never both at once
//!
//! Userland is multi-threaded, so this process-global state is locked — with
//! the runtime's futex mutex, because a preempted holder would make a spinning
//! waiter burn a whole quantum against a thread that is not running. The state
//! is split in two: the *caches* are pure memory and their lock is held for the
//! span of a run; the *channel* owns the one transport and its lock *is* held
//! across the `fontd` round trip. A fetch releases the caches before taking the
//! channel, so the caches lock never spans a syscall and no thread ever holds
//! both — the whole discipline, with no lock order to get wrong. The crate's
//! internal font-client trait is where it lives.
//!
//! # Local caches
//!
//! Each glyph reply is memoised per `(scalar, family, pixel height, weight)`
//! in a [`tairix_reclaim::ReclaimCache`] ([`crate::glyph_cache`]), so a
//! steady-state redraw of the same text in the same family, size, and weight
//! issues no IPC. The byte budget is derived from the machine's total RAM,
//! never a hand-picked entry count, so a hostile or careless caller who
//! renders at ever more sizes and scalars can grow the cache only up to that
//! budget before the oldest entries are evicted. Until a cache is installed —
//! before an `rt` program's first draw, or in a host test that never calls
//! [`set_glyph_cache`] — every glyph is fetched and served uncached: correct,
//! merely one IPC per glyph.
//!
//! Each family's line metrics are memoised per `(family, pixel height,
//! weight)` in a small fixed-capacity cache: a process draws only a handful
//! of distinct combinations (the theme's role ladder, times a couple of
//! families, times the three weights), so unlike the glyph cache a byte
//! budget would be over-engineering — a bounded ring buffer of entries is
//! cheap, simple, and already generous for that working set.
//!
//! A proportional family's *measurements* are memoised the same way as its
//! glyphs, in the crate's `measure` module: the per-character advance walk a
//! label costs is done once and answered from thereafter, so a repaint of
//! unchanged text measures nothing. A monospace family never reaches the memo
//! — its width is a multiplication.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::font_ipc::{
    decode_families_reply, decode_glyph_reply, decode_metrics_reply, FamilyEntry, FamilyKey,
    FontMetrics, FontRequest, FontWeight, FONT_FAMILY_KEY_LEN, FONT_MAX_FAMILIES_REPLY,
    FONT_MAX_GLYPH_REPLY, FONT_METRICS_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_reclaim::ReclaimCache;
use tairix_rt::sync::{Mutex, MutexGuard};

use crate::atlas;
use crate::glyph_cache::CachedGlyph;
use crate::measure::{self, MeasureCache, MeasureEpoch, MeasuredText};

/// One synchronous font-service call: send one request frame, receive one
/// reply frame. The `ipc_call` syscall behind a seam, so the render path is
/// host-testable.
pub trait FontTransport: Send {
    /// Issue one call to the font endpoint, returning the reply length
    /// written into `reply`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the transport surfaces (no such endpoint, a dead
    /// server); the caller treats a transport failure exactly like a refused
    /// request and composites nothing (a glyph) or falls back (metrics).
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// A glyph cache key: the Unicode scalar, the family it was rendered from
/// (its NUL-padded wire bytes), the pixel height it was rendered at, and the
/// wire weight it was rendered in. A different family, height, or heavier
/// weight is a different bitmap of the same scalar, so each is part of the
/// key rather than overwriting one another.
pub type GlyphKey = (u32, [u8; FONT_FAMILY_KEY_LEN], u32, u16);

/// The render path's glyph cache: the shared bounded, classified,
/// pressure-governed cache holding [`CachedGlyph`] coverage under a
/// [`GlyphKey`].
///
/// The generation token is `()` because nothing invalidates a fetched glyph
/// while the process lives: `fontd` parses its face set once at startup and
/// never reloads it, so the same scalar from the same family at the same
/// height and weight is the same bitmap every time. Entries leave only by
/// eviction, by pressure, or with the cache itself — which is exactly the
/// owner-teardown invalidation the shared classification declares. Inventing
/// a churning epoch would throw a live working set away for no event.
pub type GlyphCache = ReclaimCache<GlyphKey, CachedGlyph, ()>;

/// The audit label the client's own cache is named by in reclaim records.
#[cfg(feature = "rt")]
const CLIENT_CACHE_LABEL: &str = "font.client.glyphs";

/// The audit label the client's measurement memo is named by.
#[cfg(feature = "rt")]
const MEASURE_CACHE_LABEL: &str = "font.client.measure";

/// The owner the client's own cache charges its bytes to.
///
/// The cache names *itself*, not the program it is linked into: this client
/// is embedded in every text-drawing program, and a reader of the audit trail
/// wants to know which subsystem inside the process is holding the memory.
#[cfg(feature = "rt")]
const CLIENT_CACHE_OWNER: &str = "font-client";

/// A metrics cache key: the family (its NUL-padded wire bytes), the pixel
/// height, and the wire weight the metrics were measured at.
type MetricsKey = ([u8; FONT_FAMILY_KEY_LEN], u32, u16);

/// Entries a [`MetricsCache`] retains before the oldest is evicted to make
/// room for a new one.
///
/// A themed desktop draws only a handful of distinct `(family, pixel height,
/// weight)` combinations in one process — the role ladder times a couple of
/// families times the three weights — so this comfortably covers a real
/// session with headroom for a font picker previewing a few more, while
/// still bounding the cache to a fixed size regardless of how many distinct
/// sizes a hostile or careless caller asks for.
const METRICS_CACHE_CAPACITY: usize = 64;

/// A small, fixed-capacity cache of fetched [`FontMetrics`], keyed by
/// `(family, pixel_height, weight)`.
///
/// Unlike the glyph cache, a metrics entry is a handful of bytes and the
/// working set is tiny, so a byte-budgeted [`tairix_reclaim::ReclaimCache`]
/// would be pure overhead here: a fixed-size ring buffer that evicts the
/// oldest entry on a miss once full is simpler, allocates nothing beyond its
/// own fixed array, and is cheap enough to consult on every draw.
struct MetricsCache {
    entries: [Option<(MetricsKey, FontMetrics)>; METRICS_CACHE_CAPACITY],
    /// The slot the next inserted entry lands in; wraps to evict the oldest.
    next: usize,
}

impl MetricsCache {
    const fn new() -> Self {
        Self {
            entries: [None; METRICS_CACHE_CAPACITY],
            next: 0,
        }
    }

    fn get(&self, key: MetricsKey) -> Option<FontMetrics> {
        self.entries
            .iter()
            .find_map(|slot| slot.filter(|&(k, _)| k == key).map(|(_, metrics)| metrics))
    }

    fn insert(&mut self, key: MetricsKey, metrics: FontMetrics) {
        if self.get(key).is_some() {
            return;
        }
        self.entries[self.next] = Some((key, metrics));
        self.next = (self.next + 1) % METRICS_CACHE_CAPACITY;
    }
}

/// The process's font caches: fetched glyph coverage, fetched line metrics,
/// and measured text.
///
/// Reached under its own lock, which is **never** held across a font-service
/// call: everything here is pure memory, so a critical section over it is
/// bounded by arithmetic and never by a syscall.
pub(crate) struct Caches {
    /// `None` until a cache is installed, in which case every glyph is
    /// fetched and served without being retained — correct, merely one IPC
    /// per glyph.
    glyphs: Option<GlyphCache>,
    metrics: MetricsCache,
    /// `None` until a memo is installed, in which case every measurement is
    /// walked afresh — correct, merely one advance lookup per character.
    measure: Option<MeasureCache>,
    /// The generation retained measurements are keyed to, moved on whenever a
    /// transport is installed.
    advance_source: MeasureEpoch,
}

impl Caches {
    const fn new() -> Self {
        Self {
            glyphs: None,
            metrics: MetricsCache::new(),
            measure: None,
            advance_source: 0,
        }
    }

    /// Shrink both byte-budgeted caches to the band's ceiling, returning the
    /// bytes released. A cache that is not installed is nothing to release,
    /// and neither is built to answer.
    fn trim(&mut self) -> usize {
        let glyphs = self
            .glyphs
            .as_mut()
            .map_or(0, ReclaimCache::enforce_pressure);
        let measurements = self
            .measure
            .as_mut()
            .map_or(0, ReclaimCache::enforce_pressure);
        glyphs.saturating_add(measurements)
    }
}

/// The process's one channel to the font service: the installed transport and
/// the reusable buffer it receives replies into.
///
/// Reached under its own lock, which *is* held across the service round trip —
/// that is what the lock is for, since there is one transport and one buffer.
/// A second thread's fetch therefore parks on it rather than issuing a
/// duplicate call.
pub(crate) struct Channel {
    transport: Option<Box<dyn FontTransport>>,
    reply: Vec<u8>,
    /// Whether this build's lazy defaults have been installed, so the
    /// installation happens once however many threads draw first.
    #[cfg(any(feature = "rt", feature = "test-util"))]
    defaulted: bool,
}

impl Channel {
    const fn new() -> Self {
        Self {
            transport: None,
            reply: Vec::new(),
            #[cfg(any(feature = "rt", feature = "test-util"))]
            defaulted: false,
        }
    }

    /// Install `transport` as this channel's advance source.
    fn install_transport(&mut self, transport: Box<dyn FontTransport>) {
        self.transport = Some(transport);
    }

    /// Fetch one glyph's coverage, or `None` when no transport is installed or
    /// the call or its reply could not be read (fail closed: the caller
    /// composites nothing).
    fn glyph(
        &mut self,
        scalar: char,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> Option<CachedGlyph> {
        let transport = self.transport.as_mut()?;
        fetch_glyph(
            transport.as_mut(),
            &mut self.reply,
            scalar,
            family,
            pixel_height,
            weight,
        )
    }

    /// Fetch one family's line metrics, or `None` on any refusal.
    fn metrics(
        &mut self,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> Option<FontMetrics> {
        let transport = self.transport.as_mut()?;
        fetch_metrics(
            transport.as_mut(),
            &mut self.reply,
            family,
            pixel_height,
            weight,
        )
    }

    /// The installed families, or an empty list when no transport is
    /// installed or the service refuses the request (fail closed).
    fn families(&mut self) -> Vec<FamilyEntry> {
        let Some(transport) = self.transport.as_mut() else {
            return Vec::new();
        };
        if self.reply.len() < FONT_MAX_FAMILIES_REPLY {
            self.reply.resize(FONT_MAX_FAMILIES_REPLY, 0);
        }
        let request = FontRequest::Families.to_le_bytes();
        let Ok(len) = transport.call(&request, &mut self.reply) else {
            return Vec::new();
        };
        let Some(frame) = self.reply.get(..len) else {
            return Vec::new();
        };
        decode_families_reply(frame)
            .map(|list| list.entries().to_vec())
            .unwrap_or_default()
    }
}

/// The font client: the process's caches, plus the channel reached only
/// through [`fetch_glyph`](FontClient::fetch_glyph) and its siblings.
///
/// # The locking discipline this trait exists to express
///
/// The two pieces of state have separate locks, and **no thread ever holds
/// both**. An implementation's `fetch_*` gives the caches up for the duration
/// of the service call and takes them again afterwards, so the caches lock
/// never spans a syscall and a thread waiting for it is never waiting on a
/// holder that is asleep in the kernel. There is no lock *order* to get wrong,
/// because there is never a moment when one is held while the other is taken.
///
/// The serve logic below is written once, here, so the process-global client
/// and a host test's local one cannot diverge on what a hit costs, what a miss
/// retains, or what a refusal falls back to.
pub(crate) trait FontClient {
    /// The caches, re-acquired if a `fetch_*` released them.
    fn caches(&mut self) -> &mut Caches;

    /// Fetch one glyph over the channel, with the caches released.
    fn fetch_glyph(
        &mut self,
        scalar: char,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> Option<CachedGlyph>;

    /// Fetch one family's line metrics over the channel, with the caches
    /// released.
    fn fetch_metrics(
        &mut self,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> Option<FontMetrics>;

    /// List the installed families over the channel, with the caches
    /// released.
    fn fetch_families(&mut self) -> Vec<FamilyEntry>;

    /// Install `transport` into the channel, with the caches released.
    fn set_transport(&mut self, transport: Box<dyn FontTransport>);

    /// Install (or replace) the advance source: the transport, and the
    /// measurement epoch that moves on with it.
    ///
    /// The transport goes in *first* and the epoch after it, so a thread
    /// measuring in between retains under the outgoing epoch and has its entry
    /// discarded — never served against a source that did not produce it.
    fn install_advance_source(&mut self, transport: Box<dyn FontTransport>) {
        self.set_transport(transport);
        let caches = self.caches();
        caches.advance_source = caches.advance_source.wrapping_add(1);
    }

    /// Serve `(scalar, family, pixel_height, weight)` to `f`, fetching it over
    /// the channel on a miss and retaining it when a cache is installed and
    /// admits it.
    ///
    /// A hit serves from inside the cache. A miss serves from the *fetched*
    /// value — with no lock held at all — and offers it to the cache
    /// afterwards, which is what keeps the coverage blit off both locks and
    /// counts the one lookup exactly once.
    ///
    /// `None` — composite nothing, fail closed — when no transport is
    /// installed or the call or its reply could not be read.
    fn with_glyph<R>(
        &mut self,
        scalar: char,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
        f: impl FnOnce(&CachedGlyph) -> R,
    ) -> Option<R> {
        let key = (
            scalar as u32,
            family.to_wire(),
            pixel_height,
            weight.to_wire(),
        );
        if let Some(cache) = self.caches().glyphs.as_mut() {
            // `build` answers `None` on purpose: the lookup is counted here,
            // the value is produced with the lock released, and `retain`
            // below admits it without counting a second lookup.
            if let Some(served) = cache.get_or_build(&(), key, || None) {
                return Some(f(&served));
            }
        }
        let glyph = self.fetch_glyph(scalar, family, pixel_height, weight)?;
        let served = f(&glyph);
        if let Some(cache) = self.caches().glyphs.as_mut() {
            cache.retain(&(), key, glyph);
        }
        Some(served)
    }

    /// Serve `text`'s measurement in `(family, pixel_height, weight)` to `f`,
    /// walking the per-character advances on a miss and retaining the walk
    /// when a memo is installed and admits it.
    ///
    /// Never fails: with no memo installed, a memo that will not admit the
    /// entry, or a fingerprint clash, the walk is done for this caller alone
    /// and `f` sees exactly what it would have.
    ///
    /// A walk the advance source could not complete is served but not
    /// retained, so a service that recovers is measured again rather than
    /// answered forever from a walk of zeros.
    fn with_measurement<R>(
        &mut self,
        text: &str,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
        f: impl FnOnce(&MeasuredText) -> R,
    ) -> R {
        let key = measure::measure_key(family, pixel_height, weight, text);
        let epoch = self.caches().advance_source;
        if let Some(memo) = self.caches().measure.as_mut() {
            if let Some(served) = memo.get_or_build(&epoch, key, || None) {
                if served.is_of(text) {
                    return f(&served);
                }
            }
        }
        let (measured, resolved) = self.measure_text(text, family, pixel_height, weight);
        let served = f(&measured);
        if resolved {
            if let Some(memo) = self.caches().measure.as_mut() {
                memo.retain(&epoch, key, measured);
            }
        }
        served
    }

    /// Walk `text`'s per-character advances, reporting whether every one of
    /// them resolved.
    fn measure_text(
        &mut self,
        text: &str,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> (MeasuredText, bool) {
        measure::measure(text, |scalar| {
            self.with_glyph(scalar, family, pixel_height, weight, |glyph| glyph.advance)
        })
    }

    /// `family`'s line metrics at `pixel_height` in `weight`, fetched over the
    /// channel on a cache miss.
    ///
    /// Never fails: no transport installed or a refused request yields the
    /// console-atlas geometry scaled to `pixel_height` ([`fallback_metrics`])
    /// rather than leaving a caller with nothing to lay text out with.
    fn metrics(&mut self, family: FamilyKey, pixel_height: u32, weight: FontWeight) -> FontMetrics {
        let key = (family.to_wire(), pixel_height, weight.to_wire());
        if let Some(metrics) = self.caches().metrics.get(key) {
            return metrics;
        }
        let fetched = self.fetch_metrics(family, pixel_height, weight);
        let Some(metrics) = fetched else {
            return fallback_metrics(pixel_height);
        };
        self.caches().metrics.insert(key, metrics);
        metrics
    }

    /// The installed selectable families, or an empty list on any refusal.
    fn families(&mut self) -> Vec<FamilyEntry> {
        self.fetch_families()
    }
}

/// Fetch one glyph's coverage over `transport` into the reusable `reply`
/// buffer.
///
/// Every failure — a refused call, a length the reply buffer cannot hold, a
/// frame that does not decode — yields `None`, so a caller composites nothing
/// rather than reading a bitmap the service did not send.
fn fetch_glyph(
    transport: &mut dyn FontTransport,
    reply: &mut Vec<u8>,
    scalar: char,
    family: FamilyKey,
    pixel_height: u32,
    weight: FontWeight,
) -> Option<CachedGlyph> {
    if reply.len() < FONT_MAX_GLYPH_REPLY {
        reply.resize(FONT_MAX_GLYPH_REPLY, 0);
    }
    let request = FontRequest::Glyph {
        family,
        scalar,
        pixel_height,
        weight,
    }
    .to_le_bytes();
    let len = transport.call(&request, reply).ok()?;
    let frame = reply.get(..len)?;
    let coverage = decode_glyph_reply(frame).ok()?;
    Some(CachedGlyph::new(
        coverage.width,
        coverage.height,
        coverage.advance,
        coverage.left,
        Box::from(coverage.coverage),
    ))
}

/// Fetch `family`'s line metrics at `pixel_height` in `weight` over
/// `transport`.
///
/// `None` on any failure — a refused call, an undersized reply, a frame that
/// does not decode — so the caller falls back rather than trusting a partial
/// answer.
fn fetch_metrics(
    transport: &mut dyn FontTransport,
    reply: &mut Vec<u8>,
    family: FamilyKey,
    pixel_height: u32,
    weight: FontWeight,
) -> Option<FontMetrics> {
    if reply.len() < FONT_METRICS_REPLY_LEN {
        reply.resize(FONT_METRICS_REPLY_LEN, 0);
    }
    let request = FontRequest::Metrics {
        family,
        pixel_height,
        weight,
    }
    .to_le_bytes();
    let len = transport.call(&request, reply).ok()?;
    let frame = reply.get(..len)?;
    decode_metrics_reply(frame).ok()
}

/// Scale a compiled-in atlas metric (measured at the atlas's own cell height,
/// [`atlas::CELL_HEIGHT`]) to `pixel_height`, rounding to the nearest whole
/// pixel and never below one.
fn scale_atlas_metric(value: u32, pixel_height: u32) -> u32 {
    let scaled = (u64::from(value) * u64::from(pixel_height) + u64::from(atlas::CELL_HEIGHT) / 2)
        / u64::from(atlas::CELL_HEIGHT);
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

/// The metrics a caller gets with no font service to ask: the compiled-in
/// console-atlas geometry scaled to `pixel_height`.
///
/// Scaled over the atlas cell rather than the face's own units, so that at the
/// atlas's own height it reports the cell exactly — the cell being what a
/// caller drawing compiled-in glyphs actually gets. Reporting the face's
/// unrounded ratio instead would be a shade more faithful at other sizes and
/// wrong at the only size that is not an approximation.
///
/// This is family-agnostic on purpose — it is what keeps `lib/fbcon` and the
/// boot console (which never install a transport) laying text out correctly
/// with no service running at all, and what keeps a desktop whose font
/// service has died drawing at a sane approximate size instead of collapsing
/// to zero. It always reports a monospace advance because the atlas it
/// approximates is monospace; a live service is what tells a proportional
/// family's caller its family is actually proportional.
fn fallback_metrics(pixel_height: u32) -> FontMetrics {
    FontMetrics {
        pixel_height,
        baseline: scale_atlas_metric(atlas::BASELINE, pixel_height),
        line_height: pixel_height,
        monospace_advance: scale_atlas_metric(atlas::CELL_WIDTH, pixel_height),
    }
}

/// Install whatever defaults this build has, once per process, leaving
/// anything a consumer installed itself alone.
///
/// A real program build talks to the font service over the runtime
/// transport, and caches glyphs in a cache budgeted from the machine's RAM
/// (which only a real program can read). A consumer's host tests enable
/// `test-util` instead, for deterministic glyph coverage with no service
/// running; the runtime transport takes precedence when both are present.
///
/// A build with **neither** — `lib/fbcon` and the boot console, which draw
/// from the compiled-in atlas, and the host builds of crates that merely
/// link the render path — has no default to install, so neither this nor its
/// caller exists there and every glyph request fails closed until a consumer
/// installs a transport itself.
///
/// Building the caches reads the machine's RAM over IPC, so it happens under
/// the *channel* lock — the one this module holds across a syscall — and the
/// built caches are installed after it is released, keeping the two locks
/// disjoint. A second thread that arrives while the first is still building
/// parks on the channel, then finds the work done.
#[cfg(any(feature = "rt", feature = "test-util"))]
fn install_defaults() {
    #[cfg(feature = "rt")]
    {
        let built = {
            let mut channel = CHANNEL.lock();
            if channel.defaulted {
                return;
            }
            channel.defaulted = true;
            if channel.transport.is_none() {
                channel.install_transport(Box::new(RtTransport));
            }
            (default_cache(), default_measure_cache())
        };
        let mut caches = CACHES.lock();
        if caches.glyphs.is_none() {
            caches.glyphs = Some(built.0);
        }
        if caches.measure.is_none() {
            caches.measure = Some(built.1);
        }
    }
    #[cfg(all(feature = "test-util", not(feature = "rt")))]
    {
        let mut channel = CHANNEL.lock();
        if channel.defaulted {
            return;
        }
        channel.defaulted = true;
        if channel.transport.is_none() {
            channel.install_transport(Box::new(SolidTestTransport));
        }
    }
}

/// Build the client's own glyph cache, budgeted from the machine's total
/// usable RAM.
///
/// A RAM read that fails — no System Information service, a refused or
/// malformed reply — is a zero total, hence a zero budget, hence a cache that
/// admits nothing and serves every glyph freshly fetched. That is the honest
/// outcome: slower, never wrong, and never a hand-picked ceiling standing in
/// for a figure the machine did not supply.
///
/// The process gauge is primed in the same breath, because a cache built
/// against an unreported gauge is born unable to retain anything: the gauge
/// answers "critical" until told otherwise, and at that band nothing is
/// admitted, so every glyph would cost an IPC round trip for the life of the
/// process. Priming is not a substitute for the program parking on the
/// pressure wake — that is what keeps the band *current* — but it means a
/// program that has not yet armed the wake starts from the machine's real
/// band rather than a fail-closed guess.
#[cfg(feature = "rt")]
fn default_cache() -> GlyphCache {
    use tairix_reclaim::ReclaimOwner;

    static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;

    let _ = tairix_procinfo::pressure::refresh();
    let total = tairix_procinfo::memory_total_bytes(&tairix_procinfo::IpcTransport).unwrap_or(0);
    let cache = ReclaimCache::new(
        CLIENT_CACHE_LABEL,
        crate::glyph_cache::glyph_cache_candidate(ReclaimOwner::UserlandProcess(
            CLIENT_CACHE_OWNER,
        )),
        crate::glyph_cache::glyph_cache_budget(total),
        tairix_rt::pressure::gauge(),
        &LOG_SINK,
    );
    // This cache is built here, on the process's behalf, so it is this
    // library — not the program linking it — that tells the process-wide
    // reporter about it; the program never sees the cache to register it
    // itself.
    if let Some(ledger) = cache.ledger() {
        tairix_rt::cachereport::register(ledger);
    }
    cache
}

/// Build the client's measurement memo, on the same RAM-derived ceiling the
/// glyph cache takes.
///
/// One derivation serves both: the ceiling is a fraction of the machine's
/// total usable RAM, and a measurement is far smaller than the glyph bitmaps
/// it was walked from, so the memo's real working set sits well inside it. A
/// RAM read that fails is a zero total, hence a zero budget, hence a memo that
/// retains nothing and walks every measurement — slower, never wrong.
#[cfg(feature = "rt")]
fn default_measure_cache() -> MeasureCache {
    use tairix_reclaim::ReclaimOwner;

    static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;

    let total = tairix_procinfo::memory_total_bytes(&tairix_procinfo::IpcTransport).unwrap_or(0);
    let cache = ReclaimCache::new(
        MEASURE_CACHE_LABEL,
        measure::measure_cache_candidate(ReclaimOwner::UserlandProcess(CLIENT_CACHE_OWNER)),
        crate::glyph_cache::glyph_cache_budget(total),
        tairix_rt::pressure::gauge(),
        &LOG_SINK,
    );
    if let Some(ledger) = cache.ledger() {
        tairix_rt::cachereport::register(ledger);
    }
    cache
}

/// The process's font caches.
///
/// Guarded by the runtime's futex mutex, not a spinlock: userland is
/// multi-threaded, so a holder can be preempted mid-section and a spinning
/// waiter would burn its whole quantum against a holder that is not running.
/// A waiter here parks and is woken by the release.
static CACHES: Mutex<Caches> = Mutex::new(Caches::new());

/// The process's one channel to the font service.
///
/// A separate lock from [`CACHES`], because this one *is* held across the
/// `fontd` round trip while that one must never be: see [`FontClient`] for the
/// discipline and why no thread ever holds both.
static CHANNEL: Mutex<Channel> = Mutex::new(Channel::new());

/// A client over a locked caches/channel pair: the caches borrowed for the span
/// of one run, and the channel taken only for a fetch.
///
/// The guard is an [`Option`] because a fetch **gives it up**: `fetch_*` drops
/// it, takes the channel, calls the service, releases the channel, and the next
/// [`caches`](FontClient::caches) takes it again. That release is what keeps the
/// caches lock off every syscall.
///
/// The lock pair is a parameter rather than the statics directly so a host test
/// can drive this very release logic over locks it owns, and observe from inside
/// its own transport that the caches were genuinely free while the service ran.
pub(crate) struct LockedClient<'a> {
    caches_lock: &'a Mutex<Caches>,
    channel_lock: &'a Mutex<Channel>,
    caches: Option<MutexGuard<'a, Caches>>,
}

impl<'a> LockedClient<'a> {
    /// A client over `caches` and `channel`, holding neither yet.
    pub(crate) const fn over(caches: &'a Mutex<Caches>, channel: &'a Mutex<Channel>) -> Self {
        Self {
            caches_lock: caches,
            channel_lock: channel,
            caches: None,
        }
    }

    /// Run `call` against the channel with the caches released.
    fn with_channel<R>(&mut self, call: impl FnOnce(&mut Channel) -> R) -> R {
        // Released *before* the channel is taken, so the two are never held at
        // once and there is no order to get wrong.
        self.caches = None;
        let mut channel = self.channel_lock.lock();
        call(&mut channel)
    }
}

impl FontClient for LockedClient<'_> {
    fn caches(&mut self) -> &mut Caches {
        let lock = self.caches_lock;
        self.caches.get_or_insert_with(|| lock.lock())
    }

    fn fetch_glyph(
        &mut self,
        scalar: char,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> Option<CachedGlyph> {
        self.with_channel(|channel| channel.glyph(scalar, family, pixel_height, weight))
    }

    fn fetch_metrics(
        &mut self,
        family: FamilyKey,
        pixel_height: u32,
        weight: FontWeight,
    ) -> Option<FontMetrics> {
        self.with_channel(|channel| channel.metrics(family, pixel_height, weight))
    }

    fn fetch_families(&mut self) -> Vec<FamilyEntry> {
        self.with_channel(Channel::families)
    }

    fn set_transport(&mut self, transport: Box<dyn FontTransport>) {
        self.with_channel(|channel| channel.install_transport(transport));
    }
}

/// Install (or replace) the font-service transport.
///
/// Production installs the `ipc_call`-backed transport; a host test installs
/// a mock. Under the `rt` feature the transport also defaults lazily on first
/// use, so an `rt` program needs no explicit install.
///
/// Retained measurements do not survive the change: the incoming transport is
/// a new advance source, and a width measured through the outgoing one is not
/// evidence about this one. The transport goes in *first* and the epoch moves
/// after it, so a thread measuring in between retains under the outgoing epoch
/// and its entry is discarded — never served against the wrong source.
pub fn set_font_transport(transport: Box<dyn FontTransport>) {
    with_client(|client| client.install_advance_source(transport));
}

/// Install (or replace) the process-global glyph cache fetched coverage is
/// memoised in.
///
/// The same seam as [`set_font_transport`], for the same reason: the cache
/// reads the machine's RAM size to size itself and so cannot be built in the
/// `const` initialiser of the caches `static`. Under the `rt` feature one is
/// installed lazily on first use, so a program needs no explicit call; a host
/// test installs a cache built from its own gauge and sink. Until then every
/// glyph is fetched and served without being retained.
///
/// Replacing a cache drops the outgoing one, wiping its entries as its
/// declared sensitivity requires.
pub fn set_glyph_cache(cache: GlyphCache) {
    CACHES.lock().glyphs = Some(cache);
}

/// Shrink the client's caches — glyph coverage and text measurements — to
/// what the band now in force allows, returning the bytes released.
///
/// The client counterpart of the service's own trim: a program that learns
/// the band moved gives the memory back at that moment rather than at its
/// next draw. Without it a program that has stopped drawing — a minimised
/// window, an idle tray client — would hold its rendered glyphs through a
/// pressure event it was told about, which is precisely what the shared
/// reclaim model exists to prevent. Both caches answer here, so a caller has
/// one thing to arm on the pressure wake rather than one per cache the client
/// happens to keep.
///
/// Trims only already-installed caches: it never builds one, so a program
/// that has not drawn yet pays no service round trip to report zero.
pub fn trim_glyph_cache() -> usize {
    CACHES.lock().trim()
}

/// Run `f` against the process-global client.
///
/// One acquisition of the caches serves a whole measurement or a whole drawn
/// run — the face's metrics, then either the memo or a walk of per-character
/// glyphs — where a lock taken per glyph would pay for the same thing again for
/// every character. A miss inside the run releases it for the service call and
/// takes it again after.
pub(crate) fn with_client<R>(f: impl FnOnce(&mut LockedClient<'static>) -> R) -> R {
    #[cfg(any(feature = "rt", feature = "test-util"))]
    install_defaults();
    f(&mut LockedClient::over(&CACHES, &CHANNEL))
}

/// `family`'s line metrics at `pixel_height` in `weight`.
///
/// Fetched from the font service and cached in this process; when no
/// transport is installed or the service refuses, falls back to the
/// compiled-in console-atlas geometry scaled to `pixel_height` (see
/// [`fallback_metrics`]) rather than leaving a caller with nothing to lay
/// text out with.
pub(crate) fn metrics(family: FamilyKey, pixel_height: u32, weight: FontWeight) -> FontMetrics {
    with_client(|client| client.metrics(family, pixel_height, weight))
}

/// The installed selectable families, or an empty list when no transport is
/// installed or the service refuses the request (fail closed).
///
/// A settings surface calls this to offer exactly the families the store
/// holds. This returns an owned `Vec` rather than the wire
/// [`tairix_abi::font_ipc::FamilyList`] because that type's fields are
/// private to its own decoder — there is no public way to construct an empty
/// one to fail closed to — and this crate already owns `alloc` under the
/// `render` feature this function rides.
#[must_use]
pub fn families() -> Vec<FamilyEntry> {
    with_client(FontClient::families)
}

/// The production transport: the `ipc_call` syscall to [`FONT_ENDPOINT`].
///
/// [`FONT_ENDPOINT`]: tairix_abi::font_ipc::FONT_ENDPOINT
#[cfg(feature = "rt")]
struct RtTransport;

#[cfg(feature = "rt")]
impl FontTransport for RtTransport {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        tairix_rt::ipc_call(tairix_abi::font_ipc::FONT_ENDPOINT, request, reply)
            .map_err(Errno::from_syscall)
    }
}

/// A deterministic test transport for host tests: it answers every request
/// operation ([`FontRequest::Glyph`], [`FontRequest::Metrics`],
/// [`FontRequest::Families`]) without a running `fontd`.
///
/// [`FamilyKey::MONO`] is served as a monospace family (a uniform advance
/// scaled from the console-atlas geometry, matching what the atlas itself
/// draws); any other family is served as proportional, with a per-scalar
/// advance that actually varies (rather than a second uniform cell), so a
/// consumer's tests exercise real proportional measurement, truncation, and
/// hit-testing rather than merely a relabelled grid. Coverage is solid for
/// every scalar but space (which is ink-less, as the real faces render it) —
/// rendering fidelity is `fontd`'s job, tested there. Weight only has to be
/// accepted: synthetic emboldening changes coverage, never geometry or the
/// advance, and a solid cell is already saturated. Consumers enable it
/// through the `test-util` feature (installed lazily on first draw, or
/// explicitly via [`install_test_transport`]); rendering fidelity is
/// `fontd`'s job, tested there.
#[cfg(any(test, feature = "test-util"))]
pub struct SolidTestTransport;

#[cfg(any(test, feature = "test-util"))]
impl FontTransport for SolidTestTransport {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        match FontRequest::from_bytes(request)? {
            FontRequest::Glyph {
                family,
                scalar,
                pixel_height,
                weight: _,
            } => test_glyph_reply(reply, family, scalar, pixel_height),
            FontRequest::Metrics {
                family,
                pixel_height,
                weight: _,
            } => test_metrics_reply(reply, family, pixel_height),
            FontRequest::Families => test_families_reply(reply),
        }
    }
}

/// Whether `family` is served as fixed-pitch by [`SolidTestTransport`].
#[cfg(any(test, feature = "test-util"))]
fn test_family_is_monospace(family: FamilyKey) -> bool {
    family == FamilyKey::MONO
}

/// The advance [`SolidTestTransport`] reports for `scalar` at `pixel_height`
/// in `family`.
///
/// A monospace family shares one cell width times [`tairix_vt::char_width`];
/// a proportional family varies by scalar (a small deterministic spread
/// around the same cell width) so measurement and truncation tests exercise
/// genuinely different per-character advances rather than a relabelled grid.
#[cfg(any(test, feature = "test-util"))]
fn test_advance(family: FamilyKey, scalar: char, pixel_height: u32) -> u32 {
    let cell = fallback_metrics(pixel_height).monospace_advance;
    if test_family_is_monospace(family) {
        return cell.saturating_mul(u32::from(tairix_vt::char_width(scalar)));
    }
    if scalar == ' ' {
        return cell.max(1) / 2;
    }
    // A deterministic spread of roughly 0.6x to 1.4x the cell width, so
    // different scalars measure to genuinely different widths.
    let spread = u32::from(scalar) % 5;
    (cell.saturating_mul(6 + 2 * spread) / 10).max(1)
}

/// Encode a [`FontRequest::Glyph`] reply for [`SolidTestTransport`].
#[cfg(any(test, feature = "test-util"))]
fn test_glyph_reply(
    reply: &mut [u8],
    family: FamilyKey,
    scalar: char,
    pixel_height: u32,
) -> Result<usize, Errno> {
    use alloc::vec;
    use tairix_abi::font_ipc::{encode_glyph_reply, GlyphCoverage};

    let advance = test_advance(family, scalar, pixel_height);
    // Space is blank, like the real face: no ink, no coverage bytes.
    if scalar == ' ' {
        return encode_glyph_reply(
            reply,
            &GlyphCoverage {
                width: 0,
                height: pixel_height,
                advance,
                left: 0,
                coverage: &[],
            },
        );
    }
    let width = advance.max(1);
    let level = 255u8;
    let coverage = vec![level; (width * pixel_height) as usize];
    encode_glyph_reply(
        reply,
        &GlyphCoverage {
            width,
            height: pixel_height,
            advance,
            left: 0,
            coverage: &coverage,
        },
    )
}

/// Encode a [`FontRequest::Metrics`] reply for [`SolidTestTransport`].
#[cfg(any(test, feature = "test-util"))]
fn test_metrics_reply(
    reply: &mut [u8],
    family: FamilyKey,
    pixel_height: u32,
) -> Result<usize, Errno> {
    use tairix_abi::font_ipc::encode_metrics_reply;

    let mut metrics = fallback_metrics(pixel_height);
    if !test_family_is_monospace(family) {
        metrics.monospace_advance = 0;
    }
    let bytes = encode_metrics_reply(Ok(metrics));
    let len = bytes.len();
    reply
        .get_mut(..len)
        .ok_or(Errno::BufferTooSmall)?
        .copy_from_slice(&bytes);
    Ok(len)
}

/// Encode a [`FontRequest::Families`] reply for [`SolidTestTransport`]: the
/// built-in monospace family plus one synthetic proportional family, so a
/// consumer's family-picker tests have more than one entry to choose from.
#[cfg(any(test, feature = "test-util"))]
fn test_families_reply(reply: &mut [u8]) -> Result<usize, Errno> {
    use tairix_abi::font_ipc::{encode_families_reply, FamilyKind};

    let proportional = FamilyKey::new("test-sans").unwrap_or(FamilyKey::MONO);
    let entries = [
        FamilyEntry::new(FamilyKey::MONO, "Mono", FamilyKind::Monospace)?,
        FamilyEntry::new(proportional, "Test Sans", FamilyKind::Proportional)?,
    ];
    encode_families_reply(reply, Ok(&entries))
}

/// Install the [`SolidTestTransport`] as the process-global font transport for
/// a consumer's host tests (also installed lazily on first draw when the
/// `test-util` feature is enabled).
#[cfg(any(test, feature = "test-util"))]
pub fn install_test_transport() {
    set_font_transport(Box::new(SolidTestTransport));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tairix_log::DiscardSink;
    use tairix_reclaim::{CacheBudget, PressureBand, ReclaimOwner, ReportedPressure};

    use crate::glyph_cache::{glyph_cache_budget, glyph_cache_candidate};

    /// A client over caches and a channel this test owns outright.
    ///
    /// The production client reaches process-global state under two locks,
    /// which a test wanting isolated caches cannot use; the *serve* logic is
    /// [`FontClient`]'s own, so the two can never disagree on what a hit
    /// costs, what a miss retains, or what a refusal falls back to.
    pub(crate) struct LocalClient {
        pub(crate) caches: Caches,
        pub(crate) channel: Channel,
    }

    impl LocalClient {
        /// An empty client: no transport, no caches.
        fn new() -> Self {
            Self {
                caches: Caches::new(),
                channel: Channel::new(),
            }
        }
    }

    impl FontClient for LocalClient {
        fn caches(&mut self) -> &mut Caches {
            &mut self.caches
        }

        fn fetch_glyph(
            &mut self,
            scalar: char,
            family: FamilyKey,
            pixel_height: u32,
            weight: FontWeight,
        ) -> Option<CachedGlyph> {
            self.channel.glyph(scalar, family, pixel_height, weight)
        }

        fn fetch_metrics(
            &mut self,
            family: FamilyKey,
            pixel_height: u32,
            weight: FontWeight,
        ) -> Option<FontMetrics> {
            self.channel.metrics(family, pixel_height, weight)
        }

        fn fetch_families(&mut self) -> Vec<FamilyEntry> {
            self.channel.families()
        }

        fn set_transport(&mut self, transport: Box<dyn FontTransport>) {
            self.channel.install_transport(transport);
        }
    }

    pub(crate) const INTER: FamilyKey = match FamilyKey::new("inter") {
        Ok(key) => key,
        Err(_) => FamilyKey::MONO,
    };

    /// Glyph lookups this client has paid.
    ///
    /// The glyph cache records exactly one hit or miss per glyph asked of it,
    /// so its event counters *are* the work counter for a measurement or for a
    /// drawn run — whether the coverage came from the service or the cache.
    pub(crate) fn glyph_lookups(client: &LocalClient) -> u64 {
        let accounting = client
            .caches
            .glyphs
            .as_ref()
            .expect("a cache is installed")
            .accounting();
        accounting.hits() + accounting.misses()
    }

    /// A transport that records whether the caches lock was free while it ran.
    ///
    /// This is the whole regression: the caches lock must not span a
    /// font-service call, because a thread waiting for it would then be waiting
    /// on a holder that is asleep in the kernel. The probe reports what the
    /// client actually did rather than what its comments claim.
    struct LockProbe {
        caches: &'static Mutex<Caches>,
        caches_were_free: Arc<AtomicUsize>,
    }

    impl FontTransport for LockProbe {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            if self.caches.try_lock().is_some() {
                self.caches_were_free.fetch_add(1, Ordering::Relaxed);
            }
            SolidTestTransport.call(request, reply)
        }
    }

    /// A transport that always refuses, to exercise the fail-closed path.
    struct Refusing;
    impl FontTransport for Refusing {
        fn call(&mut self, _request: &[u8], _reply: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::NotFound)
        }
    }

    /// A transport that claims to have written more than the buffer holds, so
    /// the client's own reply-length handling is exercised.
    struct Overlong;
    impl FontTransport for Overlong {
        fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            Ok(reply.len() + 1)
        }
    }

    pub(super) fn client_with(transport: impl FontTransport + 'static) -> LocalClient {
        let mut client = LocalClient::new();
        client.channel.transport = Some(Box::new(transport));
        client
    }

    /// A shared tally of the service calls a [`CountingTransport`] served.
    #[derive(Clone, Default)]
    struct CallTally(Arc<AtomicUsize>);

    impl CallTally {
        fn get(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    /// A [`SolidTestTransport`] that tallies the calls it served, so a test
    /// can assert what a redraw *costs* rather than only what it looks like.
    /// The tally is shared because the transport is moved into the client and
    /// cannot be read back out.
    struct CountingTransport(CallTally);

    impl FontTransport for CountingTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            self.0 .0.fetch_add(1, Ordering::Relaxed);
            SolidTestTransport.call(request, reply)
        }
    }

    /// A client over a counting transport, with the tally to read it by.
    fn counting_client() -> (LocalClient, CallTally) {
        let tally = CallTally::default();
        (client_with(CountingTransport(tally.clone())), tally)
    }

    /// A client over the deterministic test transport with its glyph cache
    /// installed, as any drawing or measuring program runs.
    pub(crate) fn caching_client(
        band: PressureBand,
        budget: CacheBudget,
    ) -> (LocalClient, &'static ReportedPressure) {
        let (cache, gauge) = cache_at(band, budget);
        let mut client = client_with(SolidTestTransport);
        client.caches.glyphs = Some(cache);
        (client, gauge)
    }

    /// A cache built exactly as production builds one — the shared
    /// classification, the shared budget derivation — but from a gauge the
    /// test drives and a sink a host test has nowhere to send records to.
    pub(super) fn cache_at(
        band: PressureBand,
        budget: CacheBudget,
    ) -> (GlyphCache, &'static ReportedPressure) {
        static SINK: DiscardSink = DiscardSink;
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(band);
        let cache = ReclaimCache::new(
            "test.font.glyphs",
            glyph_cache_candidate(ReclaimOwner::UserlandProcess("test.font")),
            budget,
            gauge,
            &SINK,
        );
        (cache, gauge)
    }

    /// A comfortable machine's client: solid glyphs, room to cache them.
    fn cached_client() -> (LocalClient, &'static ReportedPressure) {
        let (cache, gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let mut client = client_with(SolidTestTransport);
        client.caches.glyphs = Some(cache);
        (client, gauge)
    }

    /// The coverage the client hands the blitter, copied out so it can be
    /// compared across cache states.
    fn coverage(
        client: &mut LocalClient,
        scalar: char,
        family: FamilyKey,
        height: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        client.with_glyph(scalar, family, height, FontWeight::Regular, |glyph| {
            (glyph.width, glyph.height, glyph.data.to_vec())
        })
    }

    #[test]
    fn a_glyph_is_fetched_then_served_from_cache() {
        let (mut client, _gauge) = cached_client();
        let (width, height, data) =
            coverage(&mut client, 'A', FamilyKey::MONO, 28).expect("fetched");
        assert_eq!(height, 28);
        assert_eq!(width, test_advance(FamilyKey::MONO, 'A', 28));
        assert_eq!(data.len(), (width * height) as usize);
        assert!(data.iter().all(|&c| c == 255));

        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        let cache = client.caches.glyphs.as_ref().expect("installed");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.accounting().hits(), 1);
        assert_eq!(cache.accounting().misses(), 1);
    }

    #[test]
    fn a_different_family_is_a_distinct_cache_entry() {
        let (mut client, _gauge) = cached_client();
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        assert!(coverage(&mut client, 'A', INTER, 28).is_some());
        assert_eq!(client.caches.glyphs.as_ref().expect("installed").len(), 2);
    }

    #[test]
    fn a_proportional_family_reports_varying_advances() {
        let (mut client, _gauge) = cached_client();
        let widths: Vec<u32> = ['i', 'M', 'x', 'W']
            .into_iter()
            .map(|ch| {
                client
                    .with_glyph(ch, INTER, 28, FontWeight::Regular, |g| g.advance)
                    .expect("fetched")
            })
            .collect();
        assert!(
            widths.iter().any(|&w| w != widths[0]),
            "a proportional family must not report one uniform advance: {widths:?}"
        );
    }

    #[test]
    fn a_heavier_weight_is_a_distinct_cache_entry() {
        let (mut client, _gauge) = cached_client();
        for weight in [FontWeight::Regular, FontWeight::Medium, FontWeight::Bold] {
            assert!(client
                .with_glyph('A', FamilyKey::MONO, 28, weight, |_| ())
                .is_some());
        }
        // The same scalar at the same height in three weights is three
        // bitmaps, so a bold run can never be served a regular raster.
        assert_eq!(client.caches.glyphs.as_ref().expect("installed").len(), 3);
    }

    // Only meaningful without a transport feature: with `test-util` (or `rt`)
    // the client installs a default transport lazily, so there is never a
    // "no transport" state to observe.
    #[cfg(not(any(feature = "rt", feature = "test-util")))]
    #[test]
    fn no_transport_composites_nothing() {
        let mut client = LocalClient::new();
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 20).is_none());
    }

    #[test]
    fn a_refused_call_fails_closed() {
        let (cache, _gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let mut client = client_with(Refusing);
        client.caches.glyphs = Some(cache);
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 20).is_none());
        // Nothing is cached, so a later working transport is still consulted.
        assert_eq!(client.caches.glyphs.as_ref().expect("installed").len(), 0);
        // Metrics fail closed to the atlas-scaled fallback rather than
        // leaving the caller with nothing to lay text out with.
        let metrics = client.metrics(FamilyKey::MONO, 20, FontWeight::Regular);
        assert_eq!(metrics, fallback_metrics(20));
        // Families fail closed to an empty list.
        assert!(client.families().is_empty());
    }

    #[test]
    fn a_reply_longer_than_the_buffer_fails_closed() {
        let mut client = client_with(Overlong);
        assert!(
            coverage(&mut client, 'A', FamilyKey::MONO, 20).is_none(),
            "a length the buffer cannot hold is refused, never read past the end"
        );
    }

    #[test]
    fn the_byte_budget_bounds_the_cache_however_many_glyphs_are_drawn() {
        let budget = CacheBudget::from_ceiling(64 * 1024);
        let (cache, _gauge) = cache_at(PressureBand::Normal, budget);
        let mut client = client_with(SolidTestTransport);
        client.caches.glyphs = Some(cache);
        // Far more distinct glyphs than the ceiling can hold, each a full
        // bitmap: the cache must evict, never grow past the ceiling.
        for scalar in 0..1024u32 {
            let ch = char::from_u32(scalar).unwrap_or('A');
            assert!(
                coverage(&mut client, ch, FamilyKey::MONO, 28).is_some(),
                "still rendered"
            );
            let cache = client.caches.glyphs.as_ref().expect("installed");
            assert!(
                cache.charged_bytes() <= budget.hard(),
                "charged {} exceeds the ceiling {}",
                cache.charged_bytes(),
                budget.hard()
            );
        }
        let cache = client.caches.glyphs.as_ref().expect("installed");
        assert!(cache.accounting().evictions() > 0, "the bound must bite");
        assert!(!cache.poisoned(), "bounding is ordinary, not a defect");
    }

    #[test]
    fn an_unknown_ram_size_caches_nothing_yet_still_renders() {
        let (cache, _gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(0));
        let mut client = client_with(SolidTestTransport);
        client.caches.glyphs = Some(cache);
        let (width, height, data) =
            coverage(&mut client, 'A', FamilyKey::MONO, 28).expect("still rendered");
        assert_eq!(data.len(), (width * height) as usize);
        assert!(data.iter().all(|&c| c == 255));
        let cache = client.caches.glyphs.as_ref().expect("installed");
        assert_eq!(cache.len(), 0, "a zero budget retains nothing");
        assert_eq!(cache.charged_bytes(), 0);
    }

    #[test]
    fn mild_pressure_empties_the_cache_and_refuses_further_growth() {
        let (mut client, gauge) = cached_client();
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        assert_eq!(client.caches.glyphs.as_ref().expect("installed").len(), 1);

        gauge.report(PressureBand::Mild);
        let cache = client.caches.glyphs.as_mut().expect("installed");
        assert!(cache.enforce_pressure() > 0, "mild pressure must release");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.charged_bytes(), 0);

        assert!(
            coverage(&mut client, 'B', FamilyKey::MONO, 28).is_some(),
            "a shrunk cache still renders"
        );
        assert_eq!(
            client.caches.glyphs.as_ref().expect("installed").len(),
            0,
            "no growth while the band forbids it"
        );
    }

    /// The bug this counting exists to catch: a client whose gauge was never
    /// told a band admits nothing, so every character drawn is a fresh
    /// service call. On a desktop redrawing text that is one IPC round trip
    /// per glyph per repaint, and the font service carries all of it.
    #[test]
    fn an_unreported_band_makes_every_draw_a_service_call() {
        static SINK: DiscardSink = DiscardSink;
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        let (mut client, tally) = counting_client();
        client.caches.glyphs = Some(ReclaimCache::new(
            "test.font.glyphs",
            glyph_cache_candidate(ReclaimOwner::UserlandProcess("test.font")),
            glyph_cache_budget(1 << 30),
            gauge,
            &SINK,
        ));

        for _ in 0..8 {
            assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        }
        assert_eq!(
            tally.get(),
            8,
            "an unreported gauge retains nothing, so every draw re-fetches"
        );
        assert_eq!(client.caches.glyphs.as_ref().expect("installed").len(), 0);

        // Learning the band is the whole fix: the very next draw is retained
        // and every repeat after it is free.
        gauge.report(PressureBand::Normal);
        for _ in 0..8 {
            assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        }
        assert_eq!(tally.get(), 9, "one fetch to populate the cache, then none");
    }

    #[test]
    fn redrawing_a_run_of_text_issues_no_further_calls() {
        let (cache, _gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let (mut client, tally) = counting_client();
        client.caches.glyphs = Some(cache);

        let run = "Switchboard";
        let distinct = {
            let mut seen: Vec<char> = run.chars().collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len()
        };
        for _ in 0..5 {
            for ch in run.chars() {
                assert!(coverage(&mut client, ch, FamilyKey::MONO, 28).is_some());
            }
        }
        assert_eq!(
            tally.get(),
            distinct,
            "a steady-state repaint must cost nothing beyond the first sight of each scalar"
        );
    }

    #[test]
    fn trimming_releases_the_cache_and_needs_no_cache_to_be_installed() {
        let (mut client, gauge) = cached_client();
        assert_eq!(
            client.caches.trim(),
            0,
            "nothing retained, nothing to release"
        );
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        assert!(
            client
                .caches
                .glyphs
                .as_ref()
                .expect("installed")
                .charged_bytes()
                > 0
        );

        gauge.report(PressureBand::Mild);
        assert!(
            client.caches.trim() > 0,
            "the band's ceiling is applied at once"
        );
        assert_eq!(client.caches.glyphs.as_ref().expect("installed").len(), 0);

        let mut bare = client_with(SolidTestTransport);
        assert_eq!(bare.caches.trim(), 0, "no cache installed is not an error");
        assert!(bare.caches.glyphs.is_none(), "trimming never builds one");
    }

    #[test]
    fn the_same_glyph_renders_identically_cached_uncached_and_after_a_shrink() {
        let mut uncached = client_with(SolidTestTransport);
        let expected = coverage(&mut uncached, 'A', FamilyKey::MONO, 28)
            .expect("rendered with no cache at all");
        assert!(uncached.caches.glyphs.is_none());

        let (mut client, gauge) = cached_client();
        assert_eq!(
            coverage(&mut client, 'A', FamilyKey::MONO, 28).as_ref(),
            Some(&expected)
        );
        assert_eq!(
            coverage(&mut client, 'A', FamilyKey::MONO, 28).as_ref(),
            Some(&expected),
            "a cache hit serves the same bitmap the fetch did"
        );

        gauge.report(PressureBand::Mild);
        let _ = client
            .caches
            .glyphs
            .as_mut()
            .expect("installed")
            .enforce_pressure();
        assert_eq!(
            coverage(&mut client, 'A', FamilyKey::MONO, 28).as_ref(),
            Some(&expected),
            "the cache is an accelerator; losing it changes nothing"
        );
    }

    #[test]
    fn metrics_are_fetched_then_served_from_the_metrics_cache() {
        let mut client = client_with(SolidTestTransport);
        let first = client.metrics(FamilyKey::MONO, 28, FontWeight::Regular);
        assert_eq!(
            first.monospace_advance,
            fallback_metrics(28).monospace_advance
        );
        // A second call for the same key must not need the transport at all;
        // swap in a refusing one and confirm the cached value still answers.
        client.channel.transport = Some(Box::new(Refusing));
        let second = client.metrics(FamilyKey::MONO, 28, FontWeight::Regular);
        assert_eq!(first, second);
    }

    #[test]
    fn a_proportional_familys_metrics_report_no_monospace_advance() {
        let mut client = client_with(SolidTestTransport);
        let metrics = client.metrics(INTER, 28, FontWeight::Regular);
        assert_eq!(metrics.monospace_advance, 0);
    }

    #[test]
    fn the_metrics_cache_is_bounded_and_evicts_the_oldest_entry() {
        let mut client = client_with(SolidTestTransport);
        let capacity = u32::try_from(METRICS_CACHE_CAPACITY).expect("a small fixed capacity");
        for height in 8..8 + capacity + 8 {
            let _ = client.metrics(FamilyKey::MONO, height, FontWeight::Regular);
        }
        let occupied = client
            .caches
            .metrics
            .entries
            .iter()
            .filter(|slot| slot.is_some())
            .count();
        assert_eq!(
            occupied, METRICS_CACHE_CAPACITY,
            "the ring buffer never grows past its capacity"
        );
    }

    #[test]
    fn families_lists_at_least_the_built_in_monospace_family() {
        let mut client = client_with(SolidTestTransport);
        let families = client.families();
        assert!(families.iter().any(|entry| entry.key == FamilyKey::MONO));
    }

    /// A font-service call must run with the caches lock released.
    ///
    /// Driven over locks this test owns, so the observation is about the
    /// client's own discipline and cannot be disturbed by whatever else the
    /// harness runs in parallel. The client under test is the production one:
    /// its lock pair is a parameter, not a hard-coded static.
    #[test]
    fn a_service_call_runs_with_the_caches_lock_released() {
        let caches: &'static Mutex<Caches> = Box::leak(Box::new(Mutex::new(Caches::new())));
        let channel: &'static Mutex<Channel> = Box::leak(Box::new(Mutex::new(Channel::new())));
        let free = Arc::new(AtomicUsize::new(0));
        channel.lock().install_transport(Box::new(LockProbe {
            caches,
            caches_were_free: Arc::clone(&free),
        }));

        let mut client = LockedClient::over(caches, channel);
        // A metrics miss and a glyph miss are the two fetch shapes; both have to
        // reach the service with nothing held.
        let metrics = client.metrics(FamilyKey::MONO, 20, FontWeight::Regular);
        let ink = client
            .with_glyph('A', FamilyKey::MONO, 20, FontWeight::Regular, |glyph| {
                glyph.data.iter().any(|&level| level > 0)
            })
            .expect("the probe serves the test transport's coverage");

        assert!(
            metrics.monospace_advance > 0,
            "the metrics fetch was served"
        );
        assert!(ink, "the glyph fetch was served");
        assert_eq!(
            free.load(Ordering::Relaxed),
            2,
            "a font-service call ran with the caches lock still held"
        );
    }

    /// Neither lock may outlive a run, so the next run — on this thread or
    /// another — is never blocked by a guard nobody dropped.
    #[test]
    fn a_finished_run_holds_neither_lock() {
        let caches: &'static Mutex<Caches> = Box::leak(Box::new(Mutex::new(Caches::new())));
        let channel: &'static Mutex<Channel> = Box::leak(Box::new(Mutex::new(Channel::new())));
        channel
            .lock()
            .install_transport(Box::new(SolidTestTransport));

        let mut client = LockedClient::over(caches, channel);
        let _ = client.metrics(FamilyKey::MONO, 20, FontWeight::Regular);
        drop(client);

        assert!(caches.try_lock().is_some(), "the caches stayed locked");
        assert!(channel.try_lock().is_some(), "the channel stayed locked");
    }
}

/// The measurement memo's tests, which borrow this module's test clients and
/// gauges rather than standing up a second set of them.
#[cfg(test)]
#[path = "measure_tests.rs"]
mod measure_tests;
