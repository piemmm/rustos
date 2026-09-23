//! Unit tests for the front ↔ decoder channel.

use super::{
    FromDecoder, ToDecoder, CACHE_KEY_LEN, CONFIGURE_LEN, DATAGRAM_HEADER_LEN, DEADLINE_LEN,
    MAX_TO_DECODER, RNG_KEY_LEN, TICK_LEN,
};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::net::SOCKET_MAX_DATAGRAM;
use tairix_abi::net_ipc::IF_NAME_LEN;
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
    ] {
        let bytes = encoded(&frame);
        assert_eq!(ToDecoder::decode(&bytes), Ok(frame));
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

#[test]
fn the_decoder_frame_round_trips_and_nothing_else_is_believed() {
    for deadline in [None, Some(0), Some(1), Some(u64::MAX)] {
        let frame = FromDecoder::Deadline(deadline);
        assert_eq!(FromDecoder::decode(&frame.encode()), Ok(frame));
    }
    let good = FromDecoder::Deadline(Some(9)).encode();
    let mut tag = good;
    tag[0] = 2;
    assert_eq!(FromDecoder::decode(&tag), Err(WireError::Malformed));
    let mut flag = good;
    flag[1] = 2;
    assert_eq!(FromDecoder::decode(&flag), Err(WireError::Malformed));
    // "No deadline" carries no instant.
    let mut absent = FromDecoder::Deadline(None).encode();
    absent[2] = 1;
    assert_eq!(FromDecoder::decode(&absent), Err(WireError::Malformed));
    assert_eq!(
        FromDecoder::decode(&good[..DEADLINE_LEN - 1]),
        Err(WireError::Truncated)
    );
    let mut long = good.to_vec();
    long.push(0);
    assert_eq!(FromDecoder::decode(&long), Err(WireError::Malformed));
}
