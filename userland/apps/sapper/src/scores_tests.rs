//! Best-times store tests, driven over the shared fake app-data service so
//! they exercise the real `appdata-v1` codec rather than a private idea of it.

use super::*;

use tairix_abi::Errno;
use tairix_appdata::fake::FakeService;

use crate::board::Dimensions;

/// The command word this bundle is installed under.
const OWN_WORD: &str = "sapper";

/// The bundle directory the resolution order finds it at.
const BUNDLE: &str = "/System/Applications/sapper.app";

fn service() -> FakeService {
    FakeService::for_word(OWN_WORD).with_bundle(BUNDLE)
}

fn holding(text: &str) -> FakeService {
    service().with_store(text)
}

fn custom() -> Difficulty {
    Difficulty::Custom(Dimensions::new(12, 12, 20).expect("legal"))
}

#[test]
fn a_fresh_account_has_no_best_times() {
    let mut host = service();
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert!(times.is_empty());
    assert!(refused.is_empty());
    for preset in Difficulty::PRESETS {
        assert_eq!(times.best(preset), None);
    }
}

#[test]
fn a_recorded_time_survives_a_round_trip() {
    let mut host = service();
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        let mut times = BestTimes::default();
        assert!(times.record(Difficulty::Beginner, 42));
        assert!(times.record(Difficulty::Expert, 300));
        times.save(&mut settings).expect("the fake commits");
    }
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert!(refused.is_empty());
    assert_eq!(times.best(Difficulty::Beginner), Some(42));
    assert_eq!(times.best(Difficulty::Expert), Some(300));
    assert_eq!(times.best(Difficulty::Intermediate), None);
}

#[test]
fn only_a_faster_game_becomes_the_new_best() {
    let mut times = BestTimes::default();
    assert!(times.record(Difficulty::Beginner, 50));
    assert!(!times.record(Difficulty::Beginner, 60), "slower");
    assert!(!times.record(Difficulty::Beginner, 50), "the same");
    assert!(times.record(Difficulty::Beginner, 49), "faster");
    assert_eq!(times.best(Difficulty::Beginner), Some(49));
}

#[test]
fn a_custom_board_keeps_no_time() {
    let mut times = BestTimes::default();
    assert!(!times.record(custom(), 10));
    assert_eq!(times.best(custom()), None);
    assert!(times.is_empty());
}

#[test]
fn a_time_outside_the_bounds_is_not_a_record() {
    let mut times = BestTimes::default();
    assert!(!times.record(Difficulty::Beginner, 0), "a zero-second game");
    assert!(!times.record(Difficulty::Beginner, MAX_TIME_SECS + 1));
    assert!(!times.record(Difficulty::Beginner, u32::MAX));
    assert!(times.is_empty());
    assert!(times.record(Difficulty::Beginner, MAX_TIME_SECS));
}

#[test]
fn a_stored_time_past_the_bound_is_refused_and_named() {
    let mut host = holding("best.beginner = 100000\nbest.expert = 12\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert_eq!(times.best(Difficulty::Beginner), None);
    assert_eq!(
        times.best(Difficulty::Expert),
        Some(12),
        "one bad entry costs only itself"
    );
    assert_eq!(
        refused,
        alloc::vec![Refused {
            key: "best.beginner",
            reason: Reason::OutOfRange
        }]
    );
}

#[test]
fn a_stored_time_that_is_not_a_number_is_refused_and_named() {
    let mut host = holding("best.intermediate = fastest\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert_eq!(times.best(Difficulty::Intermediate), None);
    assert_eq!(
        refused,
        alloc::vec![Refused {
            key: "best.intermediate",
            reason: Reason::Malformed
        }]
    );
}

#[test]
fn a_stored_zero_is_refused_rather_than_read_as_a_record() {
    let mut host = holding("best.beginner = 0\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert_eq!(times.best(Difficulty::Beginner), None);
    assert_eq!(refused.len(), 1);
}

#[test]
fn a_save_writes_only_what_changed() {
    let mut host = service();
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        let mut times = BestTimes::default();
        times.record(Difficulty::Beginner, 30);
        times.save(&mut settings).expect("commits");
    }
    assert_eq!(host.committed().settings().count(), 1);
    assert_eq!(host.committed().get("best.beginner"), Some("30"));
    assert_eq!(host.committed().get("best.expert"), None);
}

#[test]
fn a_save_with_nothing_to_write_leaves_the_document_alone() {
    let mut host = holding("best.beginner = 30\n");
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        let (times, _) = BestTimes::load(&settings);
        times.save(&mut settings).expect("nothing to do");
        assert!(!settings.is_dirty(), "nothing was staged");
    }
    assert_eq!(host.committed().get("best.beginner"), Some("30"));
    assert_eq!(host.committed().settings().count(), 1);
}

#[test]
fn clearing_removes_the_stored_times() {
    let mut host = holding("best.beginner = 30\nbest.expert = 200\n");
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        let (mut times, _) = BestTimes::load(&settings);
        assert!(!times.is_empty());
        times.clear();
        assert!(times.is_empty());
        times.save(&mut settings).expect("commits");
    }
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert!(times.is_empty());
    assert!(refused.is_empty());
    assert_eq!(host.committed().get("best.beginner"), None);
}

#[test]
fn a_store_the_service_will_not_serve_leaves_the_game_playable() {
    let mut host = service();
    host.read_refusal().set(Some(Errno::NotFound));
    let settings = Settings::open(&mut host, OWN_WORD);
    let (times, refused) = BestTimes::load(&settings);
    assert!(times.is_empty(), "no times, and no panic");
    assert!(
        refused.is_empty(),
        "an absent store refused no single entry"
    );
}
