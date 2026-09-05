//! [`RangeSet`] — disjoint ranges kept maximally coalesced.

use core::iter::FusedIterator;
use core::ops::Range;

use crate::range::{self, RangeKey, RangeMap};

/// A set of half-open `[start, end)` ranges, held disjoint **and never
/// adjacent**.
///
/// Insertion absorbs every range it overlaps or touches and removal splits the
/// ranges it cuts, so the representation is canonical: two sets holding the
/// same elements hold the same ranges whatever order they were built in. That
/// makes the range count a property of what the set covers rather than of the
/// order a caller happened to build it in — a set of block runs costs one
/// entry per contiguous extent, not one per block.
///
/// It is [`RangeMap`] with a zero-sized value and coalescing insertion, so it
/// inherits the disjointness invariant and the neighbour probes rather than
/// repeating them. Where a range is an *identity* whose neighbour must stay a
/// separate entry — a reservation, a mapping — the map is the right container.
pub struct RangeSet<K> {
    spans: RangeMap<K, ()>,
    /// Elements held across every range, kept in step with `spans` so a
    /// caller's accounting reads it rather than summing.
    covered: u64,
}

impl<K> Default for RangeSet<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> RangeSet<K> {
    /// An empty set. Allocates nothing until the first insertion.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            spans: RangeMap::new(),
            covered: 0,
        }
    }

    /// Ranges held. This is the set's memory cost; [`Self::covered`] is what
    /// it accounts for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// `true` when the set holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Elements held, across every range.
    #[must_use]
    pub const fn covered(&self) -> u64 {
        self.covered
    }

    /// Drop every range.
    pub fn clear(&mut self) {
        self.spans.clear();
        self.covered = 0;
    }
}

impl<K: RangeKey> RangeSet<K> {
    /// Add `range`, absorbing every held range it overlaps or touches.
    ///
    /// A range covering nothing adds nothing.
    pub fn insert(&mut self, range: Range<K>) {
        if range.start >= range.end {
            return;
        }
        let mut union = range;
        // A range touching or overlapping the union's start extends it
        // downward. One probe suffices, and that rests on the set's own
        // invariant rather than on disjointness alone: were two held ranges
        // ever adjacent, absorbing the lower one would expose a second
        // touching predecessor this never looks for.
        if let Some(below) = self
            .spans
            .entry_at_or_below(union.start)
            .map(|(held, ())| held)
            .filter(|held| held.end >= union.start)
        {
            union.start = below.start;
            union.end = union.end.max(below.end);
            self.take(below.start);
        }
        // Absorb upward: a range that overlaps the union, or begins exactly
        // where it ends, may in turn bring the next within reach. Each pass
        // removes an entry, so this ends.
        loop {
            let above = self
                .spans
                .overlapping(union.clone())
                .next()
                .or_else(|| self.spans.get(union.end))
                .map(|(held, ())| held);
            let Some(above) = above else { break };
            union.end = union.end.max(above.end);
            self.take(above.start);
        }
        self.put(union);
    }

    /// Drop `range`, splitting the held ranges it cuts.
    ///
    /// A range covering nothing drops nothing.
    pub fn remove(&mut self, range: Range<K>) {
        if range.start >= range.end {
            return;
        }
        // Each pass takes one intersecting range and puts back only the parts
        // outside the cut, which intersect the cut no longer.
        loop {
            let cut = self
                .spans
                .overlapping(range.clone())
                .next()
                .map(|(held, ())| held);
            let Some(cut) = cut else { break };
            self.take(cut.start);
            self.put(cut.start..range.start);
            self.put(range.end..cut.end);
        }
    }

    /// Whether `point` lies in a held range.
    #[must_use]
    pub fn contains(&self, point: K) -> bool {
        self.covering(point).is_some()
    }

    /// The held range containing `point`.
    #[must_use]
    pub fn covering(&self, point: K) -> Option<Range<K>> {
        self.spans.covering(point).map(|(held, ())| held)
    }

    /// The lowest part of `query` the set holds.
    ///
    /// Walking a query by alternating this with [`Self::first_gap`] is how a
    /// caller splits one range into the parts two different sets own, without
    /// visiting an element.
    #[must_use]
    pub fn first_overlap(&self, query: Range<K>) -> Option<Range<K>> {
        let held = self.spans.overlapping(query.clone()).next()?.0;
        Some(held.start.max(query.start)..held.end.min(query.end))
    }

    /// The lowest part of `query` the set does **not** hold.
    #[must_use]
    pub fn first_gap(&self, query: Range<K>) -> Option<Range<K>> {
        if query.start >= query.end {
            return None;
        }
        let start = self
            .covering(query.start)
            .map_or(query.start, |held| held.end);
        if start >= query.end {
            return None;
        }
        let end = self
            .spans
            .overlapping(start..query.end)
            .next()
            .map_or(query.end, |(held, ())| held.start);
        Some(start..end)
    }

    /// Remove and return the lowest held range.
    pub fn pop_first(&mut self) -> Option<Range<K>> {
        let first = self.spans.iter().next()?.0;
        self.take(first.start)
    }

    /// Every held range, in ascending order.
    #[must_use]
    pub fn iter(&self) -> Ranges<'_, K> {
        Ranges {
            inner: self.spans.iter(),
        }
    }

    /// Record `range` verbatim, charging what it covers. Only ever called
    /// with a range the caller has just proved disjoint from every other, so
    /// a refusal charges nothing and the accounting stays exact.
    fn put(&mut self, range: Range<K>) {
        let covered = range.end.distance_from(range.start);
        if self.spans.insert(range, ()).is_ok() {
            self.covered = self.covered.saturating_add(covered);
        }
    }

    /// Remove the range beginning at `start`, discharging what it covered.
    fn take(&mut self, start: K) -> Option<Range<K>> {
        let (range, ()) = self.spans.remove(start)?;
        self.covered = self
            .covered
            .saturating_sub(range.end.distance_from(range.start));
        Some(range)
    }
}

/// Ascending iterator over a [`RangeSet`]'s ranges.
pub struct Ranges<'a, K> {
    inner: range::Iter<'a, K, ()>,
}

impl<K: RangeKey> Iterator for Ranges<'_, K> {
    type Item = Range<K>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(held, ())| held)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K: RangeKey> DoubleEndedIterator for Ranges<'_, K> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|(held, ())| held)
    }
}

impl<K: RangeKey> ExactSizeIterator for Ranges<'_, K> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K: RangeKey> FusedIterator for Ranges<'_, K> {}

impl<'a, K: RangeKey> IntoIterator for &'a RangeSet<K> {
    type Item = Range<K>;
    type IntoIter = Ranges<'a, K>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
#[path = "rangeset_tests.rs"]
mod tests;
