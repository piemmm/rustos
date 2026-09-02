//! Unit tests for the artwork desk's policy.
//!
//! Every rule an embedder depends on is exercised here with no thread and no
//! lock: the ask/answer handshake, the deduplication that stops one asset
//! being decoded twice, the hand-out order, the round rule that stops a
//! landing chasing its own tail, and the teardown.

use super::*;

use alloc::string::String;

fn asset(path: &str) -> ArtworkKey {
    ArtworkKey::Asset(String::from(path))
}

fn job(path: &str, side: u32) -> ArtworkJob {
    ArtworkJob {
        key: asset(path),
        side,
    }
}

/// A distinguishable picture, so a test can tell one delivery from another.
fn picture(side: u32) -> Surface {
    Surface::new(side, side).expect("a square surface")
}

fn is_pending(resolved: &Resolved) -> bool {
    matches!(resolved, Resolved::Pending)
}

#[test]
fn a_first_ask_records_the_decode_and_answers_pending() {
    let mut desk = ArtworkDesk::new();
    assert!(is_pending(&desk.collect(&asset("/a.png"), 32)));
    assert!(desk.has_work());
    assert_eq!(desk.next_job(), Some(job("/a.png", 32)));
}

#[test]
fn asking_again_for_the_same_asset_starts_no_second_decode() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 32);
    let _ = desk.collect(&asset("/a.png"), 32);
    assert!(desk.next_job().is_some());
    assert!(
        desk.next_job().is_none(),
        "a decode already in flight was handed out twice"
    );
    assert!(!desk.has_work());
}

/// The pixel side is part of the key, so the same asset at two sides is two
/// decodes — a scale change must not be served a resized copy of the old one.
#[test]
fn the_same_asset_at_a_different_side_is_a_different_decode() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 32);
    let _ = desk.collect(&asset("/a.png"), 48);
    assert_eq!(desk.next_job(), Some(job("/a.png", 32)));
    assert_eq!(desk.next_job(), Some(job("/a.png", 48)));
}

#[test]
fn decodes_are_handed_out_in_the_order_they_were_asked_for() {
    let mut desk = ArtworkDesk::new();
    for name in ["/c.png", "/a.png", "/b.png"] {
        let _ = desk.collect(&asset(name), 16);
    }
    assert_eq!(desk.next_job(), Some(job("/c.png", 16)));
    assert_eq!(desk.next_job(), Some(job("/a.png", 16)));
    assert_eq!(desk.next_job(), Some(job("/b.png", 16)));
}

#[test]
fn a_delivered_decode_is_collected_once_and_reports_a_landing() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(8))));
    assert!(desk.take_landed(), "a delivery owes the embedder a repaint");
    assert!(!desk.take_landed(), "the landing is reported once");

    match desk.collect(&asset("/a.png"), 8) {
        Resolved::Done(Some(art)) => assert_eq!(art.width(), 8),
        _ => panic!("the delivered picture was not served"),
    }
    assert!(
        is_pending(&desk.collect(&asset("/a.png"), 8)),
        "the answer is moved out to the cache, never served twice"
    );
}

/// The prefetch half: asking early records the decode without collecting
/// anything, so a surface that knows what it is about to draw starts the wait
/// before the frame that needs it.
#[test]
fn a_want_records_a_decode_without_collecting_it() {
    let mut desk = ArtworkDesk::new();
    desk.want(&asset("/a.png"), 32);
    assert!(desk.has_work());
    assert_eq!(desk.next_job(), Some(job("/a.png", 32)));
    assert!(
        !desk.take_landed(),
        "asking for a decode is not the same as one landing"
    );
}

/// A prefetch must never disturb what a draw is about to collect: the answer
/// stays put, and is still served once.
#[test]
fn a_want_never_consumes_an_answer_a_draw_is_about_to_collect() {
    let mut desk = ArtworkDesk::new();
    desk.want(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(8))));

    desk.want(&asset("/a.png"), 8);
    assert!(
        !desk.has_work(),
        "a delivered answer was queued for a second decode"
    );
    assert!(matches!(
        desk.collect(&asset("/a.png"), 8),
        Resolved::Done(Some(_))
    ));
}

/// Warming the same set repeatedly — the desktop re-reads its catalog on every
/// launcher press — must cost nothing beyond the lookup.
#[test]
fn warming_the_same_key_twice_starts_one_decode() {
    let mut desk = ArtworkDesk::new();
    desk.want(&asset("/a.png"), 8);
    desk.want(&asset("/a.png"), 8);
    assert!(desk.next_job().is_some());
    assert!(desk.next_job().is_none());
}

/// Warming a key whose answer has been handed over starts a decode again: the
/// cache owns that answer now and is the only thing that knows whether it still
/// holds it, so the desk cannot second-guess a miss. A cache that does still
/// hold it never asks (it peeks before warming).
#[test]
fn a_want_for_a_key_already_handed_over_starts_a_decode_again() {
    let mut desk = ArtworkDesk::new();
    desk.want(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(8))));
    assert!(matches!(
        desk.collect(&asset("/a.png"), 8),
        Resolved::Done(_)
    ));

    desk.want(&asset("/a.png"), 8);
    assert!(desk.has_work());
}

#[test]
fn a_stopping_desk_warms_nothing() {
    let mut desk = ArtworkDesk::new();
    desk.stop();
    desk.want(&asset("/a.png"), 8);
    assert!(!desk.has_work());
    assert!(desk.next_job().is_none());
}

/// A refusal is an answer like any other: it is delivered, collected, and
/// retained by the cache, so a broken asset is read once rather than each frame.
#[test]
fn a_refusal_is_delivered_and_collected_like_a_picture() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/broken.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, None));
    assert!(matches!(
        desk.collect(&asset("/broken.png"), 8),
        Resolved::Done(None)
    ));
}

/// The reported file-manager defect: an answer the cache took and then evicted
/// must be produced again the next time a paint misses on it. Remembering the
/// key as answered instead left the tile drawing its built-in glyph with
/// nothing left to re-decode it, so the window sat wrong until unrelated input
/// arrived.
#[test]
fn a_key_the_cache_dropped_is_decoded_again() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(8))));
    assert!(matches!(
        desk.collect(&asset("/a.png"), 8),
        Resolved::Done(_)
    ));

    // The cache took that answer and then dropped it under its budget, so the
    // next paint misses on the very same key.
    assert!(is_pending(&desk.collect(&asset("/a.png"), 8)));
    assert!(is_pending(&desk.collect(&asset("/a.png"), 8)));
    assert_eq!(
        desk.next_job(),
        Some(job("/a.png", 8)),
        "a miss on a key the cache no longer holds is a genuine miss"
    );
    assert!(
        desk.next_job().is_none(),
        "and it is queued once, not once per asking paint"
    );
}

/// A refusal the *cache* made — no room the band allows — is not a reason to
/// decode again. Without this the desktop read and decoded every icon it drew
/// on every repaint, spending the disk and the parser sandbox precisely when
/// the machine was short of the memory that would have held the answer.
#[test]
fn a_declined_answer_is_not_offered_again_until_the_band_moves() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(8))));
    assert!(matches!(
        desk.collect(&asset("/a.png"), 8),
        Resolved::Done(_)
    ));
    desk.decline(&asset("/a.png"), 8);

    // Every later paint asks for nothing: the draw takes its glyph.
    for _ in 0..3 {
        assert!(is_pending(&desk.collect(&asset("/a.png"), 8)));
        assert!(!desk.has_work(), "a declined key queues no decode");
        assert!(desk.next_job().is_none());
    }

    // The band moving is what makes the answer worth having again.
    desk.retry_declined();
    assert!(is_pending(&desk.collect(&asset("/a.png"), 8)));
    assert_eq!(desk.next_job(), Some(job("/a.png", 8)));
}

/// The collect that hands an answer over forgets the key, so the refusal that
/// follows it names a key the desk is no longer holding. It must still be
/// recorded — that report is the whole of what stops the refusal renewing
/// itself.
#[test]
fn declining_a_key_the_collect_forgot_still_records_the_refusal() {
    let mut desk = ArtworkDesk::new();
    desk.decline(&asset("/gone.png"), 8);
    assert!(!desk.has_work());
    assert!(is_pending(&desk.collect(&asset("/gone.png"), 8)));
    assert!(
        desk.next_job().is_none(),
        "a refused key was offered for decoding again"
    );

    desk.retry_declined();
    assert!(is_pending(&desk.collect(&asset("/gone.png"), 8)));
    assert_eq!(desk.next_job(), Some(job("/gone.png", 8)));
}

/// A refusal reported while a producer is midway through that key leaves the
/// decode alone: it has yet to be answered, and the collect that answers it
/// will report the refusal again if it still stands.
#[test]
fn declining_a_key_in_flight_leaves_the_decode_to_land() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");

    desk.decline(&asset("/a.png"), 8);
    assert!(
        desk.deliver(&running, Some(picture(8))),
        "the decode in flight was stranded"
    );
    assert!(matches!(
        desk.collect(&asset("/a.png"), 8),
        Resolved::Done(Some(_))
    ));
}

/// Asking again never throws work away: an answer nobody has collected yet is
/// still there to collect, and a decode in flight is awaited rather than
/// re-recorded.
#[test]
fn asking_again_keeps_work_in_flight_and_answers_not_yet_collected() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/done.png"), 8);
    let _ = desk.collect(&asset("/running.png"), 8);
    let done = desk.next_job().expect("the first job");
    let running = desk.next_job().expect("the second job");
    assert!(desk.deliver(&done, Some(picture(8))));

    assert!(
        matches!(
            desk.collect(&asset("/done.png"), 8),
            Resolved::Done(Some(_))
        ),
        "an uncollected answer was discarded"
    );
    assert!(
        is_pending(&desk.collect(&asset("/running.png"), 8)),
        "a decode in flight was re-recorded rather than awaited"
    );
    assert!(!desk.has_work(), "and it was not queued a second time");
    assert!(desk.deliver(&running, Some(picture(8))));
}

/// A key re-asked after its answer was handed over is queued afresh, and
/// exactly once: the entry the first hand-out left behind must not become a
/// second one.
#[test]
fn a_key_re_asked_after_its_answer_is_handed_out_exactly_once() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(8))));
    assert!(matches!(
        desk.collect(&asset("/a.png"), 8),
        Resolved::Done(_)
    ));

    let _ = desk.collect(&asset("/a.png"), 8);
    assert_eq!(desk.next_job(), Some(job("/a.png", 8)));
    assert!(desk.next_job().is_none());
}

/// Teardown releases a decode nobody collected — overwriting its pixels first,
/// on the same terms the artwork cache wipes its own, so one user's rendered
/// artwork does not outlive their session in reusable heap.
#[test]
fn teardown_releases_a_decode_it_is_still_holding() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 4);
    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(4))));

    desk.stop();
    assert!(
        matches!(desk.collect(&asset("/a.png"), 4), Resolved::Pending),
        "a torn-down desk still holding an answer would serve wiped pixels"
    );
}

/// An answer for a job the desk is no longer holding — the embedder tore down
/// between the hand-out and the delivery — is dropped, and owes no wake.
#[test]
fn an_answer_the_desk_is_not_holding_is_dropped() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    let running = desk.next_job().expect("a job");
    desk.stop();
    assert!(!desk.deliver(&running, Some(picture(8))));
    assert!(!desk.take_landed());
}

#[test]
fn a_stopping_desk_records_nothing_and_hands_out_nothing() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&asset("/a.png"), 8);
    desk.stop();
    assert!(desk.stopping());
    assert!(!desk.has_work());
    assert!(desk.next_job().is_none());
    assert!(is_pending(&desk.collect(&asset("/b.png"), 8)));
    assert!(
        !desk.has_work(),
        "a stopping desk recorded a decode nobody will run"
    );
}

/// A bundle key and an asset key of the same spelling are different slots, so
/// resolving one can never serve the other's manifest-derived picture.
#[test]
fn a_bundle_and_an_asset_of_the_same_spelling_are_different_decodes() {
    let mut desk = ArtworkDesk::new();
    let _ = desk.collect(&ArtworkKey::Asset(String::from("/Apps/E.app")), 8);
    let _ = desk.collect(&ArtworkKey::Bundle(String::from("/Apps/E.app")), 8);
    assert!(desk.next_job().is_some());
    assert!(desk.next_job().is_some());
    assert!(desk.next_job().is_none());
}

/// The desk's own [`ArtworkResolver`] impl is the deferring resolver a
/// single-threaded embedder hands the cache: a miss records the decode and
/// answers `Pending`, and the answer is served once a job has been delivered.
#[test]
fn the_desk_answers_as_a_deferring_resolver() {
    let mut desk = ArtworkDesk::new();
    let key = asset("/a.png");
    assert!(is_pending(&ArtworkResolver::resolve(&mut desk, &key, 16)));

    let running = desk.next_job().expect("the miss recorded a decode");
    assert_eq!(running, job("/a.png", 16));
    assert!(desk.deliver(&running, Some(picture(16))));
    assert!(matches!(
        ArtworkResolver::resolve(&mut desk, &key, 16),
        Resolved::Done(Some(_))
    ));
}

/// `prefetch` and `declined` reach the same policy the inherent
/// [`ArtworkDesk::want`] / [`ArtworkDesk::decline`] do, so a cache driving the
/// desk through the trait cannot get different behaviour from one driving it
/// directly.
#[test]
fn the_resolver_impl_prefetches_and_declines_through_the_same_policy() {
    let mut desk = ArtworkDesk::new();
    let key = asset("/a.png");
    ArtworkResolver::prefetch(&mut desk, &key, 16);
    assert!(desk.has_work(), "a prefetch records the decode");

    let running = desk.next_job().expect("a job");
    assert!(desk.deliver(&running, Some(picture(16))));
    assert!(matches!(
        ArtworkResolver::resolve(&mut desk, &key, 16),
        Resolved::Done(Some(_))
    ));

    ArtworkResolver::declined(&mut desk, &key, 16);
    assert!(is_pending(&ArtworkResolver::resolve(&mut desk, &key, 16)));
    assert!(!desk.has_work(), "a declined key must not be re-offered");
}
