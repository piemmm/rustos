//! The client's cache of fetched glyph **outlines**, declared beside the
//! coverage cache it sits next to.
//!
//! A vector consumer asks the font service for geometry rather than pixels
//! (`plans/SVG.md` S23), and a session opening one drawing after another
//! asks for the same Latin alphabet each time. The retained geometry is the
//! same kind of memory as a retained bitmap — cheap to lose, expensive to
//! re-fetch, and revealing of which characters a user has had displayed — so
//! it is classified and budgeted exactly as [`crate::glyph_cache`] is,
//! rather than growing a second, unbounded memo beside it.

use alloc::vec::Vec;

use tairix_abi::font_ipc::{FamilyKey, FontStretch, FontStyle, FontWeight, FONT_FAMILY_KEY_LEN};
use tairix_hash::BuildSipHash13;
use tairix_reclaim::{
    CacheBudget, CacheCandidate, CachedBytes, InvalidationSource, RebuildCost, ReclaimCache,
    ReclaimClass, ReclaimOwner, ReclaimRule, Sensitivity, UI_CACHE_RESERVE_BYTES,
};

use crate::client::{OwnedContour, OwnedOutline, OwnedSegment};

/// Everything a retained outline depends on: which face answered, and which
/// scalar of it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutlineKey {
    /// The family asked for, as its wire key.
    pub family: [u8; FONT_FAMILY_KEY_LEN],
    /// The scalar.
    pub scalar: char,
    /// The `wght` coordinate asked for.
    pub weight: u16,
    /// The posture asked for, as its wire discriminant.
    pub style: u16,
    /// The `wdth` coordinate asked for.
    pub stretch: u16,
}

impl OutlineKey {
    /// The key one `(family, scalar, instance)` request files under.
    #[must_use]
    pub fn new(
        family: FamilyKey,
        scalar: char,
        weight: FontWeight,
        style: FontStyle,
        stretch: FontStretch,
    ) -> Self {
        Self {
            family: family.to_wire(),
            scalar,
            weight: weight.to_wire(),
            style: style.to_wire(),
            stretch: stretch.to_wire(),
        }
    }
}

/// One retained outline: the glyph as the service answered it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CachedOutline(pub OwnedOutline);

impl CachedBytes for CachedOutline {
    fn payload_bytes(&self) -> usize {
        self.0
            .contours
            .iter()
            .map(|contour| {
                core::mem::size_of::<OwnedContour>()
                    + contour.segments.len() * core::mem::size_of::<OwnedSegment>()
            })
            .sum()
    }

    fn wipe(&mut self) {
        // Which glyph shapes were retained reveals which characters a user
        // has had displayed, exactly as the coverage cache's bitmaps do, so a
        // released entry is scrubbed rather than left readable in reused heap.
        for contour in &mut self.0.contours {
            contour.segments.clear();
            contour.start = (0.0, 0.0);
        }
        self.0.contours = Vec::new();
    }
}

/// The client's outline cache: the shared bounded, classified,
/// pressure-governed cache holding one [`CachedOutline`] per
/// [`OutlineKey`].
///
/// The generation token is `()`: nothing invalidates a glyph's geometry
/// while the service lives, since a face's bytes never change once read.
pub type OutlineCache = ReclaimCache<OutlineKey, CachedOutline, (), BuildSipHash13>;

/// The per-entry bookkeeping bytes an outline cache declares: its key plus
/// one map entry and one recency slot, with headroom.
pub const OUTLINE_CACHE_ENTRY_METADATA_BYTES: usize = 80;

/// The classification an outline cache declares.
///
/// The same judgement the coverage cache makes, for the same reasons: the
/// canonical face rebuilds it, the round trip that does so is expensive, and
/// the set of retained glyphs is user data.
#[must_use]
pub const fn outline_cache_candidate(owner: ReclaimOwner) -> CacheCandidate {
    CacheCandidate {
        class: Some(ReclaimClass::DisposableUi),
        owner: Some(owner),
        rebuild_cost: RebuildCost::Expensive,
        sensitivity: Some(Sensitivity::UserData),
        invalidation: Some(InvalidationSource::OwnerTeardown),
        rule: Some(ReclaimRule::Drop),
        entry_metadata_bytes: OUTLINE_CACHE_ENTRY_METADATA_BYTES,
    }
}

/// Derive an outline cache's byte budget from the machine's total usable
/// physical RAM, never a hand-picked constant.
///
/// It shares the coverage cache's ceiling: the two retain the same
/// repertoire in two forms, and a machine that can afford one can afford the
/// other. Zero total RAM yields a zero budget, which admits nothing — every
/// run is then fetched afresh, correct and merely slower.
#[must_use]
pub fn outline_cache_budget(total_ram_bytes: u64) -> CacheBudget {
    CacheBudget::from_ceiling(crate::glyph_cache::glyph_cache_ceiling(total_ram_bytes))
        .with_reserved_floor(UI_CACHE_RESERVE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_candidate_classifies_without_refusal() {
        let owner = ReclaimOwner::UserlandProcess("test.font");
        let policy = outline_cache_candidate(owner)
            .classify()
            .expect("admissible");
        assert_eq!(policy.class(), ReclaimClass::DisposableUi);
        assert_eq!(policy.owner(), owner);
    }

    #[test]
    fn the_payload_counts_every_segment_it_holds() {
        let entry = CachedOutline(OwnedOutline {
            contours: vec![OwnedContour {
                start: (0.0, 0.0),
                segments: vec![OwnedSegment::Line { to: (1.0, 1.0) }; 4],
            }],
            ..OwnedOutline::default()
        });
        assert!(entry.payload_bytes() >= 4 * core::mem::size_of::<OwnedSegment>());
    }

    #[test]
    fn wiping_releases_every_retained_contour() {
        let mut entry = CachedOutline(OwnedOutline {
            contours: vec![OwnedContour {
                start: (3.0, 4.0),
                segments: vec![OwnedSegment::Line { to: (1.0, 1.0) }],
            }],
            ..OwnedOutline::default()
        });
        entry.wipe();
        assert_eq!(entry.payload_bytes(), 0);
        assert!(entry.0.contours.is_empty());
    }

    #[test]
    fn a_machine_with_no_reported_ram_admits_nothing() {
        assert_eq!(outline_cache_budget(0).hard(), 0);
    }
}
