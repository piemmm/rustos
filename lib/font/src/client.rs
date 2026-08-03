//! The font-service client: the render path's thin, cached front end to the
//! sandboxed OS font service (`fontd`) over the reserved `FONT_ENDPOINT`.
//!
//! `lib/font` parses no TrueType and holds no font outline: the four faces
//! live only in `fontd`, which rasterises a scalar at a chosen cell height in
//! its own minimum-capability sandbox and returns the small 8-bit coverage
//! bitmap the blitter composites. A malformed face can therefore fault only
//! that sandbox, never a compositor or a terminal.
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
//! # Local cache
//!
//! Each reply is memoised per `(scalar, cell height, weight)` in a
//! [`tairix_reclaim::ReclaimCache`] ([`crate::glyph_cache`]), so a
//! steady-state redraw of the same text in the same size and weight issues no
//! IPC. The byte budget is derived from the machine's total RAM, never a
//! hand-picked entry count, so a hostile or careless caller who renders at
//! ever more sizes and scalars can grow the cache only up to that budget
//! before the oldest entries are evicted. Until a cache is installed —
//! before an `rt` program's first draw, or in a host test that never calls
//! [`set_glyph_cache`] — every glyph is fetched and served uncached: correct,
//! merely one IPC per glyph.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::font_ipc::{decode_glyph_reply, FontRequest, FontWeight, FONT_MAX_GLYPH_REPLY};
use tairix_abi::Errno;
use tairix_reclaim::ReclaimCache;
use tairix_sync::SpinLock;

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
    /// request and composites nothing.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// A client cache key: the Unicode scalar, the cell height it was rendered
/// at, and the wire weight it was rendered in. A heavier weight is a
/// different bitmap of the same scalar, so it is part of the key rather than
/// overwriting it.
pub type GlyphKey = (u32, u32, u16);

/// The render path's glyph cache: the shared bounded, classified,
/// pressure-governed cache holding [`CachedGlyph`] coverage under a
/// [`GlyphKey`].
///
/// The generation token is `()` because nothing invalidates a fetched glyph
/// while the process lives: `fontd` parses its face set once at startup and
/// never reloads it, so the same scalar at the same height and weight is the
/// same bitmap every time. Entries leave only by eviction, by pressure, or
/// with the cache itself — which is exactly the owner-teardown invalidation
/// the shared classification declares. Inventing a churning epoch would throw
/// a live working set away for no event.
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

/// The render path's process-global font client: the installed transport, a
/// reusable receive buffer, and the optional local glyph cache.
struct GlyphClient {
    transport: Option<Box<dyn FontTransport>>,
    reply: Vec<u8>,
    /// `None` until a cache is installed, in which case every glyph is
    /// fetched and served without being retained — correct, merely one IPC
    /// per glyph.
    cache: Option<GlyphCache>,
}

impl GlyphClient {
    const fn new() -> Self {
        Self {
            transport: None,
            reply: Vec::new(),
            cache: None,
        }
    }

    /// Serve `(scalar, cell_height, weight)` to `f`, fetching it over the
    /// transport on a miss and retaining it when a cache is installed and
    /// admits it.
    ///
    /// `None` — composite nothing, fail closed — when no transport is
    /// installed or the call or its reply could not be read.
    fn with_glyph<R>(
        &mut self,
        scalar: char,
        cell_height: u32,
        weight: FontWeight,
        f: impl FnOnce(&CachedGlyph) -> R,
    ) -> Option<R> {
        // Neither the transport nor the cache can be built in the `const`
        // initialiser of the client `static` — one issues syscalls, the other
        // reads the machine's RAM size — so a real program's defaults are
        // installed on first use instead, keeping it free of setup.
        #[cfg(feature = "rt")]
        {
            if self.transport.is_none() {
                self.transport = Some(Box::new(RtTransport));
            }
            if self.cache.is_none() {
                self.cache = Some(default_cache());
            }
        }
        // A consumer's host tests enable `test-util` to get deterministic
        // glyph coverage without a running service; the runtime transport
        // takes precedence when both are present (a real program build).
        #[cfg(all(feature = "test-util", not(feature = "rt")))]
        if self.transport.is_none() {
            self.transport = Some(Box::new(SolidTestTransport));
        }
        let Self {
            transport,
            reply,
            cache,
        } = self;
        let transport = transport.as_mut()?;
        let Some(cache) = cache.as_mut() else {
            let glyph = fetch(transport.as_mut(), reply, scalar, cell_height, weight)?;
            return Some(f(&glyph));
        };
        let key = (scalar as u32, cell_height, weight.to_wire());
        let served = cache.get_or_build(&(), key, || {
            fetch(transport.as_mut(), reply, scalar, cell_height, weight)
        })?;
        Some(f(&served))
    }
}

/// Fetch one glyph's coverage over `transport` into the reusable `reply`
/// buffer.
///
/// Every failure — a refused call, a length the reply buffer cannot hold, a
/// frame that does not decode — yields `None`, so a caller composites nothing
/// rather than reading a bitmap the service did not send.
fn fetch(
    transport: &mut dyn FontTransport,
    reply: &mut Vec<u8>,
    scalar: char,
    cell_height: u32,
    weight: FontWeight,
) -> Option<CachedGlyph> {
    if reply.len() < FONT_MAX_GLYPH_REPLY {
        reply.resize(FONT_MAX_GLYPH_REPLY, 0);
    }
    let request = FontRequest::Glyph {
        scalar,
        cell_height,
        weight,
    }
    .to_le_bytes();
    let len = transport.call(&request, reply).ok()?;
    let frame = reply.get(..len)?;
    let coverage = decode_glyph_reply(frame).ok()?;
    Some(CachedGlyph::new(
        coverage.width,
        coverage.height,
        Box::from(coverage.coverage),
    ))
}

/// Build the client's own glyph cache, budgeted from the machine's total
/// usable RAM.
///
/// A RAM read that fails — no System Information service, a refused or
/// malformed reply — is a zero total, hence a zero budget, hence a cache that
/// admits nothing and serves every glyph freshly fetched. That is the honest
/// outcome: slower, never wrong, and never a hand-picked ceiling standing in
/// for a figure the machine did not supply.
#[cfg(feature = "rt")]
fn default_cache() -> GlyphCache {
    use tairix_reclaim::ReclaimOwner;

    static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;

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

/// Fetch the coverage glyph for `(scalar, cell_height, weight)` and hand it to
/// `f`, or return `None` (compositing nothing) when the service is unreachable.
///
/// The global lock is held across `f` so glyph fetch and blit see a
/// consistent cache; `f` does only the bounded per-glyph blit.
pub(crate) fn with_glyph<R>(
    scalar: char,
    cell_height: u32,
    weight: FontWeight,
    f: impl FnOnce(&CachedGlyph) -> R,
) -> Option<R> {
    CLIENT.lock().with_glyph(scalar, cell_height, weight, f)
}

/// The production transport: the `ipc_call` syscall to [`FONT_ENDPOINT`].
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

/// A deterministic solid-coverage transport for host tests.
///
/// It answers every glyph with a fully-covered bitmap two cells wide, sized
/// exactly as the real service would (the cell width scaled from the console
/// atlas geometry), so a consumer's tests exercise layout, caching, and the
/// blit path without a running `fontd`. The requested weight only has to be
/// accepted — synthetic emboldening changes coverage, never geometry or the
/// advance, and a solid cell is already saturated. Consumers enable it through
/// the `test-util` feature (installed lazily on first draw, or explicitly via
/// [`install_test_transport`]); rendering fidelity is `fontd`'s job, tested
/// there.
#[cfg(any(test, feature = "test-util"))]
pub struct SolidTestTransport;

#[cfg(any(test, feature = "test-util"))]
impl FontTransport for SolidTestTransport {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        use tairix_abi::font_ipc::encode_glyph_reply;
        let FontRequest::Glyph {
            scalar,
            cell_height,
            weight: _,
        } = FontRequest::from_bytes(request)?
        else {
            return Err(Errno::NotImplemented);
        };
        let native = crate::atlas::CELL_HEIGHT;
        let cell_width = ((crate::atlas::CELL_WIDTH * cell_height + native / 2) / native).max(1);
        let width = cell_width.saturating_mul(2);
        let advance = cell_width
            .saturating_mul(u32::from(tairix_vt::char_width(scalar)))
            .max(1);
        // Space is blank, like the real face; every other scalar is solid so a
        // consumer's layout/ink assertions hold without a real rasteriser.
        let level = if scalar == ' ' { 0 } else { 255 };
        let coverage = alloc::vec![level; (width * cell_height) as usize];
        encode_glyph_reply(reply, width, cell_height, advance, &coverage)
    }
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

    use alloc::vec::Vec;
    use tairix_log::DiscardSink;
    use tairix_reclaim::{CacheBudget, PressureBand, ReclaimOwner, ReportedPressure};

    use crate::glyph_cache::{glyph_cache_budget, glyph_cache_candidate};

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
        height: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        client.with_glyph(scalar, height, FontWeight::Regular, |glyph| {
            (glyph.width, glyph.height, glyph.data.to_vec())
        })
    }

    #[test]
    fn a_glyph_is_fetched_then_served_from_cache() {
        let (mut client, _gauge) = cached_client();
        let (width, height, data) = coverage(&mut client, 'A', 28).expect("fetched");
        // Two cells wide, `cell_height` tall, solid coverage.
        assert_eq!(height, 28);
        assert_eq!(width, 2 * crate::atlas::CELL_WIDTH);
        assert_eq!(data.len(), (width * height) as usize);
        assert!(data.iter().all(|&c| c == 255));

        assert!(coverage(&mut client, 'A', 28).is_some());
        let cache = client.cache.as_ref().expect("installed");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.accounting().hits(), 1);
        assert_eq!(cache.accounting().misses(), 1);
    }

    #[test]
    fn a_heavier_weight_is_a_distinct_cache_entry() {
        let (mut client, _gauge) = cached_client();
        for weight in [FontWeight::Regular, FontWeight::Medium, FontWeight::Bold] {
            assert!(client.with_glyph('A', 28, weight, |_| ()).is_some());
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
        assert!(coverage(&mut client, 'A', 20).is_none());
    }

    #[test]
    fn a_refused_call_fails_closed() {
        let (cache, _gauge) = cache_at(PressureBand::Normal, glyph_cache_budget(1 << 30));
        let mut client = client_with(Refusing);
        client.cache = Some(cache);
        assert!(coverage(&mut client, 'A', 20).is_none());
        // Nothing is cached, so a later working transport is still consulted.
        assert_eq!(client.cache.as_ref().expect("installed").len(), 0);
    }

    #[test]
    fn a_reply_longer_than_the_buffer_fails_closed() {
        let mut client = client_with(Overlong);
        assert!(
            coverage(&mut client, 'A', 20).is_none(),
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
            assert!(coverage(&mut client, ch, 28).is_some(), "still rendered");
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
        let (width, height, data) = coverage(&mut client, 'A', 28).expect("still rendered");
        assert_eq!(data.len(), (width * height) as usize);
        assert!(data.iter().all(|&c| c == 255));
        let cache = client.cache.as_ref().expect("installed");
        assert_eq!(cache.len(), 0, "a zero budget retains nothing");
        assert_eq!(cache.charged_bytes(), 0);
    }

    #[test]
    fn mild_pressure_empties_the_cache_and_refuses_further_growth() {
        let (mut client, gauge) = cached_client();
        assert!(coverage(&mut client, 'A', 28).is_some());
        assert_eq!(client.cache.as_ref().expect("installed").len(), 1);

        gauge.report(PressureBand::Mild);
        let cache = client.cache.as_mut().expect("installed");
        assert!(cache.enforce_pressure() > 0, "mild pressure must release");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.charged_bytes(), 0);

        assert!(
            coverage(&mut client, 'B', 28).is_some(),
            "a shrunk cache still renders"
        );
        assert_eq!(
            client.cache.as_ref().expect("installed").len(),
            0,
            "no growth while the band forbids it"
        );
    }

    #[test]
    fn the_same_glyph_renders_identically_cached_uncached_and_after_a_shrink() {
        let mut uncached = client_with(SolidTestTransport);
        let expected = coverage(&mut uncached, 'A', 28).expect("rendered with no cache at all");
        assert!(uncached.cache.is_none());

        let (mut client, gauge) = cached_client();
        assert_eq!(coverage(&mut client, 'A', 28).as_ref(), Some(&expected));
        assert_eq!(
            coverage(&mut client, 'A', 28).as_ref(),
            Some(&expected),
            "a cache hit serves the same bitmap the fetch did"
        );

        gauge.report(PressureBand::Mild);
        let _ = client.cache.as_mut().expect("installed").enforce_pressure();
        assert_eq!(
            coverage(&mut client, 'A', 28).as_ref(),
            Some(&expected),
            "the cache is an accelerator; losing it changes nothing"
        );
    }
}
