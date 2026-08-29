//! Block-run bookkeeping: an ordered set of physical block runs.
//!
//! Every set a transaction keeps about blocks — the ones it allocated, the
//! ones it released, the map marks it could not apply yet, the runs waiting
//! for a device discard — is a set of *runs*, never a set of blocks. A
//! transaction that releases a whole file releases its extents, and an extent
//! is contiguous by construction, so the bookkeeping costs one entry per
//! extent rather than one per block: deleting a hundred-terabyte file is a
//! handful of entries where a per-block set asked for one per four kilobytes
//! of it.
//!
//! Runs are held maximally coalesced — disjoint and never adjacent — so the
//! entry count is a property of the volume's layout and not of the order the
//! caller happened to free things in.

use alloc::collections::BTreeMap;

use tairix_reclaim::MAP_ENTRY_OVERHEAD;

/// Bytes one run costs: its start and its length, plus the per-entry map
/// bookkeeping every bounded pool in the tree charges on top of a payload.
///
/// This is what makes a set's `run_count` a *byte* figure the write-back
/// ceiling can be compared against, so a transaction that dirties almost
/// nothing while releasing millions of runs still meets the same bound as one
/// that stages blocks.
const RUN_ENTRY_BYTES: usize = 2 * size_of::<u64>() + MAP_ENTRY_OVERHEAD;

/// An ordered set of `[start, start + len)` block runs, kept disjoint and
/// non-adjacent.
///
/// Insertion absorbs every run it overlaps or touches and removal splits the
/// runs it cuts, so the representation is canonical: two sets holding the same
/// blocks hold the same entries whatever order they were built in.
#[derive(Default)]
pub(crate) struct RunSet {
    /// `start -> len`, with every run non-empty, disjoint, and separated from
    /// its neighbours by at least one block.
    runs: BTreeMap<u64, u64>,
    /// Blocks the set holds, kept in step with `runs` so the commit's free
    /// accounting reads it rather than summing.
    blocks: u64,
}

impl RunSet {
    /// An empty set. Allocates nothing until the first run is inserted.
    pub(crate) const fn new() -> Self {
        Self {
            runs: BTreeMap::new(),
            blocks: 0,
        }
    }

    /// Whether the set holds nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Runs the set holds. This is the set's memory cost; [`Self::blocks`] is
    /// the space it accounts for.
    pub(crate) fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// Blocks the set holds, across every run.
    pub(crate) fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Bytes the set occupies.
    pub(crate) fn bytes(&self) -> usize {
        self.runs.len().saturating_mul(RUN_ENTRY_BYTES)
    }

    /// Whether `block` lies in one of the runs.
    pub(crate) fn contains(&self, block: u64) -> bool {
        self.covering(block).is_some()
    }

    /// The run containing `block`, as `(start, len)`.
    fn covering(&self, block: u64) -> Option<(u64, u64)> {
        self.runs
            .range(..=block)
            .next_back()
            .map(|(&start, &len)| (start, len))
            .filter(|&(start, len)| block - start < len)
    }

    /// Every run the set holds, in ascending address order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.runs.iter().map(|(&start, &len)| (start, len))
    }

    /// Remove and return the lowest run.
    pub(crate) fn pop_first(&mut self) -> Option<(u64, u64)> {
        let (start, len) = self.runs.pop_first()?;
        self.blocks = self.blocks.saturating_sub(len);
        Some((start, len))
    }

    /// Drop every run.
    pub(crate) fn clear(&mut self) {
        self.runs.clear();
        self.blocks = 0;
    }

    /// Add `start..start + len`, absorbing every run it overlaps or touches.
    pub(crate) fn insert(&mut self, start: u64, len: u64) {
        let Some(mut end) = Self::span(start, len) else {
            return;
        };
        let mut start = start;
        if let Some((prev_start, prev_len)) = self
            .runs
            .range(..=start)
            .next_back()
            .map(|(&s, &l)| (s, l))
            .filter(|&(s, l)| s.saturating_add(l) >= start)
        {
            start = prev_start;
            end = end.max(prev_start.saturating_add(prev_len));
            self.take(prev_start);
        }
        // A touching successor extends the run, which may bring the next one
        // within reach in turn; each pass removes an entry, so this ends.
        while let Some((next_start, next_len)) =
            self.runs.range(start..=end).next().map(|(&s, &l)| (s, l))
        {
            end = end.max(next_start.saturating_add(next_len));
            self.take(next_start);
        }
        self.put(start, end - start);
    }

    /// Drop `start..start + len`, splitting the runs it cuts.
    pub(crate) fn remove(&mut self, start: u64, len: u64) {
        let Some(end) = Self::span(start, len) else {
            return;
        };
        if let Some((prev_start, prev_len)) = self
            .runs
            .range(..start)
            .next_back()
            .map(|(&s, &l)| (s, l))
            .filter(|&(s, l)| s.saturating_add(l) > start)
        {
            let prev_end = prev_start.saturating_add(prev_len);
            self.take(prev_start);
            self.put(prev_start, start - prev_start);
            if prev_end > end {
                self.put(end, prev_end - end);
            }
        }
        while let Some((cut_start, cut_len)) =
            self.runs.range(start..end).next().map(|(&s, &l)| (s, l))
        {
            let cut_end = cut_start.saturating_add(cut_len);
            self.take(cut_start);
            if cut_end > end {
                self.put(end, cut_end - end);
            }
        }
    }

    /// The lowest part of `start..start + len` the set holds.
    ///
    /// Walking a query run by alternating this with [`Self::first_gap`] is how
    /// a caller splits one free into the parts two different sets own, without
    /// visiting a block.
    pub(crate) fn first_overlap(&self, start: u64, len: u64) -> Option<(u64, u64)> {
        let end = Self::span(start, len)?;
        if let Some((cover_start, cover_len)) = self.covering(start) {
            return Some((
                start,
                cover_start.saturating_add(cover_len).min(end) - start,
            ));
        }
        self.runs
            .range(start..end)
            .next()
            .map(|(&s, &l)| (s, s.saturating_add(l).min(end) - s))
    }

    /// The lowest part of `start..start + len` the set does **not** hold.
    pub(crate) fn first_gap(&self, start: u64, len: u64) -> Option<(u64, u64)> {
        let end = Self::span(start, len)?;
        let gap_start = match self.covering(start) {
            Some((cover_start, cover_len)) => cover_start.saturating_add(cover_len),
            None => start,
        };
        if gap_start >= end {
            return None;
        }
        let gap_end = self
            .runs
            .range(gap_start..end)
            .next()
            .map_or(end, |(&s, _)| s);
        Some((gap_start, gap_end - gap_start))
    }

    /// End of a non-empty run, or `None` for one that spans nothing.
    fn span(start: u64, len: u64) -> Option<u64> {
        let end = start.checked_add(len)?;
        (end > start).then_some(end)
    }

    /// Remove the run recorded at `start`, discharging its blocks.
    fn take(&mut self, start: u64) {
        if let Some(len) = self.runs.remove(&start) {
            self.blocks = self.blocks.saturating_sub(len);
        }
    }

    /// Record `start..start + len` verbatim, without coalescing. Only for
    /// pieces of a run that was already maximal, so the invariant holds.
    fn put(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        if self.runs.insert(start, len).is_none() {
            self.blocks = self.blocks.saturating_add(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn runs(set: &RunSet) -> Vec<(u64, u64)> {
        set.iter().collect()
    }

    /// The invariant every operation must leave: runs ascending, non-empty,
    /// separated by at least one block, and the block count exact.
    fn assert_canonical(set: &RunSet) {
        let mut blocks = 0u64;
        let mut prev_end = None;
        for (start, len) in set.iter() {
            assert!(len > 0, "an empty run was recorded at {start}");
            if let Some(prev_end) = prev_end {
                assert!(
                    start > prev_end,
                    "runs at {prev_end} and {start} are adjacent or overlapping"
                );
            }
            prev_end = Some(start + len);
            blocks += len;
        }
        assert_eq!(set.blocks(), blocks, "the block count drifted");
    }

    #[test]
    fn an_insert_absorbs_every_run_it_touches() {
        let mut set = RunSet::new();
        set.insert(10, 5);
        set.insert(20, 5);
        set.insert(30, 5);
        assert_eq!(runs(&set), [(10, 5), (20, 5), (30, 5)]);
        // Bridges the first two and touches the third's start.
        set.insert(15, 15);
        assert_eq!(runs(&set), [(10, 25)]);
        assert_canonical(&set);
    }

    #[test]
    fn adjacent_runs_coalesce_from_either_side() {
        let mut set = RunSet::new();
        set.insert(100, 1);
        set.insert(101, 1);
        set.insert(99, 1);
        assert_eq!(runs(&set), [(99, 3)]);
        assert_eq!(set.run_count(), 1);
        assert_eq!(set.blocks(), 3);
        assert_canonical(&set);
    }

    #[test]
    fn a_repeated_insert_changes_nothing() {
        let mut set = RunSet::new();
        set.insert(8, 4);
        set.insert(9, 2);
        set.insert(8, 4);
        assert_eq!(runs(&set), [(8, 4)]);
        assert_canonical(&set);
    }

    #[test]
    fn a_removal_splits_the_run_it_cuts() {
        let mut set = RunSet::new();
        set.insert(0, 100);
        set.remove(40, 10);
        assert_eq!(runs(&set), [(0, 40), (50, 50)]);
        assert_eq!(set.blocks(), 90);
        assert!(!set.contains(45));
        assert!(set.contains(39) && set.contains(50));
        assert_canonical(&set);
    }

    #[test]
    fn a_removal_spanning_several_runs_trims_both_edges() {
        let mut set = RunSet::new();
        set.insert(0, 10);
        set.insert(20, 10);
        set.insert(40, 10);
        set.remove(5, 40);
        assert_eq!(runs(&set), [(0, 5), (45, 5)]);
        assert_canonical(&set);
    }

    #[test]
    fn removing_a_whole_run_and_beyond_empties_the_set() {
        let mut set = RunSet::new();
        set.insert(7, 3);
        set.remove(0, 100);
        assert!(set.is_empty() && set.blocks() == 0);
        set.remove(0, 100);
        assert_canonical(&set);
    }

    #[test]
    fn an_empty_or_overflowing_run_is_ignored() {
        let mut set = RunSet::new();
        set.insert(5, 0);
        assert!(set.is_empty());
        set.insert(u64::MAX, 2);
        assert!(set.is_empty(), "an overflowing span records nothing");
        set.insert(5, 5);
        set.remove(5, 0);
        assert_eq!(runs(&set), [(5, 5)]);
        assert_eq!(set.first_overlap(5, 0), None);
        assert_eq!(set.first_gap(5, 0), None);
    }

    #[test]
    fn first_overlap_reports_the_held_part_of_a_query() {
        let mut set = RunSet::new();
        set.insert(10, 10);
        set.insert(40, 10);
        assert_eq!(set.first_overlap(0, 100), Some((10, 10)));
        assert_eq!(set.first_overlap(15, 100), Some((15, 5)));
        assert_eq!(set.first_overlap(15, 2), Some((15, 2)));
        assert_eq!(set.first_overlap(20, 20), None);
        assert_eq!(set.first_overlap(45, 100), Some((45, 5)));
    }

    #[test]
    fn first_gap_reports_the_unheld_part_of_a_query() {
        let mut set = RunSet::new();
        set.insert(10, 10);
        set.insert(40, 10);
        assert_eq!(set.first_gap(0, 100), Some((0, 10)));
        assert_eq!(set.first_gap(10, 100), Some((20, 20)));
        assert_eq!(set.first_gap(15, 5), None);
        assert_eq!(set.first_gap(45, 5), None);
        assert_eq!(set.first_gap(45, 10), Some((50, 5)));
    }

    #[test]
    fn walking_overlaps_and_gaps_covers_a_query_exactly() {
        let mut set = RunSet::new();
        set.insert(3, 2);
        set.insert(9, 4);
        let (start, len) = (0u64, 20u64);
        let mut pos = start;
        let mut covered = 0u64;
        while pos < start + len {
            let step = set
                .first_overlap(pos, start + len - pos)
                .filter(|&(s, _)| s == pos)
                .or_else(|| set.first_gap(pos, start + len - pos))
                .expect("every block is in exactly one of the two");
            assert_eq!(step.0, pos, "the walk must not skip a block");
            covered += step.1;
            pos += step.1;
        }
        assert_eq!(covered, len);
    }

    #[test]
    fn pop_first_drains_in_address_order() {
        let mut set = RunSet::new();
        set.insert(50, 2);
        set.insert(10, 3);
        assert_eq!(set.pop_first(), Some((10, 3)));
        assert_eq!(set.blocks(), 2);
        assert_eq!(set.pop_first(), Some((50, 2)));
        assert_eq!(set.pop_first(), None);
        assert_canonical(&set);
    }

    #[test]
    fn every_operation_matches_a_per_block_model() {
        // A run set is only worth having if it answers exactly what a set of
        // blocks would, so it is checked against one: a deterministic sequence
        // of inserts and removals over a small window, with the canonical form,
        // the block count, membership, and both walk primitives compared to a
        // flat block set after every step.
        const WINDOW: u64 = 48;
        let mut set = RunSet::new();
        let mut model = alloc::collections::BTreeSet::new();
        // xorshift, so the sequence is fixed and reproducible without a clock.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for step in 0..600 {
            let raw = next();
            let start = raw % WINDOW;
            let len = 1 + (raw >> 8) % 9;
            let end = (start + len).min(WINDOW);
            let inserting = (raw >> 16) & 1 == 0;
            if inserting {
                set.insert(start, end - start);
            } else {
                set.remove(start, end - start);
            }
            for slot in start..end {
                if inserting {
                    model.insert(slot);
                } else {
                    model.remove(&slot);
                }
            }

            assert_canonical(&set);
            let held = u64::try_from(model.len()).expect("the window is small");
            assert_eq!(set.blocks(), held, "step {step}");
            for block in 0..WINDOW {
                assert_eq!(
                    set.contains(block),
                    model.contains(&block),
                    "step {step}, block {block}"
                );
            }
            // The two walk primitives must agree with the model about the first
            // held and the first unheld part of every query.
            for probe in 0..WINDOW {
                let span = WINDOW - probe;
                let run_from = |from: u64, held: bool| {
                    let to = (from..WINDOW)
                        .find(|b| model.contains(b) != held)
                        .unwrap_or(WINDOW);
                    (from, to - from)
                };
                assert_eq!(
                    set.first_overlap(probe, span),
                    (probe..WINDOW)
                        .find(|b| model.contains(b))
                        .map(|from| run_from(from, true)),
                    "step {step}, overlap at {probe}"
                );
                assert_eq!(
                    set.first_gap(probe, span),
                    (probe..WINDOW)
                        .find(|b| !model.contains(b))
                        .map(|from| run_from(from, false)),
                    "step {step}, gap at {probe}"
                );
            }
        }
    }

    #[test]
    fn a_million_block_run_costs_one_entry() {
        let mut set = RunSet::new();
        set.insert(1 << 20, 1 << 20);
        assert_eq!(set.run_count(), 1);
        assert_eq!(set.blocks(), 1 << 20);
        // Freeing the block after it extends the same entry.
        set.insert((1 << 21) + 1, 1);
        assert_eq!(set.run_count(), 2, "one block of separation is a gap");
        set.insert(1 << 21, 1);
        assert_eq!(set.run_count(), 1, "closing the gap merges them");
        assert_eq!(set.blocks(), (1 << 20) + 2);
        assert_canonical(&set);
    }
}
