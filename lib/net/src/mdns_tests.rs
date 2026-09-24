//! Unit tests for the multicast DNS vocabulary.

use super::*;

fn name(dotted: &str) -> Name {
    Name::encode(dotted).expect("a test name encodes")
}

fn instance() -> Name {
    Name::from_labels(&[b"Hall Printer", b"_ipp", b"_tcp", b"local"])
        .expect("an instance name is labels, not a host name")
}

// -- TXT records ---------------------------------------------------------

#[test]
fn txt_accepts_a_run_of_length_prefixed_strings() {
    let txt = TxtRecord::new(b"\x05pdl=x\x03a=b").expect("well formed");
    let strings: alloc::vec::Vec<&[u8]> = txt.strings().collect();
    assert_eq!(strings, alloc::vec![&b"pdl=x"[..], &b"a=b"[..]]);
}

#[test]
fn txt_rejects_a_length_running_past_the_end() {
    assert_eq!(TxtRecord::new(b"\x09short"), Err(TxtError::Malformed));
}

#[test]
fn txt_rejects_more_than_the_fixed_bound() {
    let oversize = alloc::vec![0u8; MAX_TXT_LEN + 1];
    assert_eq!(TxtRecord::new(&oversize), Err(TxtError::TooLong));
}

#[test]
fn txt_empty_rdata_normalises_to_one_zero_length_string() {
    // One representation of "no attributes", so the codec round trip is a
    // fixed point and two equal records compare equal.
    let txt = TxtRecord::new(b"").expect("an empty TXT is the empty set");
    assert_eq!(txt, TxtRecord::empty());
    assert_eq!(txt.as_octets(), &[0]);
    let strings: alloc::vec::Vec<&[u8]> = txt.strings().collect();
    assert_eq!(strings, alloc::vec![&b""[..]]);
    assert_eq!(TxtRecord::new(&[0]).expect("well formed"), txt);
}

#[test]
fn txt_debug_does_not_print_peer_authored_octets() {
    let txt = TxtRecord::new(b"\x06secret").expect("well formed");
    let rendered = alloc::format!("{txt:?}");
    assert!(!rendered.contains("secret"), "{rendered}");
}

// -- NSEC type bitmaps ---------------------------------------------------

#[test]
fn type_bitmap_holds_exactly_what_was_inserted() {
    let mut bitmap = TypeBitmap::new();
    assert!(bitmap.is_empty());
    bitmap.insert(RecordType::Srv);
    bitmap.insert(RecordType::Txt);
    assert!(bitmap.contains(RecordType::Srv));
    assert!(bitmap.contains(RecordType::Txt));
    assert!(!bitmap.contains(RecordType::A));
    assert!(!bitmap.is_empty());
}

// -- question types ------------------------------------------------------

#[test]
fn any_is_a_question_type_and_never_a_record_type() {
    assert_eq!(QuestionType::from_value(255), Some(QuestionType::Any));
    assert_eq!(RecordType::from_value(255), None);
    assert!(QuestionType::Any.matches(RecordType::Nsec));
    assert!(QuestionType::Record(RecordType::Srv).matches(RecordType::Srv));
    assert!(!QuestionType::Record(RecordType::Srv).matches(RecordType::Txt));
}

#[test]
fn an_undecodable_question_type_is_refused_rather_than_guessed_at() {
    // MX: a real type this engine has no decoder for.
    assert_eq!(QuestionType::from_value(15), None);
}

// -- record shapes -------------------------------------------------------

#[test]
fn a_host_naming_record_takes_the_short_ttl() {
    let host = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    assert_eq!(host.ttl, TTL_HOST_SECS);
    assert!(host.cache_flush);

    let shared = Record::shared(name("_ipp._tcp.local"), RData::Ptr(instance()));
    assert_eq!(shared.ttl, TTL_OTHER_SECS);
    assert!(!shared.cache_flush);
}

#[test]
fn record_identity_ignores_ttl_and_the_cache_flush_bit() {
    let data = RData::A(Ipv4Addr::new(10, 0, 0, 5));
    let a = Record::unique(name("printer.local"), data);
    let mut b = Record::shared(name("PRINTER.local"), data);
    b.ttl = 7;
    assert!(a.same_record(&b), "case folds and TTL does not count");
}

// -- renaming after a conflict -------------------------------------------

#[test]
fn a_host_name_gains_a_dash_ordinal() {
    let first = rename(&name("printer.local"), NameKind::Host).expect("renames");
    assert_eq!(first, name("printer-2.local"));
    let second = rename(&first, NameKind::Host).expect("renames");
    assert_eq!(second, name("printer-3.local"));
}

#[test]
fn an_instance_name_gains_a_parenthesised_ordinal() {
    let first = rename(&instance(), NameKind::Instance).expect("renames");
    let labels: alloc::vec::Vec<&[u8]> = first.labels().collect();
    assert_eq!(labels[0], b"Hall Printer (2)");
    assert_eq!(&labels[1..], &[&b"_ipp"[..], b"_tcp", b"local"]);

    let second = rename(&first, NameKind::Instance).expect("renames");
    let labels: alloc::vec::Vec<&[u8]> = second.labels().collect();
    assert_eq!(labels[0], b"Hall Printer (3)");
}

#[test]
fn renaming_increments_rather_than_stacking_suffixes() {
    // The point of incrementing: eight conflicts must not leave a name
    // reading `printer-2-2-2-2-2-2-2-2`.
    let mut current = name("printer.local");
    for _ in 0..8 {
        current = rename(&current, NameKind::Host).expect("renames");
    }
    assert_eq!(current, name("printer-9.local"));
}

#[test]
fn a_stem_that_merely_looks_like_an_ordinal_is_left_alone() {
    // `x-1` is a name, not this engine's rename of `x`; renaming it must
    // still produce something, and must not eat the operator's own suffix
    // when there is no stem to keep.
    let renamed = rename(&name("-5.local"), NameKind::Host).expect("renames");
    assert_eq!(renamed, name("-5-2.local"));
}

#[test]
fn renaming_a_name_with_no_labels_is_refused() {
    assert!(rename(&Name::root(), NameKind::Host).is_err());
}

#[test]
fn renaming_past_the_name_bound_is_refused_rather_than_truncated() {
    let long = Name::from_labels(&[&[b'a'; 63], &[b'b'; 63], &[b'c'; 63], &[b'd'; 60]])
        .expect("just inside the bound");
    assert!(rename(&long, NameKind::Instance).is_err());
}

#[test]
fn only_a_name_under_local_or_a_link_local_address_is_the_links() {
    use super::{is_link_local_address, is_link_local_name};
    use crate::{IpAddr, Ipv4Addr, Ipv6Addr};
    for (name, links) in [
        ("printer.local", true),
        ("a.b.LOCAL", true),
        ("local", false),
        ("printer.localhost", false),
        ("printer.example.com", false),
        ("local.example", false),
    ] {
        assert_eq!(
            is_link_local_name(&Name::encode(name).expect("a name")),
            links,
            "{name}"
        );
    }
    for (address, links) in [
        (IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2)), true),
        (IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), false),
        (IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)), true),
        (IpAddr::V6(Ipv6Addr::new(0xfebf, 0, 0, 0, 0, 0, 0, 1)), true),
        (
            IpAddr::V6(Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 1)),
            false,
        ),
    ] {
        assert_eq!(is_link_local_address(address), links, "{address}");
    }
}
