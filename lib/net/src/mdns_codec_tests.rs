//! Unit tests for the multicast DNS message codec.

use super::*;
use crate::mdns::{Service, MAX_TXT_LEN};
use alloc::vec::Vec;

fn name(dotted: &str) -> Name {
    Name::encode(dotted).expect("a test name encodes")
}

fn instance() -> Name {
    Name::from_labels(&[b"Hall Printer", b"_ipp", b"_tcp", b"local"]).expect("labels encode")
}

/// Build a message with `fill`, returning the octets.
fn build(id: u16, response: bool, fill: impl FnOnce(&mut MessageWriter<'_>)) -> Vec<u8> {
    let mut buf = [0u8; 2048];
    let len = {
        let mut writer = MessageWriter::new(&mut buf, id, response).expect("a header fits");
        fill(&mut writer);
        writer.finish()
    };
    buf[..len].to_vec()
}

fn records_of(bytes: &[u8]) -> Vec<(Section, Record)> {
    Message::parse(bytes)
        .expect("valid message")
        .records()
        .collect()
}

// -- round trips ---------------------------------------------------------

#[test]
fn a_question_round_trips_with_its_unicast_bit() {
    let mut question = Question::new(
        name("_ipp._tcp.local"),
        QuestionType::Record(RecordType::Ptr),
    );
    question.unicast_response = true;
    let bytes = build(0, false, |writer| {
        assert!(writer.push_question(&question));
    });
    let message = Message::parse(&bytes).expect("valid");
    assert!(!message.response);
    let read: Vec<Question> = message.questions().collect();
    assert_eq!(read, alloc::vec![question]);
}

#[test]
fn every_record_type_round_trips() {
    let txt = TxtRecord::new(b"\x07pdl=pdf").expect("well formed");
    let mut bitmap = TypeBitmap::new();
    bitmap.insert(RecordType::Srv);
    bitmap.insert(RecordType::Txt);
    bitmap.insert(RecordType::Nsec);
    let originals = alloc::vec![
        Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5))),
        Record::unique(
            name("printer.local"),
            RData::Aaaa(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
        ),
        Record::shared(name("_ipp._tcp.local"), RData::Ptr(instance())),
        Record::unique(
            instance(),
            RData::Srv(Service {
                priority: 0,
                weight: 0,
                port: 631,
                target: name("printer.local"),
            }),
        ),
        Record::unique(instance(), RData::Txt(txt)),
        Record::unique(instance(), RData::Nsec(bitmap)),
    ];
    let bytes = build(0, true, |writer| {
        for record in &originals {
            assert!(writer.push_record(Section::Answer, record), "{record:?}");
        }
    });
    let read: Vec<Record> = records_of(&bytes)
        .into_iter()
        .map(|(section, record)| {
            assert_eq!(section, Section::Answer);
            record
        })
        .collect();
    assert_eq!(read, originals);
}

#[test]
fn a_response_is_authoritative_and_a_query_is_not() {
    let bytes = build(0, true, |writer| {
        assert!(writer.push_record(
            Section::Answer,
            &Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5))),
        ));
    });
    let message = Message::parse(&bytes).expect("valid");
    assert!(message.response && message.authoritative);

    let bytes = build(0, false, |writer| {
        assert!(writer.push_question(&Question::new(name("printer.local"), QuestionType::Any)));
    });
    let message = Message::parse(&bytes).expect("valid");
    assert!(!message.response && !message.authoritative);
}

#[test]
fn the_sections_are_kept_apart() {
    let a = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    let bytes = build(0, false, |writer| {
        assert!(writer.push_question(&Question::new(name("printer.local"), QuestionType::Any)));
        assert!(writer.push_record(Section::Authority, &a));
        assert!(writer.push_record(Section::Additional, &a));
    });
    let sections: Vec<Section> = records_of(&bytes)
        .into_iter()
        .map(|(section, _)| section)
        .collect();
    assert_eq!(
        sections,
        alloc::vec![Section::Authority, Section::Additional]
    );
}

#[test]
fn a_section_may_not_be_written_backwards() {
    let a = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    let mut buf = [0u8; 512];
    let mut writer = MessageWriter::new(&mut buf, 0, true).expect("header fits");
    assert!(writer.push_record(Section::Additional, &a));
    assert!(!writer.push_record(Section::Answer, &a));
    assert!(!writer.push_question(&Question::new(name("printer.local"), QuestionType::Any)));
}

#[test]
fn the_truncation_bit_survives_the_round_trip() {
    let bytes = build(0, false, |writer| {
        assert!(writer.push_question(&Question::new(name("x.local"), QuestionType::Any)));
        writer.set_truncated();
    });
    assert!(Message::parse(&bytes).expect("valid").truncated);
}

// -- compression ---------------------------------------------------------

#[test]
fn a_repeated_name_is_written_as_a_pointer() {
    let a = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    let b = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 6)));
    let one = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &a));
    });
    let two = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &a));
        assert!(writer.push_record(Section::Answer, &b));
    });
    // The second record's owner costs a two-octet pointer rather than the
    // fourteen octets of `printer.local`.
    assert_eq!(two.len() - one.len(), 2 + 2 + 2 + 4 + 2 + 4);
    assert_eq!(records_of(&two).len(), 2);
}

#[test]
fn a_shared_suffix_is_compressed_and_still_expands() {
    let ptr = Record::shared(name("_ipp._tcp.local"), RData::Ptr(instance()));
    let txt = Record::unique(instance(), RData::Txt(TxtRecord::empty()));
    let bytes = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &ptr));
        assert!(writer.push_record(Section::Answer, &txt));
    });
    let read = records_of(&bytes);
    assert_eq!(read[0].1, ptr);
    assert_eq!(read[1].1, txt);
    // The instance name appears once in full; the second use points at it.
    let occurrences = bytes
        .windows(b"Hall Printer".len())
        .filter(|window| *window == b"Hall Printer")
        .count();
    assert_eq!(occurrences, 1);
}

#[test]
fn an_srv_target_is_never_compressed() {
    // RFC 3597 §4 forbids compressing a name inside the rdata of a type
    // defined after RFC 1035, and a receiver that expands one anyway must
    // not be what this depends on.
    let a = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    let srv = Record::unique(
        instance(),
        RData::Srv(Service {
            priority: 0,
            weight: 0,
            port: 631,
            target: name("printer.local"),
        }),
    );
    let bytes = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &a));
        assert!(writer.push_record(Section::Answer, &srv));
    });
    let occurrences = bytes
        .windows(b"printer".len())
        .filter(|window| *window == b"printer")
        .count();
    assert_eq!(occurrences, 2, "the target is spelled out, not pointed at");
    assert_eq!(records_of(&bytes)[1].1, srv);
}

#[test]
fn a_record_that_does_not_fit_leaves_the_buffer_exactly_as_it_was() {
    let a = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    let big = Record::unique(
        name("printer.local"),
        RData::Txt(TxtRecord::new(&alloc::vec![0u8; 200]).expect("well formed")),
    );
    let mut buf = [0u8; 64];
    let len = {
        let mut writer = MessageWriter::new(&mut buf, 0, true).expect("header fits");
        assert!(writer.push_record(Section::Answer, &a));
        assert!(!writer.push_record(Section::Answer, &big));
        writer.finish()
    };
    assert_eq!(records_of(&buf[..len]), alloc::vec![(Section::Answer, a)]);
}

// -- rejection -----------------------------------------------------------

#[test]
fn a_short_header_is_refused() {
    assert!(Message::parse(&[0u8; 11]).is_none());
}

#[test]
fn a_non_zero_opcode_or_rcode_is_refused() {
    let mut bytes = build(0, false, |writer| {
        assert!(writer.push_question(&Question::new(name("x.local"), QuestionType::Any)));
    });
    let mut with_opcode = bytes.clone();
    with_opcode[2] |= 0x08;
    assert!(Message::parse(&with_opcode).is_none());
    bytes[3] |= 0x03;
    assert!(Message::parse(&bytes).is_none());
}

#[test]
fn a_count_that_the_body_does_not_back_is_refused() {
    let mut bytes = build(0, true, |writer| {
        assert!(writer.push_record(
            Section::Answer,
            &Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5))),
        ));
    });
    bytes[6..8].copy_from_slice(&2u16.to_be_bytes());
    assert!(Message::parse(&bytes).is_none());
}

#[test]
fn a_forward_compression_pointer_is_refused() {
    // A pointer that does not point strictly backwards is how a name is
    // made to loop; the reader refuses it rather than following it.
    let mut bytes = alloc::vec![0u8; 12];
    bytes[4..6].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&[0xC0, 0x20]);
    bytes.extend_from_slice(&[0, 12, 0, 1]);
    assert!(Message::parse(&bytes).is_none());
}

#[test]
fn a_self_referential_pointer_is_refused() {
    let mut bytes = alloc::vec![0u8; 12];
    bytes[4..6].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&[0xC0, 0x0C]);
    bytes.extend_from_slice(&[0, 12, 0, 1]);
    assert!(Message::parse(&bytes).is_none());
}

#[test]
fn an_rdata_length_past_the_message_is_refused() {
    let mut bytes = build(0, true, |writer| {
        assert!(writer.push_record(
            Section::Answer,
            &Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5))),
        ));
    });
    let rdlength_at = bytes.len() - 6;
    bytes[rdlength_at..rdlength_at + 2].copy_from_slice(&600u16.to_be_bytes());
    assert!(Message::parse(&bytes).is_none());
}

#[test]
fn more_records_than_the_bound_admits_is_refused() {
    let mut bytes = alloc::vec![0u8; 12];
    let too_many = u16::try_from(MAX_MESSAGE_RECORDS + 1).expect("small");
    bytes[6..8].copy_from_slice(&too_many.to_be_bytes());
    assert!(Message::parse(&bytes).is_none());
}

// -- skipping rather than guessing ---------------------------------------

#[test]
fn a_record_of_an_unknown_type_is_skipped_and_the_rest_still_reads() {
    let a = Record::unique(name("printer.local"), RData::A(Ipv4Addr::new(10, 0, 0, 5)));
    let mut bytes = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &a));
    });
    // Append an MX record (type 15), which this engine has no decoder for.
    bytes.extend_from_slice(&[0xC0, 0x0C]);
    bytes.extend_from_slice(&15u16.to_be_bytes());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&120u32.to_be_bytes());
    bytes.extend_from_slice(&2u16.to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes[6..8].copy_from_slice(&2u16.to_be_bytes());
    assert_eq!(records_of(&bytes), alloc::vec![(Section::Answer, a)]);
}

#[test]
fn an_a_record_of_the_wrong_length_is_skipped_not_reinterpreted() {
    let mut bytes = alloc::vec![0u8; 12];
    bytes[6..8].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(name("printer.local").as_wire());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&120u32.to_be_bytes());
    bytes.extend_from_slice(&16u16.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    assert!(
        records_of(&bytes).is_empty(),
        "sixteen octets is not an IPv4 address"
    );
}

#[test]
fn a_record_of_a_class_other_than_in_is_skipped() {
    let mut bytes = alloc::vec![0u8; 12];
    bytes[6..8].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(name("printer.local").as_wire());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&3u16.to_be_bytes());
    bytes.extend_from_slice(&120u32.to_be_bytes());
    bytes.extend_from_slice(&4u16.to_be_bytes());
    bytes.extend_from_slice(&[10, 0, 0, 5]);
    assert!(records_of(&bytes).is_empty());
}

#[test]
fn an_nsec_naming_someone_elses_owner_is_dropped() {
    // RFC 6762 §6.1 fixes the next-domain field to the owner name; a record
    // that says otherwise asserts absence on a name it does not own.
    let mut bitmap = TypeBitmap::new();
    bitmap.insert(RecordType::A);
    let honest = Record::unique(name("printer.local"), RData::Nsec(bitmap));
    let bytes = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &honest));
    });
    assert_eq!(records_of(&bytes).len(), 1);

    // Rewrite the next-domain field to a different name of the same length.
    let mut forged = bytes.clone();
    let owner = name("printer.local");
    let needle = owner.as_wire();
    let first = forged
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("the owner name is in the message");
    let second = forged[first + 1..]
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("the next-domain field repeats it")
        + first
        + 1;
    forged[second + 1] = b'X';
    assert!(records_of(&forged).is_empty());
}

#[test]
fn an_nsec_type_bitmap_beyond_the_first_window_is_ignored_not_refused() {
    let mut bytes = alloc::vec![0u8; 12];
    bytes[6..8].copy_from_slice(&1u16.to_be_bytes());
    let owner = name("printer.local");
    bytes.extend_from_slice(owner.as_wire());
    bytes.extend_from_slice(&47u16.to_be_bytes());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&120u32.to_be_bytes());
    // rdata: the owner name, window 0 with type 1 set, then window 1.
    let mut rdata = owner.as_wire().to_vec();
    rdata.extend_from_slice(&[0, 1, 0x40]);
    rdata.extend_from_slice(&[1, 1, 0x80]);
    bytes.extend_from_slice(&u16::try_from(rdata.len()).expect("small").to_be_bytes());
    bytes.extend_from_slice(&rdata);

    let read = records_of(&bytes);
    assert_eq!(read.len(), 1);
    let RData::Nsec(bitmap) = read[0].1.data else {
        panic!("an NSEC record");
    };
    assert!(bitmap.contains(RecordType::A));
    assert!(!bitmap.contains(RecordType::Nsec));
}

#[test]
fn an_nsec_bitmap_with_a_bad_window_length_is_skipped() {
    let mut bytes = alloc::vec![0u8; 12];
    bytes[6..8].copy_from_slice(&1u16.to_be_bytes());
    let owner = name("printer.local");
    bytes.extend_from_slice(owner.as_wire());
    bytes.extend_from_slice(&47u16.to_be_bytes());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&120u32.to_be_bytes());
    let mut rdata = owner.as_wire().to_vec();
    rdata.extend_from_slice(&[0, 33]);
    rdata.extend_from_slice(&[0u8; 33]);
    bytes.extend_from_slice(&u16::try_from(rdata.len()).expect("small").to_be_bytes());
    bytes.extend_from_slice(&rdata);
    assert!(records_of(&bytes).is_empty());
}

#[test]
fn an_empty_txt_is_emitted_as_one_zero_length_string() {
    let record = Record::unique(instance(), RData::Txt(TxtRecord::empty()));
    let bytes = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &record));
    });
    // The last two octets are the rdata length and the one empty string.
    assert_eq!(&bytes[bytes.len() - 3..], &[0, 1, 0]);
    assert_eq!(records_of(&bytes)[0].1, record);
}

#[test]
fn a_txt_at_the_bound_round_trips() {
    let octets = {
        let mut octets = alloc::vec![0u8; MAX_TXT_LEN];
        // Two maximal strings and a remainder, so the whole span is valid.
        octets[0] = 255;
        octets[256] = 255;
        octets
    };
    let txt = TxtRecord::new(&octets).expect("well formed at the bound");
    let record = Record::unique(instance(), RData::Txt(txt));
    let bytes = build(0, true, |writer| {
        assert!(writer.push_record(Section::Answer, &record));
    });
    assert_eq!(records_of(&bytes)[0].1, record);
}

#[test]
fn a_message_that_asks_and_asserts_nothing_is_empty() {
    let bytes = build(0, false, |_| {});
    assert!(Message::parse(&bytes)
        .expect("a bare header is valid")
        .is_empty());
}
