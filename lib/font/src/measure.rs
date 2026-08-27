//! The text-measurement memo: one string's cumulative per-`char` advances,
//! retained beside the glyph bitmaps they were derived from.
//!
//! A proportional family has no cell width to multiply by, so measuring a
//! label costs one advance lookup per character — work a repaint redid every
//! frame for text that never changed. [`measure`] walks the string once into a
//! [`MeasuredText`], and width, truncation, and elision are then all queries
//! over that one array: the width is its last entry, and the longest prefix
//! that fits a box is a binary search for the last entry within it.
//!
//! # The key fingerprints the text, the value keeps it
//!
//! The measured bytes live in the value, keyed by their length and CRC-32C.
//! A lookup then builds its key without allocating, and the cache wipes a
//! released value where a dropped key would leave the text — a user's own
//! filenames and window titles — readable in reused heap. A fingerprint clash
//! is caught by comparing the retained bytes, so a colliding string is
//! measured afresh rather than served the wrong width.
//!
//! # The face is in the key, not the epoch
//!
//! An epoch change empties the whole cache, and one frame measures several
//! text roles at several sizes, so an epoch keyed on the face would throw
//! away every other role's measurements at each switch. Family, pixel height,
//! and weight are part of the key instead: a different scale or face is a
//! different entry, so a stale answer cannot be served. The epoch carries what
//! changes the advances of a face already measured — the generation of the
//! advance source itself, moved on whenever a font transport is installed or
//! replaced.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::font_ipc::{FamilyKey, FontWeight, FONT_FAMILY_KEY_LEN};
#[cfg(any(feature = "rt", test))]
use tairix_reclaim::{
    CacheBudget, CacheCandidate, InvalidationSource, RebuildCost, ReclaimClass, ReclaimOwner,
    ReclaimRule, Sensitivity,
};
use tairix_reclaim::{CachedBytes, ReclaimCache};

#[cfg(any(feature = "rt", test))]
use crate::glyph_cache::{glyph_cache_ceiling, GLYPH_CACHE_ENTRY_METADATA_BYTES};

/// What one measurement is retained under: the face's wire bytes, its pixel
/// height, its wire weight, and the measured text's byte length and CRC-32C.
pub(crate) type MeasureKey = ([u8; FONT_FAMILY_KEY_LEN], u32, u16, u32, u32);

/// The advance source's generation, moved on when a transport is installed.
pub(crate) type MeasureEpoch = u64;

/// The client's measurement memo.
pub(crate) type MeasureCache = ReclaimCache<MeasureKey, MeasuredText, MeasureEpoch>;

/// The key `text` measured in `family` at `pixel_height` and `weight` is
/// retained under.
///
/// A string too long to state its length in the key is fingerprinted all the
/// same, so an outsized input is measured correctly rather than refused.
pub(crate) fn measure_key(
    family: FamilyKey,
    pixel_height: u32,
    weight: FontWeight,
    text: &str,
) -> MeasureKey {
    (
        family.to_wire(),
        pixel_height,
        weight.to_wire(),
        u32::try_from(text.len()).unwrap_or(u32::MAX),
        tairix_crc32c::checksum(text.as_bytes()),
    )
}

/// How the memo declares itself to the reclaim model.
///
/// Rebuilding is a per-character walk of the advance source, the measured
/// text is the user's own, the advance source's generation is what
/// invalidates an entry, and the walk itself is the canonical source a
/// dropped entry is rebuilt from. The per-entry allowance is the glyph
/// cache's: a measurement key is the same shape and size class, so one
/// declared bound covers both of the client's caches.
///
/// Declared where a memo can actually be built: a real program's lazy default
/// and a host test that installs its own.
#[cfg(any(feature = "rt", test))]
pub(crate) fn measure_cache_candidate(owner: ReclaimOwner) -> CacheCandidate {
    CacheCandidate {
        class: Some(ReclaimClass::DisposableUi),
        owner: Some(owner),
        rebuild_cost: RebuildCost::Moderate,
        sensitivity: Some(Sensitivity::UserData),
        invalidation: Some(InvalidationSource::GenerationToken),
        rule: Some(ReclaimRule::Drop),
        entry_metadata_bytes: GLYPH_CACHE_ENTRY_METADATA_BYTES,
    }
}

/// The memo's byte budget, on the same RAM-derived ceiling as the glyph
/// cache ([`glyph_cache_ceiling`]) and with no floor of any kind.
///
/// The memo holds no pixels and rebuilds an entry by walking advances it
/// already has, so — unlike a glyph bitmap, which costs a rasterisation or
/// an IPC round trip — there is nothing here worth keeping through pressure:
/// it is speculation all the way down, and the first tightening may take all
/// of it.
#[cfg(any(feature = "rt", test))]
#[must_use]
pub(crate) fn measure_cache_budget(total_ram_bytes: u64) -> CacheBudget {
    CacheBudget::from_ceiling(glyph_cache_ceiling(total_ram_bytes))
}

/// One string measured in one face: the bytes measured, and the pen position
/// after each `char` of them.
#[derive(Debug)]
pub(crate) struct MeasuredText {
    /// The measured bytes, kept so a fingerprint clash is detected instead of
    /// served, and wiped on release because they are the user's own text.
    text: Box<[u8]>,
    /// Running advance after each `char`, saturating and therefore
    /// non-decreasing, which is what makes a fit a binary search.
    advances: Box<[u32]>,
}

impl MeasuredText {
    /// The whole string's advance.
    pub(crate) fn width(&self) -> u32 {
        self.advances.last().copied().unwrap_or(0)
    }

    /// How many leading `char`s fit within `limit`.
    pub(crate) fn chars_within(&self, limit: u32) -> usize {
        self.advances.partition_point(|&advance| advance <= limit)
    }

    /// Whether this is a measurement of `text` rather than of a string that
    /// merely fingerprints alike.
    pub(crate) fn is_of(&self, text: &str) -> bool {
        *self.text == *text.as_bytes()
    }
}

impl CachedBytes for MeasuredText {
    fn payload_bytes(&self) -> usize {
        self.advances
            .len()
            .saturating_mul(size_of::<u32>())
            .saturating_add(self.text.len())
    }

    fn wipe(&mut self) {
        self.text.fill(0);
        self.advances.fill(0);
    }
}

/// Measure `text` by asking `advance` for each `char`'s advance, reporting
/// whether every one of them resolved.
///
/// An unresolved advance contributes nothing, exactly as a single-character
/// measurement of it would, and is reported so the caller can decline to
/// remember a walk the advance source could not complete.
pub(crate) fn measure(
    text: &str,
    mut advance: impl FnMut(char) -> Option<u32>,
) -> (MeasuredText, bool) {
    let mut advances = Vec::with_capacity(text.chars().count());
    let mut resolved = true;
    let mut pen = 0u32;
    for ch in text.chars() {
        match advance(ch) {
            Some(step) => pen = pen.saturating_add(step),
            None => resolved = false,
        }
        advances.push(pen);
    }
    let measured = MeasuredText {
        text: Box::from(text.as_bytes()),
        advances: advances.into_boxed_slice(),
    };
    (measured, resolved)
}
