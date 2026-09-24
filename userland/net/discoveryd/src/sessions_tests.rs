//! Unit tests for the client session table.

use super::{Sessions, MAX_SESSIONS};
use alloc::vec;
use tairix_abi::discovery_ipc::{
    Answer, Change, CollectReply, Entry, COLLECT_MIN_CAPACITY, DISCOVERY_MAX_REPLY,
    REQUESTS_PER_SESSION, SESSIONS_PER_ACCOUNT, SESSION_QUEUE_BYTES,
};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::{Errno, ProcId};
use tairix_inline::ArrayVec;

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn owner(n: u8) -> ProcId {
    ProcId::from_raw([n; 16])
}

fn text(octets: &[u8]) -> Entry<'_> {
    Entry::Answer {
        request: 0,
        interface: iface(b"eth0"),
        change: Change::Added,
        ttl: 4500,
        answer: Answer::Text { octets },
    }
}

/// A session of owner 1 with one request.
fn one() -> (Sessions, u32, u32) {
    let mut sessions = Sessions::default();
    let id = sessions.open(owner(1), 1000, 77).unwrap();
    let request = sessions
        .owned(id, owner(1))
        .unwrap()
        .add_request(ArrayVec::new())
        .unwrap();
    (sessions, id, request)
}

fn collect(sessions: &mut Sessions, id: u32) -> (usize, bool) {
    let mut reply = vec![0u8; DISCOVERY_MAX_REPLY];
    let len = sessions
        .owned(id, owner(1))
        .unwrap()
        .collect(DISCOVERY_MAX_REPLY, &mut reply)
        .unwrap();
    let parsed = CollectReply::parse(&reply[..len]).unwrap();
    (parsed.len(), parsed.more)
}

#[test]
fn a_session_belongs_to_whoever_opened_it() {
    let (mut sessions, id, _) = one();
    assert_eq!(
        sessions.owned(id, owner(2)).map(|_| ()),
        Err(Errno::NotFound)
    );
    assert_eq!(
        sessions.close(id, owner(2)).map(|_| ()),
        Err(Errno::NotFound)
    );
    assert!(sessions.holds_any(owner(1)));
    assert!(!sessions.holds_any(owner(2)));
    assert_eq!(
        sessions.close(id, owner(1)).map(|requests| requests.len()),
        Ok(1)
    );
    assert!(!sessions.holds_any(owner(1)));
}

#[test]
fn an_account_holds_a_bounded_number_of_sessions_and_so_does_the_service() {
    let mut sessions = Sessions::default();
    for n in 0..SESSIONS_PER_ACCOUNT {
        let who = owner(u8::try_from(n).unwrap());
        assert!(sessions.open(who, 1000, 1).is_ok());
    }
    assert_eq!(sessions.open(owner(99), 1000, 1), Err(Errno::LimitExceeded));
    let mut all = Sessions::default();
    for n in 0..MAX_SESSIONS {
        let uid = u32::try_from(n).unwrap();
        assert!(all.open(owner(1), uid, 1).is_ok());
    }
    assert_eq!(all.open(owner(1), 5_000_000, 1), Err(Errno::LimitExceeded));
}

#[test]
fn a_session_holds_a_bounded_number_of_requests() {
    let (mut sessions, id, _) = one();
    let session = sessions.owned(id, owner(1)).unwrap();
    for _ in 1..REQUESTS_PER_SESSION {
        assert!(session.add_request(ArrayVec::new()).is_ok());
    }
    assert!(!session.has_room());
    assert_eq!(
        session.add_request(ArrayVec::new()),
        Err(Errno::LimitExceeded)
    );
}

#[test]
fn a_session_is_rung_once_until_it_has_taken_everything() {
    let (mut sessions, id, request) = one();
    assert_eq!(sessions.queue(id, request, &text(b"\x03a=1")), Some(77));
    assert_eq!(
        sessions.queue(id, request, &text(b"\x03b=2")),
        None,
        "already rung"
    );
    assert_eq!(collect(&mut sessions, id), (2, false));
    assert_eq!(
        sessions.queue(id, request, &text(b"\x03c=3")),
        Some(77),
        "quiet again"
    );
}

#[test]
fn an_entry_is_rewritten_to_name_its_request() {
    let (mut sessions, id, request) = one();
    sessions.queue(id, request, &text(b"\x03a=1"));
    let mut reply = vec![0u8; DISCOVERY_MAX_REPLY];
    let len = sessions
        .owned(id, owner(1))
        .unwrap()
        .collect(DISCOVERY_MAX_REPLY, &mut reply)
        .unwrap();
    let parsed = CollectReply::parse(&reply[..len]).unwrap();
    let entry = parsed.entries().next().unwrap();
    assert!(matches!(entry, Entry::Answer { request: r, .. } if r == request));
}

#[test]
fn a_full_queue_drops_the_requests_updates_and_says_so_after_the_rest() {
    let (mut sessions, id, request) = one();
    let big = [0x10u8; 400];
    let mut queued = 0;
    while sessions.owned(id, owner(1)).is_ok() {
        sessions.queue(id, request, &text(&big));
        queued += 1;
        if queued * (big.len() + 30) > SESSION_QUEUE_BYTES + 1000 {
            break;
        }
    }
    let mut taken = 0;
    let mut lost = 0;
    loop {
        let mut reply = vec![0u8; DISCOVERY_MAX_REPLY];
        let len = sessions
            .owned(id, owner(1))
            .unwrap()
            .collect(DISCOVERY_MAX_REPLY, &mut reply)
            .unwrap();
        let parsed = CollectReply::parse(&reply[..len]).unwrap();
        for entry in parsed.entries() {
            if let Entry::Lost { request: r } = entry {
                assert_eq!(r, request);
                lost += 1;
            } else {
                assert_eq!(lost, 0, "the loss comes after what was queued before it");
                taken += 1;
            }
        }
        if !parsed.more {
            break;
        }
    }
    assert!(taken > 0 && taken < queued);
    assert_eq!(lost, 1, "told once");
    assert_eq!(
        sessions.queue(id, request, &text(b"\x01x")),
        None,
        "nothing more is queued for it"
    );
    assert_eq!(collect(&mut sessions, id), (0, false));
}

#[test]
fn a_collect_names_a_capacity_that_can_always_hold_one_entry() {
    let (mut sessions, id, _) = one();
    let session = sessions.owned(id, owner(1)).unwrap();
    let mut reply = vec![0u8; DISCOVERY_MAX_REPLY];
    assert_eq!(
        session.collect(COLLECT_MIN_CAPACITY - 1, &mut reply),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        session.collect(COLLECT_MIN_CAPACITY, &mut reply[..COLLECT_MIN_CAPACITY - 1]),
        Err(Errno::BufferTooSmall)
    );
    let len = session.collect(COLLECT_MIN_CAPACITY, &mut reply).unwrap();
    assert!(CollectReply::parse(&reply[..len]).unwrap().is_empty());
}

#[test]
fn a_doorbell_refused_for_room_is_owed_until_it_is_posted() {
    let (mut sessions, id, request) = one();
    assert_eq!(sessions.queue(id, request, &text(b"\x01x")), Some(77));
    sessions.rang(id, Err(Errno::WouldBlock));
    assert_eq!(sessions.owed_rings().collect::<alloc::vec::Vec<_>>(), [77]);
    assert_eq!(
        sessions.queue(id, request, &text(b"\x01y")),
        None,
        "owed, not rung"
    );
    assert_eq!(sessions.take_owed(), [(id, 77)]);
    sessions.rang(id, Ok(()));
    assert_eq!(sessions.owed_rings().count(), 0);
    assert!(sessions.take_owed().is_empty());
}

#[test]
fn an_exited_owners_sessions_all_close() {
    let mut sessions = Sessions::default();
    let a = sessions.open(owner(1), 1000, 1).unwrap();
    let b = sessions.open(owner(1), 1000, 2).unwrap();
    let kept = sessions.open(owner(2), 1000, 3).unwrap();
    let mut closed: alloc::vec::Vec<u32> = sessions
        .close_all(owner(1))
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    closed.sort_unstable();
    assert_eq!(closed, [a, b]);
    assert!(sessions.owned(kept, owner(2)).is_ok());
    let _ = Entry::Flush {
        request: 0,
        interface: iface(b"eth0"),
    };
}

#[test]
fn a_queue_that_is_not_whole_entries_is_dropped_rather_than_read() {
    for bogus in [
        // A length past any entry.
        vec![0xFF, 0xFF, 1, 2, 3],
        // A length past what is queued.
        vec![40, 0, 1, 2, 3],
    ] {
        let (mut sessions, id, _) = one();
        sessions
            .owned(id, owner(1))
            .unwrap()
            .queue
            .extend(bogus.iter().copied());
        assert_eq!(collect(&mut sessions, id), (0, false));
        assert!(sessions.owned(id, owner(1)).unwrap().queue.is_empty());
    }
}
