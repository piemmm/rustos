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

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::font_ipc::{
    decode_families_reply, decode_glyph_reply, decode_metrics_reply, FamilyEntry, FamilyKey,
    FontMetrics, FontRequest, FontWeight, FONT_FAMILY_KEY_LEN, FONT_MAX_FAMILIES_REPLY,
    FONT_MAX_GLYPH_REPLY, FONT_METRICS_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_reclaim::ReclaimCache;
use tairix_sync::SpinLock;

use crate::atlas;
use crate::glyph_cache::CachedGlyph;

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

/// The render path's process-global font client: the installed transport, a
/// reusable receive buffer, the optional local glyph cache, and the bounded
/// metrics cache.
struct GlyphClient {
    transport: Option<Box<dyn FontTransport>>,
    reply: Vec<u8>,
    /// `None` until a cache is installed, in which case every glyph is
    /// fetched and served without being retained — correct, merely one IPC
    /// per glyph.
    cache: Option<GlyphCache>,
    metrics: MetricsCache,
}

impl GlyphClient {
    const fn new() -> Self {
        Self {
            transport: None,
            reply: Vec::new(),
            cache: None,
            metrics: MetricsCache::new(),
        }
    }

    /// Install this process's default transport and glyph cache on first
    /// use, exactly once.
    ///
    /// Neither can be built in the `const` initialiser of the client
    /// `static` — one issues syscalls, the other reads the machine's RAM
    /// size — so a real program's defaults are installed here instead,
    /// keeping the client free of explicit setup.
    ///
    /// Exists only where there is a default to install: without either
    /// transport feature the client has nothing it could reach for, and a
    /// draw with no injected transport composites nothing and fails closed.
    #[cfg(any(feature = "rt", feature = "test-util"))]
    fn ensure_defaults(&mut self) {
        install_defaults(self);
    }

    /// Serve `(scalar, family, pixel_height, weight)` to `f`, fetching it
    /// over the transport on a miss and retaining it when a cache is
    /// installed and admits it.
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
        #[cfg(any(feature = "rt", feature = "test-util"))]
        self.ensure_defaults();
        let Self {
            transport,
            reply,
            cache,
            ..
        } = self;
        let transport = transport.as_mut()?;
        let Some(cache) = cache.as_mut() else {
            let glyph = fetch_glyph(
                transport.as_mut(),
                reply,
                scalar,
                family,
                pixel_height,
                weight,
            )?;
            return Some(f(&glyph));
        };
        let key = (
            scalar as u32,
            family.to_wire(),
            pixel_height,
            weight.to_wire(),
        );
        let served = cache.get_or_build(&(), key, || {
            fetch_glyph(
                transport.as_mut(),
                reply,
                scalar,
                family,
                pixel_height,
                weight,
            )
        })?;
        Some(f(&served))
    }

    /// `family`'s line metrics at `pixel_height` in `weight`, fetched over
    /// the transport on a cache miss.
    ///
    /// Never fails: no transport installed or a refused request yields the
    /// console-atlas geometry scaled to `pixel_height` ([`fallback_metrics`])
    /// rather than leaving a caller with nothing to lay text out with.
    fn metrics(&mut self, family: FamilyKey, pixel_height: u32, weight: FontWeight) -> FontMetrics {
        #[cfg(any(feature = "rt", feature = "test-util"))]
        self.ensure_defaults();
        let key = (family.to_wire(), pixel_height, weight.to_wire());
        if let Some(metrics) = self.metrics.get(key) {
            return metrics;
        }
        let fetched = self.transport.as_mut().and_then(|transport| {
            fetch_metrics(
                transport.as_mut(),
                &mut self.reply,
                family,
                pixel_height,
                weight,
            )
        });
        let metrics = fetched.unwrap_or_else(|| fallback_metrics(pixel_height));
        if fetched.is_some() {
            self.metrics.insert(key, metrics);
        }
        metrics
    }

    /// Shrink the glyph cache to the band's ceiling, returning the bytes
    /// released. No cache installed is nothing to release, and no cache is
    /// built to answer.
    fn trim(&mut self) -> usize {
        self.cache
            .as_mut()
            .map_or(0, ReclaimCache::enforce_pressure)
    }

    /// The installed families, or an empty list when no transport is
    /// installed or the service refuses the request (fail closed).
    fn families(&mut self) -> Vec<FamilyEntry> {
        #[cfg(any(feature = "rt", feature = "test-util"))]
        self.ensure_defaults();
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

/// Scale an atlas geometry constant (measured at the atlas's native cell
/// height, [`atlas::CELL_HEIGHT`]) to `pixel_height`, rounding to the
/// nearest whole pixel and never below one.
fn scale_atlas_metric(value: u32, pixel_height: u32) -> u32 {
    let scaled = (value * pixel_height + atlas::CELL_HEIGHT / 2) / atlas::CELL_HEIGHT;
    scaled.max(1)
}

/// The metrics a caller gets with no font service to ask: the compiled-in
/// console-atlas geometry scaled to `pixel_height`, exactly as the
/// monospace-only client scaled it before a font service existed.
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

/// Install whatever defaults this build has for `client`, leaving anything
/// already installed alone.
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
#[cfg(any(feature = "rt", feature = "test-util"))]
fn install_defaults(client: &mut GlyphClient) {
    #[cfg(feature = "rt")]
    {
        if client.transport.is_none() {
            client.transport = Some(Box::new(RtTransport));
        }
        if client.cache.is_none() {
            client.cache = Some(default_cache());
        }
    }
    #[cfg(all(feature = "test-util", not(feature = "rt")))]
    if client.transport.is_none() {
        client.transport = Some(Box::new(SolidTestTransport));
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

/// The one process-global font client.
static CLIENT: SpinLock<GlyphClient> = SpinLock::new(GlyphClient::new());

/// Install (or replace) the font-service transport.
///
/// Production installs the `ipc_call`-backed transport; a host test installs
/// a mock. Under the `rt` feature the transport also defaults lazily on first
/// use, so an `rt` program needs no explicit install.
pub fn set_font_transport(transport: Box<dyn FontTransport>) {
    CLIENT.lock().transport = Some(transport);
}

/// Install (or replace) the process-global glyph cache fetched coverage is
/// memoised in.
///
/// The same seam as [`set_font_transport`], for the same reason: the cache
/// reads the machine's RAM size to size itself and so cannot be built in the
/// `const` initialiser of the client `static`. Under the `rt` feature one is
/// installed lazily on first use, so a program needs no explicit call; a host
/// test installs a cache built from its own gauge and sink. Until then every
/// glyph is fetched and served without being retained.
///
/// Replacing a cache drops the outgoing one, wiping its entries as its
/// declared sensitivity requires.
pub fn set_glyph_cache(cache: GlyphCache) {
    CLIENT.lock().cache = Some(cache);
}

/// Shrink the glyph cache to what the band now in force allows, returning the
/// bytes released.
///
/// The client counterpart of the service's own trim: a program that learns
/// the band moved gives the memory back at that moment rather than at its
/// next draw. Without it a program that has stopped drawing — a minimised
/// window, an idle tray client — would hold its rendered glyphs through a
/// pressure event it was told about, which is precisely what the shared
/// reclaim model exists to prevent.
///
/// Trims only an already-installed cache: it never builds one, so a program
/// that has not drawn yet pays no service round trip to report zero.
pub fn trim_glyph_cache() -> usize {
    CLIENT.lock().trim()
}

/// Fetch the coverage glyph for `(scalar, family, pixel_height, weight)` and
/// hand it to `f`, or return `None` (compositing nothing) when the service is
/// unreachable.
///
/// The global lock is held across `f` so glyph fetch and blit see a
/// consistent cache; `f` does only the bounded per-glyph blit.
pub(crate) fn with_glyph<R>(
    scalar: char,
    family: FamilyKey,
    pixel_height: u32,
    weight: FontWeight,
    f: impl FnOnce(&CachedGlyph) -> R,
) -> Option<R> {
    CLIENT
        .lock()
        .with_glyph(scalar, family, pixel_height, weight, f)
}

/// `family`'s line metrics at `pixel_height` in `weight`.
///
/// Fetched from the font service and cached in this process; when no
/// transport is installed or the service refuses, falls back to the
/// compiled-in console-atlas geometry scaled to `pixel_height` (see
/// [`fallback_metrics`]) rather than leaving a caller with nothing to lay
/// text out with.
pub(crate) fn metrics(family: FamilyKey, pixel_height: u32, weight: FontWeight) -> FontMetrics {
    CLIENT.lock().metrics(family, pixel_height, weight)
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
    CLIENT.lock().families()
}

/// The production transport: the `ipc_call` syscall to [`FONT_ENDPOINT`].
///
/// [`FONT_ENDPOINT`]: tairix_abi::font_ipc::FONT_ENDPOINT
#[cfg(feature = "rt")]
struct RtTransport;

#[cfg(feature = "rt")]
impl FontTransport for RtTransport {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        tairix_rt::ipc_call(tairix_abi::font_ipc::FONT_ENDPOINT, request, reply).map_err(errno_from)
    }
}

/// Recover the [`Errno`] a syscall encoded as a negative register (`-ret`); an
/// unrecognised code fails closed as [`Errno::NotImplemented`] rather than
/// being guessed.
#[cfg(feature = "rt")]
fn errno_from(ret: i64) -> Errno {
    i32::try_from(-ret)
        .ok()
        .and_then(Errno::from_i32)
        .unwrap_or(Errno::NotImplemented)
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
    let cell = scale_atlas_metric(atlas::CELL_WIDTH, pixel_height);
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

    let monospace_advance = if test_family_is_monospace(family) {
        scale_atlas_metric(atlas::CELL_WIDTH, pixel_height)
    } else {
        0
    };
    let metrics = FontMetrics {
        pixel_height,
        baseline: scale_atlas_metric(atlas::BASELINE, pixel_height),
        line_height: pixel_height,
        monospace_advance,
    };
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
mod tests {
    use super::*;

    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tairix_log::DiscardSink;
    use tairix_reclaim::{CacheBudget, PressureBand, ReclaimOwner, ReportedPressure};

    use crate::glyph_cache::{glyph_cache_budget, glyph_cache_candidate};

    const INTER: FamilyKey = match FamilyKey::new("inter") {
        Ok(key) => key,
        Err(_) => FamilyKey::MONO,
    };

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

    fn client_with(transport: impl FontTransport + 'static) -> GlyphClient {
        let mut client = GlyphClient::new();
        client.transport = Some(Box::new(transport));
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
    fn counting_client() -> (GlyphClient, CallTally) {
        let tally = CallTally::default();
        (client_with(CountingTransport(tally.clone())), tally)
    }

    /// A cache built exactly as production builds one — the shared
    /// classification, the shared budget derivation — but from a gauge the
    /// test drives and a sink a host test has nowhere to send records to.
    fn cache_at(
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
    fn cached_client() -> (GlyphClient, &'static ReportedPressure) {
        let (cache, gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let mut client = client_with(SolidTestTransport);
        client.cache = Some(cache);
        (client, gauge)
    }

    /// The coverage the client hands the blitter, copied out so it can be
    /// compared across cache states.
    fn coverage(
        client: &mut GlyphClient,
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
        let cache = client.cache.as_ref().expect("installed");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.accounting().hits(), 1);
        assert_eq!(cache.accounting().misses(), 1);
    }

    #[test]
    fn a_different_family_is_a_distinct_cache_entry() {
        let (mut client, _gauge) = cached_client();
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        assert!(coverage(&mut client, 'A', INTER, 28).is_some());
        assert_eq!(client.cache.as_ref().expect("installed").len(), 2);
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
        assert_eq!(client.cache.as_ref().expect("installed").len(), 3);
    }

    // Only meaningful without a transport feature: with `test-util` (or `rt`)
    // the client installs a default transport lazily, so there is never a
    // "no transport" state to observe.
    #[cfg(not(any(feature = "rt", feature = "test-util")))]
    #[test]
    fn no_transport_composites_nothing() {
        let mut client = GlyphClient::new();
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 20).is_none());
    }

    #[test]
    fn a_refused_call_fails_closed() {
        let (cache, _gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let mut client = client_with(Refusing);
        client.cache = Some(cache);
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 20).is_none());
        // Nothing is cached, so a later working transport is still consulted.
        assert_eq!(client.cache.as_ref().expect("installed").len(), 0);
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
        client.cache = Some(cache);
        // Far more distinct glyphs than the ceiling can hold, each a full
        // bitmap: the cache must evict, never grow past the ceiling.
        for scalar in 0..1024u32 {
            let ch = char::from_u32(scalar).unwrap_or('A');
            assert!(
                coverage(&mut client, ch, FamilyKey::MONO, 28).is_some(),
                "still rendered"
            );
            let cache = client.cache.as_ref().expect("installed");
            assert!(
                cache.charged_bytes() <= budget.hard(),
                "charged {} exceeds the ceiling {}",
                cache.charged_bytes(),
                budget.hard()
            );
        }
        let cache = client.cache.as_ref().expect("installed");
        assert!(cache.accounting().evictions() > 0, "the bound must bite");
        assert!(!cache.poisoned(), "bounding is ordinary, not a defect");
    }

    #[test]
    fn an_unknown_ram_size_caches_nothing_yet_still_renders() {
        let (cache, _gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(0));
        let mut client = client_with(SolidTestTransport);
        client.cache = Some(cache);
        let (width, height, data) =
            coverage(&mut client, 'A', FamilyKey::MONO, 28).expect("still rendered");
        assert_eq!(data.len(), (width * height) as usize);
        assert!(data.iter().all(|&c| c == 255));
        let cache = client.cache.as_ref().expect("installed");
        assert_eq!(cache.len(), 0, "a zero budget retains nothing");
        assert_eq!(cache.charged_bytes(), 0);
    }

    #[test]
    fn mild_pressure_empties_the_cache_and_refuses_further_growth() {
        let (mut client, gauge) = cached_client();
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        assert_eq!(client.cache.as_ref().expect("installed").len(), 1);

        gauge.report(PressureBand::Mild);
        let cache = client.cache.as_mut().expect("installed");
        assert!(cache.enforce_pressure() > 0, "mild pressure must release");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.charged_bytes(), 0);

        assert!(
            coverage(&mut client, 'B', FamilyKey::MONO, 28).is_some(),
            "a shrunk cache still renders"
        );
        assert_eq!(
            client.cache.as_ref().expect("installed").len(),
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
        client.cache = Some(ReclaimCache::new(
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
        assert_eq!(client.cache.as_ref().expect("installed").len(), 0);

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
        client.cache = Some(cache);

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
        assert_eq!(client.trim(), 0, "nothing retained, nothing to release");
        assert!(coverage(&mut client, 'A', FamilyKey::MONO, 28).is_some());
        assert!(client.cache.as_ref().expect("installed").charged_bytes() > 0);

        gauge.report(PressureBand::Mild);
        assert!(client.trim() > 0, "the band's ceiling is applied at once");
        assert_eq!(client.cache.as_ref().expect("installed").len(), 0);

        let mut bare = client_with(SolidTestTransport);
        assert_eq!(bare.trim(), 0, "no cache installed is not an error");
        assert!(bare.cache.is_none(), "trimming never builds one");
    }

    #[test]
    fn the_same_glyph_renders_identically_cached_uncached_and_after_a_shrink() {
        let mut uncached = client_with(SolidTestTransport);
        let expected = coverage(&mut uncached, 'A', FamilyKey::MONO, 28)
            .expect("rendered with no cache at all");
        assert!(uncached.cache.is_none());

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
        let _ = client.cache.as_mut().expect("installed").enforce_pressure();
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
            scale_atlas_metric(atlas::CELL_WIDTH, 28)
        );
        // A second call for the same key must not need the transport at all;
        // swap in a refusing one and confirm the cached value still answers.
        client.transport = Some(Box::new(Refusing));
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
}
