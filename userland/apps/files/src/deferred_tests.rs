use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_browse::{ListingClient, Probe};

use super::{FilesClient, Probes};

fn path(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| String::from(*name)).collect()
}

/// The browser is the app's only listing consumer, and the desk's slot order
/// is its own declaration.
#[test]
fn the_browser_is_the_sole_listing_consumer() {
    assert_eq!(FilesClient::ALL, &[FilesClient::Browser]);
}

/// The rule that lets a paint resolve occupancy: an ask performs no I/O, it
/// records one and answers "not yet".
#[test]
fn a_first_ask_records_a_probe_and_answers_pending() {
    let mut probes = Probes::new();
    assert_eq!(
        probes.ask(&path(&["Users", "notes"])),
        (Probe::Pending, true)
    );
    assert!(probes.has_work());
    assert_eq!(
        probes.next_batch(),
        Some(vec![path(&["Users", "notes"])]),
        "the recorded ask is the batch"
    );
}

/// The renderer asks on every frame, so a re-ask must not queue a second probe
/// of the same folder — nor while one is in flight.
#[test]
fn asking_again_records_no_second_probe() {
    let mut probes = Probes::new();
    let folder = path(&["Users"]);
    assert_eq!(probes.ask(&folder), (Probe::Pending, true));
    for _ in 0..4 {
        assert_eq!(
            probes.ask(&folder),
            (Probe::Pending, false),
            "a re-ask records nothing, so it wakes nobody"
        );
    }
    assert_eq!(probes.next_batch(), Some(vec![folder.clone()]));
    // In flight now: the next frame's asks record nothing.
    for _ in 0..5 {
        assert_eq!(probes.ask(&folder), (Probe::Pending, false));
    }
    assert!(!probes.has_work());
    assert_eq!(probes.next_batch(), None);
}

/// A screenful of folders is one batch and one repaint, not one of each per
/// folder.
#[test]
fn every_outstanding_probe_is_taken_as_one_batch() {
    let mut probes = Probes::new();
    for name in ["a", "b", "c"] {
        assert_eq!(probes.ask(&path(&[name])), (Probe::Pending, true));
    }
    let batch = probes.next_batch().expect("a batch");
    assert_eq!(batch.len(), 3);
    assert!(probes.deliver(
        batch
            .into_iter()
            .map(|folder| (folder, true))
            .collect::<Vec<_>>()
    ));
    for name in ["a", "b", "c"] {
        assert_eq!(probes.ask(&path(&[name])), (Probe::Ready(true), false));
    }
}

/// An answer is served once: the renderer latches it onto the entry, so a
/// later ask means the listing was replaced and the question is fresh.
#[test]
fn an_answer_is_served_once_and_then_asked_again() {
    let mut probes = Probes::new();
    let folder = path(&["Empty"]);
    let _ = probes.ask(&folder);
    let batch = probes.next_batch().expect("a batch");
    assert!(probes.deliver(batch.into_iter().map(|f| (f, false)).collect()));
    assert_eq!(probes.ask(&folder), (Probe::Ready(false), false));
    assert_eq!(probes.ask(&folder), (Probe::Pending, true));
    assert!(probes.has_work());
}

/// A batch that answered nothing owes no repaint, so a wake costs no frame.
#[test]
fn an_empty_delivery_owes_no_repaint() {
    let mut probes = Probes::new();
    let _ = probes.ask(&path(&["a"]));
    let _ = probes.next_batch();
    assert!(!probes.deliver(Vec::new()));
}

/// A folder re-asked while its probe is in flight is answered by that probe,
/// not left needing a second one.
#[test]
fn a_probe_in_flight_answers_the_re_asks_it_absorbed() {
    let mut probes = Probes::new();
    let folder = path(&["Users"]);
    let _ = probes.ask(&folder);
    let batch = probes.next_batch().expect("a batch");
    let _ = probes.ask(&folder);
    assert!(probes.deliver(batch.into_iter().map(|f| (f, true)).collect()));
    assert_eq!(probes.ask(&folder), (Probe::Ready(true), false));
}

/// The held answers are bounded by one screenful, not by every folder the user
/// ever scrolled past: a batch replaces what was held, so a hundred-thousand-
/// entry directory scrolled end to end does not accumulate a hundred thousand
/// answers.
#[test]
fn a_delivery_replaces_the_answers_it_did_not_supersede() {
    let mut probes = Probes::new();
    let _ = probes.ask(&path(&["scrolled-away"]));
    let batch = probes.next_batch().expect("a batch");
    assert!(probes.deliver(batch.into_iter().map(|f| (f, true)).collect()));

    // The view moved on without ever drawing that folder's cue.
    let _ = probes.ask(&path(&["now-visible"]));
    let batch = probes.next_batch().expect("a second batch");
    assert!(probes.deliver(batch.into_iter().map(|f| (f, false)).collect()));

    assert_eq!(
        probes.ask(&path(&["now-visible"])),
        (Probe::Ready(false), false)
    );
    assert_eq!(
        probes.ask(&path(&["scrolled-away"])),
        (Probe::Pending, true),
        "the answer nothing drew was dropped, not hoarded"
    );
}

/// A stopping desk records nothing and offers nothing, so a parked worker
/// leaves rather than finding fresh work on the way out.
#[test]
fn stopping_records_nothing_and_hands_out_no_work() {
    let mut probes = Probes::new();
    let _ = probes.ask(&path(&["a"]));
    probes.stop();
    assert!(probes.stopping());
    assert!(!probes.has_work());
    assert_eq!(probes.next_batch(), None);
    assert_eq!(probes.ask(&path(&["b"])), (Probe::Pending, false));
    assert!(!probes.has_work());
}
