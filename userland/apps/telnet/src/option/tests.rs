//! Host tests for the RFC 1143 negotiation state machine: an exchange settles,
//! never loops, and an unsupported option is always refused.

use alloc::vec::Vec;

use super::{
    option_name, NegotiationFault, Options, Outcome, BINARY, ECHO, LINEMODE, NAWS, Q, SUPPORTED,
    SUPPRESS_GO_AHEAD,
};
use crate::nvt::{DO, DONT, WILL, WONT};

#[test]
fn a_fresh_table_has_every_option_disabled() {
    let table = Options::new();
    for &option in SUPPORTED {
        assert!(!table.local(option), "{option}");
        assert!(!table.remote(option), "{option}");
    }
}

#[test]
fn a_will_for_a_supported_option_is_accepted_once() {
    let mut table = Options::new();
    assert_eq!(
        table.on_will(SUPPRESS_GO_AHEAD),
        Outcome {
            reply: Some((DO, SUPPRESS_GO_AHEAD)),
            changed: Some(true),
            fault: None,
        }
    );
    assert!(table.remote(SUPPRESS_GO_AHEAD));
    // A repeat in the settled state answers nothing: this is the loop breaker.
    assert_eq!(table.on_will(SUPPRESS_GO_AHEAD), Outcome::quiet());
}

#[test]
fn a_do_for_a_supported_option_is_accepted_once() {
    let mut table = Options::new();
    assert_eq!(
        table.on_do(NAWS),
        Outcome {
            reply: Some((WILL, NAWS)),
            changed: Some(true),
            fault: None,
        }
    );
    assert!(table.local(NAWS));
    assert_eq!(table.on_do(NAWS), Outcome::quiet());
}

#[test]
fn an_unsupported_option_is_always_refused() {
    let mut table = Options::new();
    // 37 is AUTHENTICATION, which this client does not implement.
    assert_eq!(table.on_will(37).reply, Some((DONT, 37)));
    assert_eq!(table.on_do(37).reply, Some((WONT, 37)));
    assert!(!table.remote(37));
    assert!(!table.local(37));
}

#[test]
fn echo_is_accepted_from_the_server_and_never_offered_by_us() {
    let mut table = Options::new();
    assert_eq!(table.on_will(ECHO).reply, Some((DO, ECHO)));
    assert!(table.remote(ECHO), "the server may echo for us");
    // A client that echoed on the server's behalf would double every
    // character, so `DO ECHO` is refused however supported the option is.
    assert_eq!(table.on_do(ECHO).reply, Some((WONT, ECHO)));
    assert!(!table.local(ECHO));
}

#[test]
fn refuse_then_permit_flips_the_agreement_in_both_directions() {
    let mut table = Options::new();
    table.refuse(BINARY);
    assert_eq!(table.on_will(BINARY).reply, Some((DONT, BINARY)));
    assert_eq!(table.on_do(BINARY).reply, Some((WONT, BINARY)));
    table.permit(BINARY);
    assert_eq!(table.on_will(BINARY).reply, Some((DO, BINARY)));
    assert_eq!(table.on_do(BINARY).reply, Some((WILL, BINARY)));
}

#[test]
fn a_local_request_transmits_once_and_settles_on_the_answer() {
    let mut table = Options::new();
    let mut out = Vec::new();
    assert_eq!(table.ask_remote_enable(LINEMODE, &mut out), Ok(()));
    assert_eq!(out, alloc::vec![crate::nvt::IAC, DO, LINEMODE]);
    assert_eq!(table.him(LINEMODE), Q::WantYesEmpty);
    // The peer's agreement completes the exchange with no further traffic.
    assert_eq!(table.on_will(LINEMODE), Outcome::quiet().with_change(true));
    assert!(table.remote(LINEMODE));
}

#[test]
fn a_refused_local_request_settles_without_a_second_ask() {
    let mut table = Options::new();
    let mut out = Vec::new();
    assert_eq!(table.ask_remote_enable(LINEMODE, &mut out), Ok(()));
    out.clear();
    assert_eq!(table.on_wont(LINEMODE), Outcome::quiet());
    assert_eq!(table.him(LINEMODE), Q::No);
    assert!(out.is_empty(), "a refusal is accepted, never re-asked");
}

#[test]
fn asking_for_a_state_the_option_is_already_in_is_reported_not_transmitted() {
    let mut table = Options::new();
    let mut out = Vec::new();
    table.on_will(SUPPRESS_GO_AHEAD);
    assert_eq!(
        table.ask_remote_enable(SUPPRESS_GO_AHEAD, &mut out),
        Err(NegotiationFault::AlreadyThere)
    );
    assert!(out.is_empty());

    assert_eq!(
        table.ask_remote_disable(BINARY, &mut out),
        Err(NegotiationFault::AlreadyThere)
    );
    assert!(out.is_empty());
}

#[test]
fn a_duplicate_local_request_is_reported_not_transmitted_twice() {
    let mut table = Options::new();
    let mut out = Vec::new();
    assert_eq!(table.ask_local_enable(BINARY, &mut out), Ok(()));
    let after_first = out.len();
    assert_eq!(
        table.ask_local_enable(BINARY, &mut out),
        Err(NegotiationFault::AlreadyQueued)
    );
    assert_eq!(out.len(), after_first, "no second WILL on the wire");
}

#[test]
fn a_queued_opposite_request_is_carried_out_when_the_answer_lands() {
    let mut table = Options::new();
    let mut out = Vec::new();
    // Enable, then — before the answer — ask to disable again. RFC 1143 queues
    // the reversal rather than transmitting it, so the peer sees one exchange
    // at a time.
    table.on_do(BINARY);
    assert_eq!(table.ask_local_disable(BINARY, &mut out), Ok(()));
    assert_eq!(table.us(BINARY), Q::WantNoEmpty);
    out.clear();
    assert_eq!(table.ask_local_enable(BINARY, &mut out), Ok(()));
    assert_eq!(table.us(BINARY), Q::WantNoOpposite);
    assert!(out.is_empty(), "the reversal is queued, not sent");
    // The peer confirms the disable; the queued enable now goes out.
    assert_eq!(table.on_dont(BINARY).reply, Some((WILL, BINARY)));
    assert_eq!(table.us(BINARY), Q::WantYesEmpty);
}

#[test]
fn an_answer_the_wrong_way_round_resynchronises_and_is_reported() {
    let mut table = Options::new();
    let mut out = Vec::new();
    table.on_will(LINEMODE);
    assert_eq!(table.ask_remote_disable(LINEMODE, &mut out), Ok(()));
    assert_eq!(table.him(LINEMODE), Q::WantNoEmpty);
    // A peer that answers our DONT with WILL is in error; the state machine
    // takes the peer's word and reports the irregularity.
    let outcome = table.on_will(LINEMODE);
    assert_eq!(outcome.fault, Some(NegotiationFault::AnsweredWrongWay));
    assert_eq!(table.him(LINEMODE), Q::No);
    assert_eq!(outcome.reply, None);
}

#[test]
fn a_negotiation_storm_terminates() {
    // A peer that re-sends the same request forever must not make the client
    // reply forever: after the exchange settles, every repeat is silent.
    let mut table = Options::new();
    let mut replies = 0usize;
    for _ in 0..1000 {
        if table.on_will(SUPPRESS_GO_AHEAD).reply.is_some() {
            replies += 1;
        }
        if table.on_do(NAWS).reply.is_some() {
            replies += 1;
        }
    }
    assert_eq!(replies, 2, "exactly one reply per option, then silence");
}

#[test]
fn every_supported_option_has_a_name_and_the_set_is_sorted() {
    for &option in SUPPORTED {
        assert!(option_name(option).is_some(), "{option} has no name");
    }
    assert!(
        SUPPORTED.windows(2).all(|pair| pair[0] < pair[1]),
        "the supported set is ascending and duplicate-free"
    );
    assert_eq!(
        option_name(200),
        None,
        "no invented label for an unknown code"
    );
}
