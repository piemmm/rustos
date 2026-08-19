//! Unit tests for the listing desk's policy.
//!
//! Every rule the worker and the session depend on is exercised here with no
//! thread and no lock: the request/answer handshake, the staleness rule, the
//! deduplication that stops one directory being read twice, and the round-robin
//! that keeps one consumer from starving the other.

use super::*;

use alloc::vec;

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
    assert_eq!(
        desk.take(ListingClient::Picker, &home),
        Ok(Listing::Pending)
    );
    assert!(desk.has_work());
    assert_eq!(
        desk.next_job(),
        Some((ListingClient::Picker, home)),
        "the recorded request is the job"
    );
}

#[test]
fn asking_again_for_the_same_directory_starts_no_second_read() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users"]);
    let _ = desk.take(ListingClient::Picker, &home);
    let _ = desk.take(ListingClient::Picker, &home);
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
    let _ = desk.take(ListingClient::Pinboard, &home);
    let (client, target) = desk.next_job().expect("a job");
    assert!(desk.deliver(client, target, Ok(entries(&["a", "b"]))));

    assert_eq!(
        desk.take(ListingClient::Pinboard, &home),
        Ok(Listing::Ready(entries(&["a", "b"])))
    );
    // Consumed: the consumer has adopted those entries, so asking again means
    // it wants to know what is there *now*.
    assert_eq!(
        desk.take(ListingClient::Pinboard, &home),
        Ok(Listing::Pending)
    );
    assert!(desk.has_work());
}

#[test]
fn a_refusal_is_delivered_and_served_exactly_like_a_listing() {
    let mut desk = ListingDesk::new();
    let home = path(&["Locked"]);
    let _ = desk.take(ListingClient::Picker, &home);
    let (client, target) = desk.next_job().expect("a job");
    assert!(desk.deliver(client, target, Err(Errno::PermissionDenied)));
    assert_eq!(
        desk.take(ListingClient::Picker, &home),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn an_answer_for_somewhere_the_consumer_left_is_never_served() {
    let mut desk = ListingDesk::new();
    let first = path(&["Users"]);
    let second = path(&["Apps"]);
    let _ = desk.take(ListingClient::Picker, &first);
    let (client, target) = desk.next_job().expect("a job");
    // The user clicks elsewhere while the first read is in flight.
    let _ = desk.take(ListingClient::Picker, &second);
    assert!(
        !desk.deliver(client, target, Ok(entries(&["stale"]))),
        "an abandoned read must report that nobody wants it"
    );
    assert_eq!(
        desk.take(ListingClient::Picker, &second),
        Ok(Listing::Pending),
        "the stale answer leaked into the new request"
    );
    assert_eq!(
        desk.next_job(),
        Some((ListingClient::Picker, second)),
        "the new target was not queued"
    );
}

#[test]
fn one_consumers_answer_is_not_the_others() {
    let mut desk = ListingDesk::new();
    let home = path(&["Users"]);
    let _ = desk.take(ListingClient::Pinboard, &home);
    let (client, target) = desk.next_job().expect("a job");
    assert_eq!(client, ListingClient::Pinboard);
    assert!(desk.deliver(client, target, Ok(entries(&["mine"]))));
    assert_eq!(
        desk.take(ListingClient::Picker, &home),
        Ok(Listing::Pending),
        "the picker was served the icon column's answer"
    );
}

#[test]
fn two_busy_consumers_are_served_in_turn() {
    let mut desk = ListingDesk::new();
    let mut served = vec![];
    for _ in 0..4 {
        let _ = desk.take(ListingClient::Pinboard, &path(&["Desktop"]));
        let _ = desk.take(ListingClient::Picker, &path(&["Users"]));
        let (client, target) = desk.next_job().expect("a job");
        served.push(client);
        assert!(desk.deliver(client, target, Ok(entries(&["x"]))));
        // Adopt it, so the consumer asks again on the next round.
        let _ = desk.take(client, &[]);
    }
    assert_eq!(
        served,
        vec![
            ListingClient::Pinboard,
            ListingClient::Picker,
            ListingClient::Pinboard,
            ListingClient::Picker,
        ],
        "one consumer starved the other"
    );
}

#[test]
fn stopping_hands_out_no_more_work() {
    let mut desk = ListingDesk::new();
    let _ = desk.take(ListingClient::Picker, &path(&["Users"]));
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
    assert_eq!(
        desk.take(ListingClient::Pinboard, &home),
        Ok(Listing::Pending)
    );
    // The worker wakes, takes the job, reads, and delivers.
    let (client, target) = desk.next_job().expect("a job");
    assert_eq!(target, home);
    assert!(desk.deliver(client, target, Ok(entries(&["notes.txt"]))));
    // The session wakes on the pipe byte and asks again.
    assert_eq!(
        desk.take(ListingClient::Pinboard, &home),
        Ok(Listing::Ready(entries(&["notes.txt"])))
    );
    assert!(!desk.has_work(), "nothing is left outstanding");
}
