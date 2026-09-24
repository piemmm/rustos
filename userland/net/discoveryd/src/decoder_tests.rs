//! Unit tests for the decoder worker, driven frame by frame exactly as its
//! serve loop drives it.

use super::Decoder;
use crate::wire::{Form, FromDecoder, ToDecoder, CACHE_KEY_LEN, MAX_TO_DECODER, RNG_KEY_LEN};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::discovery_ipc::{Answer, Change, Entry};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_net::dns::{Name, RecordType};
use tairix_net::mdns::{Destination, MessageWriter, RData, Record, Section, Service};
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

/// What one frame from the decoder said, owned.
#[derive(Debug, PartialEq)]
enum Said {
    Deadline(Option<u64>),
    Answer(u32, [u8; IF_NAME_LEN], Change, Vec<u8>),
    Held(u32, u32),
    Replayed(u32),
    Transmit([u8; IF_NAME_LEN], Destination),
    Linked([u8; IF_NAME_LEN], bool),
}

fn label_of(entry: &Entry<'_>) -> Vec<u8> {
    match entry {
        Entry::Answer {
            answer: Answer::Instance { label },
            ..
        } => label.to_vec(),
        Entry::Answer {
            answer: Answer::Address { address },
            ..
        } => alloc::format!("{address}").into_bytes(),
        Entry::Answer {
            answer: Answer::Service { port, .. },
            ..
        } => alloc::format!("port {port}").into_bytes(),
        _ => Vec::new(),
    }
}

impl Collected {
    fn said(&self) -> Vec<Said> {
        self.frames
            .iter()
            .map(
                |frame| match FromDecoder::decode(frame).expect("a frame the front believes") {
                    FromDecoder::Deadline(at) => Said::Deadline(at),
                    FromDecoder::Answer(entry) => {
                        let Entry::Answer {
                            request,
                            interface,
                            change,
                            ..
                        } = entry
                        else {
                            unreachable!("the wire admits answers only")
                        };
                        Said::Answer(request, interface, change, label_of(&entry))
                    }
                    FromDecoder::Held { token, entry } => {
                        let Entry::Answer { request, .. } = entry else {
                            unreachable!("the wire admits answers only")
                        };
                        Said::Held(token, request)
                    }
                    FromDecoder::Replayed { token } => Said::Replayed(token),
                    FromDecoder::Transmit { interface, to, .. } => Said::Transmit(interface, to),
                    FromDecoder::Linked { interface, up } => Said::Linked(interface, up),
                },
            )
            .collect()
    }

    fn answers(&self) -> Vec<Said> {
        self.said()
            .into_iter()
            .filter(|said| matches!(said, Said::Answer(..)))
            .collect()
    }

    fn take(&mut self) -> Vec<Said> {
        let said = self.said();
        self.frames.clear();
        said
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

fn response(records: &[Record]) -> Vec<u8> {
    let mut out = vec![0u8; 1200];
    let mut writer = MessageWriter::new(&mut out, 0, true).expect("room for a header");
    for record in records {
        assert!(writer.push_record(Section::Answer, record));
    }
    let len = writer.finish();
    out.truncate(len);
    out
}

/// A multicast DNS response announcing `host` at `last` with a `ttl`.
fn announcement(host: &str, last: u8, ttl: u32) -> Vec<u8> {
    let mut record = Record::unique(
        Name::encode(host).expect("a host name"),
        RData::A(Ipv4Addr::new(10, 0, 0, last)),
    );
    record.ttl = ttl;
    response(&[record])
}

fn ipp_type() -> Name {
    Name::encode("_ipp._tcp.local").unwrap()
}

fn instance_of(label: &[u8], service: &str) -> Name {
    let (name, transport) = service.split_once('.').unwrap();
    Name::from_labels(&[label, name.as_bytes(), transport.as_bytes(), b"local"]).unwrap()
}

/// A browse answer: `_ipp._tcp.local PTR <label>.<service>.local`.
fn pointer(label: &[u8], service: &str) -> Record {
    Record::shared(ipp_type(), RData::Ptr(instance_of(label, service)))
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

fn link(now: u64, interface: &[u8], up: bool) -> Vec<u8> {
    frame(&ToDecoder::Link {
        now,
        interface: iface(interface),
        up,
    })
}

fn ask(now: u64, question: u32, form: Form, name: &Name) -> Vec<u8> {
    frame(&ToDecoder::Ask {
        now,
        question,
        form,
        name: name.as_wire(),
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

/// A decoder running `eth0`.
fn linked() -> (Decoder, Collected) {
    let mut decoder = configured();
    let mut out = Collected::default();
    assert_eq!(
        handled(&mut decoder, &link(0, b"eth0", true), &mut out),
        SessionStep::Continue
    );
    assert_eq!(out.take()[0], Said::Linked(iface(b"eth0"), true));
    (decoder, out)
}

#[test]
fn nothing_but_configuration_is_accepted_first() {
    let mut out = Collected::default();
    for first in [
        datagram(SEC, b"eth0", &announcement("printer.local", 5, 120)),
        frame(&ToDecoder::Tick { now: SEC }),
        link(0, b"eth0", true),
    ] {
        let mut decoder = Decoder::new();
        assert_eq!(
            handled(&mut decoder, &first, &mut out),
            SessionStep::Finished
        );
    }
    assert!(out.frames.is_empty());
}

#[test]
fn a_second_configuration_or_an_undecodable_frame_ends_the_session() {
    let mut out = Collected::default();
    assert_eq!(
        handled(&mut configured(), &configure(), &mut out),
        SessionStep::Finished
    );
    assert_eq!(
        handled(&mut configured(), b"\x7Fgarbage", &mut out),
        SessionStep::Finished
    );
}

#[test]
fn a_frame_the_front_would_never_send_ends_the_session() {
    let ipp = ipp_type();
    for (setup, violation) in [
        // A datagram for an interface it was never told of.
        (vec![], datagram(SEC, b"wlan0", b"x")),
        // A link told up twice, or down when it was never up.
        (vec![], link(0, b"eth0", true)),
        (vec![], link(0, b"wlan0", false)),
        // A question asked twice, or stopped when it was never asked.
        (
            vec![ask(0, 1, Form::Instance, &ipp)],
            ask(0, 1, Form::Instance, &ipp),
        ),
        (vec![], frame(&ToDecoder::Stop { question: 9 })),
    ] {
        let (mut decoder, mut out) = linked();
        for frame in setup {
            assert_eq!(
                handled(&mut decoder, &frame, &mut out),
                SessionStep::Continue
            );
        }
        assert_eq!(
            handled(&mut decoder, &violation, &mut out),
            SessionStep::Finished
        );
    }
}

#[test]
fn an_asked_question_is_sent_and_told_each_answer_as_the_front_names_it() {
    let (mut decoder, mut out) = linked();
    assert_eq!(
        handled(
            &mut decoder,
            &ask(0, 7, Form::Instance, &ipp_type()),
            &mut out
        ),
        SessionStep::Continue
    );
    // Nothing is held yet; the question is due within its first delay.
    assert!(out.answers().is_empty());
    handled(
        &mut decoder,
        &frame(&ToDecoder::Tick { now: SEC }),
        &mut out,
    );
    assert!(out
        .take()
        .contains(&Said::Transmit(iface(b"eth0"), Destination::Group)));

    let answer = response(&[pointer(b"Hall Printer", "_ipp._tcp")]);
    handled(&mut decoder, &datagram(2 * SEC, b"eth0", &answer), &mut out);
    assert_eq!(
        out.answers(),
        [Said::Answer(
            7,
            iface(b"eth0"),
            Change::Added,
            b"Hall Printer".to_vec()
        )]
    );
}

#[test]
fn a_browse_answer_naming_another_type_is_no_answer() {
    let (mut decoder, mut out) = linked();
    handled(
        &mut decoder,
        &ask(0, 1, Form::Instance, &ipp_type()),
        &mut out,
    );
    let foreign = response(&[pointer(b"Shell", "_ssh._tcp")]);
    handled(&mut decoder, &datagram(SEC, b"eth0", &foreign), &mut out);
    assert!(out.answers().is_empty());
}

#[test]
fn a_question_asked_after_the_segment_volunteered_starts_from_the_cache() {
    let (mut decoder, mut out) = linked();
    handled(
        &mut decoder,
        &datagram(SEC, b"eth0", &announcement("printer.local", 5, 120)),
        &mut out,
    );
    out.take();
    let host = Name::encode("printer.local").unwrap();
    handled(
        &mut decoder,
        &ask(2 * SEC, 3, Form::AddressV4, &host),
        &mut out,
    );
    assert_eq!(
        out.answers(),
        [Said::Answer(
            3,
            iface(b"eth0"),
            Change::Added,
            b"10.0.0.5".to_vec()
        )]
    );
}

#[test]
fn a_replay_sends_what_is_held_under_its_token_and_ends() {
    let (mut decoder, mut out) = linked();
    let target = Name::encode("printer.local").unwrap();
    let service = Record::unique(
        instance_of(b"Hall Printer", "_ipp._tcp"),
        RData::Srv(Service {
            priority: 0,
            weight: 0,
            port: 631,
            target,
        }),
    );
    let name = instance_of(b"Hall Printer", "_ipp._tcp");
    handled(&mut decoder, &ask(0, 4, Form::Service, &name), &mut out);
    handled(
        &mut decoder,
        &datagram(SEC, b"eth0", &response(&[service])),
        &mut out,
    );
    out.take();
    handled(
        &mut decoder,
        &frame(&ToDecoder::Replay {
            question: 4,
            token: 11,
        }),
        &mut out,
    );
    let said = out.take();
    assert_eq!(said[..2], [Said::Held(11, 4), Said::Replayed(11)]);
}

#[test]
fn a_stopped_question_is_told_nothing_more() {
    let (mut decoder, mut out) = linked();
    let host = Name::encode("printer.local").unwrap();
    handled(&mut decoder, &ask(0, 2, Form::AddressV4, &host), &mut out);
    handled(
        &mut decoder,
        &frame(&ToDecoder::Stop { question: 2 }),
        &mut out,
    );
    out.take();
    handled(
        &mut decoder,
        &datagram(SEC, b"eth0", &announcement("printer.local", 5, 120)),
        &mut out,
    );
    assert!(out.answers().is_empty());
}

#[test]
fn a_link_that_goes_down_takes_its_engine_and_one_that_comes_up_is_asked_everything() {
    let (mut decoder, mut out) = linked();
    let host = Name::encode("printer.local").unwrap();
    handled(&mut decoder, &ask(0, 5, Form::AddressV4, &host), &mut out);
    handled(
        &mut decoder,
        &datagram(SEC, b"eth0", &announcement("printer.local", 5, 120)),
        &mut out,
    );
    handled(&mut decoder, &link(2 * SEC, b"eth0", false), &mut out);
    assert!(out.take().contains(&Said::Linked(iface(b"eth0"), false)));
    assert!(decoder.links.is_empty());
    // A datagram for it now is one the front would never relay.
    assert_eq!(
        handled(&mut decoder, &datagram(3 * SEC, b"eth0", b"x"), &mut out),
        SessionStep::Finished
    );

    let (mut decoder, mut out) = linked();
    handled(&mut decoder, &ask(0, 6, Form::AddressV4, &host), &mut out);
    handled(&mut decoder, &link(SEC, b"wlan0", true), &mut out);
    let wlan = decoder
        .links
        .iter()
        .find(|link| link.name == iface(b"wlan0"))
        .expect("an engine for the new link");
    assert_eq!(wlan.asked.len(), 1, "asked the standing question");
}

#[test]
fn a_record_learned_on_one_interface_never_reaches_another() {
    let (mut decoder, mut out) = linked();
    handled(&mut decoder, &link(0, b"wlan0", true), &mut out);
    let host = Name::encode("printer.local").unwrap();
    handled(&mut decoder, &ask(0, 8, Form::AddressV4, &host), &mut out);
    out.take();
    handled(
        &mut decoder,
        &datagram(SEC, b"wlan0", &announcement("printer.local", 5, 120)),
        &mut out,
    );
    assert_eq!(
        out.answers(),
        [Said::Answer(
            8,
            iface(b"wlan0"),
            Change::Added,
            b"10.0.0.5".to_vec()
        )]
    );
    let eth0 = decoder
        .links
        .iter()
        .find(|link| link.name == iface(b"eth0"))
        .expect("eth0 runs");
    assert_eq!(eth0.engine.cache().lookup(&host, RecordType::A).count(), 0);
}

#[test]
fn an_unchanged_deadline_is_reported_once_and_a_tick_is_always_answered() {
    let (mut decoder, mut out) = linked();
    let announced = announcement("printer.local", 5, 120);
    handled(&mut decoder, &datagram(SEC, b"eth0", &announced), &mut out);
    // The same record at the same instant renews it to the same expiry.
    handled(&mut decoder, &datagram(SEC, b"eth0", &announced), &mut out);
    assert_eq!(out.take(), [Said::Deadline(Some(121 * SEC))]);
    handled(
        &mut decoder,
        &frame(&ToDecoder::Tick { now: 2 * SEC }),
        &mut out,
    );
    assert_eq!(out.take(), [Said::Deadline(Some(121 * SEC))]);
}

#[test]
fn time_never_runs_backwards() {
    let (mut decoder, mut out) = linked();
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
    assert_eq!(decoder.links[0].engine.cache().len(), 1);
    assert_eq!(decoder.now, 50 * SEC);
}

#[test]
fn hostile_payloads_are_dropped_and_leave_nothing_cached() {
    let (mut decoder, mut out) = linked();
    for payload in [&b""[..], b"\x00", &[0xFF; 600][..], &[0u8; 12][..]] {
        assert_eq!(
            handled(&mut decoder, &datagram(SEC, b"eth0", payload), &mut out),
            SessionStep::Continue
        );
    }
    assert!(decoder.links[0].engine.cache().is_empty());
}
