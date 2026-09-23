//! Deterministic fuzz harness for the multicast DNS engine.
//!
//! An mDNS datagram is unsolicited, unauthenticated, and arrives at a rate
//! its sender chooses, so the invariants here are the ones that keep a
//! hostile segment from being able to crash, wedge, or grow this host:
//!
//! 1. [`Message::parse`] never panics, on any bytes, and every record it
//!    surfaces is one the writer can put back on the wire.
//! 2. Driving the engine with arbitrary datagrams from arbitrary sources at
//!    arbitrary times never panics and always yields a coherent
//!    next-deadline decision.
//! 3. The cache never grows past its fixed bounds, and no source is ever
//!    holding more than its own share of it, whatever it sends.
//! 4. A datagram from off-link changes nothing at all.
//! 5. Every datagram the engine builds parses back.
//! 6. The DNS-SD grammar is total on any labels: a name that parses into an
//!    instance/type/domain triple spells back to the same name, and one
//!    that does not is refused rather than guessed at.
//! 7. Every `TXT` attribute the reader yields has a key the RFC's grammar
//!    admits, and `get` answers with the first occurrence of a repeated one.
//! 8. A record the builder produces reads back as exactly the attributes it
//!    accepted.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::time::Duration64;
use tairix_fuzzseed::Prng;
use tairix_hash::HashSeed;
use tairix_net::dns::{Name, RecordType};
use tairix_net::dnssd::{
    ServiceInstance, ServiceType, TxtAttributes, TxtBuilder, TxtValue, MAX_TXT_STRING_LEN,
};
use tairix_net::mdns::{
    Destination, LinkScope, MdnsConfig, MdnsEngine, NameKind, QuestionType, RData, Record,
    RecordCache, Service, TxtRecord, MAX_RECORDS, MAX_RECORDS_PER_SOURCE, MAX_TXT_LEN, PORT,
};
use tairix_net::route::Prefix;
use tairix_net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 2_000;

/// The datagram buffer the engine writes into.
const BUF: usize = 4096;

fn on_link(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
}

fn engine() -> MdnsEngine {
    let mut link = LinkScope::new();
    link.add_v4(Prefix::new(Ipv4Addr::new(192, 168, 1, 0), 24).expect("valid prefix"));
    MdnsEngine::new(MdnsConfig {
        link,
        hash_key: HashSeed::UNKEYED,
    })
}

/// A name drawn from a small set, so the engine explores real matches
/// rather than only failing to match.
fn draw_name(rng: &mut Prng) -> Name {
    const NAMES: [&str; 5] = [
        "printer.local",
        "scanner.local",
        "_ipp._tcp.local",
        "_http._tcp.local",
        "host.local",
    ];
    if rng.next_u64().is_multiple_of(4) {
        return Name::from_labels(&[b"Hall Printer", b"_ipp", b"_tcp", b"local"])
            .expect("labels encode");
    }
    Name::encode(rng.pick(&NAMES)).expect("fixed names encode")
}

fn draw_rdata(rng: &mut Prng) -> RData {
    match rng.next_u64() % 6 {
        0 => RData::A(Ipv4Addr::from(rng.next_u32().to_be_bytes())),
        1 => {
            let mut octets = [0u8; 16];
            rng.fill(&mut octets);
            RData::Aaaa(Ipv6Addr::from(octets))
        }
        2 => RData::Ptr(draw_name(rng)),
        3 => RData::Srv(Service {
            priority: rng.next_u16(),
            weight: rng.next_u16(),
            port: rng.next_u16(),
            target: draw_name(rng),
        }),
        4 => RData::Txt(draw_txt(rng)),
        _ => {
            let mut bitmap = tairix_net::mdns::TypeBitmap::new();
            for record_type in [
                RecordType::A,
                RecordType::Aaaa,
                RecordType::Ptr,
                RecordType::Srv,
                RecordType::Txt,
                RecordType::Nsec,
            ] {
                if rng.next_u64() & 1 == 0 {
                    bitmap.insert(record_type);
                }
            }
            RData::Nsec(bitmap)
        }
    }
}

fn alloc_vec(len: usize) -> Vec<u8> {
    vec![0u8; len]
}

/// A `TXT` record whose length-prefixed strings span exactly, with drawn
/// content — so the constructor is exercised on accepted input, and the
/// DNS-SD reader on attribute text no publisher would have written.
fn draw_txt(rng: &mut Prng) -> TxtRecord {
    let mut octets = alloc_vec(rng.below(MAX_TXT_LEN / 4));
    let mut pos = 0usize;
    while pos < octets.len() {
        let room = octets.len() - pos - 1;
        let take = if room == 0 { 0 } else { rng.below(room + 1) };
        octets[pos] = u8::try_from(take).unwrap_or(0);
        if let Some(body) = octets.get_mut(pos + 1..pos + 1 + take) {
            rng.fill(body);
        }
        pos += 1 + take;
    }
    TxtRecord::new(&octets).unwrap_or_else(|_| TxtRecord::empty())
}

fn draw_record(rng: &mut Prng) -> Record {
    let name = draw_name(rng);
    let data = draw_rdata(rng);
    let mut record = if rng.next_u64() & 1 == 0 {
        Record::unique(name, data)
    } else {
        Record::shared(name, data)
    };
    record.ttl = match rng.next_u64() % 4 {
        0 => 0,
        1 => 1,
        2 => 120,
        _ => u32::MAX,
    };
    record
}

/// Feed arbitrary bytes to the parser, and confirm every record it
/// surfaces can be written back out.
fn exercise_parse(bytes: &[u8]) {
    let Some(message) = tairix_net::mdns::Message::parse(bytes) else {
        return;
    };
    let mut out = [0u8; BUF];
    let mut writer = tairix_net::mdns::MessageWriter::new(&mut out, message.id, message.response)
        .expect("a header fits");
    for question in message.questions() {
        let _ = writer.push_question(&question);
    }
    for (section, record) in message.records() {
        let _ = writer.push_record(section, &record);
    }
    let len = writer.finish();
    // What the writer produced must itself be a valid message: a codec
    // whose own output it cannot read is one a peer cannot read either.
    assert!(
        tairix_net::mdns::Message::parse(&out[..len]).is_some(),
        "the writer's own output must parse"
    );
}

/// Drive the cache with arbitrary records from arbitrary sources and check
/// the bounds hold whatever is thrown at it.
fn exercise_cache(rng: &mut Prng) {
    let mut cache = RecordCache::new(HashSeed::UNKEYED);
    let mut sources = Vec::new();
    for _ in 0..64 {
        let now = Duration64::from_secs(i64::from(rng.next_u32() % 300));
        let record = draw_record(rng);
        let source = on_link(u8::try_from(rng.below(6)).unwrap_or(0));
        if !sources.contains(&source) {
            sources.push(source);
        }
        cache.learn(now, &record, source);
        if rng.next_u64().is_multiple_of(8) {
            cache.set_watched(&record.name, record.record_type(), rng.next_u64() & 1 == 0);
        }
        if rng.next_u64().is_multiple_of(8) {
            cache.advance(now, &mut |_, _| {});
        }
        let _ = cache.holds_fresher(now, &record);
        let _ = cache.lookup(&record.name, record.record_type()).count();

        assert!(cache.len() <= MAX_RECORDS, "the global bound holds");
        for source in &sources {
            assert!(
                cache.len_from(*source) <= MAX_RECORDS_PER_SOURCE,
                "no source holds more than its share"
            );
        }
        if let Some(deadline) = cache.next_deadline() {
            assert!(deadline.secs() >= 0);
        }
    }
}

/// Drive the whole engine with arbitrary datagrams at arbitrary times.
fn exercise_engine(rng: &mut Prng) {
    let mut csprng = Prng::new(rng.next_u64());
    let mut rand = || csprng.next_u32();
    let mut engine = engine();
    let mut buf = [0u8; BUF];

    for _ in 0..=rng.below(3) {
        let data = [draw_rdata(rng)];
        let kind = if rng.next_u64() & 1 == 0 {
            NameKind::Host
        } else {
            NameKind::Instance
        };
        let _ = engine.publish(
            Duration64::from_secs(0),
            draw_name(rng),
            kind,
            rng.next_u64() & 1 == 0,
            &data,
            &mut rand,
        );
    }
    for _ in 0..rng.below(3) {
        let qtype = if rng.next_u64() & 1 == 0 {
            QuestionType::Any
        } else {
            QuestionType::Record(RecordType::Ptr)
        };
        let _ = engine.ask(Duration64::from_secs(0), draw_name(rng), qtype, &mut rand);
    }

    let mut millis = 0u64;
    let mut datagram = [0u8; 512];
    for _ in 0..48 {
        millis = millis.saturating_add(u64::from(rng.next_u32() % 3_000));
        let now = Duration64::from_nanos(millis.saturating_mul(1_000_000));
        match rng.next_u64() % 3 {
            0 => {
                let size = rng.below(datagram.len() + 1);
                rng.fill(&mut datagram[..size]);
                let source = on_link(u8::try_from(rng.below(8)).unwrap_or(0));
                let port = if rng.next_u64() & 1 == 0 {
                    PORT
                } else {
                    rng.next_u16()
                };
                let emitted =
                    engine.on_message(now, &datagram[..size], source, port, &mut rand, &mut buf);
                check_emit(emitted.map(|emit| (emit.to, emit.len)), &buf);
            }
            1 => {
                // A structurally valid message, so the deeper paths are
                // reached rather than only the parser's rejection.
                let built = build_message(rng);
                let source = on_link(u8::try_from(rng.below(8)).unwrap_or(0));
                let emitted = engine.on_message(now, &built, source, PORT, &mut rand, &mut buf);
                check_emit(emitted.map(|emit| (emit.to, emit.len)), &buf);
            }
            _ => {
                while let Some(emit) = engine.poll(now, &mut rand, &mut buf) {
                    check_emit(Some((emit.to, emit.len)), &buf);
                }
            }
        }
        while engine.take_event().is_some() {}
        if let Some(deadline) = engine.next_deadline() {
            assert!(deadline.secs() >= 0);
        }
        assert!(engine.cache().len() <= MAX_RECORDS);
    }

    // An off-link source changes nothing at all: not the cache, not a
    // schedule, not a reply.
    let before = engine.cache().len();
    let deadline_before = engine.next_deadline();
    let built = build_message(rng);
    let off_link = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    let now = Duration64::from_nanos(millis.saturating_mul(1_000_000));
    assert!(engine
        .on_message(now, &built, off_link, PORT, &mut rand, &mut buf)
        .is_none());
    assert_eq!(engine.cache().len(), before);
    assert_eq!(engine.next_deadline(), deadline_before);
}

/// Drive the DNS-SD grammar with names and attributes a peer chose.
fn exercise_dnssd(rng: &mut Prng) {
    // Both shapes every call. A harness that leaves a structural case to a
    // coin flip can spend a whole run on one side of it.
    for with_instance in [true, false] {
        exercise_dnssd_name(&draw_dnssd_name(rng, with_instance));
    }
    // Labels of wholly arbitrary bytes: parsing must be total on those too,
    // not only on the shapes a publisher would have written.
    let mut random = alloc_vec(1 + rng.below(20));
    rng.fill(&mut random);
    if let Ok(name) = Name::from_labels(&[&random]) {
        exercise_dnssd_name(&name);
    }
    exercise_dnssd_txt(rng);
    exercise_dnssd_builder(rng);
}

/// A name assembled from parts that are sometimes legal and sometimes not,
/// so both the accepting and the refusing paths are reached.
fn draw_dnssd_name(rng: &mut Prng, with_instance: bool) -> Name {
    const INSTANCES: [&[u8]; 4] = [
        b"Hall Printer",
        "Caf\u{e9}".as_bytes(),
        b"x",
        b"bad\x01name",
    ];
    const SERVICES: [&[u8]; 5] = [b"_ipp", b"_IPP", b"_dns-sd", b"ipp", b"_-bad"];
    const TRANSPORTS: [&[u8]; 4] = [b"_tcp", b"_UDP", b"_sctp", b"local"];
    const DOMAINS: [&[u8]; 3] = [b"local", b"example", b"com"];

    let mut labels: Vec<&[u8]> = Vec::new();
    if with_instance {
        labels.push(*rng.pick(&INSTANCES));
    }
    labels.push(*rng.pick(&SERVICES));
    labels.push(*rng.pick(&TRANSPORTS));
    for _ in 0..rng.below(4) {
        labels.push(*rng.pick(&DOMAINS));
    }
    Name::from_labels(&labels).unwrap_or_else(|_| Name::root())
}

/// Whatever a name spells, reading it is total — and what it parses into
/// must spell the same name back.
fn exercise_dnssd_name(name: &Name) {
    if let Ok(triple) = ServiceInstance::from_name(name) {
        let spelled = triple
            .to_name()
            .expect("a triple parsed from a name is short enough to be that name again");
        assert_eq!(spelled, *name, "a parsed instance must spell back");
        assert_eq!(
            ServiceInstance::from_name(&spelled),
            Ok(triple),
            "and parse back to itself"
        );
    }
    if let Ok((service, domain)) = ServiceType::from_name(name) {
        let spelled = service
            .to_name(&domain)
            .expect("a type parsed from a name is short enough to be that name again");
        assert_eq!(spelled, *name, "a parsed service type must spell back");
        assert_eq!(ServiceType::from_name(&spelled), Ok((service, domain)));
    }
}

/// Every attribute the reader yields is one the RFC's grammar admits, and
/// `get` answers with the first occurrence of a repeated key.
fn exercise_dnssd_txt(rng: &mut Prng) {
    let record = draw_txt(rng);
    let mut first_seen: Vec<&[u8]> = Vec::new();
    for attribute in TxtAttributes::new(&record) {
        assert!(!attribute.key.is_empty(), "an empty key is never yielded");
        assert!(
            attribute
                .key
                .iter()
                .all(|byte| (0x20..=0x7E).contains(byte) && *byte != b'='),
            "a key outside the grammar is never yielded"
        );
        assert!(attribute.has_key(attribute.key));
        let found = TxtAttributes::new(&record).get(attribute.key);
        if first_seen
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(attribute.key))
        {
            // A repeat never displaces the value already published.
            assert_ne!(found, None);
        } else {
            assert_eq!(found, Some(attribute.value), "the first occurrence wins");
            first_seen.push(attribute.key);
        }
    }
}

/// A record the builder produces reads back as exactly what it accepted.
fn exercise_dnssd_builder(rng: &mut Prng) {
    let mut builder = TxtBuilder::new();
    let mut accepted: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    for _ in 0..rng.below(10) {
        // Keys are drawn inside the printable range so the accepting path
        // is reached; the refusals have their own unit tests.
        let key: Vec<u8> = (0..=rng.below(6))
            .map(|_| {
                let byte = u8::try_from(0x20 + rng.below(0x5F)).unwrap_or(b'k');
                if byte == b'=' {
                    b'k'
                } else {
                    byte
                }
            })
            .collect();
        let mut value = alloc_vec(rng.below(MAX_TXT_STRING_LEN));
        rng.fill(&mut value);
        let drawn = if rng.next_u64() & 1 == 0 {
            TxtValue::Flag
        } else {
            TxtValue::Value(&value)
        };
        if builder.push(&key, drawn).is_ok() {
            accepted.push((
                key,
                match drawn {
                    TxtValue::Flag => None,
                    TxtValue::Value(bytes) => Some(bytes.to_vec()),
                },
            ));
        }
    }
    let record = builder
        .build()
        .expect("a builder's own output is well formed");
    let read: Vec<(Vec<u8>, Option<Vec<u8>>)> = TxtAttributes::new(&record)
        .map(|attribute| {
            (
                attribute.key.to_vec(),
                match attribute.value {
                    TxtValue::Flag => None,
                    TxtValue::Value(bytes) => Some(bytes.to_vec()),
                },
            )
        })
        .collect();
    assert_eq!(read, accepted, "a built record reads back as it was given");
}

/// Anything the engine emits must be a message a peer could read, and must
/// name somewhere to send it.
fn check_emit(emitted: Option<(Destination, usize)>, buf: &[u8]) {
    let Some((to, len)) = emitted else {
        return;
    };
    assert!(len <= buf.len());
    assert!(
        tairix_net::mdns::Message::parse(&buf[..len]).is_some(),
        "the engine must only emit messages it can itself read"
    );
    if let Destination::Peer { port, .. } = to {
        assert_ne!(port, 0, "a reply must name a port to reach");
    }
}

/// Build a structurally valid message out of drawn parts.
fn build_message(rng: &mut Prng) -> Vec<u8> {
    let mut out = [0u8; BUF];
    let len = {
        let mut writer =
            tairix_net::mdns::MessageWriter::new(&mut out, rng.next_u16(), rng.next_u64() & 1 == 0)
                .expect("a header fits");
        for _ in 0..rng.below(3) {
            let qtype = match rng.next_u64() % 3 {
                0 => QuestionType::Any,
                1 => QuestionType::Record(RecordType::Ptr),
                _ => QuestionType::Record(RecordType::A),
            };
            let mut question = tairix_net::mdns::Question::new(draw_name(rng), qtype);
            question.unicast_response = rng.next_u64() & 1 == 0;
            let _ = writer.push_question(&question);
        }
        for _ in 0..rng.below(6) {
            let section = match rng.next_u64() % 3 {
                0 => tairix_net::mdns::Section::Answer,
                1 => tairix_net::mdns::Section::Authority,
                _ => tairix_net::mdns::Section::Additional,
            };
            let _ = writer.push_record(section, &draw_record(rng));
        }
        if rng.next_u64().is_multiple_of(8) {
            writer.set_truncated();
        }
        writer.finish()
    };
    out[..len].to_vec()
}

#[test]
fn random_inputs_never_panic() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 640];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = rng.below(buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size]);
            exercise_parse(&build_message(&mut rng));
            exercise_cache(&mut rng);
            exercise_engine(&mut rng);
            exercise_dnssd(&mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
