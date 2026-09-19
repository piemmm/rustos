use tairix_wintersun_net::bounds::MAX_TICK_HZ;
use tairix_wintersun_net::value::{TickInstant, TickPhase};

use super::{place, TickRate};
use crate::bounds::{DIMINISH_RESET_SECONDS, INTENT_LOOKBEHIND_TICKS};
use crate::error::{Refusal, RuleError};

#[test]
fn the_default_rate_is_thirty_and_valid() {
    assert_eq!(TickRate::default().hz(), 30);
    assert_eq!(TickRate::new(30), Ok(TickRate::default_rate()));
}

#[test]
fn a_rate_outside_the_protocol_ceiling_is_refused() {
    assert_eq!(TickRate::new(0), Err(RuleError::TickRate));
    assert_eq!(TickRate::new(MAX_TICK_HZ + 1), Err(RuleError::TickRate));
    assert!(TickRate::new(MAX_TICK_HZ).is_ok());
    assert!(TickRate::new(1).is_ok());
}

#[test]
fn the_diminish_window_is_the_same_wall_clock_length_at_every_rate() {
    for hz in [1_u16, 20, 30, 60, MAX_TICK_HZ] {
        let rate = TickRate::new(hz).expect("a legal rate");
        assert_eq!(
            rate.diminish_reset_ticks(),
            DIMINISH_RESET_SECONDS * u32::from(hz),
            "the window must scale with the rate, not sit at a tick count"
        );
    }
}

#[test]
fn a_second_conversion_saturates_rather_than_wrapping() {
    let rate = TickRate::new(MAX_TICK_HZ).expect("a legal rate");
    assert_eq!(rate.ticks_for_seconds(u32::MAX), u32::MAX);
}

#[test]
fn a_sample_from_the_future_is_refused() {
    let sampled = TickInstant {
        tick: 101,
        phase: TickPhase(0),
    };
    assert_eq!(place(100, sampled), Err(Refusal::SampledInTheFuture));
}

#[test]
fn a_sample_inside_the_window_is_taken_as_sent() {
    for back in 0..=INTENT_LOOKBEHIND_TICKS {
        let sampled = TickInstant {
            tick: 1000 - back,
            phase: TickPhase(0x4000),
        };
        assert_eq!(place(1000, sampled), Ok(sampled), "back {back} is honoured");
    }
}

#[test]
fn back_dating_past_the_window_buys_no_further_priority() {
    let edge = place(
        1000,
        TickInstant {
            tick: 1000 - INTENT_LOOKBEHIND_TICKS - 1,
            phase: TickPhase(0),
        },
    )
    .expect("clamped, not refused");
    let far = place(
        1000,
        TickInstant {
            tick: 0,
            phase: TickPhase(0),
        },
    )
    .expect("clamped, not refused");
    assert_eq!(edge, far, "every over-old sample lands on one edge");
    assert_eq!(edge.tick, 1000 - INTENT_LOOKBEHIND_TICKS);
}

#[test]
fn a_clamped_sample_loses_the_phase_it_claimed() {
    let placed = place(
        1000,
        TickInstant {
            tick: 500,
            phase: TickPhase(u16::MAX),
        },
    )
    .expect("clamped");
    assert_eq!(placed.phase, TickPhase(0));
}

#[test]
fn the_window_does_not_underflow_near_the_first_tick() {
    let placed = place(
        2,
        TickInstant {
            tick: 0,
            phase: TickPhase(7),
        },
    )
    .expect("inside the window from tick zero");
    assert_eq!(placed.tick, 0);
}
