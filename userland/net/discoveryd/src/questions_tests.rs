//! Unit tests for the front's question table.

use super::{Questions, Verdict};
use crate::wire::Form;
use alloc::vec::Vec;
use tairix_abi::discovery_ipc::{Answer, Change, Entry};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::Errno;
use tairix_hash::HashSeed;
use tairix_net::dns::Name;
use tairix_net::mdns::{MAX_QUESTIONS, MAX_RECORDS};

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn ipp() -> Name {
    Name::encode("_ipp._tcp.local").unwrap()
}

fn table() -> Questions {
    Questions::new(HashSeed::from_words(1, 2))
}

fn instance(question: u32, change: Change, label: &[u8]) -> Entry<'_> {
    Entry::Answer {
        request: question,
        interface: iface(b"eth0"),
        change,
        ttl: 120,
        answer: Answer::Instance { label },
    }
}

/// A table asking one browse for session 1 request 1, the decoder asked.
fn asked() -> (Questions, u32) {
    let mut questions = table();
    let id = questions.subscribe(Form::Instance, ipp(), 1, 1).unwrap();
    assert_eq!(questions.next_ask(), Some((id, Form::Instance, ipp())));
    questions.asked(id);
    assert_eq!(questions.next_ask(), None);
    (questions, id)
}

#[test]
fn requests_asking_the_same_question_share_it() {
    let mut questions = table();
    let first = questions.subscribe(Form::Instance, ipp(), 1, 1).unwrap();
    let second = questions.subscribe(Form::Instance, ipp(), 2, 5).unwrap();
    assert_eq!(first, second);
    let other = questions
        .subscribe(
            Form::Instance,
            Name::encode("_ssh._tcp.local").unwrap(),
            1,
            2,
        )
        .unwrap();
    assert_ne!(first, other);
    // Nothing was asked of the decoder yet, so nobody is owed a replay.
    assert_eq!(questions.next_replay(), None);
}

#[test]
fn every_answer_moves_as_an_honest_engine_moves_it() {
    let (mut questions, id) = asked();
    let told = |verdict| matches!(verdict, Verdict::Tell(ref who) if who == &[(1, 1)]);
    assert!(told(
        questions.on_answer(&instance(id, Change::Added, b"A"), true)
    ));
    assert!(
        told(questions.on_answer(&instance(id, Change::Refreshed, b"a"), true)),
        "names compare without case"
    );
    assert!(told(
        questions.on_answer(&instance(id, Change::Retired, b"A"), true)
    ));
    // What is not held cannot be renewed or retired, and what is held is
    // added once: each of those crossed a flush.
    for (change, label) in [(Change::Refreshed, b"A"), (Change::Retired, b"A")] {
        assert_eq!(
            questions.on_answer(&instance(id, change, label), true),
            Verdict::Stale
        );
    }
    assert!(told(
        questions.on_answer(&instance(id, Change::Added, b"B"), true)
    ));
    assert_eq!(
        questions.on_answer(&instance(id, Change::Added, b"B"), true),
        Verdict::Stale
    );
}

#[test]
fn an_answer_for_a_question_never_asked_is_a_lie_and_one_stopped_is_stale() {
    let (mut questions, id) = asked();
    assert_eq!(
        questions.on_answer(&instance(id + 1, Change::Added, b"A"), true),
        Verdict::Lie
    );
    questions.unsubscribe(id, 1, 1);
    assert_eq!(questions.next_stop(), Some(id));
    questions.stopped(id);
    assert_eq!(questions.next_stop(), None);
    assert_eq!(
        questions.on_answer(&instance(id, Change::Added, b"A"), true),
        Verdict::Stale
    );
}

#[test]
fn an_answer_on_a_link_the_decoder_has_not_settled_is_stale() {
    let (mut questions, id) = asked();
    assert_eq!(
        questions.on_answer(&instance(id, Change::Added, b"A"), false),
        Verdict::Stale
    );
}

#[test]
fn a_question_holds_no_more_than_an_honest_engine_could() {
    let (mut questions, id) = asked();
    for n in 0..MAX_RECORDS {
        let label = alloc::format!("i{n}");
        assert!(matches!(
            questions.on_answer(&instance(id, Change::Added, label.as_bytes()), true),
            Verdict::Tell(_)
        ));
    }
    assert_eq!(
        questions.on_answer(&instance(id, Change::Added, b"one more"), true),
        Verdict::Lie
    );
}

#[test]
fn a_late_request_is_replayed_what_is_held_and_nothing_else_until_it_ends() {
    let (mut questions, id) = asked();
    let _ = questions.on_answer(&instance(id, Change::Added, b"A"), true);
    assert_eq!(questions.subscribe(Form::Instance, ipp(), 2, 9), Ok(id));
    let (question, token) = questions.next_replay().expect("owed a replay");
    assert_eq!(question, id);
    questions.replay_sent(token);
    assert_eq!(questions.next_replay(), None);
    // A live edge meanwhile is told only to the request already live.
    assert_eq!(
        questions.on_answer(&instance(id, Change::Added, b"B"), true),
        Verdict::Tell(alloc::vec![(1, 1)])
    );
    assert_eq!(
        questions.on_held(token, &instance(id, Change::Added, b"A")),
        Verdict::Tell(alloc::vec![(2, 9)])
    );
    assert_eq!(
        questions.on_held(token, &instance(id, Change::Added, b"not held")),
        Verdict::Stale
    );
    assert_eq!(
        questions.on_held(token + 1, &instance(id, Change::Added, b"A")),
        Verdict::Stale
    );
    questions.replayed(token);
    assert_eq!(
        questions.on_answer(&instance(id, Change::Retired, b"A"), true),
        Verdict::Tell(alloc::vec![(1, 1), (2, 9)])
    );
}

#[test]
fn a_flushed_link_names_each_request_that_held_something_there() {
    let (mut questions, id) = asked();
    let _ = questions.subscribe(Form::Instance, ipp(), 3, 3);
    let _ = questions.on_answer(&instance(id, Change::Added, b"A"), true);
    let flushed: Vec<(u32, u32)> = questions.flush_link(iface(b"eth0"));
    assert_eq!(flushed, [(1, 1), (3, 3)]);
    assert!(questions.flush_link(iface(b"eth0")).is_empty());
    assert_eq!(
        questions.on_answer(&instance(id, Change::Retired, b"A"), true),
        Verdict::Stale,
        "retiring what the flush already voided"
    );
}

#[test]
fn a_replaced_decoder_is_asked_everything_again_and_every_hold_is_flushed() {
    let (mut questions, id) = asked();
    let _ = questions.subscribe(Form::Instance, ipp(), 2, 2);
    let _ = questions.on_answer(&instance(id, Change::Added, b"A"), true);
    let flushed = questions.forget_decoder();
    assert_eq!(flushed, [(1, 1, iface(b"eth0")), (2, 2, iface(b"eth0"))]);
    assert_eq!(questions.next_ask(), Some((id, Form::Instance, ipp())));
    assert_eq!(
        questions.next_replay(),
        None,
        "a new decoder tells everyone"
    );
}

#[test]
fn a_question_nobody_asks_any_more_is_stopped_only_if_it_was_asked() {
    let mut questions = table();
    let id = questions.subscribe(Form::Instance, ipp(), 1, 1).unwrap();
    questions.unsubscribe(id, 1, 1);
    assert_eq!(questions.next_stop(), None, "never asked, nothing to stop");
    assert!(questions.is_empty());
}

#[test]
fn no_more_questions_are_asked_than_an_engine_holds() {
    let mut questions = table();
    for n in 0..MAX_QUESTIONS {
        let name = Name::encode(&alloc::format!("host{n}.local")).unwrap();
        questions.subscribe(Form::AddressV4, name, 1, 1).unwrap();
    }
    assert_eq!(
        questions.subscribe(Form::AddressV4, Name::encode("one.local").unwrap(), 1, 1),
        Err(Errno::LimitExceeded)
    );
    // Joining a question already asked still fits.
    let first = Name::encode("host0.local").unwrap();
    assert!(questions.subscribe(Form::AddressV4, first, 2, 2).is_ok());
}
