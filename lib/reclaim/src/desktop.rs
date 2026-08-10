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
    ReclaimRule, Sensitivity,
};
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
pub fn disposable_ui_cache<K, V, E>(
    label: &'static str,
    seat: u64,
    fb_bytes: usize,
    entry_metadata_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<K, V, E>
where
    K: Ord + Clone,
    V: CachedBytes,
    E: PartialEq + Clone,
{
    ReclaimCache::new(
        label,
        disposable_ui_candidate(seat, entry_metadata_bytes),
        CacheBudget::from_backing(fb_bytes),
        pressure,
        sink,
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
pub fn screenful_ui_cache<K, V, E>(
    label: &'static str,
    seat: u64,
    fb_bytes: usize,
    entry_metadata_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<K, V, E>
where
    K: Ord + Clone,
    V: CachedBytes,
    E: PartialEq + Clone,
{
    ReclaimCache::new(
        label,
        disposable_ui_candidate(seat, entry_metadata_bytes),
        CacheBudget::from_ceiling(fb_bytes),
        pressure,
        sink,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AdmissionRefusal;
    use crate::pressure::{PressureBand, ReportedPressure};
    use tairix_log::DiscardSink;

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
        let furniture = |key: u32, cache: &mut ReclaimCache<u32, Glyph, u64>| {
            let _ = cache.get_or_build(&1, key, || {
                Some(Glyph {
                    bytes: vec![0x11; strips],
                })
            });
        };

        let mut chrome: ReclaimCache<u32, Glyph, u64> =
            screenful_ui_cache("test.chrome", 1, fb_bytes, 32, gauge, sink);
        let mut glyphs: ReclaimCache<u32, Glyph, u64> =
            disposable_ui_cache("test.glyphs", 1, fb_bytes, 32, gauge, sink);
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

    #[test]
    fn a_wired_cache_is_bounded_by_the_real_backing_size() {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
        let mut cache: ReclaimCache<u32, Glyph, u64> =
            disposable_ui_cache("test.wired", 1, 64 * 1024, 32, gauge, sink);
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
