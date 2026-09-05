//! [`RangeMap`] — disjoint half-open ranges, each carrying a value.

use core::fmt;
use core::iter::FusedIterator;
use core::ops::Range;

use alloc::collections::{btree_map, BTreeMap};

/// An endpoint the disjoint-range containers can be keyed on.
///
/// Ordering alone decides overlap, adjacency, and splitting, so the
/// arithmetic here is only what *measuring* a range needs: turning a
/// `(base, count)` pair into a range, and reporting how much a range holds.
pub trait RangeKey: Copy + Ord {
    /// The endpoint `count` elements above `self`, or `None` when that leaves
    /// the key's representable range.
    fn advance(self, count: u64) -> Option<Self>;

    /// Elements in `[lower, self)`, saturating at zero.
    fn distance_from(self, lower: Self) -> u64;

    /// The half-open range of `count` elements from `self`, or `None` for a
    /// zero count or one that runs past the key's range.
    ///
    /// The one place a `(base, count)` pair becomes a range, so a caller that
    /// counts pages, blocks, or slots does not repeat the checked arithmetic.
    fn span(self, count: u64) -> Option<Range<Self>> {
        if count == 0 {
            return None;
        }
        Some(self..self.advance(count)?)
    }
}

impl RangeKey for u64 {
    fn advance(self, count: u64) -> Option<Self> {
        self.checked_add(count)
    }

    fn distance_from(self, lower: Self) -> u64 {
        self.saturating_sub(lower)
    }
}

impl RangeKey for usize {
    fn advance(self, count: u64) -> Option<Self> {
        self.checked_add(usize::try_from(count).ok()?)
    }

    fn distance_from(self, lower: Self) -> u64 {
        u64::try_from(self.saturating_sub(lower)).unwrap_or(u64::MAX)
    }
}

/// Why a [`RangeMap`] refused an insertion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RangeError {
    /// The range covers nothing (`start >= end`).
    Empty,
    /// The range intersects one the map already holds. An entry here is an
    /// identity — a reservation, a mapping — so an overlapping insertion is
    /// refused rather than replacing or splitting the holder.
    Overlap,
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the range covers nothing",
            Self::Overlap => "the range overlaps one already held",
        })
    }
}

/// One held range's exclusive end and its value. The start is the key.
struct Entry<K, V> {
    end: K,
    value: V,
}

/// A map from disjoint half-open `[start, end)` ranges to values.
///
/// Every held range is non-empty and intersects no other, so a point lies in
/// at most one — which is what makes [`Self::covering`] an answer rather than
/// a choice. **Adjacent ranges stay distinct**: a value-carrying range is an
/// identity, and a reservation that happens to abut its neighbour is still its
/// own. The set that canonicalises instead — absorbing everything it touches —
/// is [`RangeSet`](crate::RangeSet).
///
/// Lookup, iteration, and gap search allocate nothing. Insertion inherits
/// `alloc`'s `BTreeMap` allocation behaviour, which cannot be made fallible
/// from outside `alloc`.
pub struct RangeMap<K, V> {
    entries: BTreeMap<K, Entry<K, V>>,
}

impl<K, V> Default for RangeMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> RangeMap<K, V> {
    /// An empty map. Allocates nothing until the first insertion.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Ranges held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the map holds no range.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every range.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl<K: RangeKey, V> RangeMap<K, V> {
    /// Record `range` carrying `value`.
    ///
    /// # Errors
    ///
    /// [`RangeError::Empty`] for a range covering nothing, or
    /// [`RangeError::Overlap`] when it intersects one already held — in which
    /// case the map is untouched and `value` is dropped.
    pub fn insert(&mut self, range: Range<K>, value: V) -> Result<(), RangeError> {
        if range.start >= range.end {
            return Err(RangeError::Empty);
        }
        if self.overlapping(range.clone()).next().is_some() {
            return Err(RangeError::Overlap);
        }
        self.entries.insert(
            range.start,
            Entry {
                end: range.end,
                value,
            },
        );
        Ok(())
    }

    /// Remove the range beginning exactly at `start`, returning it and its
    /// value.
    pub fn remove(&mut self, start: K) -> Option<(Range<K>, V)> {
        let entry = self.entries.remove(&start)?;
        Some((start..entry.end, entry.value))
    }

    /// The range beginning exactly at `start`, and its value.
    #[must_use]
    pub fn get(&self, start: K) -> Option<(Range<K>, &V)> {
        let entry = self.entries.get(&start)?;
        Some((start..entry.end, &entry.value))
    }

    /// The range containing `point`, and its value.
    #[must_use]
    pub fn covering(&self, point: K) -> Option<(Range<K>, &V)> {
        self.entry_at_or_below(point)
            .filter(|(range, _)| point < range.end)
    }

    /// The highest held range that *ends* at or below `point`.
    ///
    /// Disjointness bounds the search to two entries: the highest range
    /// starting at or below `point` either ends there too, or covers `point`,
    /// in which case its predecessor cannot reach past its start.
    #[must_use]
    pub fn ending_at_or_below(&self, point: K) -> Option<Range<K>> {
        self.entries
            .range(..=point)
            .rev()
            .map(|(&start, entry)| start..entry.end)
            .find(|range| range.end <= point)
    }

    /// Every held range intersecting `query`, in ascending order, each with
    /// its value. An empty `query` intersects nothing.
    pub fn overlapping(&self, query: Range<K>) -> impl Iterator<Item = (Range<K>, &V)> + '_ {
        let live = query.start < query.end;
        // At most one held range can start below the query and still reach
        // into it; the rest begin inside it.
        let straddling = live
            .then(|| {
                self.entry_at_or_below(query.start)
                    .filter(|(range, _)| range.start < query.start && range.end > query.start)
            })
            .flatten();
        let inside = live
            .then(|| self.entries.range(query.start..query.end))
            .into_iter()
            .flatten()
            .map(entry_pair);
        straddling.into_iter().chain(inside)
    }

    /// Place `count` elements first-fit inside `within`, carrying `value`,
    /// and return the range placed — or `None` when no gap in `within` holds
    /// that many.
    ///
    /// This is the allocation half of the container: a window hands out
    /// ranges and takes them back, and the gaps between what it has handed
    /// out *are* its free space, so it keeps no second free-list to drift out
    /// of step with the first. A refusal leaves the map untouched and drops
    /// `value`, as a refused [`insert`](Self::insert) does.
    pub fn place(&mut self, within: Range<K>, count: u64, value: V) -> Option<Range<K>> {
        let start = self.first_free(within, count)?;
        // The gap search proved the range non-empty, inside the key's own
        // span, and disjoint from every held one, so the entry goes in
        // without a second overlap probe that could only agree.
        let range = start..start.advance(count)?;
        self.entries.insert(
            range.start,
            Entry {
                end: range.end,
                value,
            },
        );
        Some(range)
    }

    /// The lowest start inside `within` where `count` free elements fit,
    /// first-fit over the gaps between held ranges.
    ///
    /// Costs one pass over the ranges intersecting `within`, so a window's
    /// placement scales with what it has handed out rather than with how
    /// large the window is.
    fn first_free(&self, within: Range<K>, count: u64) -> Option<K> {
        if count == 0 || within.start >= within.end {
            return None;
        }
        let mut cursor = within.start;
        for (held, _) in self.overlapping(within.clone()) {
            if held.start.distance_from(cursor) >= count {
                return Some(cursor);
            }
            cursor = cursor.max(held.end);
        }
        (within.end.distance_from(cursor) >= count).then_some(cursor)
    }

    /// Every held range and its value, in ascending order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            inner: self.entries.iter(),
        }
    }

    /// The highest held range starting at or below `point`, and its value.
    /// It may or may not cover `point`; [`Self::covering`] is the filtered
    /// form, and the coalescing set needs the unfiltered one.
    pub(crate) fn entry_at_or_below(&self, point: K) -> Option<(Range<K>, &V)> {
        self.entries.range(..=point).next_back().map(entry_pair)
    }
}

fn entry_pair<'a, K: RangeKey, V>((start, entry): (&K, &'a Entry<K, V>)) -> (Range<K>, &'a V) {
    (*start..entry.end, &entry.value)
}

/// Ascending iterator over a [`RangeMap`]'s ranges and values.
pub struct Iter<'a, K, V> {
    inner: btree_map::Iter<'a, K, Entry<K, V>>,
}

impl<'a, K: RangeKey, V> Iterator for Iter<'a, K, V> {
    type Item = (Range<K>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(entry_pair)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K: RangeKey, V> DoubleEndedIterator for Iter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(entry_pair)
    }
}

impl<K: RangeKey, V> ExactSizeIterator for Iter<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K: RangeKey, V> FusedIterator for Iter<'_, K, V> {}

impl<'a, K: RangeKey, V> IntoIterator for &'a RangeMap<K, V> {
    type Item = (Range<K>, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
#[path = "range_tests.rs"]
mod tests;
