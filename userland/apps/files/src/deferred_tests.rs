use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::fs::{FileId, FileStat};
use tairix_abi::time::Time64;
use tairix_abi::{Errno, NodeTimes};
use tairix_browse::{EntryKind, ListingClient, Probe, Properties};

use super::{FilesClient, Probes, PropertyJob, PropertyReads};

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
    assert!(
        !probes.take_landed(),
        "a delivery with no answers leaves nothing to adopt"
    );
}

/// Regression: a delivered batch has to *tell the loop* it landed.
///
/// The worker probed correctly and woke the loop, but the desk recorded only
/// the answers — so the wake found nothing to adopt, the loop re-parked, and
/// every folder kept its empty cue until some unrelated gesture happened to
/// repaint and latch the answers that had been sitting here all along.
#[test]
fn a_delivered_batch_owes_the_loop_one_adoption() {
    let mut probes = Probes::new();
    let folder = path(&["Users"]);
    let _ = probes.ask(&folder);
    let batch = probes.next_batch().expect("a batch");
    assert!(probes.deliver(batch.into_iter().map(|f| (f, true)).collect()));

    assert!(probes.take_landed(), "the delivery owes the loop a repaint");
    assert!(
        !probes.take_landed(),
        "and owes exactly one: a second turn of the loop repaints nothing"
    );
    assert_eq!(
        probes.ask(&folder),
        (Probe::Ready(true), false),
        "the adoption the repaint performs is what draws the cue"
    );
}

/// A desk asked to stop owes no repaint: there is nothing left to draw it.
#[test]
fn stopping_drops_an_unconsumed_adoption() {
    let mut probes = Probes::new();
    let _ = probes.ask(&path(&["a"]));
    let batch = probes.next_batch().expect("a batch");
    assert!(probes.deliver(batch.into_iter().map(|f| (f, true)).collect()));
    probes.stop();
    assert!(!probes.take_landed());
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

/// A summary for the node at `name`, so an answer can be told from another's.
fn summary(name: &str) -> Properties {
    Properties::from_stat(
        name,
        EntryKind::File,
        &FileStat {
            kind: tairix_abi::fs::FileKind::Regular,
            nlink: 1,
            size: 0,
            allocated: 0,
            mode: 0o644,
            uid: 0,
            gid: 0,
            id: FileId::NONE,
            times: NodeTimes {
                created: Time64::UNIX_EPOCH,
                modified: Time64::UNIX_EPOCH,
                accessed: Time64::UNIX_EPOCH,
                changed: Time64::UNIX_EPOCH,
            },
        },
    )
}

fn job(window: u64, path: &str) -> PropertyJob {
    PropertyJob {
        window,
        path: String::from(path),
        kind: EntryKind::File,
    }
}

/// Several Properties windows are open at once, so one window's read must
/// neither displace nor be answered into another's.
#[test]
fn two_properties_windows_are_answered_independently() {
    let mut desk = PropertyReads::new();
    assert!(desk.submit(job(1, "/a")));
    assert!(desk.submit(job(2, "/b")));
    assert!(desk.has_work());

    let first = desk.next_job().expect("a read");
    let second = desk.next_job().expect("a second read");
    assert_eq!((first.window, second.window), (1, 2), "asked order, served");
    assert!(!desk.has_work());

    // Answered out of order, each lands in its own window's slot.
    assert!(desk.deliver(second.window, Ok(summary("b"))));
    assert!(desk.deliver(first.window, Ok(summary("a"))));
    assert!(desk.take_landed());
    assert_eq!(
        desk.take(1)
            .expect("window 1's answer")
            .map(|p| p.name().into()),
        Ok(String::from("a"))
    );
    assert_eq!(
        desk.take(2)
            .expect("window 2's answer")
            .map(|p| p.name().into()),
        Ok(String::from("b"))
    );
    assert!(desk.take(1).is_none(), "an answer is collected once");
}

/// A re-read follows a write the window just made, so the older answer
/// describes a node that has since changed: showing it would undo the edit.
#[test]
fn a_re_read_supersedes_the_same_windows_outstanding_answer() {
    let mut desk = PropertyReads::new();
    assert!(desk.submit(job(7, "/a")));
    let first = desk.next_job().expect("a read");
    assert!(desk.deliver(first.window, Ok(summary("stale"))));

    assert!(desk.submit(job(7, "/a")));
    assert!(
        desk.take(7).is_none(),
        "the stale answer went with the request it superseded"
    );
    let again = desk.next_job().expect("the re-read");
    assert!(desk.deliver(again.window, Ok(summary("fresh"))));
    assert_eq!(
        desk.take(7)
            .expect("the fresh answer")
            .map(|p| p.name().into()),
        Ok(String::from("fresh"))
    );
    assert!(desk.next_job().is_none());
}

/// A refusal is an answer: the window states it rather than showing nothing.
#[test]
fn a_refused_read_is_delivered_as_the_reason_it_failed() {
    let mut desk = PropertyReads::new();
    assert!(desk.submit(job(3, "/gone")));
    let read = desk.next_job().expect("a read");
    assert!(desk.deliver(read.window, Err(Errno::NotFound)));
    assert_eq!(desk.take(3), Some(Err(Errno::NotFound)));
}

/// A closed window's answer belongs to nobody — and a window id could be
/// reused, so an in-flight answer must not land in whatever took its place.
#[test]
fn a_closed_windows_read_is_forgotten_and_its_answer_dropped() {
    let mut desk = PropertyReads::new();
    assert!(desk.submit(job(4, "/a")));
    let read = desk.next_job().expect("a read");
    desk.forget(4);
    assert!(
        !desk.deliver(read.window, Ok(summary("a"))),
        "nothing owns the slot, so no repaint is owed"
    );
    assert!(desk.take(4).is_none());
    assert!(!desk.take_landed());

    // A request not yet started is simply dropped.
    let mut desk = PropertyReads::new();
    assert!(desk.submit(job(5, "/a")));
    desk.forget(5);
    assert!(!desk.has_work());
    assert!(desk.next_job().is_none());
}

/// A stopping desk records nothing and offers nothing, so a parked worker
/// leaves rather than finding fresh work on the way out.
#[test]
fn a_stopping_property_desk_hands_out_no_work() {
    let mut desk = PropertyReads::new();
    assert!(desk.submit(job(1, "/a")));
    desk.stop();
    assert!(!desk.has_work());
    assert!(desk.next_job().is_none());
    assert!(!desk.submit(job(2, "/b")));
    assert!(desk.take(1).is_none());
    assert!(!desk.take_landed());
}
