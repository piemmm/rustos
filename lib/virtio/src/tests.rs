//! Cross-module unit tests: end-to-end split-virtqueue protocol
//! against the in-process [`crate::MockTransport`] peer.

use crate::dma::{BounceBuffer, DmaSlab};
use crate::host::{MockHost, VirtioHost};
use crate::packed::PackedQueue;
use crate::queue::{ChainSegment, SplitQueue};
use crate::transport::{ChainView, Direction, MockTransport, Status, Transport, VirtioError};
use alloc::boxed::Box;
use rustos_abi::driver::BufferClass;

/// Build a `'static` reference to a freshly-leaked `MockHost` —
/// the unit tests hand this to `SplitQueue::new`.
fn static_host() -> &'static MockHost {
    Box::leak(Box::new(MockHost::new()))
}

#[test]
fn split_queue_initialises_free_list_and_programs_transport() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let q = SplitQueue::new(&mut t, host, 0, 8).expect("setup");
    assert_eq!(q.index(), 0);
    assert_eq!(q.size(), 8);
    assert_eq!(q.free_count(), 8);
}

#[test]
fn split_queue_rejects_non_power_of_two() {
    let mut t = MockTransport::new(1, 16, 0, 0);
    let host = static_host();
    assert_eq!(
        SplitQueue::new(&mut t, host, 0, 7).map(|_| ()),
        Err(VirtioError::QueueSizeTooLarge)
    );
}

#[test]
fn add_chain_consumes_descriptors_and_publishes_avail() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, 8).unwrap();
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
    let mut q = SplitQueue::new(&mut t, host, 0, 8).unwrap();
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
    let mut q = SplitQueue::new(&mut t, host, 0, 4).unwrap();
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
    let mut q = SplitQueue::new(&mut t, host, 0, 4).unwrap();
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
    let mut q = SplitQueue::new(&mut t, host, 0, 4).unwrap();
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
    let mut q = SplitQueue::new(&mut t, host, 0, 4).unwrap();
    assert_eq!(q.poll_used(), Err(VirtioError::NoCompletion));
}

#[test]
fn transport_setup_records_status_progression() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    t.reset();
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
    let q = PackedQueue::new(&mut t, host, 0, 8).expect("setup");
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
        PackedQueue::new(&mut t, host, 0, 7).map(|_| ()),
        Err(VirtioError::QueueSizeTooLarge)
    );
}

#[test]
fn packed_add_chain_consumes_slots() {
    let mut t = MockTransport::new(1, 8, 0, 0);
    let host = static_host();
    let mut q = PackedQueue::new(&mut t, host, 0, 8).unwrap();
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
    let mut q = PackedQueue::new(&mut t, host, 0, 8).unwrap();
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
    let mut q = PackedQueue::new(&mut t, host, 0, 4).unwrap();
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
    let mut q = PackedQueue::new(&mut t, host, 0, 4).unwrap();
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
    let mut q = PackedQueue::new(&mut t, host, 0, 4).unwrap();
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
    let mut q = PackedQueue::new(&mut t, host, 0, 4).unwrap();
    assert_eq!(q.poll_used(), Err(VirtioError::NoCompletion));
}

#[test]
fn packed_drain_is_noop_without_available_chain() {
    let mut t = MockTransport::new(1, 4, 0, 0);
    let host = static_host();
    let _q = PackedQueue::new(&mut t, host, 0, 4).unwrap();
    t.install_shim(0, Box::new(|_chain: &mut ChainView<'_>| Ok(0)));
    assert_eq!(t.drain_packed_queue(0).unwrap(), 0);
}
