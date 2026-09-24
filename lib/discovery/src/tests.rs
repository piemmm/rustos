//! Host tests for the discovery client, against an in-memory service.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::discovery_ipc::{
    encode_id_reply, Answer, Change, CollectWriter, DiscoveryRequest, Entry, Query,
    ServiceTypeField, Transport as Proto,
};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::reply::encode_status_reply;
use tairix_abi::Errno;

use super::{Applied, DiscoveryError, Held, OwnedAnswer, Session, Transport};

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn instance<'a>(change: Change, label: &'a [u8], on: &[u8]) -> Entry<'a> {
    Entry::Answer {
        request: 1,
        interface: iface(on),
        change,
        ttl: 120,
        answer: Answer::Instance { label },
    }
}

/// A service that answers as the real one does, recording every call.
#[derive(Default)]
struct Fake {
    calls: Vec<Vec<u8>>,
    /// The entries the next collect answers with.
    waiting: Vec<(Change, Vec<u8>)>,
    refuse_start: Option<Errno>,
    absent: bool,
}

impl Transport for &mut Fake {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        if self.absent {
            return Err(Errno::NotFound);
        }
        self.calls.push(request.to_vec());
        match DiscoveryRequest::decode(request).expect("the client encodes what it sends") {
            DiscoveryRequest::Open { .. } => encode_id_reply(Ok(7), reply),
            DiscoveryRequest::Start { .. } => {
                encode_id_reply(self.refuse_start.map_or(Ok(1), Err), reply)
            }
            DiscoveryRequest::Stop { .. } | DiscoveryRequest::Close { .. } => {
                let status = encode_status_reply(Ok(()));
                reply[..status.len()].copy_from_slice(&status);
                Ok(status.len())
            }
            DiscoveryRequest::Collect { capacity, .. } => {
                let mut writer = CollectWriter::new(&mut reply[..capacity as usize])?;
                for (change, label) in self.waiting.drain(..) {
                    writer.push(&instance(change, &label, b"eth0"))?;
                }
                Ok(writer.finish(false))
            }
        }
    }
}

fn ipp() -> Query<'static> {
    Query::Browse {
        service: ServiceTypeField {
            name: b"ipp",
            transport: Proto::Tcp,
        },
    }
}

#[test]
fn a_session_opens_starts_collects_and_closes() {
    let mut fake = Fake {
        waiting: vec![(Change::Added, b"Hall".to_vec())],
        ..Fake::default()
    };
    let mut session = Session::open(&mut fake, 9).unwrap();
    assert_eq!(session.id(), 7);
    assert_eq!(session.start(&ipp()), Ok(1));
    let mut seen = Vec::new();
    let more = session
        .collect(&mut |entry| seen.push(super::request_of(entry)))
        .unwrap();
    assert!(!more);
    assert_eq!(seen, [1]);
    session.stop(1).unwrap();
    session.close().unwrap();
    let ops: Vec<u16> = fake
        .calls
        .iter()
        .map(|call| u16::from_le_bytes([call[6], call[7]]))
        .collect();
    assert_eq!(ops, [1, 3, 5, 4, 2], "open, start, collect, stop, close");
}

#[test]
fn a_dropped_session_is_closed() {
    let mut fake = Fake::default();
    drop(Session::open(&mut fake, 9).unwrap());
    let last = fake.calls.last().expect("a call");
    assert_eq!(u16::from_le_bytes([last[6], last[7]]), 2, "closed");
}

#[test]
fn no_service_is_unavailable_and_a_refusal_says_why() {
    let mut fake = Fake {
        absent: true,
        ..Fake::default()
    };
    assert_eq!(
        Session::open(&mut fake, 9).map(|_| ()),
        Err(DiscoveryError::Unavailable)
    );
    let mut fake = Fake {
        refuse_start: Some(Errno::PermissionDenied),
        ..Fake::default()
    };
    let mut session = Session::open(&mut fake, 9).unwrap();
    assert_eq!(
        session.start(&ipp()),
        Err(DiscoveryError::Refused(Errno::PermissionDenied))
    );
}

#[test]
fn a_held_set_follows_every_edge_and_every_flush() {
    let mut held = Held::default();
    assert_eq!(
        held.apply(&instance(Change::Added, b"Hall", b"eth0")),
        Applied::Changed
    );
    assert_eq!(
        held.apply(&instance(Change::Added, b"Hall", b"wlan0")),
        Applied::Changed,
        "the same answer on another link is another answer"
    );
    assert_eq!(
        held.apply(&instance(Change::Refreshed, b"HALL", b"eth0")),
        Applied::Unchanged,
        "names compare without case"
    );
    assert_eq!(held.answers().len(), 2);
    assert_eq!(
        held.apply(&Entry::Flush {
            request: 1,
            interface: iface(b"eth0"),
        }),
        Applied::Changed
    );
    assert_eq!(held.answers().len(), 1);
    assert_eq!(held.answers()[0].interface, iface(b"wlan0"));
    assert_eq!(
        held.apply(&instance(Change::Retired, b"Hall", b"wlan0")),
        Applied::Changed
    );
    assert!(held.answers().is_empty());
    assert_eq!(
        held.apply(&instance(Change::Retired, b"Hall", b"wlan0")),
        Applied::Unchanged
    );
    assert_eq!(held.apply(&Entry::Lost { request: 1 }), Applied::Lost);
    assert!(held.is_lost());
}

#[test]
fn an_owned_answer_keeps_what_the_entry_said() {
    let mut held = Held::default();
    held.apply(&Entry::Answer {
        request: 1,
        interface: iface(b"eth0"),
        change: Change::Added,
        ttl: 120,
        answer: Answer::Service {
            priority: 1,
            weight: 2,
            port: 631,
            target: b"\x07printer\x05local\x00",
        },
    });
    assert_eq!(
        held.answers()[0].answer,
        OwnedAnswer::Service {
            priority: 1,
            weight: 2,
            port: 631,
            target: b"\x07printer\x05local\x00".to_vec(),
        }
    );
}
