//! Unit tests for the DNS stub-resolver engine.

use super::*;
use alloc::vec::Vec;
use tairix_abi::time::Duration64;

fn secs(s: i64) -> Duration64 {
    Duration64::from_secs(s)
}

/// A counting RNG: distinct value each call, so query ids and jitter are
/// deterministic and reproducible in tests.
fn counter() -> impl FnMut() -> u32 {
    let mut n: u32 = 0x1234_0000;
    move || {
        n = n.wrapping_add(0x0001_0001);
        n
    }
}

/// Append the wire encoding of a dotted name (no compression) to `out`.
fn push_name(out: &mut Vec<u8>, dotted: &str) {
    out.extend_from_slice(Name::encode(dotted).unwrap().as_wire());
}

/// Options controlling a synthetic response built by [`build_response`].
struct RespOpts<'a> {
    id: u16,
    qname: &'a str,
    qtype: u16,
    /// `(QR, opcode, TC, RD, RA)` collapsed into the raw flags high bits we
    /// vary; the RCODE is the `rcode` field.
    qr: bool,
    opcode: u16,
    tc: bool,
    rcode: u8,
    /// Whether to echo a question section (qdcount = 1).
    with_question: bool,
    /// The answer records: `(owner, rtype, rclass, ttl, rdata)`.
    answers: &'a [(&'a str, u16, u16, u32, Vec<u8>)],
}

fn build_response(o: &RespOpts<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut flags: u16 = 0;
    if o.qr {
        flags |= 0x8000;
    }
    flags |= (o.opcode & 0xF) << 11;
    if o.tc {
        flags |= 0x0200;
    }
    flags |= u16::from(o.rcode) & 0x000F;
    out.extend_from_slice(&o.id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    let qd: u16 = u16::from(o.with_question);
    out.extend_from_slice(&qd.to_be_bytes());
    out.extend_from_slice(&u16::try_from(o.answers.len()).unwrap().to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    if o.with_question {
        push_name(&mut out, o.qname);
        out.extend_from_slice(&o.qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
    }
    for (owner, rtype, rclass, ttl, rdata) in o.answers {
        push_name(&mut out, owner);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&rclass.to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        out.extend_from_slice(&u16::try_from(rdata.len()).unwrap().to_be_bytes());
        out.extend_from_slice(rdata);
    }
    out
}

/// A standard positive-answer response builder for the common case.
fn ok_response(
    id: u16,
    qname: &str,
    qtype: u16,
    answers: &[(&str, u16, u16, u32, Vec<u8>)],
) -> Vec<u8> {
    build_response(&RespOpts {
        id,
        qname,
        qtype,
        qr: true,
        opcode: 0,
        tc: false,
        rcode: 0,
        with_question: true,
        answers,
    })
}

fn query_spec(id: u16, name: &str, rt: RecordType) -> QuerySpec {
    QuerySpec {
        id,
        name: Name::encode(name).unwrap(),
        record_type: rt,
        recursion_desired: true,
    }
}

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

fn send_parts(action: &Action) -> (QuerySpec, IpAddr) {
    match action {
        Action::Send { query, server } => (*query, *server),
        Action::Finished(_) => panic!("expected Send, got Finished"),
    }
}

fn finished(action: &Action) -> Resolution {
    match action {
        Action::Finished(res) => *res,
        Action::Send { .. } => panic!("expected Finished, got Send"),
    }
}

const SERVERS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
];

// -- Name encoding / decoding --------------------------------------------

#[test]
fn name_encode_round_trips_and_folds_case() {
    let a = Name::encode("WWW.Example.COM").unwrap();
    let b = Name::encode("www.example.com").unwrap();
    assert_eq!(a, b, "names compare case-insensitively");
    // Wire form: 3www7example3com0
    assert_eq!(
        b.as_wire(),
        &[
            3, b'w', b'w', b'w', 7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm',
            0
        ]
    );
}

#[test]
fn name_trailing_dot_is_fully_qualified_form() {
    assert_eq!(
        Name::encode("example.com.").unwrap(),
        Name::encode("example.com").unwrap()
    );
}

#[test]
fn name_empty_and_dot_are_root() {
    assert_eq!(Name::encode("").unwrap(), Name::root());
    assert_eq!(Name::encode(".").unwrap(), Name::root());
    assert_eq!(Name::root().as_wire(), &[0]);
}

#[test]
fn name_rejects_bad_labels() {
    assert_eq!(Name::encode("a..b"), Err(DnsError::InvalidLabel));
    assert_eq!(Name::encode(".leading"), Err(DnsError::InvalidLabel));
    let too_long_label = "a".repeat(64);
    assert_eq!(Name::encode(&too_long_label), Err(DnsError::InvalidLabel));
    assert_eq!(Name::encode("bad label.com"), Err(DnsError::InvalidLabel));
    assert_eq!(Name::encode("exämple.com"), Err(DnsError::InvalidLabel));
}

#[test]
fn name_rejects_over_length() {
    // 4 * "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    // labels (63 each) exceed the 255-octet ceiling.
    let label = "a".repeat(63);
    let name = [label.as_str(); 5].join(".");
    assert_eq!(Name::encode(&name), Err(DnsError::NameTooLong));
}

#[test]
fn name_read_expands_compression_pointer() {
    let mut msg = alloc::vec![0u8; 12];
    let base_off = msg.len();
    let base = Name::encode("example.com").unwrap();
    msg.extend_from_slice(base.as_wire());
    // "www" + pointer to base_off.
    let comp_off = msg.len();
    msg.push(3);
    msg.extend_from_slice(b"www");
    msg.push(0xC0 | u8::try_from(base_off >> 8).unwrap());
    msg.push(u8::try_from(base_off & 0xFF).unwrap());
    let (name, next) = Name::read(&msg, comp_off).unwrap();
    assert_eq!(name, Name::encode("www.example.com").unwrap());
    assert_eq!(next, msg.len(), "next offset is just past the pointer");
}

#[test]
fn name_read_rejects_forward_pointer_loop() {
    // A pointer that does not point strictly backwards is rejected, so a
    // crafted loop can never hang the parser.
    let mut msg = alloc::vec![0u8; 20];
    msg[10] = 0xC0;
    msg[11] = 12; // 12 >= 10 (the pointer's own offset): forward, rejected.
    assert!(Name::read(&msg, 10).is_none());
    // A self-pointer is likewise rejected.
    msg[10] = 0xC0;
    msg[11] = 10;
    assert!(Name::read(&msg, 10).is_none());
}

#[test]
fn name_read_rejects_reserved_label_type() {
    // 0x40 / 0x80 high bits are reserved label types.
    let msg = [0x40u8, 0, 0];
    assert!(Name::read(&msg, 0).is_none());
}

// -- Query encoding ------------------------------------------------------

#[test]
fn write_query_emits_header_and_question() {
    let spec = query_spec(0xABCD, "example.com", RecordType::A);
    let mut buf = [0u8; MAX_QUERY_LEN];
    let n = write_query(&spec, &mut buf).unwrap();
    assert_eq!(&buf[0..2], &[0xAB, 0xCD]);
    assert_eq!(&buf[2..4], &[0x01, 0x00], "RD set, everything else clear");
    assert_eq!(&buf[4..6], &[0, 1], "QDCOUNT = 1");
    assert_eq!(&buf[6..12], &[0, 0, 0, 0, 0, 0], "no other records");
    // Question name then QTYPE=A, QCLASS=IN.
    let name = Name::encode("example.com").unwrap();
    assert_eq!(&buf[12..12 + name.as_wire().len()], name.as_wire());
    let tail = 12 + name.as_wire().len();
    assert_eq!(&buf[tail..tail + 4], &[0, 1, 0, 1]);
    assert_eq!(n, tail + 4);
}

#[test]
fn write_query_rejects_small_buffer() {
    let spec = query_spec(1, "example.com", RecordType::A);
    let mut buf = [0u8; 8];
    assert_eq!(write_query(&spec, &mut buf), Err(DnsError::BufferTooSmall));
}

// -- Response parsing ----------------------------------------------------

#[test]
fn parse_success_a_record() {
    let spec = query_spec(0x1111, "example.com", RecordType::A);
    let resp = ok_response(
        0x1111,
        "example.com",
        TYPE_A,
        &[(
            "example.com",
            TYPE_A,
            CLASS_IN,
            300,
            alloc::vec![93, 184, 216, 34],
        )],
    );
    let parsed = DnsResponse::parse(&resp, &spec).unwrap();
    assert_eq!(parsed.rcode, Rcode::NoError);
    assert!(!parsed.truncated);
    assert_eq!(parsed.addresses.len(), 1);
    assert_eq!(parsed.addresses.first().unwrap(), v4(93, 184, 216, 34));
    assert_eq!(parsed.min_ttl, 300);
}

#[test]
fn parse_success_aaaa_record() {
    let spec = query_spec(0x2222, "example.com", RecordType::Aaaa);
    let addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
    let resp = ok_response(
        0x2222,
        "example.com",
        TYPE_AAAA,
        &[(
            "example.com",
            TYPE_AAAA,
            CLASS_IN,
            120,
            addr.octets().to_vec(),
        )],
    );
    let parsed = DnsResponse::parse(&resp, &spec).unwrap();
    assert_eq!(parsed.addresses.first().unwrap(), IpAddr::V6(addr));
    assert_eq!(parsed.min_ttl, 120);
}

#[test]
fn parse_rejects_id_mismatch() {
    let spec = query_spec(0x1111, "example.com", RecordType::A);
    let resp = ok_response(0x9999, "example.com", TYPE_A, &[]);
    assert!(DnsResponse::parse(&resp, &spec).is_none());
}

#[test]
fn parse_rejects_question_mismatch() {
    let spec = query_spec(0x1111, "example.com", RecordType::A);
    // Wrong name.
    let wrong_name = ok_response(0x1111, "evil.com", TYPE_A, &[]);
    assert!(DnsResponse::parse(&wrong_name, &spec).is_none());
    // Wrong type.
    let wrong_type = ok_response(0x1111, "example.com", TYPE_AAAA, &[]);
    assert!(DnsResponse::parse(&wrong_type, &spec).is_none());
}

#[test]
fn parse_rejects_non_response_and_bad_opcode() {
    let spec = query_spec(0x1111, "example.com", RecordType::A);
    let as_query = build_response(&RespOpts {
        id: 0x1111,
        qname: "example.com",
        qtype: TYPE_A,
        qr: false,
        opcode: 0,
        tc: false,
        rcode: 0,
        with_question: true,
        answers: &[],
    });
    assert!(DnsResponse::parse(&as_query, &spec).is_none());
    let bad_opcode = build_response(&RespOpts {
        id: 0x1111,
        qname: "example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 2,
        tc: false,
        rcode: 0,
        with_question: true,
        answers: &[],
    });
    assert!(DnsResponse::parse(&bad_opcode, &spec).is_none());
}

#[test]
fn parse_rejects_wrong_question_count() {
    let spec = query_spec(0x1111, "example.com", RecordType::A);
    let no_question = build_response(&RespOpts {
        id: 0x1111,
        qname: "example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 0,
        tc: false,
        rcode: 0,
        with_question: false,
        answers: &[],
    });
    assert!(DnsResponse::parse(&no_question, &spec).is_none());
}

#[test]
fn parse_follows_cname_chain() {
    let spec = query_spec(0x3333, "www.example.com", RecordType::A);
    let cname_rdata = Name::encode("example.com").unwrap().as_wire().to_vec();
    let resp = ok_response(
        0x3333,
        "www.example.com",
        TYPE_A,
        &[
            ("www.example.com", TYPE_CNAME, CLASS_IN, 60, cname_rdata),
            (
                "example.com",
                TYPE_A,
                CLASS_IN,
                30,
                alloc::vec![10, 0, 0, 1],
            ),
        ],
    );
    let parsed = DnsResponse::parse(&resp, &spec).unwrap();
    assert_eq!(parsed.addresses.first().unwrap(), v4(10, 0, 0, 1));
    assert_eq!(parsed.min_ttl, 30, "minimum TTL across the chain");
}

#[test]
fn parse_nodata_is_empty_success() {
    let spec = query_spec(0x4444, "example.com", RecordType::A);
    let resp = ok_response(0x4444, "example.com", TYPE_A, &[]);
    let parsed = DnsResponse::parse(&resp, &spec).unwrap();
    assert_eq!(parsed.rcode, Rcode::NoError);
    assert!(parsed.addresses.is_empty());
    assert_eq!(parsed.min_ttl, 0);
}

#[test]
fn parse_surfaces_nxdomain_and_truncation() {
    let spec = query_spec(0x5555, "example.com", RecordType::A);
    let nx = build_response(&RespOpts {
        id: 0x5555,
        qname: "example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 0,
        tc: false,
        rcode: 3,
        with_question: true,
        answers: &[],
    });
    assert_eq!(
        DnsResponse::parse(&nx, &spec).unwrap().rcode,
        Rcode::NxDomain
    );
    let tc = build_response(&RespOpts {
        id: 0x5555,
        qname: "example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 0,
        tc: true,
        rcode: 0,
        with_question: true,
        answers: &[],
    });
    assert!(DnsResponse::parse(&tc, &spec).unwrap().truncated);
}

#[test]
fn parse_ignores_wrong_class_and_wrong_length_records() {
    let spec = query_spec(0x6666, "example.com", RecordType::A);
    let resp = ok_response(
        0x6666,
        "example.com",
        TYPE_A,
        &[
            // Wrong class: ignored.
            ("example.com", TYPE_A, 3, 300, alloc::vec![1, 2, 3, 4]),
            // Right class/type but wrong rdata length for an A: skipped.
            ("example.com", TYPE_A, CLASS_IN, 300, alloc::vec![1, 2, 3]),
            // The one valid answer.
            (
                "example.com",
                TYPE_A,
                CLASS_IN,
                300,
                alloc::vec![5, 6, 7, 8],
            ),
        ],
    );
    let parsed = DnsResponse::parse(&resp, &spec).unwrap();
    assert_eq!(parsed.addresses.len(), 1);
    assert_eq!(parsed.addresses.first().unwrap(), v4(5, 6, 7, 8));
}

#[test]
fn parse_caps_address_list() {
    let spec = query_spec(0x7777, "example.com", RecordType::A);
    let mut answers: Vec<(&str, u16, u16, u32, Vec<u8>)> = Vec::new();
    for i in 0..(MAX_ADDRESSES + 3) {
        answers.push((
            "example.com",
            TYPE_A,
            CLASS_IN,
            300,
            alloc::vec![10, 0, 0, u8::try_from(i).unwrap()],
        ));
    }
    let resp = ok_response(0x7777, "example.com", TYPE_A, &answers);
    let parsed = DnsResponse::parse(&resp, &spec).unwrap();
    assert_eq!(parsed.addresses.len(), MAX_ADDRESSES);
}

#[test]
fn parse_rejects_truncated_bytes() {
    let spec = query_spec(0x8888, "example.com", RecordType::A);
    let full = ok_response(
        0x8888,
        "example.com",
        TYPE_A,
        &[(
            "example.com",
            TYPE_A,
            CLASS_IN,
            300,
            alloc::vec![1, 2, 3, 4],
        )],
    );
    // Every strict prefix that still has a header must never panic and must
    // fail closed (the full message is the only accepted one).
    for cut in HEADER_LEN..full.len() {
        assert!(DnsResponse::parse(&full[..cut], &spec).is_none());
    }
}

// -- Resolver state machine ----------------------------------------------

#[test]
fn resolver_success_first_server() {
    let mut rng = counter();
    let mut r = DnsResolver::new(
        Name::encode("example.com").unwrap(),
        RecordType::A,
        &SERVERS,
    );
    let (q, server) = send_parts(&r.poll(secs(0), &mut rng).unwrap());
    assert_eq!(server, SERVERS[0]);
    assert!(r.next_deadline().is_some());
    let resp = ok_response(
        q.id,
        "example.com",
        TYPE_A,
        &[(
            "example.com",
            TYPE_A,
            CLASS_IN,
            300,
            alloc::vec![93, 184, 216, 34],
        )],
    );
    let res = finished(&r.on_response(secs(0), &resp, &mut rng).unwrap());
    assert_eq!(res.status, ResolveStatus::Success);
    assert_eq!(res.addresses.first().unwrap(), v4(93, 184, 216, 34));
    assert_eq!(res.ttl_secs, 300);
    assert!(r.is_done());
    assert!(r.next_deadline().is_none());
    // A finished resolver is inert.
    assert!(r.poll(secs(100), &mut rng).is_none());
}

#[test]
fn resolver_retransmits_then_fails_over_then_times_out() {
    let mut rng = counter();
    let mut r = DnsResolver::new(
        Name::encode("example.com").unwrap(),
        RecordType::A,
        &SERVERS,
    );
    let (q0, s0) = send_parts(&r.poll(secs(0), &mut rng).unwrap());
    assert_eq!(s0, SERVERS[0]);
    // Before the deadline: nothing to do.
    assert!(r.poll(secs(0), &mut rng).is_none());
    // Retransmit to the same server, same id.
    let (q1, s1) = send_parts(&r.poll(secs(100), &mut rng).unwrap());
    assert_eq!(s1, SERVERS[0]);
    assert_eq!(q1.id, q0.id, "retransmit keeps the transaction id");
    // Budget for server 0 spent: fail over to server 1 with a fresh id.
    let (q2, s2) = send_parts(&r.poll(secs(200), &mut rng).unwrap());
    assert_eq!(s2, SERVERS[1]);
    assert_ne!(q2.id, q0.id, "a new server gets a fresh id");
    // Retransmit to server 1.
    let (_q3, s3) = send_parts(&r.poll(secs(300), &mut rng).unwrap());
    assert_eq!(s3, SERVERS[1]);
    // All servers exhausted: timeout.
    let res = finished(&r.poll(secs(400), &mut rng).unwrap());
    assert_eq!(res.status, ResolveStatus::Timeout);
    assert!(res.addresses.is_empty());
    assert!(r.is_done());
}

#[test]
fn resolver_fails_over_on_servfail() {
    let mut rng = counter();
    let mut r = DnsResolver::new(
        Name::encode("example.com").unwrap(),
        RecordType::A,
        &SERVERS,
    );
    let (q0, _) = send_parts(&r.poll(secs(0), &mut rng).unwrap());
    let servfail = build_response(&RespOpts {
        id: q0.id,
        qname: "example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 0,
        tc: false,
        rcode: 2,
        with_question: true,
        answers: &[],
    });
    let (q1, s1) = send_parts(&r.on_response(secs(0), &servfail, &mut rng).unwrap());
    assert_eq!(s1, SERVERS[1]);
    assert_ne!(q1.id, q0.id);
}

#[test]
fn resolver_fails_over_on_truncation() {
    let mut rng = counter();
    let mut r = DnsResolver::new(
        Name::encode("example.com").unwrap(),
        RecordType::A,
        &SERVERS,
    );
    let (q0, _) = send_parts(&r.poll(secs(0), &mut rng).unwrap());
    let tc = build_response(&RespOpts {
        id: q0.id,
        qname: "example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 0,
        tc: true,
        rcode: 0,
        with_question: true,
        answers: &[],
    });
    let (_q1, s1) = send_parts(&r.on_response(secs(0), &tc, &mut rng).unwrap());
    assert_eq!(s1, SERVERS[1], "truncated UDP answer fails the server over");
}

#[test]
fn resolver_surfaces_nxdomain() {
    let mut rng = counter();
    let mut r = DnsResolver::new(
        Name::encode("nope.example.com").unwrap(),
        RecordType::A,
        &SERVERS,
    );
    let (q0, _) = send_parts(&r.poll(secs(0), &mut rng).unwrap());
    let nx = build_response(&RespOpts {
        id: q0.id,
        qname: "nope.example.com",
        qtype: TYPE_A,
        qr: true,
        opcode: 0,
        tc: false,
        rcode: 3,
        with_question: true,
        answers: &[],
    });
    let res = finished(&r.on_response(secs(0), &nx, &mut rng).unwrap());
    assert_eq!(res.status, ResolveStatus::NonExistent);
    assert!(r.is_done());
}

#[test]
fn resolver_ignores_unmatched_datagram() {
    let mut rng = counter();
    let mut r = DnsResolver::new(
        Name::encode("example.com").unwrap(),
        RecordType::A,
        &SERVERS,
    );
    let _ = send_parts(&r.poll(secs(0), &mut rng).unwrap());
    // A datagram with the wrong id is dropped; the resolver keeps waiting.
    let spoof = ok_response(0xDEAD, "example.com", TYPE_A, &[]);
    assert!(r.on_response(secs(0), &spoof, &mut rng).is_none());
    assert!(!r.is_done());
    assert!(r.next_deadline().is_some());
}

#[test]
fn resolver_with_no_servers_times_out_immediately() {
    let mut rng = counter();
    let mut r = DnsResolver::new(Name::encode("example.com").unwrap(), RecordType::A, &[]);
    let res = finished(&r.poll(secs(0), &mut rng).unwrap());
    assert_eq!(res.status, ResolveStatus::Timeout);
    assert!(r.is_done());
}
