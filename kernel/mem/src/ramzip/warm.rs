//! Thrash detection for the compressed anonymous-memory tier
//! (`plans/SWAPSWAPSWAP.md` section 13).
//!
//! Compression stops helping when the same task's pages cycle through
//! compress → fault-in → recompress: every cycle costs CPU and the
//! working set clearly is not cold. The detector counts, per task, how
//! many restores hit entries that were sealed only a short while ago —
//! measured on the tier's own monotonic *event clock* (one tick per
//! compression or restore), never a wall clock, so the policy is
//! deterministic and testable. A task whose recent-cycle score crosses
//! the threshold is marked thrashing: the tier refuses to compress its
//! pages until the score decays, and the caller escalates through the
//! pressure policy instead of spinning.
//!
//! Scores decay by halving on a fixed event cadence, so a task that
//! stops churning is forgiven deterministically; there is no timer, no
//! background work, and no retry loop.

use alloc::collections::BTreeMap;

/// A restore counts toward thrash when the entry it restores was
/// sealed fewer than this many events earlier.
const RECENT_CYCLE_EVENTS: u64 = 64;

/// A task is thrashing once its decayed recent-cycle score reaches
/// this value.
const THRASH_SCORE_LIMIT: u32 = 8;

/// All scores halve every time the event clock advances this far —
/// deterministic forgiveness, no wall clock.
const DECAY_INTERVAL_EVENTS: u64 = 256;

/// The per-task thrash detector.
#[derive(Debug, Default)]
pub(crate) struct ThrashDetector {
    scores: BTreeMap<u64, u32>,
    last_decay: u64,
}

impl ThrashDetector {
    /// A fresh detector with no history.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a restore of an entry sealed at event `sealed_at`,
    /// observed at event `now`. Returns `true` when this restore
    /// *newly* pushed the task over the thrash threshold (so the
    /// caller can count one detection, not one per churned page).
    pub(crate) fn on_restore(&mut self, task: u64, sealed_at: u64, now: u64) -> bool {
        self.decay(now);
        if now.saturating_sub(sealed_at) >= RECENT_CYCLE_EVENTS {
            return false;
        }
        let score = self.scores.entry(task).or_insert(0);
        let was_thrashing = *score >= THRASH_SCORE_LIMIT;
        *score = score.saturating_add(1);
        !was_thrashing && *score >= THRASH_SCORE_LIMIT
    }

    /// Whether `task` is currently marked thrashing at event `now`.
    pub(crate) fn is_thrashing(&mut self, task: u64, now: u64) -> bool {
        self.decay(now);
        self.scores
            .get(&task)
            .is_some_and(|score| *score >= THRASH_SCORE_LIMIT)
    }

    /// Apply the deterministic halving decay for every full
    /// [`DECAY_INTERVAL_EVENTS`] window the clock has advanced since
    /// the last decay. Scores that reach zero are removed so an idle
    /// detector holds no per-task residue.
    fn decay(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.last_decay);
        let halvings = elapsed / DECAY_INTERVAL_EVENTS;
        if halvings == 0 {
            return;
        }
        self.last_decay = now;
        // A `u32` score is zero after at most 31 halvings; clamping the
        // shift keeps the operation defined for any clock jump.
        let shift = u32::try_from(halvings.min(31)).unwrap_or(31);
        self.scores.retain(|_, score| {
            *score >>= shift;
            *score > 0
        });
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_detector_marks_nothing() {
        let mut detector = ThrashDetector::new();
        assert!(!detector.is_thrashing(1, 0));
    }

    #[test]
    fn rapid_recompress_cycles_cross_the_threshold_once() {
        let mut detector = ThrashDetector::new();
        let mut detections = 0;
        for i in 0..THRASH_SCORE_LIMIT + 3 {
            let now = u64::from(i) + 1;
            // Every restore hits an entry sealed one event earlier.
            if detector.on_restore(7, now - 1, now) {
                detections += 1;
            }
        }
        assert_eq!(detections, 1, "one detection per crossing");
        assert!(detector.is_thrashing(7, u64::from(THRASH_SCORE_LIMIT) + 3));
    }

    #[test]
    fn old_entries_do_not_count_as_churn() {
        let mut detector = ThrashDetector::new();
        for i in 0..100u64 {
            let now = RECENT_CYCLE_EVENTS * (i + 2);
            assert!(!detector.on_restore(7, now - RECENT_CYCLE_EVENTS, now));
        }
        assert!(!detector.is_thrashing(7, RECENT_CYCLE_EVENTS * 200));
    }

    #[test]
    fn scores_decay_and_the_task_is_forgiven() {
        let mut detector = ThrashDetector::new();
        for i in 0..u64::from(THRASH_SCORE_LIMIT) {
            detector.on_restore(7, i, i + 1);
        }
        assert!(detector.is_thrashing(7, u64::from(THRASH_SCORE_LIMIT)));
        // After enough quiet windows the halvings clear the score.
        let quiet = u64::from(THRASH_SCORE_LIMIT) + DECAY_INTERVAL_EVENTS * 4;
        assert!(!detector.is_thrashing(7, quiet));
        // No residue is left for a fully decayed task.
        assert!(detector.scores.is_empty());
    }

    #[test]
    fn tasks_are_scored_independently() {
        let mut detector = ThrashDetector::new();
        for i in 0..u64::from(THRASH_SCORE_LIMIT) {
            detector.on_restore(1, i, i + 1);
        }
        assert!(detector.is_thrashing(1, u64::from(THRASH_SCORE_LIMIT)));
        assert!(!detector.is_thrashing(2, u64::from(THRASH_SCORE_LIMIT)));
    }

    #[test]
    fn detection_is_deterministic_for_equal_histories() {
        let run = || {
            let mut detector = ThrashDetector::new();
            let mut marks = alloc::vec::Vec::new();
            for i in 0..32u64 {
                marks.push(detector.on_restore(3, i, i + 1));
            }
            marks
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn a_huge_clock_jump_cannot_overflow_the_decay() {
        let mut detector = ThrashDetector::new();
        detector.on_restore(1, 0, 1);
        assert!(!detector.is_thrashing(1, u64::MAX));
        assert!(detector.scores.is_empty());
    }
}
