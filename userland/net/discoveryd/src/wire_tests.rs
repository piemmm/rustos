//! Unit tests for the front ↔ decoder channel.

use super::{
    Form, FromDecoder, ToDecoder, ASK_HEADER_LEN, CACHE_KEY_LEN, CONFIGURE_LEN,
    DATAGRAM_HEADER_LEN, DEADLINE_LEN, LINK_LEN, MAX_FROM_DECODER, MAX_TO_DECODER, REPLAYED_LEN,
    REPLAY_LEN, RNG_KEY_LEN, STOP_LEN, TICK_LEN, TRANSMIT_HEADER_LEN,
};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::discovery_ipc::{Answer, Change, Entry};
use tairix_abi::net::SOCKET_MAX_DATAGRAM;
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_net::mdns::{Destination, MAX_MESSAGE_LEN};
use tairix_net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_sandbox::wire::WireError;

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn encoded(frame: &ToDecoder<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_TO_DECODER];
    let len = frame.encode(&mut out).expect("fits");
    out.truncate(len);
    out
}

fn datagram(payload: &[u8]) -> Vec<u8> {
    encoded(&ToDecoder::Datagram {
        now: 42,
        interface: iface(b"eth0"),
        source: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9)),
        port: 5353,
        payload,
    })
}

#[test]
fn every_front_frame_round_trips() {
    let cache_key = [0x11u8; CACHE_KEY_LEN];
    let rng_key = [0x22u8; RNG_KEY_LEN];
    let v6 = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 7));
    for frame in [
        ToDecoder::Configure {
            cache_key: &cache_key,
            rng_key: &rng_key,
        },
        ToDecoder::Datagram {
            now: u64::MAX,
            interface: iface(b"wlan0"),
            source: v6,
            port: 41_234,
            payload: b"mdns bytes",
        },
        ToDecoder::Datagram {
            now: 0,
            interface: iface(b"eth0"),
            source: IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2)),
            port: 5353,
            payload: &[],
        },
        ToDecoder::Tick { now: 7 },
        ToDecoder::Link {
            now: 8,
            interface: iface(b"eth1"),
            up: true,
        },
        ToDecoder::Link {
            now: 9,
            interface: iface(b"eth1"),
            up: false,
        },
        ToDecoder::Ask {
            now: 10,
            question: u32::MAX,
            form: Form::Instance,
            name: b"\x04_ipp\x04_tcp\x05local\x00",
        },
        ToDecoder::Stop { question: 3 },
        ToDecoder::Replay {
            question: 3,
            token: 4,
        },
    ] {
        let bytes = encoded(&frame);
        assert_eq!(ToDecoder::decode(&bytes), Ok(frame));
    }
    for (frame, len) in [
        (ToDecoder::Stop { question: 1 }, STOP_LEN),
        (
            ToDecoder::Replay {
                question: 1,
                token: 2,
            },
            REPLAY_LEN,
        ),
        (
            ToDecoder::Link {
                now: 0,
                interface: iface(b"eth0"),
                up: true,
            },
            LINK_LEN,
        ),
    ] {
        assert_eq!(encoded(&frame).len(), len);
    }
    assert_eq!(
        encoded(&ToDecoder::Configure {
            cache_key: &cache_key,
            rng_key: &rng_key
        })
        .len(),
        CONFIGURE_LEN
    );
    assert_eq!(encoded(&ToDecoder::Tick { now: 1 }).len(), TICK_LEN);
}

#[test]
fn the_largest_datagram_fits_and_a_larger_one_is_never_encoded() {
    let largest = vec![0xAB; SOCKET_MAX_DATAGRAM];
    assert_eq!(datagram(&largest).len(), MAX_TO_DECODER);
    let larger = vec![0xAB; SOCKET_MAX_DATAGRAM + 1];
    let mut out = vec![0u8; MAX_TO_DECODER + 1];
    let frame = ToDecoder::Datagram {
        now: 0,
        interface: iface(b"eth0"),
        source: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        port: 5353,
        payload: &larger,
    };
    assert_eq!(frame.encode(&mut out), None);
    // A buffer too small for the frame is refused rather than overrun.
    assert_eq!(
        ToDecoder::Tick { now: 1 }.encode(&mut [0u8; TICK_LEN - 1]),
        None
    );
}

#[test]
fn a_malformed_front_frame_is_refused_whole() {
    let good = datagram(b"payload");
    // An unknown tag, and an empty frame.
    let mut unknown = good.clone();
    unknown[0] = 0x7F;
    assert_eq!(ToDecoder::decode(&unknown), Err(WireError::Malformed));
    assert_eq!(ToDecoder::decode(&[]), Err(WireError::Truncated));
    // A header cut short.
    assert_eq!(
        ToDecoder::decode(&good[..DATAGRAM_HEADER_LEN - 1]),
        Err(WireError::Truncated)
    );
    // An interface name the stack would never use.
    let mut name = good.clone();
    name[9] = b'E';
    assert_eq!(ToDecoder::decode(&name), Err(WireError::Malformed));
    // An unknown family, and an IPv4 address with a dirty tail.
    let mut family = good.clone();
    family[25] = 5;
    assert_eq!(ToDecoder::decode(&family), Err(WireError::Malformed));
    let mut tail = good.clone();
    tail[26 + 4] = 1;
    assert_eq!(ToDecoder::decode(&tail), Err(WireError::Malformed));
    // Trailing bytes after a fixed-length frame.
    let mut tick = encoded(&ToDecoder::Tick { now: 3 });
    tick.push(0);
    assert_eq!(ToDecoder::decode(&tick), Err(WireError::Malformed));
}

#[test]
fn a_datagram_frame_longer_than_any_delivery_is_refused() {
    let mut frame = datagram(&[]);
    frame.resize(MAX_TO_DECODER + 1, 0);
    assert_eq!(ToDecoder::decode(&frame), Err(WireError::Malformed));
}

fn from_decoder(frame: &FromDecoder<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_FROM_DECODER];
    let len = frame.encode(&mut out).expect("fits");
    out.truncate(len);
    out
}

fn instance(label: &[u8]) -> Entry<'_> {
    Entry::Answer {
        request: 6,
        interface: iface(b"eth0"),
        change: Change::Added,
        ttl: 4500,
        answer: Answer::Instance { label },
    }
}

#[test]
fn every_decoder_frame_round_trips() {
    let largest = vec![0x5A; MAX_MESSAGE_LEN];
    for frame in [
        FromDecoder::Deadline(None),
        FromDecoder::Deadline(Some(u64::MAX)),
        FromDecoder::Answer(instance(b"Hall Printer")),
        FromDecoder::Held {
            token: 9,
            entry: instance(b"Lobby"),
        },
        FromDecoder::Replayed { token: 9 },
        FromDecoder::Linked {
            interface: iface(b"eth3"),
            up: false,
        },
        FromDecoder::Transmit {
            interface: iface(b"eth0"),
            to: Destination::Group,
            payload: b"query",
        },
        FromDecoder::Transmit {
            interface: iface(b"wlan0"),
            to: Destination::Peer {
                addr: IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 9)),
                port: 49_152,
            },
            payload: &largest,
        },
    ] {
        assert_eq!(FromDecoder::decode(&from_decoder(&frame)), Ok(frame));
    }
    assert_eq!(
        from_decoder(&FromDecoder::Transmit {
            interface: iface(b"eth0"),
            to: Destination::Group,
            payload: &largest,
        })
        .len(),
        MAX_FROM_DECODER
    );
    assert_eq!(
        from_decoder(&FromDecoder::Replayed { token: 1 }).len(),
        REPLAYED_LEN
    );
}

#[test]
fn a_deadline_frame_is_believed_only_in_its_one_shape() {
    let good = from_decoder(&FromDecoder::Deadline(Some(9)));
    let mut tag = good.clone();
    tag[0] = 0x7F;
    assert_eq!(FromDecoder::decode(&tag), Err(WireError::Malformed));
    let mut flag = good.clone();
    flag[1] = 2;
    assert_eq!(FromDecoder::decode(&flag), Err(WireError::Malformed));
    // "No deadline" carries no instant.
    let mut absent = from_decoder(&FromDecoder::Deadline(None));
    absent[2] = 1;
    assert_eq!(FromDecoder::decode(&absent), Err(WireError::Malformed));
    assert_eq!(
        FromDecoder::decode(&good[..DEADLINE_LEN - 1]),
        Err(WireError::Truncated)
    );
    let mut long = good;
    long.push(0);
    assert_eq!(FromDecoder::decode(&long), Err(WireError::Malformed));
}

#[test]
fn an_answer_is_only_ever_an_answer_entry() {
    let flush = Entry::Flush {
        request: 1,
        interface: iface(b"eth0"),
    };
    let mut frame = vec![2u8];
    let mut entry = vec![0u8; flush.wire_len()];
    flush.encode(&mut entry).unwrap();
    frame.extend_from_slice(&entry);
    assert_eq!(FromDecoder::decode(&frame), Err(WireError::Malformed));
    let mut good = from_decoder(&FromDecoder::Answer(instance(b"x")));
    good.push(0);
    assert_eq!(
        FromDecoder::decode(&good),
        Err(WireError::Malformed),
        "bytes past the entry"
    );
}

#[test]
fn a_transmit_frame_names_one_shape_of_destination() {
    let group = from_decoder(&FromDecoder::Transmit {
        interface: iface(b"eth0"),
        to: Destination::Group,
        payload: b"q",
    });
    let mut addressed = group.clone();
    addressed[19] = 1;
    assert_eq!(
        FromDecoder::decode(&addressed),
        Err(WireError::Malformed),
        "a group has no address"
    );
    let mut kind = group.clone();
    kind[17] = 2;
    assert_eq!(FromDecoder::decode(&kind), Err(WireError::Malformed));
    assert_eq!(
        FromDecoder::decode(&group[..TRANSMIT_HEADER_LEN]),
        Err(WireError::Malformed),
        "an empty datagram"
    );
    let mut long = group;
    long.resize(TRANSMIT_HEADER_LEN + MAX_MESSAGE_LEN + 1, 0);
    assert_eq!(FromDecoder::decode(&long), Err(WireError::Malformed));
}

#[test]
fn an_ask_names_a_form_and_a_name() {
    let good = encoded(&ToDecoder::Ask {
        now: 1,
        question: 2,
        form: Form::AddressV6,
        name: b"\x04host\x05local\x00",
    });
    let mut form = good.clone();
    form[13] = 8;
    assert_eq!(ToDecoder::decode(&form), Err(WireError::Malformed));
    let mut empty = good[..ASK_HEADER_LEN].to_vec();
    empty[14] = 0;
    assert_eq!(ToDecoder::decode(&empty), Err(WireError::Malformed));
    let mut short = good.clone();
    short.pop();
    assert_eq!(ToDecoder::decode(&short), Err(WireError::Truncated));
    let mut link = encoded(&ToDecoder::Link {
        now: 0,
        interface: iface(b"eth0"),
        up: true,
    });
    link[LINK_LEN - 1] = 2;
    assert_eq!(ToDecoder::decode(&link), Err(WireError::Malformed));
}
