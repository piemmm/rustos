//! Unit tests for the listing desk's policy.
//!
//! Every rule a worker and a serve loop depend on is exercised here with no
//! thread and no lock: the request/answer handshake, the staleness rule, the
//! deduplication that stops one directory being read twice, and the round-robin
//! that keeps one consumer from starving another.

use super::*;

use alloc::vec;

/// A two-consumer program, standing in for the desktop session's icon column
/// and trusted file picker.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Consumer {
    Pinboard,
    Picker,
}

impl ListingClient for Consumer {
    const ALL: &'static [Self] = &[Self::Pinboard, Self::Picker];
}

/// A one-consumer program, standing in for the file manager's browser.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Sole {
    Browser,
}

impl ListingClient for Sole {
    const ALL: &'static [Self] = &[Self::Browser];
}

fn path(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| String::from(*name)).collect()
}

fn entries(names: &[&str]) -> Vec<Entry> {
    names.iter().map(|name| Entry::file(*name)).collect()
}

#[test]
fn a_first_ask_records_the_request_and_answers_pending() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users", "root"]);
    assert_eq!(desk.take(Consumer::Picker, &home), Ok(Listing::Pending));
    assert!(desk.has_work());
    assert_eq!(
        desk.next_job(),
        Some((Consumer::Picker, home)),
        "the recorded request is the job"
    );
}

#[test]
fn asking_again_for_the_same_directory_starts_no_second_read() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users"]);
    let _ = desk.take(Consumer::Picker, &home);
    let _ = desk.take(Consumer::Picker, &home);
    assert!(desk.next_job().is_some());
    assert!(
        desk.next_job().is_none(),
        "a read already in progress was handed out twice"
    );
    assert!(!desk.has_work());
}

#[test]
fn a_delivered_answer_is_served_once_and_then_a_fresh_read_is_asked_for() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users"]);
    let _ = desk.take(Consumer::Pinboard, &home);
    let (client, target) = desk.next_job().expect("a job");
    assert!(desk.deliver(client, target, Ok(entries(&["a", "b"]))));

    assert_eq!(
        desk.take(Consumer::Pinboard, &home),
        Ok(Listing::Ready(entries(&["a", "b"])))
    );
    // Consumed: the consumer has adopted those entries, so asking again means
    // it wants to know what is there *now*.
    assert_eq!(desk.take(Consumer::Pinboard, &home), Ok(Listing::Pending));
    assert!(desk.has_work());
}

#[test]
fn a_refusal_is_delivered_and_served_exactly_like_a_listing() {
    let mut desk = ListingDesk::new();
    let home = path(&["Locked"]);
    let _ = desk.take(Consumer::Picker, &home);
    let (client, target) = desk.next_job().expect("a job");
    assert!(desk.deliver(client, target, Err(Errno::PermissionDenied)));
    assert_eq!(
        desk.take(Consumer::Picker, &home),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn an_answer_for_somewhere_the_consumer_left_is_never_served() {
    let mut desk = ListingDesk::new();
    let first = path(&["Users"]);
    let second = path(&["Apps"]);
    let _ = desk.take(Consumer::Picker, &first);
    let (client, target) = desk.next_job().expect("a job");
    // The user clicks elsewhere while the first read is in flight.
    let _ = desk.take(Consumer::Picker, &second);
    assert!(
        !desk.deliver(client, target, Ok(entries(&["stale"]))),
        "an abandoned read must report that nobody wants it"
    );
    assert_eq!(
        desk.take(Consumer::Picker, &second),
        Ok(Listing::Pending),
        "the stale answer leaked into the new request"
    );
    assert_eq!(
        desk.next_job(),
        Some((Consumer::Picker, second)),
        "the new target was not queued"
    );
}

#[test]
fn one_consumers_answer_is_not_the_others() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users"]);
    let _ = desk.take(Consumer::Pinboard, &home);
    let (client, target) = desk.next_job().expect("a job");
    assert_eq!(client, Consumer::Pinboard);
    assert!(desk.deliver(client, target, Ok(entries(&["mine"]))));
    assert_eq!(
        desk.take(Consumer::Picker, &home),
        Ok(Listing::Pending),
        "the picker was served the icon column's answer"
    );
}

#[test]
fn two_busy_consumers_are_served_in_turn() {
    let mut desk = ListingDesk::new();
    let mut served = vec![];
    for _ in 0..4 {
        let _ = desk.take(Consumer::Pinboard, &path(&["Desktop"]));
        let _ = desk.take(Consumer::Picker, &path(&["Users"]));
        let (client, target) = desk.next_job().expect("a job");
        served.push(client);
        assert!(desk.deliver(client, target, Ok(entries(&["x"]))));
        // Adopt it, so the consumer asks again on the next round.
        let _ = desk.take(client, &[]);
    }
    assert_eq!(
        served,
        vec![
            Consumer::Pinboard,
            Consumer::Picker,
            Consumer::Pinboard,
            Consumer::Picker,
        ],
        "one consumer starved the other"
    );
}

#[test]
fn stopping_hands_out_no_more_work() {
    let mut desk = ListingDesk::new();
    let _ = desk.take(Consumer::Picker, &path(&["Users"]));
    assert!(desk.has_work());
    desk.stop();
    assert!(desk.stopping());
    assert!(!desk.has_work(), "a stopped desk still offered work");
    assert!(desk.next_job().is_none());
}

/// The whole handshake, driven end to end on one thread: the shape the worker
/// and the session run, with the lock and the wake left out.
#[test]
fn a_request_completes_when_a_reader_serves_it() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users", "root", "Desktop"]);

    // The session asks and gets nothing yet.
    assert_eq!(desk.take(Consumer::Pinboard, &home), Ok(Listing::Pending));
    // The worker wakes, takes the job, reads, and delivers.
    let (client, target) = desk.next_job().expect("a job");
    assert_eq!(target, home);
    assert!(desk.deliver(client, target, Ok(entries(&["notes.txt"]))));
    // The session wakes on the pipe byte and asks again.
    assert_eq!(
        desk.take(Consumer::Pinboard, &home),
        Ok(Listing::Ready(entries(&["notes.txt"])))
    );
    assert!(!desk.has_work(), "nothing is left outstanding");
}

/// A program with one consumer needs no fairness, and the round-robin degrades
/// to serving it every time rather than to serving it every other time.
#[test]
fn a_sole_consumer_is_served_on_every_turn() {
    let mut desk = ListingDesk::new();
    for _ in 0..3 {
        let home = path(&["Users"]);
        assert_eq!(desk.take(Sole::Browser, &home), Ok(Listing::Pending));
        let (client, target) = desk.next_job().expect("a job");
        assert_eq!(client, Sole::Browser);
        assert!(desk.deliver(client, target, Ok(entries(&["a"]))));
        assert_eq!(
            desk.take(Sole::Browser, &home),
            Ok(Listing::Ready(entries(&["a"])))
        );
    }
}

/// The defect this desk's whole point is to avoid: a worker that hands itself
/// the same read for ever.
///
/// A hand-out clones the target rather than taking it, so the request outlived
/// its own answer — the slot became workable again the instant it was answered,
/// and the serve loop went straight round to read the same directory, waking
/// the embedder on every completion. Measured on the desktop as ~150 reads a
/// second of one folder, with nothing else on that thread.
#[test]
fn an_answered_read_is_never_handed_out_again() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users", "someone", "Desktop"]);
    assert_eq!(desk.take(Sole::Browser, &home), Ok(Listing::Pending));

    let (client, target) = desk.next_job().expect("a job");
    assert!(desk.deliver(client, target, Ok(entries(&["notes.txt"]))));

    assert!(
        !desk.has_work(),
        "the answered read must not make the slot workable again"
    );
    assert!(
        desk.next_job().is_none(),
        "a worker looking for work after answering must find none and park"
    );

    // And the answer is still there to be collected.
    assert_eq!(
        desk.take(Sole::Browser, &home),
        Ok(Listing::Ready(entries(&["notes.txt"])))
    );
}

/// A read the consumer has navigated away from leaves its *newer* request
/// standing, so the abandoned answer costs one wasted read and not a stall.
#[test]
fn a_stale_answer_does_not_clear_the_newer_request() {
    let mut desk = ListingDesk::new();
    let first = path(&["Users", "someone", "Desktop"]);
    let second = path(&["Users", "someone", "Documents"]);
    assert_eq!(desk.take(Sole::Browser, &first), Ok(Listing::Pending));
    let (client, target) = desk.next_job().expect("a job");

    // The consumer moves on while the read is in flight.
    assert_eq!(desk.take(Sole::Browser, &second), Ok(Listing::Pending));
    assert!(
        !desk.deliver(client, target, Ok(entries(&["notes.txt"]))),
        "an abandoned read owes no wake"
    );

    assert!(desk.has_work(), "the newer request is still owed a read");
    let (_, target) = desk.next_job().expect("the newer job");
    assert_eq!(target, second);
}
