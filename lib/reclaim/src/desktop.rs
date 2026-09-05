//! The one desktop-session disposable-UI cache policy (`plans/SMARTRAM.md`
//! section 6.4).
//!
//! A scale/theme-rasterised pointer cursor and a rasterised notification
//! glyph are the same kind of memory from the reclaim model's point of
//! view: cheap to lose, expensive to rebuild, owned by the seat that is
//! showing them, and invalidated wholesale the moment the scale, theme, or
//! source set changes. [`disposable_ui_candidate`] is the one declaration
//! of that classification, and [`disposable_ui_cache`] is the one place a
//! [`ReclaimCache`] is assembled from it, so the window manager's cursor
//! cache and the taskbar's icon cache — which may not depend on each
//! other — never carry two copies of the same six declared dimensions.
//!
//! # Why this lives here and not in the desktop session
//!
//! The natural first guess is that this is desktop-session policy and
//! belongs in `userland/gui/session`. It cannot. The window manager and
//! the taskbar each need to *name* the type of cache they hold and to
//! offer their embedder a way to build one, and neither may depend on the
//! session — the session composes them, and the reverse edge is
//! forbidden. They may not depend on each other either. `lib/reclaim` is
//! the only crate both already depend on and that sits below every
//! `userland/gui/*` crate, so it is the sole layering-legal home for the
//! one shared definition.
//!
//! This is not new policy leaking into the generic model:
//! [`ReclaimClass::DisposableUi`] and [`ReclaimOwner::DesktopSession`]
//! are already part of the closed taxonomy `model` defines, and this
//! module only composes them into the one declaration both consumers
//! would otherwise each spell out.
//!
//! # Every one of them keeps a reserve
//!
//! All three constructors declare
//! [`UI_CACHE_RESERVE_BYTES`] irreducible,
//! clamped by each cache's own display-derived ceiling. The three differ in
//! how much they hold above that — a cursor fraction for pixels a rasterise
//! rebuilds, a screenful for the bulkier kinds bounded by what can be seen —
//! and in which bands take it; none of them is ever emptied outright, because
//! a session with no rasterised
//! pixels left redraws the same screen through a filesystem read, a
//! sandbox round trip, or an IPC call per element, every repaint.
//!
//! # One constructor, injected
//!
//! There is exactly one way to build such a cache, and it demands the
//! real backing size, the real gauge, and the real audit sink. There is
//! deliberately no parameterless fallback: a cache built without a live
//! gauge would classify and serve correctly while retaining nothing, and
//! a desktop that silently rasterised every cursor afresh — with the
//! diagnostics that would reveal it discarded — is a defect that looks
//! exactly like working software. A consumer therefore takes its cache as
//! a constructor argument, and the session that knows the display size,
//! the seat, and the process gauge assembles it.

use tairix_log::Sink;

use crate::cache::{CachedBytes, ReclaimCache};
use crate::model::{
    CacheBudget, CacheCandidate, InvalidationSource, RebuildCost, ReclaimClass, ReclaimOwner,
    ReclaimRule, Sensitivity, UI_CACHE_RESERVE_BYTES,
};
use core::hash::{BuildHasher, Hash};

use crate::pressure::PressureGauge;

/// The classification every desktop-session disposable-UI raster cache
/// declares: a rasterised cursor image or notification glyph is expensive
/// to rebuild (a rasterisation pass), holds user-visible rendered data,
/// is invalidated wholesale by a generation token (the active scale paired
/// with a theme or source-set identity), is simply dropped on reclaim (the
/// canonical vector source rebuilds it), and is charged to the seat
/// showing it.
#[must_use]
pub const fn disposable_ui_candidate(seat: u64, entry_metadata_bytes: usize) -> CacheCandidate {
    CacheCandidate {
        class: Some(ReclaimClass::DisposableUi),
        owner: Some(ReclaimOwner::DesktopSession { seat }),
        rebuild_cost: RebuildCost::Expensive,
        sensitivity: Some(Sensitivity::UserData),
        invalidation: Some(InvalidationSource::GenerationToken),
        rule: Some(ReclaimRule::Drop),
        entry_metadata_bytes,
    }
}

/// Build one desktop-session disposable-UI cache, bounded by the real
/// output's backing byte size (`fb_bytes`, run through
/// [`CacheBudget::from_backing`] so a 4K display gets a proportionately
/// larger cache than a 640×480 one) and governed by the process's live
/// `pressure` gauge.
///
/// `seat` is the owning seat (for the classification's
/// [`ReclaimOwner::DesktopSession`]) and `entry_metadata_bytes` is the
/// caller's truthful bound on its own per-entry bookkeeping (its key's
/// size plus this cache's share of the map/index node overhead).
#[must_use]
pub fn disposable_ui_cache<K, V, E, S>(
    label: &'static str,
    seat: u64,
    fb_bytes: usize,
    entry_metadata_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
    hasher: S,
) -> ReclaimCache<K, V, E, S>
where
    K: Eq + Hash,
    V: CachedBytes,
    E: PartialEq + Clone,
    S: BuildHasher,
{
    ReclaimCache::new(
        label,
        disposable_ui_candidate(seat, entry_metadata_bytes),
        CacheBudget::from_backing(fb_bytes).with_reserved_floor(UI_CACHE_RESERVE_BYTES),
        pressure,
        sink,
        hasher,
    )
}

/// Build a desktop-session cache of *screen-sized* rendered pixels,
/// ceilinged at one screenful (`fb_bytes`) instead of the small fraction
/// [`disposable_ui_cache`] allows a cursor or a notification glyph.
///
/// A window's rendered furniture strips and a window's frosted backdrop
/// are the same *kind* of memory as those — both declare the identical
/// [`disposable_ui_candidate`] classification — but they are bulkier, and
/// they are bounded by a different fact about the machine: what can be
/// seen. No more of either than fills the screen can be visible at once,
/// so a screenful is the honest ceiling, and anything above it belongs to
/// a minimised, off-screen, or stacked-under window — exactly the entries
/// the least-recently-composited eviction order should take first. Sizing
/// them by the cursor cache's fraction instead would refuse a single
/// ordinary window and rebuild it every frame.
#[must_use]
pub fn screenful_ui_cache<K, V, E, S>(
    label: &'static str,
    seat: u64,
    fb_bytes: usize,
    entry_metadata_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
    hasher: S,
) -> ReclaimCache<K, V, E, S>
where
    K: Eq + Hash,
    V: CachedBytes,
    E: PartialEq + Clone,
    S: BuildHasher,
{
    ReclaimCache::new(
        label,
        disposable_ui_candidate(seat, entry_metadata_bytes),
        CacheBudget::from_ceiling(fb_bytes).with_reserved_floor(UI_CACHE_RESERVE_BYTES),
        pressure,
        sink,
        hasher,
    )
}

/// Build a desktop-session cache of pixels the session cannot rebuild
/// without leaving its own address space — a decoded on-disk icon
/// (a capability-gated read plus a parser-sandbox round trip), a glyph
/// fetched over the font endpoint.
///
/// Declared exactly like [`disposable_ui_cache`] and ceilinged at one
/// screenful like [`screenful_ui_cache`], for that constructor's reason: no
/// more of these pixels can be visible at once than fill the output they are
/// drawn on. A grid of icons is the demanding case and it is nowhere near the
/// cursor fraction — a 480×480 file-manager window draws some 117 KiB of icon
/// where a sixteenth of its frame is 57 KiB — so that fraction cannot hold
/// what one frame draws, and the cache evicts entries the very next paint
/// asks for again.
///
/// It differs from [`screenful_ui_cache`] in what mild and moderate pressure
/// may take: those two leave a quarter of the ceiling — what one frame draws —
/// alone ([`CacheBudget::with_working_set_floor`]) rather than the whole of it,
/// because rebuilding one of these costs a capability-gated read and a sandbox
/// round trip per icon, or a round trip per glyph — the resources a machine
/// short of memory has least of. Everything above that share is scroll-back and
/// off-screen speculation and goes at the first tightening; severe and critical
/// take it down to the shared irreducible reserve like every other UI cache.
#[must_use]
pub fn working_set_ui_cache<K, V, E, S>(
    label: &'static str,
    seat: u64,
    fb_bytes: usize,
    entry_metadata_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
    hasher: S,
) -> ReclaimCache<K, V, E, S>
where
    K: Eq + Hash,
    V: CachedBytes,
    E: PartialEq + Clone,
    S: BuildHasher,
{
    let budget = CacheBudget::from_ceiling(fb_bytes);
    ReclaimCache::new(
        label,
        disposable_ui_candidate(seat, entry_metadata_bytes),
        budget
            .with_working_set_floor(budget.hard() / WORKING_SET_DIVISOR)
            .with_reserved_floor(UI_CACHE_RESERVE_BYTES),
        pressure,
        sink,
        hasher,
    )
}

/// The share of [`working_set_ui_cache`]'s one-screenful ceiling that mild and
/// moderate pressure may not take: what one frame actually draws, rather than
/// everything the ceiling allows.
///
/// Measured on the demanding case, a file manager's icon grid: its default
/// 480×480 window draws sixteen 42-pixel tiles, 117 KiB of icon against a
/// 900 KiB frame — an eighth of it. A quarter is that with headroom for the
/// glyph masks drawn beside undecoded tiles, and still leaves three quarters of
/// the ceiling reclaimable at the first tightening.
const WORKING_SET_DIVISOR: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::AdmissionRefusal;
    use crate::pressure::{PressureBand, ReportedPressure};
    use tairix_hash::BuildFastHash;
    use tairix_log::DiscardSink;

    /// The cache shape every builder under test produces.
    type GlyphCache = ReclaimCache<u32, Glyph, u64, BuildFastHash>;

    extern crate std;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    #[derive(Debug, Eq, PartialEq)]
    struct Glyph {
        bytes: Vec<u8>,
    }

    impl CachedBytes for Glyph {
        fn payload_bytes(&self) -> usize {
            self.bytes.len()
        }

        fn wipe(&mut self) {
            self.bytes.fill(0);
        }
    }

    #[test]
    fn the_candidate_classifies_without_refusal() {
        let candidate = disposable_ui_candidate(3, 64);
        let policy = candidate.classify().expect("admissible");
        assert_eq!(policy.class(), ReclaimClass::DisposableUi);
        assert_eq!(policy.owner(), ReclaimOwner::DesktopSession { seat: 3 });
        assert_eq!(policy.rebuild_cost(), RebuildCost::Expensive);
        assert_eq!(policy.sensitivity(), Sensitivity::UserData);
        assert_eq!(policy.invalidation(), InvalidationSource::GenerationToken);
        assert_eq!(policy.rule(), ReclaimRule::Drop);
    }

    #[test]
    fn oversized_entry_metadata_is_refused_like_any_other_candidate() {
        let candidate = disposable_ui_candidate(0, usize::MAX);
        assert_eq!(
            candidate.classify(),
            Err(AdmissionRefusal::UnboundedMetadata)
        );
    }

    #[test]
    fn the_chrome_ceiling_retains_a_desktop_of_furniture_the_glyph_fraction_cannot() {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
        // One 1080p output, and one full-width window's title/border
        // strips at that width — the figures a real desktop retains.
        let fb_bytes = 1920 * 1080 * 4;
        let strips = 1920 * 32 * 4;
        let furniture = |key: u32, cache: &mut ReclaimCache<u32, Glyph, u64, BuildFastHash>| {
            let _ = cache.get_or_build(&1, key, || {
                Some(Glyph {
                    bytes: vec![0x11; strips],
                })
            });
        };

        let mut chrome: ReclaimCache<u32, Glyph, u64, BuildFastHash> = screenful_ui_cache(
            "test.chrome",
            1,
            fb_bytes,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        let mut glyphs: ReclaimCache<u32, Glyph, u64, BuildFastHash> = disposable_ui_cache(
            "test.glyphs",
            1,
            fb_bytes,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        for key in 0..8u32 {
            furniture(key, &mut chrome);
            furniture(key, &mut glyphs);
        }

        assert_eq!(
            chrome.len(),
            8,
            "a screenful holds a whole desktop of furniture"
        );
        assert!(chrome.charged_bytes() <= CacheBudget::from_ceiling(fb_bytes).hard());
        assert!(
            glyphs.len() < chrome.len(),
            "the cursor/icon fraction is far too small for window furniture"
        );
        assert!(glyphs.charged_bytes() <= CacheBudget::from_backing(fb_bytes).hard());
    }

    /// The working-set ceiling holds what one frame draws. Icons are drawn
    /// *on* the output, so their pixels cannot outrun its own — but a
    /// sixteenth of it cannot hold them, and a cache that evicts an icon the
    /// next paint asks for again either draws the wrong picture or decodes it
    /// afresh every frame, at a capability-gated read and a sandbox round trip
    /// each.
    #[test]
    fn the_working_set_ceiling_holds_a_frame_of_icons_the_glyph_fraction_cannot() {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
        // A file manager's default 480x480 window, and the sixteen 42-pixel
        // icons its grid draws in one frame — the measured figures, not a
        // round number.
        let fb_bytes = 480 * 4 * 480;
        let icon = 42 * 42 * 4;
        let tiles = 16u32;

        let mut icons: ReclaimCache<u32, Glyph, u64, BuildFastHash> = working_set_ui_cache(
            "test.icons",
            1,
            fb_bytes,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        let mut fraction: ReclaimCache<u32, Glyph, u64, BuildFastHash> = disposable_ui_cache(
            "test.fraction",
            1,
            fb_bytes,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        for key in 0..tiles {
            for cache in [&mut icons, &mut fraction] {
                let _ = cache.get_or_build(&1, key, || {
                    Some(Glyph {
                        bytes: vec![0x33; icon],
                    })
                });
            }
        }

        assert_eq!(
            icons.len(),
            tiles as usize,
            "a frame's icons did not all fit the working-set ceiling"
        );
        assert!(
            fraction.len() < tiles as usize,
            "the cursor fraction is what could not hold one frame of them"
        );
    }

    /// Mild pressure hands back the scroll-back and off-screen speculation but
    /// not what a frame draws. Keeping the whole screenful there would pin a
    /// figure the session is not drawing on precisely the machine that is
    /// tightening; giving all of it back would put every icon on screen through
    /// a read and a sandbox round trip on the next repaint.
    #[test]
    fn mild_pressure_takes_the_speculation_and_leaves_a_frame_of_icons() {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
        // A 1080p output, where a quarter of the ceiling is well clear of the
        // shared reserve and so is the figure actually under test.
        let fb_bytes = 1920 * 1080 * 4;
        let floor = fb_bytes / super::WORKING_SET_DIVISOR;
        assert!(floor > UI_CACHE_RESERVE_BYTES, "the reserve would dominate");
        let icon = 42 * 42 * 4;

        let mut icons: ReclaimCache<u32, Glyph, u64, BuildFastHash> = working_set_ui_cache(
            "test.icons.mild",
            1,
            fb_bytes,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        for key in 0..u32::try_from(fb_bytes / icon).expect("a screenful of icons") {
            let _ = icons.get_or_build(&1, key, || {
                Some(Glyph {
                    bytes: vec![0x44; icon],
                })
            });
        }
        assert!(
            icons.charged_bytes() > floor,
            "the fixture never filled past the floor it is testing"
        );

        gauge.report(PressureBand::Mild);
        assert!(icons.enforce_pressure() > 0, "the speculation must go");
        assert!(icons.charged_bytes() <= floor);
        assert!(
            icons.len() * icon >= fb_bytes / 8,
            "mild pressure took what a frame draws, not just the speculation"
        );
    }

    #[test]
    fn every_desktop_cache_keeps_its_reserve_at_the_deepest_band() {
        use crate::model::UI_CACHE_RESERVE_BYTES;
        use crate::pressure::shrink_target;

        // One 1080p output, and a glyph small enough that a screenful of them
        // fits inside the smallest of the three budgets.
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
        let fb_bytes = 1920 * 1080 * 4;

        let mut caches: Vec<(&str, GlyphCache)> = vec![
            (
                "cursor",
                disposable_ui_cache(
                    "test.cursor",
                    1,
                    fb_bytes,
                    32,
                    gauge,
                    sink,
                    BuildFastHash::new(),
                ),
            ),
            (
                "chrome",
                screenful_ui_cache(
                    "test.chrome",
                    1,
                    fb_bytes,
                    32,
                    gauge,
                    sink,
                    BuildFastHash::new(),
                ),
            ),
            (
                "icons",
                working_set_ui_cache(
                    "test.icons",
                    1,
                    fb_bytes,
                    32,
                    gauge,
                    sink,
                    BuildFastHash::new(),
                ),
            ),
        ];
        for (what, cache) in &mut caches {
            let _ = cache.get_or_build(&1, 0, || {
                Some(Glyph {
                    bytes: vec![0x55; 4096],
                })
            });
            assert!(cache.charged_bytes() > 0, "{what}");
        }

        for band in [PressureBand::Severe, PressureBand::Critical] {
            gauge.report(band);
            for (what, cache) in &mut caches {
                assert_eq!(
                    cache.enforce_pressure(),
                    0,
                    "{band:?} took {what}'s reserve"
                );
                assert_eq!(cache.len(), 1, "{band:?} {what}");
            }
        }

        // Above the reserve the ordinary order still applies: a screenful
        // cache holding more than the reserve gives the excess back at mild
        // pressure and keeps the reserve. Furniture strips at this width, so
        // the figures are a real desktop's.
        gauge.report(PressureBand::Normal);
        let strips = 1920 * 32 * 4;
        let mut chrome: ReclaimCache<u32, Glyph, u64, BuildFastHash> = screenful_ui_cache(
            "test.chrome.deep",
            1,
            fb_bytes,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        for key in 0..8u32 {
            let _ = chrome.get_or_build(&1, key, || {
                Some(Glyph {
                    bytes: vec![0x22; strips],
                })
            });
        }
        assert!(chrome.charged_bytes() > UI_CACHE_RESERVE_BYTES);
        gauge.report(PressureBand::Mild);
        assert!(chrome.enforce_pressure() > 0, "the speculation must go");
        assert!(chrome.charged_bytes() <= UI_CACHE_RESERVE_BYTES);
        assert!(!chrome.is_empty(), "but not the reserve");

        // The reserve is the smaller of the shared figure and the cache's own
        // display-derived ceiling, so a small budget reserves all of itself
        // and a screen-sized one reserves the shared figure.
        let small = CacheBudget::from_backing(fb_bytes).with_reserved_floor(UI_CACHE_RESERVE_BYTES);
        let large = CacheBudget::from_ceiling(fb_bytes).with_reserved_floor(UI_CACHE_RESERVE_BYTES);
        assert_eq!(small.reserved(), small.hard());
        assert_eq!(large.reserved(), UI_CACHE_RESERVE_BYTES);
        assert_eq!(
            shrink_target(PressureBand::Critical, ReclaimClass::DisposableUi, large),
            UI_CACHE_RESERVE_BYTES
        );
    }

    #[test]
    fn a_wired_cache_is_bounded_by_the_real_backing_size() {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
        let mut cache: ReclaimCache<u32, Glyph, u64, BuildFastHash> = disposable_ui_cache(
            "test.wired",
            1,
            64 * 1024,
            32,
            gauge,
            sink,
            BuildFastHash::new(),
        );
        for key in 0..8u32 {
            let _ = cache.get_or_build(&1, key, || {
                Some(Glyph {
                    bytes: vec![0xAA; 1024],
                })
            });
        }
        let hard = CacheBudget::from_backing(64 * 1024).hard();
        assert!(cache.charged_bytes() <= hard);
        assert!(!cache.is_empty(), "normal pressure still admits entries");
    }
}
