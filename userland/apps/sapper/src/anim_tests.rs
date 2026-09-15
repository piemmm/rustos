//! Animation model tests. The clock is a parameter, so every rule is exact.

use super::*;

use alloc::vec;

const COLS: u16 = 8;

fn steps(cells: &[(u16, u16, u16)]) -> Vec<Step> {
    cells
        .iter()
        .map(|&(col, row, ring)| Step {
            at: Coord::new(col, row),
            ring,
        })
        .collect()
}

#[test]
fn a_new_motion_is_idle_and_asks_for_no_deadline() {
    let motion = Motion::new(COLS, false);
    assert!(motion.is_idle());
    assert_eq!(motion.deadline_ns(0), None);
}

#[test]
fn a_wave_asks_for_a_frame_and_stops_asking_when_it_ends() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Reveal, &steps(&[(0, 0, 0)]), 0);
    assert!(!motion.is_idle());
    assert_eq!(motion.deadline_ns(0), Some(FRAME_NS));

    let span = WaveKind::Reveal.span_ns();
    assert!(
        motion.advance(span - 1),
        "still running one nanosecond short"
    );
    assert!(!motion.advance(span), "the span is over");
    assert_eq!(motion.deadline_ns(span), None, "and no wake is owed");
}

#[test]
fn the_deadline_never_overshoots_the_end_of_the_last_wave() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Reveal, &steps(&[(0, 0, 0)]), 0);
    let span = WaveKind::Reveal.span_ns();
    // Close enough to the end that a whole frame would pass it.
    let near = span - FRAME_NS / 2;
    assert_eq!(motion.deadline_ns(near), Some(span));
}

#[test]
fn a_ring_starts_later_than_the_one_before_it() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(
        WaveKind::Reveal,
        &steps(&[(0, 0, 0), (1, 0, 1), (2, 0, 2)]),
        0,
    );
    let stagger = WaveKind::Reveal.stagger_ns();
    let at = |col, now| {
        motion
            .cell(Coord::new(col, 0), now)
            .expect("covered by the wave")
            .progress
    };
    assert!(at(0, stagger) > 0, "ring zero is already moving");
    assert_eq!(at(1, stagger), 0, "ring one starts exactly now");
    assert_eq!(at(2, stagger), 0, "ring two has not started");
    assert!(at(1, stagger * 2) > 0);
    assert_eq!(at(2, stagger * 2), 0);
}

#[test]
fn progress_runs_from_zero_to_full_across_the_span() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Detonate, &steps(&[(3, 3, 0)]), 1_000);
    let span = WaveKind::Detonate.span_ns();
    let at = |now| {
        motion
            .cell(Coord::new(3, 3), now)
            .expect("covered")
            .progress
    };
    assert_eq!(at(1_000), 0);
    assert!(at(1_000 + span / 2) > 100 && at(1_000 + span / 2) < 160);
    assert_eq!(at(1_000 + span), u8::MAX);
    assert_eq!(at(u64::MAX), u8::MAX, "and stays there");
}

#[test]
fn a_cell_outside_the_wave_is_at_rest() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Reveal, &steps(&[(0, 0, 0)]), 0);
    assert_eq!(motion.cell(Coord::new(5, 5), 0), None);
}

#[test]
fn the_newest_wave_covering_a_cell_wins() {
    let mut motion = Motion::new(COLS, false);
    let at = Coord::new(2, 2);
    motion.begin(WaveKind::Reveal, &steps(&[(2, 2, 0)]), 0);
    motion.begin(WaveKind::Mark, &steps(&[(2, 2, 0)]), 10);
    assert_eq!(motion.cell(at, 20).map(|m| m.kind), Some(WaveKind::Mark));
}

#[test]
fn past_the_wave_ceiling_the_oldest_is_dropped() {
    let mut motion = Motion::new(COLS, false);
    for n in 0..=u16::try_from(MAX_WAVES).expect("small") {
        motion.begin(WaveKind::Mark, &steps(&[(n, 0, 0)]), u64::from(n));
    }
    assert_eq!(motion.waves.len(), MAX_WAVES);
    assert_eq!(
        motion.cell(Coord::new(0, 0), 5),
        None,
        "the first wave was dropped, so its cell is at rest"
    );
    assert!(motion.cell(Coord::new(1, 0), 5).is_some());
}

#[test]
fn reduced_motion_starts_nothing() {
    let mut motion = Motion::new(COLS, true);
    motion.begin(WaveKind::Detonate, &steps(&[(0, 0, 0), (1, 1, 1)]), 0);
    assert!(motion.is_idle(), "a suppressed wave is simply not started");
    assert_eq!(motion.deadline_ns(0), None);
    assert_eq!(motion.cell(Coord::new(0, 0), 0), None);
}

#[test]
fn turning_reduced_motion_on_ends_what_is_running() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Reveal, &steps(&[(0, 0, 0)]), 0);
    assert!(!motion.is_idle());
    motion.set_reduced_motion(true);
    assert!(motion.is_idle(), "no cell is left half-animated");
    assert!(motion.reduced_motion());
}

#[test]
fn an_empty_wave_is_not_started() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Reveal, &[], 0);
    assert!(motion.is_idle());
}

#[test]
fn a_repeated_cell_in_one_wave_is_animated_once() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(
        WaveKind::Reveal,
        &steps(&[(1, 1, 0), (1, 1, 3), (2, 1, 1)]),
        0,
    );
    assert_eq!(motion.waves[0].members.len(), 2);
    let animating: Vec<Coord> = motion.animating().collect();
    assert_eq!(animating, vec![Coord::new(1, 1), Coord::new(2, 1)]);
}

#[test]
fn clearing_ends_every_wave() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Victory, &steps(&[(0, 0, 0)]), 0);
    motion.clear();
    assert!(motion.is_idle());
    assert_eq!(motion.deadline_ns(0), None);
}

#[test]
fn animating_names_every_covered_cell_for_the_repaint() {
    let mut motion = Motion::new(COLS, false);
    motion.begin(WaveKind::Reveal, &steps(&[(0, 0, 0), (7, 3, 1)]), 0);
    motion.begin(WaveKind::Mark, &steps(&[(4, 2, 0)]), 0);
    let mut animating: Vec<Coord> = motion.animating().collect();
    animating.sort_unstable();
    assert_eq!(
        animating,
        vec![Coord::new(0, 0), Coord::new(4, 2), Coord::new(7, 3)]
    );
}

// --- The curves ---------------------------------------------------------

#[test]
fn the_ease_runs_end_to_end_and_only_forwards() {
    assert_eq!(ease_out(0), 0);
    assert_eq!(ease_out(255), 255);
    let mut previous = 0;
    for t in 0..=255_u8 {
        let eased = ease_out(t);
        assert!(eased >= previous, "the ease must not go backwards at {t}");
        previous = eased;
    }
}

#[test]
fn the_ease_decelerates() {
    // Past the half-way point in time, well past it in distance.
    assert!(ease_out(128) > 200, "{}", ease_out(128));
}

#[test]
fn the_overshoot_starts_at_nothing_ends_at_full_and_passes_it_between() {
    assert_eq!(overshoot(0), 0);
    assert_eq!(overshoot(255), 1000);
    let peak = (0..=255_u8).map(overshoot).max().expect("non-empty");
    assert!(peak > 1000, "the curve must overshoot: peak {peak}");
    assert!(peak < 1400, "but not wildly: peak {peak}");
}

#[test]
fn every_curve_input_is_answered_without_panicking() {
    for t in 0..=255_u8 {
        let motion = CellMotion {
            kind: WaveKind::Mark,
            progress: t,
        };
        let _ = motion.eased();
        let _ = motion.overshoot();
    }
}

#[test]
fn a_zero_width_board_animates_nothing() {
    let mut motion = Motion::new(0, false);
    motion.begin(WaveKind::Reveal, &steps(&[(0, 0, 0)]), 0);
    assert!(motion.is_idle());
    assert_eq!(motion.animating().count(), 0);
}
