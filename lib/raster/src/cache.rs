//! Scale/theme-keyed rasterisation cache.
//!
//! The desktop's graphical assets are authored as resolution-independent
//! vector forms (a `lib/cursor` cursor, a `lib/icon` glyph, a `lib/svg`
//! image) and rasterised into the [`Surface`](crate::Surface) the compositor
//! blits. Rasterising is the expensive step, and it must happen only when it
//! can change: the SVG-first rule (`AGENTS.md` §10) requires each asset to be
//! converted **once** at the active scale and re-rendered only when the scale
//! or the theme changes — never on the hot compositing path.
//!
//! [`RasterCache`] is that one shared mechanism. The window manager caches
//! pointer cursors by cursor kind and the taskbar caches notification glyphs
//! by icon kind, but both reuse this single epoch-keyed memoisation rather
//! than each growing its own (`AGENTS.md` §2.2 / §6).
//!
//! # Epochs
//!
//! An *epoch* is whatever the caller decides invalidates every cached image:
//! typically the active scale combined with the active theme (or the precise
//! tint and pixel size derived from them). [`get_or_render`] clears the whole
//! cache the moment the epoch differs from the one the entries were rendered
//! at, so a scale or theme change re-renders, and a stable epoch reuses.
//!
//! Within one epoch, an asset is rendered at most once: a cache hit returns
//! the stored image; a miss runs the caller's render closure and stores the
//! result. Rendering may fail closed (`AGENTS.md` §2.9): a closure returning
//! `None` is not cached, so the asset is retried next time rather than a
//! failure being remembered forever.
//!
//! [`get_or_render`]: RasterCache::get_or_render

use alloc::vec::Vec;

/// A cache of rasterised values keyed by an asset identity `K`, valid only
/// for one epoch `E` (see the [module docs](self)).
///
/// `K` identifies an asset within an epoch (a cursor kind, an icon kind, …)
/// and need only be comparable. `V` is the rasterised value (a
/// [`Surface`](crate::Surface), a cursor image, …). `E` is the
/// invalidation epoch (a scale plus a theme identity); changing it empties
/// the cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RasterCache<K, V, E> {
    epoch: Option<E>,
    entries: Vec<(K, V)>,
}

impl<K, V, E> RasterCache<K, V, E> {
    /// An empty cache holding no entries and no epoch; the first
    /// [`get_or_render`](Self::get_or_render) adopts the epoch it is given.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            epoch: None,
            entries: Vec::new(),
        }
    }

    /// The number of rendered assets currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is cached (either never rendered or just cleared).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The epoch the cached entries were rendered at, or `None` before the
    /// first render.
    #[must_use]
    pub const fn epoch(&self) -> Option<&E> {
        self.epoch.as_ref()
    }

    /// Drop every cached entry and forget the epoch, so the next
    /// [`get_or_render`](Self::get_or_render) renders afresh.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.epoch = None;
    }
}

impl<K, V, E> Default for RasterCache<K, V, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: PartialEq, V, E: PartialEq + Clone> RasterCache<K, V, E> {
    /// Return the cached image for `key` at `epoch`, rendering it once if it
    /// is absent.
    ///
    /// If `epoch` differs from the epoch the cache was last rendered at,
    /// every entry is discarded first — a scale or theme change invalidates
    /// the lot. Within the epoch a present `key` is returned without calling
    /// `render`; an absent `key` runs `render` and stores its result.
    ///
    /// `render` returning `None` (a degenerate asset or scale, `AGENTS.md`
    /// §2.9) caches nothing and yields `None`, so the asset is retried on the
    /// next call rather than a failure being remembered.
    pub fn get_or_render<F>(&mut self, epoch: &E, key: K, render: F) -> Option<&V>
    where
        F: FnOnce() -> Option<V>,
    {
        if self.epoch.as_ref() != Some(epoch) {
            self.entries.clear();
            self.epoch = Some(epoch.clone());
        }

        if let Some(index) = self.entries.iter().position(|(k, _)| *k == key) {
            return self.entries.get(index).map(|(_, value)| value);
        }

        let value = render()?;
        self.entries.push((key, value));
        self.entries.last().map(|(_, value)| value)
    }
}
