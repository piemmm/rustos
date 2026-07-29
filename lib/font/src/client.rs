//! The font-service client: the render path's thin, cached front end to the
//! sandboxed OS font service (`fontd`) over the reserved `FONT_ENDPOINT`.
//!
//! `lib/font` no longer parses TrueType or holds a font outline (`AGENTS.md`
//! §16.4, §19.5): the four faces live only in `fontd`, which rasterises a
//! scalar at a chosen cell height in its own minimum-capability sandbox and
//! returns the small 8-bit coverage bitmap the blitter composites. A malformed
//! face can therefore fault only that sandbox, never a compositor or a
//! terminal.
//!
//! # Transport seam
//!
//! Drawing text takes no client handle — [`crate::BitmapFont::draw_text`] is a
//! plain method — so the client is a process-global behind a [`FontTransport`]
//! seam: production installs the `ipc_call`-backed transport, a host test
//! installs a mock. Under the optional `rt` feature the seam defaults lazily
//! to the runtime transport, so a program that links `tairix-rt` needs no
//! setup; without a transport a draw composites nothing (fail closed,
//! `AGENTS.md` §5.4) rather than reaching for a device.
//!
//! # Local cache
//!
//! Each reply is memoised per `(scalar, cell height, weight)` in a bounded FIFO
//! cache, so a steady-state redraw of the same text in the same size and weight
//! issues no IPC.
//! The cache is a client-side fail-closed bound, not a scalable capacity: a
//! pathological caller that renders at ever more sizes evicts the oldest
//! entries rather than growing without bound.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use tairix_abi::font_ipc::{decode_glyph_reply, FontRequest, FontWeight, FONT_MAX_GLYPH_REPLY};
use tairix_abi::Errno;
use tairix_sync::SpinLock;

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

/// A rasterised glyph as the blitter consumes it: the bitmap size the service
/// returned plus the owned `width * height` row-major 8-bit coverage.
///
/// The reply's `advance` field is not stored: the client lays text out from
/// its own monospace geometry (derived identically to the service's), so the
/// pen advance is a local computation, not a per-glyph value.
pub(crate) struct CachedGlyph {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) data: Box<[u8]>,
}

/// The largest number of distinct `(scalar, cell height, weight)` glyphs the
/// client retains before evicting the oldest.
///
/// The desktop draws a small number of sizes and weights over a modest visible
/// glyph repertoire, so this comfortably holds a steady-state working set while
/// capping the entry count (a fail-closed bound, not a scalable capacity).
const MAX_ENTRIES: usize = 1024;

/// The cache key: the Unicode scalar, the cell height it was rendered at, and
/// the wire weight it was rendered in. A heavier weight is a different bitmap
/// of the same scalar, so it is part of the key rather than overwriting it.
type Key = (u32, u32, u16);

/// A bounded FIFO map from [`Key`] to a rasterised coverage glyph.
struct GlyphCache {
    entries: BTreeMap<Key, CachedGlyph>,
    order: VecDeque<Key>,
}

impl GlyphCache {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Insert `glyph` for `key`, evicting the oldest entry first when the
    /// cache is full so its footprint stays bounded.
    fn insert(&mut self, key: Key, glyph: CachedGlyph) {
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
        self.entries.insert(key, glyph);
    }
}

/// The render path's process-global font client: the installed transport, a
/// reusable receive buffer, and the local glyph cache.
struct GlyphClient {
    transport: Option<Box<dyn FontTransport>>,
    reply: Vec<u8>,
    cache: GlyphCache,
}

impl GlyphClient {
    const fn new() -> Self {
        Self {
            transport: None,
            reply: Vec::new(),
            cache: GlyphCache::new(),
        }
    }

    /// Ensure `(scalar, cell_height, weight)` is cached, fetching it over the
    /// transport on a miss, and return it — or `None` when no transport is
    /// installed or the call/decoding failed (fail closed).
    fn ensure(
        &mut self,
        scalar: char,
        cell_height: u32,
        weight: FontWeight,
    ) -> Option<&CachedGlyph> {
        let key = (scalar as u32, cell_height, weight.to_wire());
        if !self.cache.entries.contains_key(&key) {
            self.fetch(scalar, cell_height, weight, key);
        }
        self.cache.entries.get(&key)
    }

    /// Fetch one glyph over the transport and cache it; a failure leaves the
    /// cache unchanged so the caller composites nothing.
    fn fetch(&mut self, scalar: char, cell_height: u32, weight: FontWeight, key: Key) {
        #[cfg(feature = "rt")]
        if self.transport.is_none() {
            self.transport = Some(Box::new(RtTransport));
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
        let Some(transport) = transport.as_mut() else {
            return;
        };
        if reply.len() < FONT_MAX_GLYPH_REPLY {
            reply.resize(FONT_MAX_GLYPH_REPLY, 0);
        }
        let request = FontRequest::Glyph {
            scalar,
            cell_height,
            weight,
        }
        .to_le_bytes();
        let Ok(len) = transport.call(&request, reply) else {
            return;
        };
        let Ok(coverage) = decode_glyph_reply(&reply[..len]) else {
            return;
        };
        cache.insert(
            key,
            CachedGlyph {
                width: coverage.width,
                height: coverage.height,
                data: Box::from(coverage.coverage),
            },
        );
    }
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
    let mut client = CLIENT.lock();
    let glyph = client.ensure(scalar, cell_height, weight)?;
    Some(f(glyph))
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

    /// A transport that always refuses, to exercise the fail-closed path.
    struct Refusing;
    impl FontTransport for Refusing {
        fn call(&mut self, _request: &[u8], _reply: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::NotFound)
        }
    }

    fn client_with(transport: impl FontTransport + 'static) -> GlyphClient {
        let mut client = GlyphClient::new();
        client.transport = Some(Box::new(transport));
        client
    }

    #[test]
    fn a_glyph_is_fetched_then_served_from_cache() {
        let mut client = client_with(SolidTestTransport);
        let first = client
            .ensure('A', 28, FontWeight::Regular)
            .expect("fetched");
        // Two cells wide, `cell_height` tall, solid coverage.
        assert_eq!(first.height, 28);
        assert_eq!(first.width, 2 * crate::atlas::CELL_WIDTH);
        assert_eq!(first.data.len(), (first.width * first.height) as usize);
        assert!(first.data.iter().all(|&c| c == 255));
        // A second lookup hits the cache: one entry, still present.
        assert!(client.ensure('A', 28, FontWeight::Regular).is_some());
        assert_eq!(client.cache.order.len(), 1);
    }

    #[test]
    fn a_heavier_weight_is_a_distinct_cache_entry() {
        let mut client = client_with(SolidTestTransport);
        for weight in [FontWeight::Regular, FontWeight::Medium, FontWeight::Bold] {
            assert!(client.ensure('A', 28, weight).is_some());
        }
        // The same scalar at the same height in three weights is three
        // bitmaps, so a bold run can never be served a regular raster.
        assert_eq!(client.cache.order.len(), 3);
    }

    // Only meaningful without a transport feature: with `test-util` (or `rt`)
    // the client installs a default transport lazily, so there is never a
    // "no transport" state to observe.
    #[cfg(not(any(feature = "rt", feature = "test-util")))]
    #[test]
    fn no_transport_composites_nothing() {
        let mut client = GlyphClient::new();
        assert!(client.ensure('A', 20, FontWeight::Regular).is_none());
    }

    #[test]
    fn a_refused_call_fails_closed() {
        let mut client = client_with(Refusing);
        assert!(client.ensure('A', 20, FontWeight::Regular).is_none());
        // Nothing is cached, so a later working transport is still consulted.
        assert!(client.cache.entries.is_empty());
    }

    #[test]
    fn the_cache_evicts_the_oldest_beyond_the_bound() {
        let mut client = client_with(SolidTestTransport);
        // Vary the scalar (the cell-height band is only a few hundred wide) so
        // the distinct-key count exceeds the bound and forces eviction.
        let count = u32::try_from(MAX_ENTRIES + 16).expect("bound fits a u32");
        for scalar in 0..count {
            let ch = char::from_u32(scalar).unwrap_or('A');
            assert!(client.ensure(ch, 20, FontWeight::Regular).is_some());
        }
        assert!(client.cache.order.len() <= MAX_ENTRIES);
    }
}
