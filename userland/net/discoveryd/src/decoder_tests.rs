//! Unit tests for the decoder worker, driven frame by frame exactly as its
//! serve loop drives it.

use super::Decoder;
use crate::wire::{FromDecoder, ToDecoder, CACHE_KEY_LEN, MAX_TO_DECODER, RNG_KEY_LEN};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_net::dns::Name;
use tairix_net::mdns::{MessageWriter, RData, Record, Section};
use tairix_net::{IpAddr, Ipv4Addr};
use tairix_sandbox::proto::ProtoError;
use tairix_sandbox::session::{FrameOut, SessionService, SessionStep};

const SEC: u64 = 1_000_000_000;

/// Collects every frame the decoder emits.
#[derive(Default)]
struct Collected {
    frames: Vec<Vec<u8>>,
}

impl FrameOut for Collected {
    fn frame(&mut self, payload: &[u8]) -> Result<(), ProtoError> {
        self.frames.push(payload.to_vec());
        Ok(())
    }
}

impl Collected {
    fn deadlines(&self) -> Vec<Option<u64>> {
        self.frames
            .iter()
            .map(|frame| {
                let FromDecoder::Deadline(at) =
                    FromDecoder::decode(frame).expect("a frame the front believes");
                at
            })
            .collect()
    }
}

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn frame(to: &ToDecoder<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_TO_DECODER];
    let len = to.encode(&mut out).expect("fits");
    out.truncate(len);
    out
}

fn configure() -> Vec<u8> {
    frame(&ToDecoder::Configure {
        cache_key: &[0x5A; CACHE_KEY_LEN],
        rng_key: &[0xA5; RNG_KEY_LEN],
    })
}

/// A multicast DNS response announcing `host` at `last` with a `ttl`.
fn announcement(host: &str, last: u8, ttl: u32) -> Vec<u8> {
    let mut out = vec![0u8; 512];
    let mut writer = MessageWriter::new(&mut out, 0, true).expect("room for a header");
    let mut record = Record::unique(
        Name::encode(host).expect("a host name"),
        RData::A(Ipv4Addr::new(10, 0, 0, last)),
    );
    record.ttl = ttl;
    assert!(writer.push_record(Section::Answer, &record));
    let len = writer.finish();
    out.truncate(len);
    out
}

fn datagram(now: u64, interface: &[u8], payload: &[u8]) -> Vec<u8> {
    frame(&ToDecoder::Datagram {
        now,
        interface: iface(interface),
        source: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9)),
        port: 5353,
        payload,
    })
}

fn handled(decoder: &mut Decoder, request: &[u8], out: &mut Collected) -> SessionStep {
    decoder.handle(request, out)
}

fn configured() -> Decoder {
    let mut decoder = Decoder::new();
    assert_eq!(
        handled(&mut decoder, &configure(), &mut Collected::default()),
        SessionStep::Continue
    );
    decoder
}

#[test]
fn nothing_but_configuration_is_accepted_first() {
    let mut out = Collected::default();
    let mut decoder = Decoder::new();
    let announced = announcement("printer.local", 5, 120);
    assert_eq!(
        handled(&mut decoder, &datagram(SEC, b"eth0", &announced), &mut out),
        SessionStep::Finished
    );
    let mut decoder = Decoder::new();
    assert_eq!(
        handled(
            &mut decoder,
            &frame(&ToDecoder::Tick { now: SEC }),
            &mut out
        ),
        SessionStep::Finished
    );
    assert!(out.frames.is_empty());
}

#[test]
fn a_second_configuration_or_an_undecodable_frame_ends_the_session() {
    let mut decoder = configured();
    let mut out = Collected::default();
    assert_eq!(
        handled(&mut decoder, &configure(), &mut out),
        SessionStep::Finished
    );
    let mut decoder = configured();
    assert_eq!(
        handled(&mut decoder, b"\x7Fgarbage", &mut out),
        SessionStep::Finished
    );
}

#[test]
fn an_announcement_is_cached_and_its_expiry_reported_then_expired_by_a_tick() {
    let mut decoder = configured();
    let mut out = Collected::default();
    let announced = announcement("printer.local", 5, 120);
    assert_eq!(
        handled(&mut decoder, &datagram(SEC, b"eth0", &announced), &mut out),
        SessionStep::Continue
    );
    assert_eq!(out.deadlines(), [Some(SEC + 120 * SEC)]);
    assert_eq!(decoder.engines[0].engine.cache().len(), 1);

    let mut out = Collected::default();
    let expired = frame(&ToDecoder::Tick {
        now: SEC + 120 * SEC,
    });
    assert_eq!(
        handled(&mut decoder, &expired, &mut out),
        SessionStep::Continue
    );
    assert_eq!(out.deadlines(), [None]);
    assert!(decoder.engines[0].engine.cache().is_empty());
}

#[test]
fn a_record_learned_on_one_interface_never_reaches_another() {
    let mut decoder = configured();
    let mut out = Collected::default();
    let wired = announcement("printer.local", 5, 120);
    let wireless = announcement("scanner.local", 6, 120);
    handled(&mut decoder, &datagram(SEC, b"eth0", &wired), &mut out);
    handled(&mut decoder, &datagram(SEC, b"wlan0", &wireless), &mut out);
    assert_eq!(decoder.engines.len(), 2);
    for (name, host) in [
        (b"eth0" as &[u8], "printer.local"),
        (b"wlan0", "scanner.local"),
    ] {
        let entry = decoder
            .engines
            .iter()
            .find(|entry| entry.name == iface(name))
            .expect("an engine per interface");
        assert_eq!(entry.engine.cache().len(), 1);
        let held = Name::encode(host).expect("a host name");
        assert_eq!(
            entry
                .engine
                .cache()
                .lookup(&held, tairix_net::dns::RecordType::A)
                .count(),
            1
        );
    }
}

#[test]
fn an_unchanged_deadline_is_reported_once_and_a_tick_is_always_answered() {
    let mut decoder = configured();
    let mut out = Collected::default();
    let announced = announcement("printer.local", 5, 120);
    handled(&mut decoder, &datagram(SEC, b"eth0", &announced), &mut out);
    // The same record at the same instant renews it to the same expiry.
    handled(&mut decoder, &datagram(SEC, b"eth0", &announced), &mut out);
    assert_eq!(out.deadlines(), [Some(121 * SEC)]);
    let mut out = Collected::default();
    handled(
        &mut decoder,
        &frame(&ToDecoder::Tick { now: 2 * SEC }),
        &mut out,
    );
    assert_eq!(out.deadlines(), [Some(121 * SEC)]);
}

#[test]
fn time_never_runs_backwards() {
    let mut decoder = configured();
    let mut out = Collected::default();
    let announced = announcement("printer.local", 5, 120);
    handled(
        &mut decoder,
        &datagram(50 * SEC, b"eth0", &announced),
        &mut out,
    );
    // A tick naming an earlier instant is taken at the latest one seen, so
    // the record it would otherwise have outlived is still held.
    handled(
        &mut decoder,
        &frame(&ToDecoder::Tick { now: SEC }),
        &mut out,
    );
    assert_eq!(decoder.engines[0].engine.cache().len(), 1);
    assert_eq!(decoder.now, 50 * SEC);
}

#[test]
fn hostile_payloads_are_dropped_and_leave_nothing_cached() {
    let mut decoder = configured();
    let mut out = Collected::default();
    for payload in [&b""[..], b"\x00", &[0xFF; 600][..], &[0u8; 12][..]] {
        assert_eq!(
            handled(&mut decoder, &datagram(SEC, b"eth0", payload), &mut out),
            SessionStep::Continue
        );
    }
    assert!(decoder
        .engines
        .iter()
        .all(|entry| entry.engine.cache().is_empty()));
}
