//! Unit tests for the discovery channel's codecs.

use super::*;
use core::net::{Ipv4Addr, Ipv6Addr};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn ipp() -> ServiceTypeField<'static> {
    ServiceTypeField {
        name: b"ipp",
        transport: Transport::Tcp,
    }
}

fn encoded(request: &DiscoveryRequest<'_>) -> Vec<u8> {
    let mut out = vec![0u8; DISCOVERY_MAX_REQUEST];
    let len = request.encode(&mut out).expect("a valid request encodes");
    out.truncate(len);
    out
}

/// `printer.local` in uncompressed wire form.
const PRINTER: &[u8] = b"\x07printer\x05local\x00";

fn every_request() -> Vec<DiscoveryRequest<'static>> {
    vec![
        DiscoveryRequest::Open {
            deliver_port: 0xDEAD_BEEF_0123_4567,
        },
        DiscoveryRequest::Close { session: 7 },
        DiscoveryRequest::Stop {
            session: 7,
            request: 9,
        },
        DiscoveryRequest::Collect {
            session: 7,
            capacity: 8192,
        },
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Browse { service: ipp() },
        },
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Resolve {
                instance: "Hall Printer \u{e9}".as_bytes(),
                service: ServiceTypeField {
                    name: b"dns-sd",
                    transport: Transport::Udp,
                },
            },
        },
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Host {
                name: PRINTER,
                families: Families {
                    v4: false,
                    v6: true,
                },
            },
        },
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Reverse {
                address: IpAddr::V4(Ipv4Addr::new(169, 254, 3, 4)),
            },
        },
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Reverse {
                address: IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            },
        },
        DiscoveryRequest::Start {
            session: 2,
            query: Query::Types,
        },
    ]
}

#[test]
fn every_request_round_trips_and_is_exactly_its_declared_length() {
    for request in every_request() {
        let bytes = encoded(&request);
        assert_eq!(bytes.len(), request.wire_len());
        assert_eq!(DiscoveryRequest::decode(&bytes), Ok(request));
    }
}

#[test]
fn a_request_with_a_dirty_unused_field_is_refused() {
    for request in every_request() {
        let bytes = encoded(&request);
        let decoded_before = DiscoveryRequest::decode(&bytes);
        for at in 8..REQUEST_HEADER_LEN {
            let mut dirty = bytes.clone();
            dirty[at] ^= 0x01;
            // A flip either lands in a field the request uses — and decodes to
            // a different request — or is refused.
            if let Ok(other) = DiscoveryRequest::decode(&dirty) {
                assert_ne!(Ok(other), decoded_before, "offset {at}");
            }
        }
    }
    // One of each spelled out: Open's session, Close's request, Collect's
    // deliver port.
    let mut open = encoded(&DiscoveryRequest::Open { deliver_port: 3 });
    open[8] = 1;
    assert_eq!(DiscoveryRequest::decode(&open), Err(Errno::BadMagic));
    let mut close = encoded(&DiscoveryRequest::Close { session: 3 });
    close[12] = 1;
    assert_eq!(DiscoveryRequest::decode(&close), Err(Errno::BadMagic));
    let mut collect = encoded(&DiscoveryRequest::Collect {
        session: 3,
        capacity: 1024,
    });
    collect[16] = 1;
    assert_eq!(DiscoveryRequest::decode(&collect), Err(Errno::BadMagic));
}

#[test]
fn a_malformed_header_is_refused_whole() {
    let good = encoded(&DiscoveryRequest::Close { session: 1 });
    assert_eq!(
        DiscoveryRequest::decode(&good[..REQUEST_HEADER_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    let mut magic = good.clone();
    magic[0] ^= 1;
    assert_eq!(DiscoveryRequest::decode(&magic), Err(Errno::BadMagic));
    let mut version = good.clone();
    version[4] = 2;
    assert_eq!(
        DiscoveryRequest::decode(&version),
        Err(Errno::AbiVersionUnsupported)
    );
    let mut op = good.clone();
    op[6] = 99;
    assert_eq!(DiscoveryRequest::decode(&op), Err(Errno::OutOfRange));
    let mut trailing = good;
    trailing.push(0);
    assert_eq!(
        DiscoveryRequest::decode(&trailing),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_start_is_refused_for_any_field_its_query_does_not_carry() {
    let host = encoded(&DiscoveryRequest::Start {
        session: 1,
        query: Query::Host {
            name: PRINTER,
            families: Families::BOTH,
        },
    });
    // A name longer or shorter than declared.
    let mut longer = host.clone();
    longer.push(0);
    assert_eq!(
        DiscoveryRequest::decode(&longer),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        DiscoveryRequest::decode(&host[..host.len() - 1]),
        Err(Errno::LengthOutOfRange)
    );
    // No family asked for.
    let mut none = host.clone();
    none[29] = 0;
    assert_eq!(DiscoveryRequest::decode(&none), Err(Errno::OutOfRange));
    // A service type on a host lookup.
    let mut service = host;
    service[30] = 1;
    assert_eq!(DiscoveryRequest::decode(&service), Err(Errno::BadMagic));

    let browse = encoded(&DiscoveryRequest::Start {
        session: 1,
        query: Query::Browse { service: ipp() },
    });
    // A name on a browse, a dirty byte past the service name, no transport.
    let mut named = browse.clone();
    named[49] = 1;
    named.push(b'x');
    assert_eq!(
        DiscoveryRequest::decode(&named),
        Err(Errno::LengthOutOfRange)
    );
    let mut padded = browse.clone();
    padded[32 + 3] = b'x';
    assert_eq!(DiscoveryRequest::decode(&padded), Err(Errno::BadMagic));
    let mut transport = browse;
    transport[30] = 3;
    assert_eq!(DiscoveryRequest::decode(&transport), Err(Errno::OutOfRange));

    let reverse = encoded(&DiscoveryRequest::Start {
        session: 1,
        query: Query::Reverse {
            address: IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
        },
    });
    let mut tail = reverse;
    tail[52 + 5] = 1;
    assert_eq!(DiscoveryRequest::decode(&tail), Err(Errno::BadMagic));
}

#[test]
fn an_encoder_refuses_what_a_decoder_would() {
    let mut out = [0u8; DISCOVERY_MAX_REQUEST];
    let long = [b'a'; SERVICE_NAME_MAX + 1];
    assert_eq!(
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Browse {
                service: ServiceTypeField {
                    name: &long,
                    transport: Transport::Tcp,
                },
            },
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
    let label = [b'a'; LABEL_MAX + 1];
    assert_eq!(
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Resolve {
                instance: &label,
                service: ipp(),
            },
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        DiscoveryRequest::Start {
            session: 1,
            query: Query::Host {
                name: PRINTER,
                families: Families {
                    v4: false,
                    v6: false,
                },
            },
        }
        .encode(&mut out),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        DiscoveryRequest::Close { session: 1 }.encode(&mut [0u8; REQUEST_HEADER_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn an_id_reply_round_trips_and_a_refusal_carries_no_id() {
    let mut out = [0u8; ID_REPLY_LEN];
    let len = encode_id_reply(Ok(42), &mut out).expect("fits");
    assert_eq!(decode_id_reply(&out[..len]), Ok(42));
    let len = encode_id_reply(Err(Errno::PermissionDenied), &mut out).expect("fits");
    assert_eq!(len, STATUS_REPLY_LEN);
    assert_eq!(decode_id_reply(&out[..len]), Err(Errno::PermissionDenied));
    // A success too short to carry its id.
    let len = encode_id_reply(Ok(1), &mut out).expect("fits");
    assert_eq!(decode_id_reply(&out[..len - 1]), Err(Errno::BufferTooSmall));
}

#[test]
fn a_doorbell_round_trips_and_nothing_else_is_believed() {
    let bell = encode_doorbell(0x0102_0304);
    assert_eq!(decode_doorbell(&bell), Ok(0x0102_0304));
    let mut magic = bell;
    magic[1] ^= 1;
    assert_eq!(decode_doorbell(&magic), Err(Errno::BadMagic));
    let mut reserved = bell;
    reserved[6] = 1;
    assert_eq!(decode_doorbell(&reserved), Err(Errno::BadMagic));
    let mut version = bell;
    version[4] = 9;
    assert_eq!(decode_doorbell(&version), Err(Errno::AbiVersionUnsupported));
    assert_eq!(decode_doorbell(&bell[..11]), Err(Errno::LengthOutOfRange));
}

fn every_entry() -> Vec<Entry<'static>> {
    let eth = iface(b"eth0");
    let answer = |change, answer| Entry::Answer {
        request: 3,
        interface: eth,
        change,
        ttl: 4500,
        answer,
    };
    vec![
        answer(
            Change::Added,
            Answer::Instance {
                label: b"Hall Printer",
            },
        ),
        answer(
            Change::Refreshed,
            Answer::Service {
                priority: 1,
                weight: 2,
                port: 631,
                target: PRINTER,
            },
        ),
        answer(
            Change::Retired,
            Answer::Text {
                octets: b"\x07pdl=pdf",
            },
        ),
        answer(
            Change::Added,
            Answer::Address {
                address: IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2)),
            },
        ),
        answer(
            Change::Added,
            Answer::Address {
                address: IpAddr::V4(Ipv4Addr::new(169, 254, 9, 9)),
            },
        ),
        answer(Change::Added, Answer::Pointer { target: PRINTER }),
        answer(Change::Added, Answer::Type { service: ipp() }),
        Entry::Flush {
            request: 3,
            interface: eth,
        },
        Entry::Lost { request: 3 },
    ]
}

#[test]
fn a_collect_reply_carries_every_entry_kind_and_reads_back_in_order() {
    let mut out = [0u8; DISCOVERY_MAX_REPLY];
    let mut writer = CollectWriter::new(&mut out).expect("room for a header");
    for entry in every_entry() {
        writer.push(&entry).expect("fits");
    }
    let len = writer.finish(true);
    let reply = CollectReply::parse(&out[..len]).expect("a valid reply");
    assert!(reply.more);
    assert_eq!(reply.len(), every_entry().len());
    assert_eq!(reply.entries().collect::<Vec<_>>(), every_entry());
}

#[test]
fn an_empty_collect_is_a_reply_with_no_entries() {
    let mut out = [0u8; COLLECT_HEADER_LEN];
    let writer = CollectWriter::new(&mut out).expect("room for a header");
    assert!(writer.is_empty());
    let len = writer.finish(false);
    let reply = CollectReply::parse(&out[..len]).expect("valid");
    assert!(reply.is_empty() && !reply.more);
}

#[test]
fn an_entry_that_does_not_fit_leaves_the_reply_as_it_was() {
    let mut out = [0u8; COLLECT_HEADER_LEN + ENTRY_HEADER_LEN + 4];
    let mut writer = CollectWriter::new(&mut out).expect("room for a header");
    let big = Entry::Answer {
        request: 1,
        interface: iface(b"eth0"),
        change: Change::Added,
        ttl: 1,
        answer: Answer::Text { octets: &[1; 64] },
    };
    assert_eq!(writer.push(&big), Err(Errno::BufferTooSmall));
    writer
        .push(&Entry::Lost { request: 1 })
        .expect("a small one still fits");
    let len = writer.finish(false);
    assert_eq!(CollectReply::parse(&out[..len]).expect("valid").len(), 1);
}

#[test]
fn a_malformed_collect_reply_is_refused_whole() {
    let mut out = [0u8; DISCOVERY_MAX_REPLY];
    let mut writer = CollectWriter::new(&mut out).expect("room");
    for entry in every_entry() {
        writer.push(&entry).expect("fits");
    }
    let len = writer.finish(false);
    let good = out[..len].to_vec();

    // A count that disagrees with the entries.
    let mut count = good.clone();
    count[STATUS_REPLY_LEN] ^= 1;
    assert!(CollectReply::parse(&count).is_err());
    // An unknown flag, and a dirty reserved byte.
    let mut flag = good.clone();
    flag[STATUS_REPLY_LEN + 2] = 2;
    assert_eq!(CollectReply::parse(&flag).err(), Some(Errno::BadMagic));
    let mut reserved = good.clone();
    reserved[STATUS_REPLY_LEN + 3] = 1;
    assert_eq!(CollectReply::parse(&reserved).err(), Some(Errno::BadMagic));
    // A truncated last entry.
    assert!(CollectReply::parse(&good[..good.len() - 1]).is_err());
    // An unknown kind in the first entry.
    let mut kind = good.clone();
    kind[COLLECT_HEADER_LEN + 2] = 99;
    assert_eq!(CollectReply::parse(&kind).err(), Some(Errno::OutOfRange));
    // A bad interface name in the first entry.
    let mut name = good.clone();
    name[COLLECT_HEADER_LEN + 8] = b'E';
    assert!(CollectReply::parse(&name).is_err());
    // A refusal status is the refusal, not a reply.
    let status = encode_status_reply(Err(Errno::NotFound));
    assert_eq!(CollectReply::parse(&status).err(), Some(Errno::NotFound));
}

#[test]
fn a_flush_or_lost_entry_carrying_an_answer_field_is_refused() {
    for entry in [
        Entry::Flush {
            request: 1,
            interface: iface(b"eth0"),
        },
        Entry::Lost { request: 1 },
    ] {
        let mut bytes = vec![0u8; entry.wire_len()];
        entry.encode(&mut bytes).expect("encodes");
        for at in [3usize, 24] {
            let mut dirty = bytes.clone();
            dirty[at] = 1;
            assert_eq!(decode_entry(&dirty), Err(Errno::BadMagic), "{at}");
        }
    }
}

#[test]
fn the_field_widths_are_the_protocol_limits() {
    assert_eq!(NAME_MAX, 255);
    assert_eq!(LABEL_MAX, 63);
    assert_eq!(SERVICE_NAME_MAX, 15);
    // The longest entry of every kind fits one longest entry's bound.
    let longest = [
        Entry::Answer {
            request: 0,
            interface: iface(b"x"),
            change: Change::Added,
            ttl: 0,
            answer: Answer::Text {
                octets: &[1; TXT_MAX],
            },
        },
        Entry::Answer {
            request: 0,
            interface: iface(b"x"),
            change: Change::Added,
            ttl: 0,
            answer: Answer::Service {
                priority: 0,
                weight: 0,
                port: 0,
                target: &[1; NAME_MAX],
            },
        },
    ];
    for entry in longest {
        assert!(entry.wire_len() <= ENTRY_MAX);
    }
}

#[test]
fn only_the_discovery_account_rings_a_doorbell() {
    use crate::origin::{CapabilitySummary, ProcId};
    let origin = |domain, uid| {
        Origin::new(
            domain,
            uid,
            101,
            7,
            ProcId::from_raw([7; 16]),
            CapabilitySummary::EMPTY,
            crate::ORIGIN_CONSOLE_NONE,
        )
    };
    assert!(from_discovery_service(&origin(
        TrustDomain::User,
        DISCOVERYD_UID
    )));
    assert!(!from_discovery_service(&origin(TrustDomain::User, 1000)));
    assert!(!from_discovery_service(&origin(
        TrustDomain::Kernel,
        DISCOVERYD_UID
    )));
}
