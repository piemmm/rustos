//! The clock does not drift, does not spiral, and stops exactly where it
//! was told to.

use super::*;

const NS: u64 = 1_000_000_000;

fn pacer() -> Pacer {
    Pacer::new(TickRate::default_rate())
}

#[test]
fn the_first_reading_establishes_the_clock_and_steps_nothing() {
    let mut pacer = pacer();
    assert_eq!(
        pacer.advance(1_000),
        0,
        "time before the clock was watched is not elapsed"
    );
    assert_eq!(pacer.alpha(), 0);
}

#[test]
fn a_second_of_real_time_is_exactly_the_rate_in_ticks() {
    let mut pacer = pacer();
    let hz = u64::from(pacer.rate().hz());
    pacer.advance(0);
    let mut ticks = 0u32;
    // In slices small enough that none is clamped by the catch-up bound.
    for step in 1..=20u64 {
        ticks += pacer.advance(step * NS / 20);
    }
    assert_eq!(
        u64::from(ticks),
        hz,
        "a second produced {ticks} ticks, not {hz}"
    );
}

#[test]
fn a_rate_that_does_not_divide_a_second_does_not_drift() {
    // Thirty ticks a second is 33 333 333.33 ns each. An accumulator
    // holding nanoseconds-per-tick would lose a tick every few minutes.
    let mut pacer = pacer();
    let hz = u64::from(pacer.rate().hz());
    pacer.advance(0);
    let mut ticks = 0u64;
    let slices = 600u64;
    for step in 1..=slices {
        ticks += u64::from(pacer.advance(step * NS / 10));
    }
    assert_eq!(
        ticks,
        hz * slices / 10,
        "sixty seconds drifted to {ticks} ticks"
    );
}

#[test]
fn a_long_stall_is_dropped_rather_than_replayed() {
    let mut pacer = pacer();
    pacer.advance(0);
    let ticks = pacer.advance(10 * NS);
    let cap = u32::try_from(MAX_CATCHUP_NS * u64::from(pacer.rate().hz()) / NS)
        .expect("a quarter second of ticks is small");
    assert!(
        ticks <= cap + 1,
        "a ten-second stall replayed {ticks} ticks, past the {cap}-tick bound"
    );
}

#[test]
fn alpha_walks_the_gap_between_two_ticks() {
    let mut pacer = pacer();
    let tick = NS / u64::from(pacer.rate().hz());
    pacer.advance(0);
    pacer.advance(tick / 2);
    let half = pacer.alpha();
    assert!(
        (120..=135).contains(&half),
        "half a tick read as {half}/255"
    );
    assert_eq!(
        pacer.advance(tick / 2 + tick),
        1,
        "a whole tick later, one tick"
    );
    let still_half = pacer.alpha();
    assert!(
        half.abs_diff(still_half) <= 1,
        "a whole tick later the fraction moved from {half} to {still_half}"
    );
}

#[test]
fn a_paused_clock_steps_nothing_and_resumes_where_it_stopped() {
    let mut pacer = pacer();
    pacer.advance(0);
    pacer.advance(NS / 100);
    let held = pacer.alpha();

    pacer.pause();
    assert!(pacer.paused());
    assert_eq!(pacer.advance(5 * NS), 0, "a paused clock stepped");
    assert_eq!(
        pacer.alpha(),
        held,
        "the fraction on screen moved while paused"
    );

    pacer.resume(60 * NS);
    assert!(!pacer.paused());
    assert_eq!(
        pacer.advance(60 * NS),
        0,
        "resuming replayed the time the seat was away"
    );
    assert_eq!(
        pacer.alpha(),
        held,
        "the frame that comes back is the frame that went"
    );
}

#[test]
fn a_clock_that_goes_backwards_steps_nothing() {
    // A monotonic clock should not, but a saturating subtraction is the
    // difference between a no-op and a wrapped near-eternity of ticks.
    let mut pacer = pacer();
    pacer.advance(10 * NS);
    assert_eq!(pacer.advance(1), 0);
}

#[test]
fn motion_interpolates_between_the_last_two_ticks() {
    let a = WorldPoint { x: 0, y: 100 };
    let b = WorldPoint { x: 1_000, y: -100 };
    let mut motion = Motion::still(a);
    motion.observe(b);
    assert_eq!(motion.current(), b);
    assert_eq!(
        motion.at(0),
        a,
        "at the start of a tick the frame reads the old state"
    );
    assert_eq!(motion.at(255), b, "at the end it reads the new one");
    let half = motion.at(128);
    assert!((498..=503).contains(&half.x), "halfway read {half:?}");
    assert!((-2..=2).contains(&half.y), "halfway read {half:?}");
}

#[test]
fn a_snap_is_not_interpolated_across() {
    let mut motion = Motion::still(WorldPoint { x: 0, y: 0 });
    motion.observe(WorldPoint { x: 1_000, y: 0 });
    motion.snap(WorldPoint { x: 500_000, y: 0 });
    for alpha in [0u8, 64, 128, 255] {
        assert_eq!(
            motion.at(alpha),
            WorldPoint { x: 500_000, y: 0 },
            "a teleport was drawn streaking across the ground"
        );
    }
}

#[test]
fn interpolating_at_the_coordinate_extremes_does_not_overflow() {
    let a = WorldPoint {
        x: i32::MIN,
        y: i32::MAX,
    };
    let b = WorldPoint {
        x: i32::MAX,
        y: i32::MIN,
    };
    for alpha in [0u8, 1, 128, 254, 255] {
        let _ = interpolate(a, b, alpha);
    }
}
