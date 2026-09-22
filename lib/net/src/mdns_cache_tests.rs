//! Unit tests for the bounded per-interface record cache.

use super::*;
use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::mdns::{RData, TxtRecord, MAX_RECORDS, MAX_RECORDS_PER_SOURCE};
use alloc::vec::Vec;

fn cache() -> RecordCache {
    RecordCache::new(HashSeed::UNKEYED)
}

fn at(secs: i64) -> Duration64 {
    Duration64::from_secs(secs)
}

fn host(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, n))
}

fn name(dotted: &str) -> Name {
    Name::encode(dotted).expect("a test name encodes")
}

fn a_record(dotted: &str, last: u8, ttl: u32) -> Record {
    let mut record = Record::unique(name(dotted), RData::A(Ipv4Addr::new(10, 0, 0, last)));
    record.ttl = ttl;
    record
}

fn ptr_record(owner: &str, target: &str, ttl: u32) -> Record {
    let mut record = Record::shared(name(owner), RData::Ptr(name(target)));
    record.ttl = ttl;
    record
}

fn held(cache: &RecordCache, dotted: &str, record_type: RecordType) -> Vec<Record> {
    cache
        .lookup(&name(dotted), record_type)
        .map(|cached| cached.record)
        .collect()
}

// -- learning and lookup -------------------------------------------------

#[test]
fn a_learned_record_is_found_by_name_and_type() {
    let mut cache = cache();
    let record = a_record("printer.local", 5, 120);
    assert_eq!(cache.learn(at(0), &record, host(1)), Learned::Added);
    assert_eq!(
        held(&cache, "printer.local", RecordType::A),
        alloc::vec![record]
    );
    assert_eq!(cache.len(), 1);
    assert!(held(&cache, "printer.local", RecordType::Aaaa).is_empty());
    assert!(held(&cache, "other.local", RecordType::A).is_empty());
}

#[test]
fn lookup_is_case_insensitive_because_dns_names_are() {
    let mut cache = cache();
    cache.learn(at(0), &a_record("Printer.LOCAL", 5, 120), host(1));
    assert_eq!(held(&cache, "printer.local", RecordType::A).len(), 1);
}

#[test]
fn two_shared_records_at_one_name_both_live_there() {
    let mut cache = cache();
    let first = ptr_record("_ipp._tcp.local", "a._ipp._tcp.local", 4500);
    let second = ptr_record("_ipp._tcp.local", "b._ipp._tcp.local", 4500);
    cache.learn(at(0), &first, host(1));
    cache.learn(at(0), &second, host(2));
    assert_eq!(held(&cache, "_ipp._tcp.local", RecordType::Ptr).len(), 2);
}

#[test]
fn relearning_a_held_record_renews_it_rather_than_duplicating_it() {
    let mut cache = cache();
    let record = a_record("printer.local", 5, 120);
    cache.learn(at(0), &record, host(1));
    assert_eq!(cache.learn(at(30), &record, host(1)), Learned::Refreshed);
    assert_eq!(cache.len(), 1);
    let entry = cache
        .lookup(&name("printer.local"), RecordType::A)
        .next()
        .expect("held");
    assert_eq!(entry.expires, at(150));
}

// -- goodbyes and the cache-flush bit ------------------------------------

#[test]
fn a_goodbye_retires_the_record_after_the_one_second_grace() {
    let mut cache = cache();
    let record = a_record("printer.local", 5, 120);
    cache.learn(at(0), &record, host(1));
    let mut goodbye = record;
    goodbye.ttl = 0;
    assert_eq!(cache.learn(at(10), &goodbye, host(1)), Learned::Retired);
    // Still held during the grace RFC 6762 §10.1 asks for.
    assert_eq!(cache.len(), 1);
    cache.advance(at(11), &mut |_, _| {});
    assert_eq!(cache.len(), 0);
}

#[test]
fn a_goodbye_from_another_host_cannot_delete_a_neighbours_record() {
    let mut cache = cache();
    let record = a_record("printer.local", 5, 120);
    cache.learn(at(0), &record, host(1));
    let mut goodbye = record;
    goodbye.ttl = 0;
    assert_eq!(cache.learn(at(10), &goodbye, host(2)), Learned::Ignored);
    cache.advance(at(20), &mut |_, _| {});
    assert_eq!(cache.len(), 1);
}

#[test]
fn the_cache_flush_bit_retires_that_sources_other_records_only() {
    let mut cache = cache();
    let old = a_record("printer.local", 5, 120);
    let other_host = a_record("printer.local", 9, 120);
    cache.learn(at(0), &old, host(1));
    cache.learn(at(0), &other_host, host(2));

    let fresh = a_record("printer.local", 6, 120);
    cache.learn(at(60), &fresh, host(1));
    cache.advance(at(62), &mut |_, _| {});

    let live = held(&cache, "printer.local", RecordType::A);
    assert!(live.contains(&fresh), "the announcing record stays");
    assert!(!live.contains(&old), "the same source's stale record goes");
    assert!(
        live.contains(&other_host),
        "another source's record is not the announcer's to flush"
    );
}

#[test]
fn the_cache_flush_bit_spares_a_record_received_in_the_last_second() {
    // A host announcing two addresses sends two messages; flushing on the
    // first would delete the second.
    let mut cache = cache();
    let first = a_record("printer.local", 5, 120);
    let second = a_record("printer.local", 6, 120);
    cache.learn(at(0), &first, host(1));
    cache.learn(at(0), &second, host(1));
    cache.advance(at(2), &mut |_, _| {});
    assert_eq!(held(&cache, "printer.local", RecordType::A).len(), 2);
}

// -- expiry and the refresh schedule -------------------------------------

#[test]
fn a_record_expires_at_its_ttl() {
    let mut cache = cache();
    cache.learn(at(0), &a_record("printer.local", 5, 100), host(1));
    cache.advance(at(99), &mut |_, _| {});
    assert_eq!(cache.len(), 1);
    cache.advance(at(100), &mut |_, _| {});
    assert_eq!(cache.len(), 0);
}

#[test]
fn an_unwatched_record_arms_nothing_before_its_expiry() {
    let mut cache = cache();
    cache.learn(at(0), &a_record("printer.local", 5, 100), host(1));
    assert_eq!(cache.next_deadline(), Some(at(100)));
}

#[test]
fn a_watched_record_is_refreshed_at_eighty_five_ninety_and_ninety_five_percent() {
    let mut cache = cache();
    cache.set_watched(&name("printer.local"), RecordType::A, true);
    cache.learn(at(0), &a_record("printer.local", 5, 100), host(1));
    assert_eq!(cache.next_deadline(), Some(at(80)));

    let mut points = Vec::new();
    for second in 0..=99 {
        cache.advance(at(second), &mut |asked, record_type| {
            assert_eq!(*asked, name("printer.local"));
            assert_eq!(record_type, RecordType::A);
            points.push(second);
        });
    }
    assert_eq!(points, alloc::vec![80, 85, 90, 95]);
}

#[test]
fn a_record_learned_while_a_question_is_live_inherits_the_schedule() {
    let mut cache = cache();
    cache.set_watched(&name("printer.local"), RecordType::A, true);
    cache.learn(at(0), &a_record("printer.local", 5, 100), host(1));
    // A second record at the same name and type joins the same watch.
    cache.learn(at(0), &a_record("printer.local", 6, 100), host(1));
    let mut refreshes = 0usize;
    for second in 0..=99 {
        cache.advance(at(second), &mut |_, _| refreshes += 1);
    }
    assert_eq!(refreshes, 8, "four points for each of two records");
}

#[test]
fn dropping_the_question_disarms_the_refresh() {
    let mut cache = cache();
    cache.set_watched(&name("printer.local"), RecordType::A, true);
    cache.learn(at(0), &a_record("printer.local", 5, 100), host(1));
    cache.set_watched(&name("printer.local"), RecordType::A, false);
    assert_eq!(cache.next_deadline(), Some(at(100)));
    let mut refreshed = false;
    for second in 0..=99 {
        cache.advance(at(second), &mut |_, _| refreshed = true);
    }
    assert!(!refreshed);
}

// -- known-answer freshness ----------------------------------------------

#[test]
fn holds_fresher_answers_the_known_answer_suppression_test() {
    let mut cache = cache();
    let record = a_record("printer.local", 5, 100);
    cache.learn(at(0), &record, host(1));
    assert!(cache.holds_fresher(at(0), &record), "full lifetime left");
    assert!(
        !cache.holds_fresher(at(60), &record),
        "less than half left no longer suppresses"
    );
    assert!(
        !cache.holds_fresher(at(0), &a_record("printer.local", 9, 100)),
        "a record the cache does not hold suppresses nothing"
    );
}

// -- the bounds ----------------------------------------------------------

#[test]
fn one_source_past_its_ceiling_evicts_its_own_oldest_record() {
    let mut cache = cache();
    let neighbour = a_record("neighbour.local", 1, 4500);
    cache.learn(at(0), &neighbour, host(2));

    for index in 0..MAX_RECORDS_PER_SOURCE {
        let owner = alloc::format!("flood{index}.local");
        cache.learn(at(1), &a_record(&owner, 5, 4500), host(1));
    }
    assert_eq!(cache.len_from(host(1)), MAX_RECORDS_PER_SOURCE);

    cache.learn(at(2), &a_record("one-more.local", 5, 4500), host(1));
    assert_eq!(
        cache.len_from(host(1)),
        MAX_RECORDS_PER_SOURCE,
        "a source cannot grow past its own ceiling"
    );
    assert!(
        held(&cache, "flood0.local", RecordType::A).is_empty(),
        "the flooder's own oldest record is what went"
    );
    assert_eq!(
        held(&cache, "neighbour.local", RecordType::A),
        alloc::vec![neighbour],
        "a neighbour's record is untouched by someone else's flood"
    );
}

#[test]
fn a_full_cache_evicts_the_least_recently_used_record() {
    let mut cache = cache();
    let sources = MAX_RECORDS / MAX_RECORDS_PER_SOURCE;
    let mut second = 0i64;
    for source in 0..sources {
        for index in 0..MAX_RECORDS_PER_SOURCE {
            let owner = alloc::format!("h{source}-{index}.local");
            let peer = IpAddr::V6(Ipv6Addr::new(
                0xfe80,
                0,
                0,
                0,
                0,
                0,
                0,
                u16::try_from(source).expect("small"),
            ));
            cache.learn(at(second), &a_record(&owner, 5, 4500), peer);
            second += 1;
        }
    }
    assert_eq!(cache.len(), MAX_RECORDS);

    cache.learn(at(second), &a_record("newcomer.local", 5, 4500), host(9));
    assert_eq!(cache.len(), MAX_RECORDS, "the ceiling holds");
    assert_eq!(held(&cache, "newcomer.local", RecordType::A).len(), 1);
    assert!(
        held(&cache, "h0-0.local", RecordType::A).is_empty(),
        "the oldest record is the one that went"
    );
}

#[test]
fn a_record_past_the_txt_bound_never_reaches_the_cache() {
    // The bound is enforced where the octets are validated, so an
    // over-long TXT cannot be built into a record at all.
    let oversize = alloc::vec![0u8; super::super::MAX_TXT_LEN + 1];
    assert!(TxtRecord::new(&oversize).is_err());
}

#[test]
fn a_cleared_cache_holds_nothing_and_arms_nothing() {
    let mut cache = cache();
    cache.learn(at(0), &a_record("printer.local", 5, 120), host(1));
    cache.clear();
    assert!(cache.is_empty());
    assert_eq!(cache.next_deadline(), None);
    assert_eq!(cache.len_from(host(1)), 0);
}

#[test]
fn a_freed_slot_is_reused_rather_than_growing_the_table() {
    let mut cache = cache();
    cache.learn(at(0), &a_record("first.local", 1, 10), host(1));
    cache.advance(at(10), &mut |_, _| {});
    assert!(cache.is_empty());
    cache.learn(at(11), &a_record("second.local", 2, 10), host(1));
    assert_eq!(held(&cache, "second.local", RecordType::A).len(), 1);
    assert!(held(&cache, "first.local", RecordType::A).is_empty());
}
