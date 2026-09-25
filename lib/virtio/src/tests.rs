//! Cross-module unit tests: end-to-end split-virtqueue protocol
//! against the in-process [`crate::MockTransport`] peer.

use crate::dma::{BounceBuffer, DmaSlab};
use crate::host::{DmaHost, MockHost, MockWait, VirtioHost};
use crate::packed::PackedQueue;
use crate::queue::{ChainSegment, SplitQueue};
use crate::request::RequestQueue;
use crate::transport::{ChainView, Direction, MockTransport, Status, Transport, VirtioError};
use alloc::boxed::Box;
use tairix_abi::driver::BufferClass;

/// Build a `'static` reference to a freshly-leaked `MockHost` —
/// the unit tests hand this to `SplitQueue::new`.
fn static_host() -> &'static MockHost {
    Box::leak(Box::new(MockHost::new()))
}

#[test]
fn split_queue_initialises_free_list_and_programs_transport() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let q = SplitQueue::new(&mut t, host, 0, 8, 1).expect("setup");
    assert_eq!(q.index(), 0);
    assert_eq!(q.size(), 8);
    assert_eq!(q.free_count(), 8);
}

#[test]
fn split_queue_rejects_non_power_of_two() {
    let mut t = MockTransport::new(1, 16, 0, 0);
    let host = static_host();
    assert_eq!(
        SplitQueue::new(&mut t, host, 0, 7, 1).map(|_| ()),
        Err(VirtioError::QueueSizeTooLarge)
    );
}

#[test]
fn a_non_conformant_device_maximum_is_rounded_not_refused() {
    // virtio §2.6 admits only power-of-two queue sizes, so a device
    // advertising a maximum that is not one is non-conformant. Where that
    // cap binds a conformant request, take the largest conformant size
    // below it: refusing would leave the device with no queue at all.
    let mut t = MockTransport::new(1, 6, 0, 0);
    let host = static_host();
    let q = SplitQueue::new(&mut t, host, 0, 8, 1).expect("setup");
    assert_eq!(q.size(), 4);
    assert_eq!(q.free_count(), 4);
}

#[test]
fn a_queue_too_shallow_for_what_the_driver_needs_is_refused_before_it_is_programmed() {
    // Capped silently, the queue would refuse the driver's chain only once
    // the driver had begun it.
    let mut t = MockTransport::new(1, 2, 0, 0);
    let host = MockHost::new();
    assert_eq!(
        SplitQueue::new(&mut t, &host, 0, 8, 3).map(|_| ()),
        Err(VirtioError::QueueTooShallow)
    );
    assert_eq!(
        PackedQueue::new(&mut t, &host, 0, 8, 3).map(|_| ()),
        Err(VirtioError::QueueTooShallow)
    );
    assert_eq!(host.bytes_allocated(), 0, "no ring was carved");
    assert_eq!(
        t.publish_raw_used(0, 0, 0),
        Err(VirtioError::DeviceFault),
        "and none handed to the device"
    );
    let q = SplitQueue::new(&mut t, &host, 0, 8, 2).expect("exactly enough");
    assert_eq!(q.size(), 2);
}

#[test]
fn add_chain_consumes_descriptors_and_publishes_avail() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 8, 1).unwrap();
    let mut slab: DmaSlab = host.alloc_dma_zeroed(64).unwrap();
    let phys = slab.phys();
    slab.as_bytes_mut()[..4].copy_from_slice(b"PING");
    let segments = [ChainSegment {
        phys,
        len: 4,
        direction: Direction::DeviceRead,
    }];
    let head = q.add_chain(&segments).unwrap();
    assert_eq!(head, 0);
    assert_eq!(q.free_count(), 7);
}

#[test]
fn descriptor_chain_round_trip_through_mock_peer() {
    // Two-segment chain: device-read input + device-write output.
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 8, 1).unwrap();
    let mut input: DmaSlab = host.alloc_dma_zeroed(8).unwrap();
    input.as_bytes_mut()[..4].copy_from_slice(b"PING");
    let output: DmaSlab = host.alloc_dma_zeroed(8).unwrap();
    let segs = [
        ChainSegment {
            phys: input.phys(),
            len: 4,
            direction: Direction::DeviceRead,
        },
        ChainSegment {
            phys: output.phys(),
            len: 8,
            direction: Direction::DeviceWrite,
        },
    ];
    let head = q.add_chain(&segs).unwrap();
    // Install an echo shim: copies device_read bytes into the
    // device_write segment with the prefix swapped to "PONG".
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            assert_eq!(chain.device_read.len(), 1);
            assert_eq!(chain.device_write.len(), 1);
            let inp = chain.device_read[0];
            assert_eq!(&inp[..4], b"PING");
            let out = &mut chain.device_write[0];
            out[..4].copy_from_slice(b"PONG");
            out[4..].fill(0);
            Ok(u32::try_from(out.len()).unwrap_or(0))
        }),
    );
    q.kick(&mut t);
    let drained = t.drain_queue(0).unwrap();
    assert_eq!(drained, 1);
    let used = q.poll_used().unwrap();
    assert_eq!(used.head, head);
    assert_eq!(used.written, 8);
    // Read back the response through the device-write region's
    // ptr. We round-trip via the raw phys we passed (which is the
    // host-leaked buffer's pointer).
    // SAFETY: `output.phys()` was set to the leaked Box pointer by
    // MockHost; the region is alive for `'static`.
    let response: &[u8] = unsafe { core::slice::from_raw_parts(output.phys() as *const u8, 8) };
    assert_eq!(&response[..4], b"PONG");
}

#[test]
fn add_chain_rejects_empty_and_too_long() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    assert_eq!(q.add_chain(&[]), Err(VirtioError::DescriptorTableOverflow));
    // Build segments larger than queue_size = 4.
    let phys = host.alloc_dma_zeroed(1).unwrap().phys();
    let too_long = [ChainSegment {
        phys,
        len: 1,
        direction: Direction::DeviceRead,
    }; 5];
    assert_eq!(
        q.add_chain(&too_long),
        Err(VirtioError::DescriptorTableOverflow)
    );
}

#[test]
fn add_chain_exhausts_free_pool() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let phys = host.alloc_dma_zeroed(1).unwrap().phys();
    // Four 1-descriptor chains: should succeed.
    for _ in 0..4 {
        q.add_chain(&[ChainSegment {
            phys,
            len: 1,
            direction: Direction::DeviceRead,
        }])
        .unwrap();
    }
    assert_eq!(q.free_count(), 0);
    // Fifth must fail with QueueFull.
    assert_eq!(
        q.add_chain(&[ChainSegment {
            phys,
            len: 1,
            direction: Direction::DeviceRead,
        }]),
        Err(VirtioError::QueueFull)
    );
}

#[test]
fn used_ring_wraps_with_reclaim() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let in_region = host.alloc_dma_zeroed(4).unwrap();
    let out_region = host.alloc_dma_zeroed(4).unwrap();
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            // No-op echo: write 1 byte.
            if let Some(out) = chain.device_write.get_mut(0) {
                if !out.is_empty() {
                    out[0] = 0x42;
                }
            }
            Ok(1)
        }),
    );
    // Cycle ten chains through the four-descriptor queue. Each
    // chain has 1 read + 1 write descriptor; reclamation must
    // recycle them so we never see QueueFull.
    for i in 0..10 {
        let head = q
            .add_chain(&[
                ChainSegment {
                    phys: in_region.phys(),
                    len: 4,
                    direction: Direction::DeviceRead,
                },
                ChainSegment {
                    phys: out_region.phys(),
                    len: 4,
                    direction: Direction::DeviceWrite,
                },
            ])
            .unwrap_or_else(|_| panic!("chain {i} should fit after reclaim"));
        q.kick(&mut t);
        t.drain_queue(0).unwrap();
        let token = q.poll_used().unwrap();
        assert_eq!(token.head, head);
        assert_eq!(token.written, 1);
    }
}

#[test]
fn poll_used_returns_no_completion_when_empty() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    assert_eq!(q.poll_used(), Err(VirtioError::NoCompletion));
}

/// Publish a two-descriptor chain on `q`, returning its head.
fn two_segment_chain(q: &mut SplitQueue, region: &DmaSlab) -> u16 {
    q.add_chain(&[
        ChainSegment {
            phys: region.phys(),
            len: 4,
            direction: Direction::DeviceRead,
        },
        ChainSegment {
            phys: region.phys(),
            len: 4,
            direction: Direction::DeviceWrite,
        },
    ])
    .expect("the chain fits")
}

#[test]
fn poll_used_rejects_a_device_head_outside_the_descriptor_table() {
    // (CWE-1257 / Thunderclap): a hostile device publishes a used
    // completion whose head escapes the granted descriptor table. The
    // driver must reject it fail-closed, never walk a
    // descriptor outside the region.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let region = host.alloc_dma_zeroed(4).unwrap();
    let head = two_segment_chain(&mut q, &region);
    // head == queue_size is the first out-of-range index.
    t.publish_raw_used(0, 4, 0).unwrap();
    assert_eq!(q.poll_used(), Err(VirtioError::MalformedCompletion));
    // The queue stays usable: the chain's own completion still works.
    t.publish_raw_used(0, head, 0).unwrap();
    assert_eq!(q.poll_used().map(|tok| tok.head), Ok(head));
    assert_eq!(q.free_count(), 4);
}

#[test]
fn a_completion_for_anything_but_a_chain_the_device_holds_is_refused() {
    // A device may name a free descriptor, the interior of a chain, or a chain
    // it has already returned; reclaiming any of those would hand one
    // descriptor to two chains.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let region = host.alloc_dma_zeroed(4).unwrap();
    let head = two_segment_chain(&mut q, &region);
    for bogus in [head + 1, head + 2] {
        t.publish_raw_used(0, bogus, 0).unwrap();
        assert_eq!(q.poll_used(), Err(VirtioError::MalformedCompletion));
    }
    assert_eq!(q.free_count(), 2, "nothing was reclaimed on a bogus word");
    t.publish_raw_used(0, head, 0).unwrap();
    assert_eq!(q.poll_used().map(|tok| tok.head), Ok(head));
    t.publish_raw_used(0, head, 0).unwrap();
    assert_eq!(
        q.poll_used(),
        Err(VirtioError::MalformedCompletion),
        "a chain is returned once"
    );
    assert_eq!(q.free_count(), 4);
}

#[test]
fn a_device_writing_over_the_descriptor_table_cannot_corrupt_the_free_list() {
    // The free list and chain links live in driver memory; a device
    // scribbling on the table it reads is never trusted for them.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let region = host.alloc_dma_zeroed(4).unwrap();
    let head = two_segment_chain(&mut q, &region);
    // `0xFF, 0xFF` in bytes 14..16 of the head's entry: `next == 0xFFFF`.
    t.poke_descriptor(0, 14, 0xFF).unwrap();
    t.poke_descriptor(0, 15, 0xFF).unwrap();
    t.publish_raw_used(0, head, 0).unwrap();
    assert_eq!(q.poll_used().map(|tok| tok.head), Ok(head));
    assert_eq!(q.free_count(), 4, "exactly the chain came back");
    let segments = [ChainSegment {
        phys: region.phys(),
        len: 4,
        direction: Direction::DeviceRead,
    }; 4];
    assert!(
        q.add_chain(&segments).is_ok(),
        "the whole table is free again"
    );
}

#[test]
fn the_peer_refuses_a_chain_that_leaves_the_table_or_loops() {
    // Followed blindly, a `next` past the table reads memory the table does
    // not own.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 2).unwrap();
    let region = host.alloc_dma_zeroed(4).unwrap();
    let head = two_segment_chain(&mut q, &region);
    assert_eq!(t.chain_descriptors(0, head).map(|chain| chain.len()), Ok(2));
    // Bytes 14..16 of the head's entry are its `next`.
    let next = usize::from(head) * 16 + 14;
    t.poke_descriptor(0, next, 0xFF).unwrap();
    t.poke_descriptor(0, next + 1, 0xFF).unwrap();
    assert_eq!(
        t.chain_descriptors(0, head),
        Err(VirtioError::DescriptorTableOverflow)
    );
    t.poke_descriptor(0, next, u8::try_from(head).unwrap())
        .unwrap();
    t.poke_descriptor(0, next + 1, 0).unwrap();
    assert_eq!(
        t.chain_descriptors(0, head),
        Err(VirtioError::DescriptorTableOverflow),
        "a chain naming itself"
    );
}

#[test]
fn transport_setup_records_status_progression() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    t.reset().expect("the mock confirms its reset");
    let mut s = t.status();
    s = s.with(Status::ACKNOWLEDGE);
    t.set_status(s);
    s = s.with(Status::DRIVER);
    t.set_status(s);
    t.set_driver_features(0x07);
    s = s.with(Status::FEATURES_OK);
    t.set_status(s);
    s = s.with(Status::DRIVER_OK);
    t.set_status(s);
    let final_status = t.status();
    assert!(final_status.contains(Status::ACKNOWLEDGE));
    assert!(final_status.contains(Status::DRIVER));
    assert!(final_status.contains(Status::FEATURES_OK));
    assert!(final_status.contains(Status::DRIVER_OK));
    assert_eq!(t.negotiated_driver_features(), 0x07);
}

#[test]
fn bounce_buffer_zeroises_on_sensitive_path() {
    let host = MockHost::new();
    let slab = host.alloc_dma_zeroed(16).unwrap();
    let mut bb = BounceBuffer::new(slab, BufferClass::Sensitive);
    bb.stage(b"top-secret-data!").unwrap();
    let phys = bb.phys();
    drop(bb);
    // SAFETY: the host leaks the box; the bytes at `phys` are
    // therefore alive for the rest of the test process. After the
    // sensitive-class drop they must be zero.
    let view: &[u8] = unsafe { core::slice::from_raw_parts(phys as *const u8, 16) };
    assert!(view.iter().all(|b| *b == 0));
}

// --- Packed virtqueue (virtio 1.1 §2.7) ------------------------------

#[test]
fn packed_queue_initialises_and_programs_transport() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let q = PackedQueue::new(&mut t, host, 0, 8, 1).expect("setup");
    assert_eq!(q.index(), 0);
    assert_eq!(q.size(), 8);
    assert_eq!(q.free_count(), 8);
    // Driver- and device-event areas are distinct allocations.
    assert_ne!(q.driver_event_phys(), q.device_event_phys());
    assert_ne!(q.driver_event_phys(), 0);
}

#[test]
fn packed_queue_rejects_non_power_of_two() {
    let mut t = MockTransport::new(1, 16, 0, 0);
    let host = static_host();
    assert_eq!(
        PackedQueue::new(&mut t, host, 0, 7, 1).map(|_| ()),
        Err(VirtioError::QueueSizeTooLarge)
    );
}

#[test]
fn packed_add_chain_consumes_slots() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 8, 1).unwrap();
    let phys = host.alloc_dma_zeroed(8).unwrap().phys();
    let id = q
        .add_chain(&[
            ChainSegment {
                phys,
                len: 4,
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys,
                len: 4,
                direction: Direction::DeviceWrite,
            },
        ])
        .unwrap();
    assert_eq!(id, 0);
    assert_eq!(q.free_count(), 6);
}

#[test]
fn packed_chain_round_trip_through_mock_peer() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 8, 1).unwrap();
    let mut input: DmaSlab = host.alloc_dma_zeroed(8).unwrap();
    input.as_bytes_mut()[..4].copy_from_slice(b"PING");
    let output: DmaSlab = host.alloc_dma_zeroed(8).unwrap();
    let segs = [
        ChainSegment {
            phys: input.phys(),
            len: 4,
            direction: Direction::DeviceRead,
        },
        ChainSegment {
            phys: output.phys(),
            len: 8,
            direction: Direction::DeviceWrite,
        },
    ];
    let id = q.add_chain(&segs).unwrap();
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            assert_eq!(chain.device_read.len(), 1);
            assert_eq!(chain.device_write.len(), 1);
            assert_eq!(&chain.device_read[0][..4], b"PING");
            let out = &mut chain.device_write[0];
            out[..4].copy_from_slice(b"PONG");
            out[4..].fill(0);
            Ok(u32::try_from(out.len()).unwrap_or(0))
        }),
    );
    q.kick(&mut t);
    let drained = t.drain_packed_queue(0).unwrap();
    assert_eq!(drained, 1);
    let used = q.poll_used().unwrap();
    assert_eq!(used.head, id);
    assert_eq!(used.written, 8);
    // SAFETY: `output.phys()` is the leaked host buffer pointer, alive
    // for `'static`.
    let response: &[u8] = unsafe { core::slice::from_raw_parts(output.phys() as *const u8, 8) };
    assert_eq!(&response[..4], b"PONG");
    // Slots reclaimed.
    assert_eq!(q.free_count(), 8);
}

#[test]
fn packed_add_chain_rejects_empty_and_too_long() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 4, 1).unwrap();
    assert_eq!(q.add_chain(&[]), Err(VirtioError::DescriptorTableOverflow));
    let phys = host.alloc_dma_zeroed(1).unwrap().phys();
    let too_long = [ChainSegment {
        phys,
        len: 1,
        direction: Direction::DeviceRead,
    }; 5];
    assert_eq!(
        q.add_chain(&too_long),
        Err(VirtioError::DescriptorTableOverflow)
    );
}

#[test]
fn packed_add_chain_exhausts_free_pool() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let phys = host.alloc_dma_zeroed(1).unwrap().phys();
    for _ in 0..4 {
        q.add_chain(&[ChainSegment {
            phys,
            len: 1,
            direction: Direction::DeviceRead,
        }])
        .unwrap();
    }
    assert_eq!(q.free_count(), 0);
    assert_eq!(
        q.add_chain(&[ChainSegment {
            phys,
            len: 1,
            direction: Direction::DeviceRead,
        }]),
        Err(VirtioError::QueueFull)
    );
}

#[test]
fn packed_ring_wraps_with_reclaim() {
    // Cycle ten 2-descriptor chains through a four-slot packed ring.
    // Each pass crosses the ring boundary, toggling both wrap
    // counters; reclamation must recycle slots so we never see
    // QueueFull and the in-band AVAIL/USED flags must stay coherent.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let in_region = host.alloc_dma_zeroed(4).unwrap();
    let out_region = host.alloc_dma_zeroed(4).unwrap();
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            if let Some(out) = chain.device_write.get_mut(0) {
                if !out.is_empty() {
                    out[0] = 0x42;
                }
            }
            Ok(1)
        }),
    );
    for i in 0..10 {
        let id = q
            .add_chain(&[
                ChainSegment {
                    phys: in_region.phys(),
                    len: 4,
                    direction: Direction::DeviceRead,
                },
                ChainSegment {
                    phys: out_region.phys(),
                    len: 4,
                    direction: Direction::DeviceWrite,
                },
            ])
            .unwrap_or_else(|_| panic!("chain {i} should fit after reclaim"));
        q.kick(&mut t);
        assert_eq!(t.drain_packed_queue(0).unwrap(), 1);
        let token = q.poll_used().unwrap();
        assert_eq!(token.head, id);
        assert_eq!(token.written, 1);
    }
    assert_eq!(q.free_count(), 4);
}

#[test]
fn packed_poll_used_returns_no_completion_when_empty() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 4, 1).unwrap();
    assert_eq!(q.poll_used(), Err(VirtioError::NoCompletion));
}

#[test]
fn packed_drain_is_noop_without_available_chain() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let _q = PackedQueue::new(&mut t, host, 0, 4, 1).unwrap();
    t.install_shim(0, Box::new(|_chain: &mut ChainView<'_>| Ok(0)));
    assert_eq!(t.drain_packed_queue(0).unwrap(), 0);
}

fn silent_host() -> &'static MockHost {
    Box::leak(Box::new(MockHost::silent()))
}

/// A request queue over a device that answers each chain by writing `tag`
/// into its one device-write segment.
fn tagging_queue(
    t: &mut MockTransport,
    host: &'static dyn crate::host::VirtioHost,
) -> RequestQueue {
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            let tag = chain.device_read.first().map_or(0, |seg| seg[0]);
            if let Some(out) = chain.device_write.first_mut() {
                out.fill(tag);
            }
            Ok(1)
        }),
    );
    RequestQueue::new(SplitQueue::new(t, host, 0, 4, 2).expect("setup"))
}

fn request(input: &DmaSlab, output: &DmaSlab) -> [ChainSegment; 2] {
    [
        ChainSegment {
            phys: input.phys(),
            len: 1,
            direction: Direction::DeviceRead,
        },
        ChainSegment {
            phys: output.phys(),
            len: 1,
            direction: Direction::DeviceWrite,
        },
    ]
}

#[test]
fn a_request_answered_in_time_returns_its_own_completion() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    t.set_synchronous_notify(true);
    let host = static_host();
    let mut q = tagging_queue(&mut t, host);
    let mut input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    input.as_bytes_mut()[0] = 0x51;
    let token = q
        .submit_and_wait(&mut t, host, &request(&input, &output), 1)
        .expect("answered");
    assert_eq!(token.written, 1);
    assert_eq!(output.as_bytes()[0], 0x51);
    assert!(!q.is_abandoned());
    assert_eq!(q.settle(&mut t, host), Ok(None), "nothing to settle");
}

#[test]
fn a_late_completion_is_never_taken_for_a_later_request() {
    // The device answers the first chain only after its deadline passed; the
    // late completion must retire that chain, never pose as the second's.
    let host = silent_host();
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let mut input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    input.as_bytes_mut()[0] = 0xA1;
    let segments = request(&input, &output);
    assert_eq!(
        q.submit_and_wait(&mut t, host, &segments, 1),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    assert!(q.is_abandoned());
    assert_eq!(
        q.submit_and_wait(&mut t, host, &segments, 1),
        Err(tairix_abi::DriverError::DeviceOffline),
        "nothing is published while the device holds the first chain"
    );
    assert_eq!(
        q.settle(&mut t, host),
        Err(tairix_abi::DriverError::DeviceOffline)
    );

    assert_eq!(
        t.drain_queue(0),
        Ok(1),
        "only the first chain was ever published"
    );
    let returned = q
        .settle(&mut t, host)
        .expect("settled")
        .expect("the chain came back");
    assert_eq!(returned.written, 1);
    assert!(!q.is_abandoned());

    t.set_synchronous_notify(true);
    input.as_bytes_mut()[0] = 0xB2;
    q.submit_and_wait(&mut t, host, &segments, 1)
        .expect("the device answers again");
    assert_eq!(output.as_bytes()[0], 0xB2, "answered for itself");
}

#[test]
fn a_wake_storm_with_no_completion_fails_closed_and_abandons_the_chain() {
    // `MockHost` wakes every wait at once and never answers.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = tagging_queue(&mut t, host);
    let input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    assert_eq!(
        q.submit_and_wait(&mut t, host, &request(&input, &output), 1),
        Err(tairix_abi::DriverError::DeviceFault)
    );
    assert_eq!(
        host.notify_log().len(),
        crate::MAX_COMPLETION_WAKES as usize
    );
    assert!(q.is_abandoned());
}

#[test]
fn a_repeated_completion_is_refused_before_anything_is_published_over_it() {
    // With nothing out, a completion in the ring answers nothing; taken for the
    // next request's, it would hand that caller the last request's bytes.
    let mut t = MockTransport::new(1, 4, 0, 0);
    t.set_synchronous_notify(true);
    let host = static_host();
    let mut q = tagging_queue(&mut t, host);
    let mut input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    input.as_bytes_mut()[0] = 0x11;
    let first = q
        .submit_and_wait(&mut t, host, &request(&input, &output), 1)
        .expect("answered");
    t.publish_raw_used(0, first.head, 1).unwrap();
    input.as_bytes_mut()[0] = 0x22;
    assert_eq!(
        q.submit_and_wait(&mut t, host, &request(&input, &output), 1),
        Err(tairix_abi::DriverError::DeviceFault)
    );
    assert_eq!(output.as_bytes()[0], 0x11, "nothing was published");
    assert!(!q.is_abandoned());
    q.submit_and_wait(&mut t, host, &request(&input, &output), 1)
        .expect("answered for itself");
    assert_eq!(output.as_bytes()[0], 0x22);
}

#[test]
fn a_request_waits_no_longer_than_its_budget_however_often_it_is_woken() {
    // Each wake restarting the wait would stretch a 20 ns budget to
    // `MAX_COMPLETION_WAKES` wakes of up to 20 ns each.
    let host = silent_host();
    host.script_waits([MockWait::Spurious { after_ns: 9 }; 8]);
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    assert_eq!(
        q.submit_and_wait(&mut t, host, &request(&input, &output), 20),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    assert_eq!(host.now_ns(), 20, "the budget, and no more");
    assert_eq!(host.notify_log().len(), 3, "9, 9, and the 2 left");
}

#[test]
fn a_request_answered_on_its_first_wake_reads_the_clock_once() {
    // The deadline needs one reading; a request that needed no second wait
    // must not pay for another.
    let host: &'static MockHost = Box::leak(Box::new(MockHost::new()));
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let device = t.into_shared();
    host.attach(&device);
    let mut transport = alloc::rc::Rc::clone(&device);
    let input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    q.submit_and_wait(&mut transport, host, &request(&input, &output), 1)
        .expect("answered");
    assert_eq!(host.notify_log().len(), 1);
    assert_eq!(host.clock_reads(), 1);
}

#[test]
fn an_abandoned_chain_is_notified_again_while_it_is_out() {
    // A notify the device missed is one reason a chain never came back.
    let host = silent_host();
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    assert_eq!(
        q.submit_and_wait(&mut t, host, &request(&input, &output), 1),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    let notified = t.notify_log.borrow().len();
    assert_eq!(
        q.settle(&mut t, host),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    assert_eq!(t.notify_log.borrow().len(), notified + 1);
}

#[test]
fn an_abandoned_chain_is_notified_again_at_most_once_per_budget() {
    // A caller retrying in a loop would otherwise ring the doorbell on every
    // try.
    let host = silent_host();
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    assert_eq!(
        q.submit_and_wait(&mut t, host, &request(&input, &output), 100),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    let notified = t.notify_log.borrow().len();
    for _ in 0..3 {
        assert_eq!(
            q.settle(&mut t, host),
            Err(tairix_abi::DriverError::DeviceOffline)
        );
    }
    assert_eq!(
        t.notify_log.borrow().len(),
        notified + 1,
        "once, not thrice"
    );
    // Another budget passes.
    host.notify_wait(0, 100);
    assert_eq!(
        q.settle(&mut t, host),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    assert_eq!(t.notify_log.borrow().len(), notified + 2);
}

#[test]
fn a_wait_that_cannot_be_made_fails_the_request_offline_at_once() {
    // A revoked or refused interrupt binding times every wait out at once;
    // waiting again would spin through the wake bound and misreport a fault.
    let host = static_host();
    host.script_waits([MockWait::Refused, MockWait::Refused]);
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    assert_eq!(
        q.submit_and_wait(&mut t, host, &request(&input, &output), 1_000),
        Err(tairix_abi::DriverError::DeviceOffline)
    );
    assert_eq!(host.notify_log().len(), 1, "one wait, not a retry storm");
    assert!(q.is_abandoned());
}

#[test]
fn a_completion_whose_interrupt_was_lost_is_taken_when_its_wait_times_out() {
    let host = silent_host();
    host.script_waits([MockWait::Lost]);
    let mut t = MockTransport::new(1, 4, 0, 0);
    let mut q = tagging_queue(&mut t, host);
    let device = t.into_shared();
    host.attach(&device);
    let mut transport = alloc::rc::Rc::clone(&device);
    let mut input = host.alloc_dma_zeroed(1).unwrap();
    let output = host.alloc_dma_zeroed(1).unwrap();
    input.as_bytes_mut()[0] = 0x7C;
    q.submit_and_wait(&mut transport, host, &request(&input, &output), 1)
        .expect("the ring is read once more");
    assert_eq!(output.as_bytes()[0], 0x7C);
    assert_eq!(host.notify_log().len(), 1);
}

#[test]
fn a_returned_chains_descriptors_are_reissued_last() {
    // Reissued at once, a returned chain's head would make the device's
    // repeat of that completion pose as the new chain's.
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 4, 1).unwrap();
    let region = host.alloc_dma_zeroed(4).unwrap();
    let first = two_segment_chain(&mut q, &region);
    t.publish_raw_used(0, first, 0).unwrap();
    assert_eq!(q.poll_used().map(|tok| tok.head), Ok(first));
    let second = two_segment_chain(&mut q, &region);
    assert_ne!(second, first);
    t.publish_raw_used(0, first, 0).unwrap();
    assert_eq!(q.poll_used(), Err(VirtioError::MalformedCompletion));
    t.publish_raw_used(0, second, 0).unwrap();
    assert_eq!(q.poll_used().map(|tok| tok.head), Ok(second));
    assert_eq!(q.free_count(), 4);
}

#[test]
fn a_withheld_queue_returns_nothing_to_its_pool() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = MockHost::new();
    let mut q = RequestQueue::new(SplitQueue::new(&mut t, &host, 0, 4, 1).unwrap());
    let held = host.slabs_outstanding();
    q.withhold();
    drop(q);
    assert_eq!(host.slabs_outstanding(), held);
}
