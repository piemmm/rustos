//! Host tests for the member agent's lifecycle.
//!
//! These prove the *policy*: what a member device's driver process does next
//! and when. The escalation arithmetic underneath is proven once in the shared
//! retry cadence; here only the decisions built on it are asserted.

use super::{AgentStep, MemberAgent, REOFFER_BASE_NS, REOFFER_CEILING_NS};

use tairix_abi::raid_ipc::MembershipEnd;
use tairix_abi::Errno;

/// Drive the agent from a fresh state to an outstanding membership, returning
/// it, so a test about what happens *after* that says only that.
fn with_membership_held(now_ns: u64) -> MemberAgent {
    let mut agent = MemberAgent::new();
    assert_eq!(agent.next_step(now_ns), AgentStep::Offer);
    agent.note_offered(true, now_ns);
    agent
}

#[test]
fn a_fresh_agent_offers_its_device_immediately() {
    assert_eq!(MemberAgent::new().next_step(0), AgentStep::Offer);
    assert_eq!(MemberAgent::default().next_step(0), AgentStep::Offer);
}

#[test]
fn a_delivered_offer_parks_on_the_reply_with_no_deadline() {
    let agent = with_membership_held(5_000);
    assert_eq!(
        agent.next_step(u64::MAX),
        AgentStep::AwaitReply,
        "a membership lasts as long as the array holds the device, so no clock ends it"
    );
}

#[test]
fn an_undelivered_offer_is_retried_on_a_paced_deadline() {
    let mut agent = MemberAgent::new();
    agent.note_offered(false, 0);
    let AgentStep::Retry { deadline_ns } = agent.next_step(0) else {
        panic!("an offer that reached no composer must be paced, never re-sent at once");
    };
    assert!(deadline_ns > 0, "parking on it must be a wait, not a spin");
    assert_eq!(
        agent.next_step(deadline_ns),
        AgentStep::Offer,
        "and the offer is made again once the deadline arrives"
    );
}

#[test]
fn repeated_undeliverable_offers_back_off_towards_the_ceiling() {
    let mut agent = MemberAgent::new();
    let mut now = 0u64;
    let mut previous = 0u64;
    for _ in 0..8 {
        agent.note_offered(false, now);
        let AgentStep::Retry { deadline_ns } = agent.next_step(now) else {
            panic!("still waiting for a composer");
        };
        let wait = deadline_ns - now;
        assert!(
            wait >= previous,
            "an agent whose composer never appears must not speed up"
        );
        assert!(
            wait <= REOFFER_CEILING_NS,
            "and must still be picked up within a bounded wait"
        );
        previous = wait;
        now = deadline_ns;
    }
    assert_eq!(
        previous, REOFFER_CEILING_NS,
        "the escalation settles at the ceiling rather than receding forever"
    );
    const { assert!(REOFFER_CEILING_NS > REOFFER_BASE_NS) };
}

#[test]
fn a_clean_release_offers_the_device_again_after_a_pace() {
    let mut agent = with_membership_held(1_000);
    agent.note_end(MembershipEnd::Released, 2_000);
    let AgentStep::Retry { deadline_ns } = agent.next_step(2_000) else {
        panic!("a released device is offered again, so the array can re-form");
    };
    assert!(
        deadline_ns > 2_000,
        "even a clean release is paced: an agent cannot tell a healthy composer \
         from one releasing every member in a loop, and must not spin on the difference"
    );
}

#[test]
fn a_composer_that_went_away_is_offered_to_again() {
    let mut agent = with_membership_held(1_000);
    agent.note_end(MembershipEnd::ComposerGone, 2_000);
    let AgentStep::Retry { deadline_ns } = agent.next_step(2_000) else {
        panic!("a composer that restarts must be able to reassemble the array");
    };
    assert_eq!(agent.next_step(deadline_ns), AgentStep::Offer);
}

#[test]
fn a_refusal_stops_the_agent_for_good() {
    let mut agent = with_membership_held(1_000);
    agent.note_end(MembershipEnd::Refused(Errno::NotFound), 2_000);
    assert_eq!(agent.next_step(2_000), AgentStep::Stop);
    assert_eq!(
        agent.next_step(u64::MAX),
        AgentStep::Stop,
        "the verdict came from the device's own metadata, so time does not change it"
    );
}

#[test]
fn a_membership_that_ends_is_no_longer_awaited() {
    let mut agent = with_membership_held(0);
    assert_eq!(agent.next_step(0), AgentStep::AwaitReply);
    agent.note_end(MembershipEnd::Released, 0);
    assert_ne!(
        agent.next_step(u64::MAX),
        AgentStep::AwaitReply,
        "an ended membership must never leave the agent parked on a reply that will not come"
    );
}

#[test]
fn a_successful_offer_after_a_backoff_clears_the_escalation() {
    let mut agent = MemberAgent::new();
    for at in [0, 4_000_000_000, 9_000_000_000] {
        agent.note_offered(false, at);
    }
    agent.note_offered(true, 20_000_000_000);
    assert_eq!(agent.next_step(20_000_000_000), AgentStep::AwaitReply);

    // The next round starts from the base delay again, not from where the
    // previous outage left off: a composer that worked once is not suspect.
    agent.note_end(MembershipEnd::ComposerGone, 30_000_000_000);
    let AgentStep::Retry { deadline_ns } = agent.next_step(30_000_000_000) else {
        panic!("the agent re-offers after the composer goes away");
    };
    assert_eq!(deadline_ns - 30_000_000_000, 2 * REOFFER_BASE_NS);
}

#[test]
fn the_first_offer_is_never_delayed() {
    // The common path — a composer already listening when the member appears —
    // must cost no wait at all, so an array assembles at boot without pause.
    let agent = MemberAgent::new();
    assert_eq!(agent.next_step(u64::MAX), AgentStep::Offer);
}
