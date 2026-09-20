//! Host tests of the audio device-channel hand-off policy.

extern crate alloc;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::*;
use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey, HwResource, HW_NODE_ROOT};
use tairix_log::DiscardSink;

/// A recording [`AudiodBind`] double: captures each bind call and answers
/// each with a scripted result.
struct RecordingBind {
    calls: RefCell<Vec<u64>>,
    results: RefCell<Vec<Result<(), Errno>>>,
}

impl RecordingBind {
    fn new(results: Vec<Result<(), Errno>>) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            results: RefCell::new(results),
        }
    }
}

impl AudiodBind for RecordingBind {
    fn bind_driver(&mut self, endpoint_id: u64) -> Result<(), Errno> {
        self.calls.borrow_mut().push(endpoint_id);
        self.results.borrow_mut().pop().unwrap_or(Ok(()))
    }
}

/// An `audiochan` node with id `id` publishing `endpoint`.
fn audiochan_node(id: u32, endpoint: u64) -> HwNode {
    let mut node = HwNode::new(id, HW_NODE_ROOT, HwDeviceClass::Audio);
    node.push_match_key(HwMatchKey::compatible(AUDIOCHAN_NODE_COMPATIBLE).expect("key"))
        .expect("push key");
    node.push_resource(HwResource::endpoint(endpoint))
        .expect("push endpoint");
    node
}

#[test]
fn only_an_audiochan_node_carrying_an_endpoint_is_recognised() {
    assert_eq!(audiochan_endpoint(&audiochan_node(1, 4_242)), Some(4_242));
    // A node of the right class but the wrong compatible string.
    let mut other = HwNode::new(2, HW_NODE_ROOT, HwDeviceClass::Audio);
    other
        .push_match_key(HwMatchKey::compatible(b"tairix,netchan").expect("key"))
        .expect("push key");
    other
        .push_resource(HwResource::endpoint(9))
        .expect("push endpoint");
    assert_eq!(audiochan_endpoint(&other), None);
    // The right node with no endpoint resource is malformed, never guessed.
    let mut bare = HwNode::new(3, HW_NODE_ROOT, HwDeviceClass::Audio);
    bare.push_match_key(HwMatchKey::compatible(AUDIOCHAN_NODE_COMPATIBLE).expect("key"))
        .expect("push key");
    assert_eq!(audiochan_endpoint(&bare), None);
}

#[test]
fn each_channel_is_handed_over_exactly_once_across_generation_bumps() {
    let nodes = alloc::vec![audiochan_node(1, 100), audiochan_node(2, 101)];
    let mut state = AudioBindState::new();
    let mut bind = RecordingBind::new(Vec::new());
    bind_new_channels(&nodes, &mut state, &mut bind, &DiscardSink);
    bind_new_channels(&nodes, &mut state, &mut bind, &DiscardSink);
    assert_eq!(*bind.calls.borrow(), alloc::vec![100, 101]);
    assert!(state.is_bound(100) && state.is_bound(101));
}

#[test]
fn a_refused_hand_off_is_retried_rather_than_recorded() {
    let nodes = alloc::vec![audiochan_node(1, 100)];
    let mut state = AudioBindState::new();
    // Scripted results pop from the back: refuse first, accept on retry.
    let mut bind = RecordingBind::new(alloc::vec![Ok(()), Err(Errno::NotConnected)]);
    bind_new_channels(&nodes, &mut state, &mut bind, &DiscardSink);
    assert!(!state.is_bound(100), "a refused bind is not recorded");
    bind_new_channels(&nodes, &mut state, &mut bind, &DiscardSink);
    assert!(state.is_bound(100), "retried and bound");
    assert_eq!(*bind.calls.borrow(), alloc::vec![100, 100]);
}
