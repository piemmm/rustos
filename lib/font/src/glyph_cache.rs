//! The one shared cached-glyph declaration used on both sides of the
//! font-service boundary: the render path's client cache ([`crate::client`])
//! and the sandboxed font service's own cache (`fontd`,
//! `userland/system/fontd`).
//!
//! A client-fetched glyph and a service-rasterised glyph are the same kind
//! of memory from the reclaimable-memory model's point of view: the same
//! 8-bit coverage bitmap, cheap to lose (the canonical source — the service
//! or the outline it parsed — rebuilds it), and revealing of a user's own
//! displayed text. Before this module they were declared twice, with the
//! same bounded-entry-count shape and the same hand-picked ceiling on each
//! side of the endpoint; that duplication is exactly what let one of the two
//! copies remain an *entry-count* bound while the other side's caller
//! (a remote, hostile-capable IPC client) could drive it to hundreds of
//! megabytes by varying the requested cell height. [`CachedGlyph`],
//! [`glyph_cache_candidate`], and [`glyph_cache_budget`] are the single
//! definition both sides build their own [`tairix_reclaim::ReclaimCache`]
//! from, so the shape and the budget can never drift apart again.
//!
//! # Why this lives in `lib/font`
//!
//! `fontd` already depends on `lib/font`'s atlas geometry constants, so
//! adding this module here creates no new reverse edge:
//! `userland/system/fontd` gains a second import from a crate it already
//! imports from, while `lib/font` gains nothing from `fontd` at all. The
//! alternative — a third crate holding just this module — would be a
//! one-line wrapper crate for two consumers that already share a home.
//!
//! # The budget fraction
//!
//! [`glyph_cache_budget`] derives its ceiling from the machine's total RAM
//! rather than a hand-picked constant, so a small board and a large server
//! each get a cache proportioned to what they actually have. The chosen
//! fraction, 1/4096th, is deliberately far smaller than the 1/16th a
//! kernel-heap-backed cache takes: a font glyph's working set is a few
//! hundred distinct `(scalar, height, weight)` combinations at any one
//! time — the visible character repertoire at the handful of sizes and
//! weights a session actually draws — each at most
//! `FONT_MAX_COVERAGE_LEN` (the widest, tallest permitted bitmap) bytes and
//! typically a few hundred. A 1 GiB machine is left 256 KiB, already generous for that
//! working set; a 64 GiB server is left 16 MiB, still a vanishing fraction
//! of its RAM. Zero total RAM (the query unanswered or the service absent)
//! yields a zero budget, which admits nothing: every glyph is then served
//! freshly built and never retained — correct, merely uncached, and never a
//! fallback to an unbounded or hand-picked cache.

use alloc::boxed::Box;

use tairix_reclaim::{
    CacheBudget, CacheCandidate, CachedBytes, InvalidationSource, RebuildCost, ReclaimClass,
    ReclaimOwner, ReclaimRule, Sensitivity, UI_CACHE_RESERVE_BYTES,
};

/// One rasterised glyph bitmap as either side of the font-service boundary
/// retains it: the geometry the service reported, its pen advance and left
/// side bearing, and the owned row-major 8-bit coverage payload.
///
/// A proportional family reports a different advance and bearing per glyph,
/// so both travel with the bitmap rather than being re-derived from a
/// monospace cell width the way the old monospace-only client did.
#[derive(Debug, Eq, PartialEq)]
pub struct CachedGlyph {
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// The pen advance for this glyph in pixels. Zero for a combining mark
    /// that occupies no space of its own.
    pub advance: u32,
    /// The bitmap's left edge relative to the pen, in pixels. Negative when
    /// the outline reaches back over the preceding glyph.
    pub left: i32,
    /// Row-major 8-bit coverage, exactly `width * height` bytes.
    pub data: Box<[u8]>,
}

impl CachedGlyph {
    /// Build a cached glyph from its rasterised geometry, advance, bearing,
    /// and coverage.
    #[must_use]
    pub const fn new(width: u32, height: u32, advance: u32, left: i32, data: Box<[u8]>) -> Self {
        Self {
            width,
            height,
            advance,
            left,
            data,
        }
    }
}

impl CachedBytes for CachedGlyph {
    fn payload_bytes(&self) -> usize {
        self.data.len()
    }

    fn wipe(&mut self) {
        // The set of cached glyphs reveals which characters a user has had
        // displayed, so a released entry is scrubbed like any other
        // user-data cache rather than left readable in reused heap.
        self.data.fill(0);
    }
}

/// The per-entry bookkeeping bytes a glyph cache declares to the
/// classification gate: the widest cache key in use (the render-path
/// client's `(scalar, family, pixel_height, weight)`, a 16-byte family key
/// plus three `u32`/`u16` fields) plus the fixed overhead of one `BTreeMap`
/// entry and one recency-index slot, with headroom.
pub const GLYPH_CACHE_ENTRY_METADATA_BYTES: usize = 80;

/// The classification every glyph cache declares: a rasterised glyph is
/// expensive to reproduce (an outline rasterisation pass, or — from the
/// client's side of the endpoint — the IPC round trip that triggers one),
/// reveals which characters its owner has displayed, is invalidated only by
/// its owner's own teardown (the service parses its face set once at
/// startup and never reloads it, so no in-life generation ever changes),
/// and is simply dropped on reclaim — the canonical face outline rebuilds
/// it.
#[must_use]
pub const fn glyph_cache_candidate(owner: ReclaimOwner) -> CacheCandidate {
    CacheCandidate {
        class: Some(ReclaimClass::DisposableUi),
        owner: Some(owner),
        rebuild_cost: RebuildCost::Expensive,
        sensitivity: Some(Sensitivity::UserData),
        invalidation: Some(InvalidationSource::OwnerTeardown),
        rule: Some(ReclaimRule::Drop),
        entry_metadata_bytes: GLYPH_CACHE_ENTRY_METADATA_BYTES,
    }
}

/// The fraction of total machine RAM a glyph cache may occupy: see the
/// [module docs](self) for why this is far smaller than a kernel-backed
/// cache's usual share.
const GLYPH_CACHE_RAM_DIVISOR: u64 = 4096;

/// Derive a glyph cache's byte budget from the machine's total usable
/// physical RAM (`tairix_procinfo::memory_total_bytes`), never a
/// hand-picked constant.
///
/// The shared irreducible reserve is declared on it, clamped by that
/// ceiling: every glyph on screen is redrawn from this cache on every
/// repaint, so a side that empties it re-rasterises — or re-fetches over
/// the endpoint — the whole visible text per frame while the machine is
/// short.
///
/// `total_ram_bytes` of `0` (the query unanswered, refused, or the service
/// unreachable) yields a zero budget, which admits nothing: fail closed to
/// uncached, not to an unbounded or hand-picked ceiling.
#[must_use]
pub fn glyph_cache_budget(total_ram_bytes: u64) -> CacheBudget {
    CacheBudget::from_ceiling(glyph_cache_ceiling(total_ram_bytes))
        .with_reserved_floor(UI_CACHE_RESERVE_BYTES)
}

/// The ceiling both font caches derive from the machine's total usable
/// physical RAM: see the [module docs](self) for the fraction.
///
/// Crate-internal: the two budgets built on it are what a consumer names.
#[must_use]
pub(crate) fn glyph_cache_ceiling(total_ram_bytes: u64) -> usize {
    let hard = total_ram_bytes / GLYPH_CACHE_RAM_DIVISOR;
    // `usize` is narrower than `u64` only on `wasm32`, where a budget this
    // small never approaches `u32::MAX`; saturating keeps the conversion
    // total rather than refusing to build a budget at all.
    usize::try_from(hard).unwrap_or(usize::MAX)
}

/// The **client** side's budget: [`glyph_cache_budget`] with the whole of it
/// declared the process's live working set
/// ([`CacheBudget::with_working_set_floor`]).
///
/// The two sides of the endpoint retain the same bitmaps but rebuild them
/// very differently. The service re-rasterises an outline it already holds in
/// memory; a client has to ask for the glyph again over the font endpoint. So
/// a client that gives its glyphs up at the first tightening of memory
/// re-fetches every character on screen, over IPC, while the machine is short
/// — which frees a fraction of a screenful and costs a round trip per glyph
/// per repaint. Mild and moderate pressure therefore leave the client's
/// glyphs alone; severe takes the speculation down to the reserve
/// [`glyph_cache_budget`] already declares, because at that point the memory
/// genuinely matters more than the round trips.
#[must_use]
pub fn client_glyph_cache_budget(total_ram_bytes: u64) -> CacheBudget {
    let budget = glyph_cache_budget(total_ram_bytes);
    budget.with_working_set_floor(budget.hard())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_candidate_classifies_without_refusal() {
        let owner = ReclaimOwner::UserlandProcess("test.font");
        let policy = glyph_cache_candidate(owner).classify().expect("admissible");
        assert_eq!(policy.class(), ReclaimClass::DisposableUi);
        assert_eq!(policy.owner(), owner);
        assert_eq!(policy.sensitivity(), Sensitivity::UserData);
        assert_eq!(policy.invalidation(), InvalidationSource::OwnerTeardown);
        assert_eq!(policy.rule(), ReclaimRule::Drop);
    }

    #[test]
    fn a_zero_ram_reading_yields_a_zero_budget() {
        let budget = glyph_cache_budget(0);
        assert_eq!(budget.hard(), 0);
        assert_eq!(budget.low(), 0);
        // Nothing to reserve either: the reserve is clamped by the ceiling,
        // so a machine whose RAM went unanswered caches nothing at all.
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn the_budget_scales_with_discovered_ram() {
        let small = glyph_cache_budget(1 << 30); // 1 GiB
        let large = glyph_cache_budget(64 << 30); // 64 GiB
        assert!(small.hard() > 0);
        assert!(large.hard() > small.hard());
        // A glyph working set is small: even a large server is left a
        // vanishing fraction of its RAM, never a large slice.
        assert!(large.hard() < (1 << 30));
    }

    #[test]
    fn the_client_budget_keeps_its_glyphs_until_severe_pressure() {
        use tairix_reclaim::{shrink_target, PressureBand, UI_CACHE_RESERVE_BYTES};

        // The client rebuilds a dropped glyph by asking the font service for
        // it again, so giving them up at the first tightening re-fetches every
        // character on screen over IPC while the machine is short. The service,
        // which re-rasterises an outline it already holds, keeps only the
        // shared reserve and gives the speculation up at mild as the reclaim
        // order says. A big enough machine for the ceiling to exceed the
        // reserve, so the two sides are genuinely distinguishable.
        let ram = 64u64 << 30;
        let client = client_glyph_cache_budget(ram);
        let service = glyph_cache_budget(ram);
        assert_eq!(client.hard(), service.hard(), "same ceiling");
        assert!(client.hard() > UI_CACHE_RESERVE_BYTES);
        assert_eq!(
            client.working_set(),
            client.hard(),
            "all of it is working set"
        );
        assert_eq!(service.working_set(), UI_CACHE_RESERVE_BYTES);
        assert_eq!(service.reserved(), UI_CACHE_RESERVE_BYTES);
        for band in [PressureBand::Mild, PressureBand::Moderate] {
            assert_eq!(
                shrink_target(band, ReclaimClass::DisposableUi, client),
                client.hard(),
                "{band:?} keeps the client's glyphs"
            );
            assert_eq!(
                shrink_target(band, ReclaimClass::DisposableUi, service),
                UI_CACHE_RESERVE_BYTES,
                "{band:?} takes the service's atlas down to its reserve"
            );
        }
        for band in [PressureBand::Severe, PressureBand::Critical] {
            assert_eq!(
                shrink_target(band, ReclaimClass::DisposableUi, client),
                UI_CACHE_RESERVE_BYTES,
                "{band:?} still leaves a screenful of text drawable"
            );
        }
        // A machine whose RAM total went unanswered gets no floor to keep,
        // because it gets no budget: fail closed to uncached.
        assert_eq!(client_glyph_cache_budget(0).working_set(), 0);
        assert_eq!(client_glyph_cache_budget(0).reserved(), 0);
    }

    #[test]
    fn a_small_machines_whole_glyph_ceiling_is_reserved() {
        use tairix_reclaim::{shrink_target, PressureBand, UI_CACHE_RESERVE_BYTES};

        // The reserve is clamped by the derived ceiling, never added to it: a
        // 1 GiB machine's glyph ceiling is well under a mebibyte, so all of it
        // is irreducible and no band empties it. That is the right answer —
        // there is nothing in a cache that small worth reclaiming, and
        // re-fetching a screenful of text costs a round trip per glyph.
        let budget = glyph_cache_budget(1 << 30);
        assert!(budget.hard() > 0 && budget.hard() < UI_CACHE_RESERVE_BYTES);
        assert_eq!(budget.reserved(), budget.hard());
        for band in PressureBand::ALL {
            assert_eq!(
                shrink_target(band, ReclaimClass::DisposableUi, budget),
                budget.hard(),
                "{band:?} keeps a ceiling that is already below the reserve"
            );
        }
    }

    #[test]
    fn a_released_glyph_is_wiped_because_it_is_declared_user_data() {
        // Two halves make the guarantee: the cache overwrites a released
        // entry for every sensitivity but `Public`, and this value's wipe
        // really does clear the coverage it retained.
        let sensitivity = glyph_cache_candidate(ReclaimOwner::UserlandProcess("test.font"))
            .classify()
            .expect("admissible")
            .sensitivity();
        assert_ne!(sensitivity, Sensitivity::Public);

        let mut glyph = CachedGlyph::new(4, 4, 4, 0, vec![0xFF; 16].into_boxed_slice());
        assert_eq!(glyph.payload_bytes(), 16);
        glyph.wipe();
        assert!(glyph.data.iter().all(|&b| b == 0));
    }
}
