//! Unit tests for the routing policy.
//!
//! The policy is a pure function of state, so it is checked over the whole
//! cross-product of (role × who holds the sink × which sink was asked for)
//! rather than over the handful of cases somebody thought of.

use alloc::vec;
use alloc::vec::Vec;

use super::{duck_millibel, route, Routing, SinkState, StreamRequest};
use crate::volume::DUCK_MILLIBEL;
use tairix_abi::audio::StreamRole;
use tairix_abi::Errno;
use tairix_seat::SeatOwner;

const EVERY_ROLE: &[StreamRole] = &[
    StreamRole::Media,
    StreamRole::Communication,
    StreamRole::Notification,
    StreamRole::Accessibility,
];

const ALICE: SeatOwner = SeatOwner(1);
const BOB: SeatOwner = SeatOwner(2);

/// Two sinks, the first the machine default, leased as asked.
fn sinks(default_holder: Option<SeatOwner>, other_holder: Option<SeatOwner>) -> Vec<SinkState> {
    vec![
        SinkState {
            device_id: 10,
            is_default: true,
            leased_to: default_holder,
        },
        SinkState {
            device_id: 20,
            is_default: false,
            leased_to: other_holder,
        },
    ]
}

#[test]
fn an_unleased_sink_plays_for_anybody_which_is_the_headless_case() {
    for role in EVERY_ROLE {
        for owner in [None, Some(ALICE), Some(BOB)] {
            let request = StreamRequest {
                role: *role,
                requested_device: None,
                owner,
            };
            assert_eq!(
                route(&request, &sinks(None, None)),
                Routing::Play { device_id: 10 },
                "{role:?} owned by {owner:?}"
            );
        }
    }
}

#[test]
fn the_session_holding_the_lease_is_mixed() {
    for role in EVERY_ROLE {
        let request = StreamRequest {
            role: *role,
            requested_device: None,
            owner: Some(ALICE),
        };
        assert_eq!(
            route(&request, &sinks(Some(ALICE), None)),
            Routing::Play { device_id: 10 },
            "{role:?}"
        );
    }
}

/// A departing user's music does not play into the arriving user's room, and
/// it does not silently vanish either: it holds its position and is told.
#[test]
fn a_session_without_the_lease_is_paused_unless_it_is_a_notification() {
    for role in EVERY_ROLE {
        for owner in [None, Some(BOB)] {
            let request = StreamRequest {
                role: *role,
                requested_device: None,
                owner,
            };
            let expected = if *role == StreamRole::Notification {
                Routing::Drop
            } else {
                Routing::Pause { device_id: 10 }
            };
            assert_eq!(
                route(&request, &sinks(Some(ALICE), None)),
                expected,
                "{role:?} owned by {owner:?}"
            );
        }
    }
}

/// One that arrives ten minutes late is noise, and the role is what says so.
#[test]
fn a_notification_from_an_inactive_session_is_dropped_rather_than_queued() {
    let request = StreamRequest {
        role: StreamRole::Notification,
        requested_device: None,
        owner: Some(BOB),
    };
    assert_eq!(route(&request, &sinks(Some(ALICE), None)), Routing::Drop);
}

#[test]
fn a_named_sink_is_used_and_arbitrated_on_its_own_lease() {
    let request = StreamRequest {
        role: StreamRole::Media,
        requested_device: Some(20),
        owner: Some(BOB),
    };
    // The default is Alice's, but the named sink is free.
    assert_eq!(
        route(&request, &sinks(Some(ALICE), None)),
        Routing::Play { device_id: 20 }
    );
    // And when the named sink is Alice's too, Bob waits on it rather than
    // falling back to one he may use.
    assert_eq!(
        route(&request, &sinks(None, Some(ALICE))),
        Routing::Pause { device_id: 20 }
    );
}

#[test]
fn a_named_sink_that_does_not_exist_is_refused_rather_than_substituted() {
    let request = StreamRequest {
        role: StreamRole::Media,
        requested_device: Some(99),
        owner: Some(ALICE),
    };
    assert_eq!(
        route(&request, &sinks(None, None)),
        Routing::Refuse(Errno::NotFound)
    );
}

/// A machine that has not been told which sink to use is a different answer
/// from one that has no sinks, and neither is a sink picked arbitrarily.
#[test]
fn a_machine_with_no_default_refuses_rather_than_guessing() {
    let request = StreamRequest {
        role: StreamRole::Media,
        requested_device: None,
        owner: Some(ALICE),
    };
    let undecided = vec![SinkState {
        device_id: 10,
        is_default: false,
        leased_to: None,
    }];
    assert_eq!(
        route(&request, &undecided),
        Routing::Refuse(Errno::DeviceOffline)
    );
    assert_eq!(route(&request, &[]), Routing::Refuse(Errno::DeviceOffline));
}

/// The whole cross-product, so no combination is decided by accident.
#[test]
fn the_policy_is_total_over_every_role_holder_and_request() {
    for role in EVERY_ROLE {
        for holder in [None, Some(ALICE), Some(BOB)] {
            for owner in [None, Some(ALICE), Some(BOB)] {
                for requested in [None, Some(10), Some(20), Some(99)] {
                    let request = StreamRequest {
                        role: *role,
                        requested_device: requested,
                        owner,
                    };
                    let decided = route(&request, &sinks(holder, holder));
                    let expected = match (requested, holder, owner) {
                        (Some(99), _, _) => Routing::Refuse(Errno::NotFound),
                        (named, None, _) => Routing::Play {
                            device_id: named.unwrap_or(10),
                        },
                        (named, Some(held), Some(mine)) if held == mine => Routing::Play {
                            device_id: named.unwrap_or(10),
                        },
                        (_, Some(_), _) if *role == StreamRole::Notification => Routing::Drop,
                        (named, Some(_), _) => Routing::Pause {
                            device_id: named.unwrap_or(10),
                        },
                    };
                    assert_eq!(
                        decided, expected,
                        "{role:?} owned by {owner:?}, sink held by {holder:?}, \
                         asking for {requested:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn media_steps_aside_for_speech_and_nothing_else_ducks() {
    assert_eq!(
        duck_millibel(StreamRole::Media, &[StreamRole::Communication]),
        DUCK_MILLIBEL
    );
    assert_eq!(
        duck_millibel(StreamRole::Media, &[StreamRole::Accessibility]),
        DUCK_MILLIBEL
    );
    assert_eq!(duck_millibel(StreamRole::Media, &[StreamRole::Media]), 0);
    assert_eq!(
        duck_millibel(StreamRole::Media, &[StreamRole::Notification]),
        0
    );
    assert_eq!(duck_millibel(StreamRole::Media, &[]), 0);
    for role in EVERY_ROLE {
        if *role == StreamRole::Media {
            continue;
        }
        assert_eq!(
            duck_millibel(*role, &[StreamRole::Communication]),
            0,
            "{role:?} must not duck under speech"
        );
    }
}
