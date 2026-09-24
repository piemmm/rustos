//! Unit tests for the peer-exit watch registry.

use super::*;
use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::UserId;
use tairix_log::Event;

struct Quiet;

impl tairix_log::Sink for Quiet {
    fn write_event(&self, _event: &Event<'_>) {}
}

fn instance(tag: u8) -> ProcId {
    let mut raw = [0xC0u8; 16];
    raw[15] = tag;
    ProcId::from_raw(raw)
}

fn record(process: u64, id: ProcId) -> TaskCapabilities {
    let mut set = CapabilitySet::empty();
    set.insert(CapabilityId::NET);
    TaskCapabilities::derive(ProcessId(process), UserId(1000), set, set, &Quiet).with_proc_id(id)
}

fn table_with(live: &[(u64, ProcId)]) -> RwLock<CapTable> {
    let table = RwLock::new(CapTable::new());
    for &(process, id) in live {
        table.write().insert(record(process, id));
    }
    table
}

fn drain(peers: &PeerWatch, watcher: TaskId) -> Vec<ProcId> {
    let mut out = Vec::new();
    while let Ok(exited) = peers.oldest(watcher) {
        peers.consume(watcher, exited);
        out.push(exited);
    }
    out
}

#[test]
fn a_watched_instance_reports_its_exit_once_its_record_is_removed() {
    let peer = instance(1);
    let caps = table_with(&[(9001, peer)]);
    let peers = PeerWatch::new();
    peers
        .watch(&caps, 70_001, peer)
        .expect("a live instance is watchable");
    assert!(!peers.ready(70_001));
    assert!(remove_record(Some(&peers), &caps, ProcessId(9001)).is_some());
    assert!(peers.ready(70_001));
    assert_eq!(drain(&peers, 70_001), [peer]);
    assert!(!peers.ready(70_001));
    assert_eq!(peers.oldest(70_001), Err(Errno::WouldBlock));
}

#[test]
fn an_instance_with_no_live_record_is_refused_so_no_exit_can_be_missed() {
    let peer = instance(2);
    let caps = table_with(&[(9002, peer)]);
    let peers = PeerWatch::new();
    assert_eq!(
        peers.watch(&caps, 70_002, instance(99)),
        Err(Errno::NotFound)
    );
    assert_eq!(
        peers.watch(&caps, 70_002, ProcId::KERNEL),
        Err(Errno::NotFound)
    );
    // The peer dies before the watch is taken: the watcher is told it is
    // gone rather than left waiting for an exit that already happened.
    remove_record(Some(&peers), &caps, ProcessId(9002));
    assert_eq!(peers.watch(&caps, 70_002, peer), Err(Errno::NotFound));
    assert!(drain(&peers, 70_002).is_empty());
}

#[test]
fn watching_twice_reports_one_exit() {
    let peer = instance(3);
    let caps = table_with(&[(9003, peer)]);
    let peers = PeerWatch::new();
    peers.watch(&caps, 70_003, peer).expect("watch");
    peers.watch(&caps, 70_003, peer).expect("idempotent");
    remove_record(Some(&peers), &caps, ProcessId(9003));
    assert_eq!(drain(&peers, 70_003), [peer]);
}

#[test]
fn an_unwatched_instance_reports_nothing() {
    let peer = instance(4);
    let caps = table_with(&[(9004, peer)]);
    let peers = PeerWatch::new();
    peers.watch(&caps, 70_004, peer).expect("watch");
    peers.unwatch(70_004, peer).expect("unwatch");
    assert_eq!(peers.unwatch(70_004, peer), Err(Errno::NotFound));
    remove_record(Some(&peers), &caps, ProcessId(9004));
    assert!(drain(&peers, 70_004).is_empty());
}

#[test]
fn every_watcher_of_an_instance_is_told_and_a_watcher_hears_each_peer_in_order() {
    let (first, second) = (instance(5), instance(6));
    let caps = table_with(&[(9005, first), (9006, second)]);
    let peers = PeerWatch::new();
    for watcher in [70_005, 70_006] {
        peers.watch(&caps, watcher, first).expect("watch");
    }
    peers.watch(&caps, 70_005, second).expect("watch");
    remove_record(Some(&peers), &caps, ProcessId(9006));
    remove_record(Some(&peers), &caps, ProcessId(9005));
    assert_eq!(drain(&peers, 70_005), [second, first]);
    assert_eq!(drain(&peers, 70_006), [first]);
}

#[test]
fn a_fired_watch_is_no_longer_held() {
    let peer = instance(7);
    let caps = table_with(&[(9007, peer)]);
    let peers = PeerWatch::new();
    peers.watch(&caps, 70_007, peer).expect("watch");
    remove_record(Some(&peers), &caps, ProcessId(9007));
    // Already fired: its exit waits to be taken, and there is nothing left
    // to unwatch.
    assert_eq!(peers.unwatch(70_007, peer), Err(Errno::NotFound));
    assert_eq!(drain(&peers, 70_007), [peer]);
}

#[test]
fn a_forgotten_watcher_leaves_nothing_for_whoever_draws_its_id_next() {
    let (peer, other) = (instance(8), instance(9));
    let caps = table_with(&[(9008, peer), (9009, other)]);
    let peers = PeerWatch::new();
    peers.watch(&caps, 70_008, peer).expect("watch");
    peers.watch(&caps, 70_008, other).expect("watch");
    remove_record(Some(&peers), &caps, ProcessId(9009));
    peers.forget_watcher(70_008);
    assert!(
        !peers.ready(70_008),
        "the untaken exit went with the watcher"
    );
    remove_record(Some(&peers), &caps, ProcessId(9008));
    assert!(drain(&peers, 70_008).is_empty(), "and so did the watch");
}

#[test]
fn many_watches_firing_together_are_all_queued() {
    let live: Vec<(u64, ProcId)> = (0..64u8)
        .map(|n| (9100 + u64::from(n), instance(100 + n)))
        .collect();
    let caps = table_with(&live);
    let peers = PeerWatch::new();
    for &(_, peer) in &live {
        peers.watch(&caps, 70_009, peer).expect("watch");
    }
    for &(process, _) in &live {
        remove_record(Some(&peers), &caps, ProcessId(process));
    }
    let told = drain(&peers, 70_009);
    assert_eq!(told.len(), live.len());
    assert!(live.iter().all(|(_, peer)| told.contains(peer)));
}

#[test]
fn a_consume_of_anything_but_the_oldest_exit_changes_nothing() {
    let peer = instance(10);
    let caps = table_with(&[(9010, peer)]);
    let peers = PeerWatch::new();
    peers.watch(&caps, 70_010, peer).expect("watch");
    remove_record(Some(&peers), &caps, ProcessId(9010));
    peers.consume(70_010, instance(77));
    assert_eq!(peers.oldest(70_010), Ok(peer));
}

#[test]
fn a_retired_watcher_leaves_the_queue_as_well_as_the_registry() {
    let peers = PeerWatch::new();
    peers.join(70_011);
    assert!(!peers.parked.is_empty());
    peers.leave(70_011);
    assert!(peers.parked.is_empty());
    // A thread torn down while parked never unwinds to its own `leave`.
    peers.join(70_012);
    peers.forget_watcher(70_012);
    assert!(
        peers.parked.is_empty(),
        "retirement took its place in the queue"
    );
}
