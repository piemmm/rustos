//! virtio-blk unit tests against the in-process [`MockTransport`].

use super::*;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_virtio::{ChainView, MockHost, MockTransport, MockWait, MAX_COMPLETION_WAKES};

const SECTOR_SIZE: usize = 512;
const SECTORS: u64 = 16;

/// Build a `MockTransport` configured as a virtio-blk device with
/// `SECTORS` 512-byte sectors and an in-memory backing store. The
/// returned `Rc` shares the backing store with the in-process peer
/// installed by this fn so tests can plant or read bytes directly.
fn build_device() -> (MockTransport, Rc<RefCell<Vec<u8>>>) {
    build_device_with_sectors(SECTORS)
}

/// [`build_device`] with a caller-chosen sector count, so the
/// chunking path (a transfer larger than [`wire::MAX_TRANSFER_LEN`])
/// can be exercised against a device big enough to hold it.
fn build_device_with_sectors(sectors: u64) -> (MockTransport, Rc<RefCell<Vec<u8>>>) {
    let mut t = MockTransport::new(1, 8, 0, 8);
    t.set_config(0, &sectors.to_le_bytes());
    let backing = Rc::new(RefCell::new(vec![
        0u8;
        SECTOR_SIZE
            * usize::try_from(sectors)
                .unwrap_or(0)
    ]));
    let backing_for_shim = Rc::clone(&backing);
    t.install_shim(
        0,
        Box::new(move |chain: &mut ChainView<'_>| {
            let header = *chain.device_read.first().ok_or(VirtioError::DeviceFault)?;
            if header.len() < wire::HEADER_LEN {
                return Err(VirtioError::DeviceFault);
            }
            let req_type = u32::from_le_bytes(header[0..4].try_into().unwrap_or([0; 4]));
            let sector_u64 = u64::from_le_bytes(header[8..16].try_into().unwrap_or([0; 8]));
            let sector = usize::try_from(sector_u64).unwrap_or(usize::MAX);
            let mut store = backing_for_shim.borrow_mut();
            let mut bytes_written = 0u32;
            match req_type {
                wire::VIRTIO_BLK_T_IN => {
                    if chain.device_write.len() < 2 {
                        return Err(VirtioError::DeviceFault);
                    }
                    let dst_len = chain.device_write[0].len();
                    let off = sector * SECTOR_SIZE;
                    if off + dst_len > store.len() {
                        let last = chain.device_write.len() - 1;
                        chain.device_write[last][0] = wire::STATUS_IOERR;
                        return Ok(1);
                    }
                    chain.device_write[0].copy_from_slice(&store[off..off + dst_len]);
                    bytes_written = u32::try_from(dst_len).unwrap_or(0);
                    let last = chain.device_write.len() - 1;
                    chain.device_write[last][0] = wire::STATUS_OK;
                }
                wire::VIRTIO_BLK_T_OUT => {
                    if chain.device_read.len() < 2 {
                        return Err(VirtioError::DeviceFault);
                    }
                    let src = chain.device_read[1];
                    let off = sector * SECTOR_SIZE;
                    if off + src.len() > store.len() {
                        if let Some(last) = chain.device_write.last_mut() {
                            last[0] = wire::STATUS_IOERR;
                        }
                        return Ok(1);
                    }
                    store[off..off + src.len()].copy_from_slice(src);
                    if let Some(last) = chain.device_write.last_mut() {
                        last[0] = wire::STATUS_OK;
                    }
                }
                wire::VIRTIO_BLK_T_FLUSH => {
                    // A flush carries header + status only; committing the
                    // (already-applied) backing store is a no-op success.
                    if let Some(last) = chain.device_write.last_mut() {
                        last[0] = wire::STATUS_OK;
                    }
                }
                _ => {
                    if let Some(last) = chain.device_write.last_mut() {
                        last[0] = 2; // VIRTIO_BLK_S_UNSUPP.
                    }
                }
            }
            Ok(bytes_written + 1)
        }),
    );
    (t, backing)
}

/// The mock device, shared by the driver under test and the host playing it.
type Device = Rc<RefCell<MockTransport>>;

type Blk = Box<VirtioBlk<'static, Device>>;

/// Open a driver on `t`, whose waits `host` answers by playing the device.
fn open_played_by(t: MockTransport, host: MockHost) -> (Blk, Device, &'static MockHost) {
    let host: &'static MockHost = Box::leak(Box::new(host));
    let device = t.into_shared();
    host.attach(&device);
    let blk = Box::new(VirtioBlk::open(Rc::clone(&device), host).expect("open"));
    (blk, device, host)
}

fn open_with_autodrain(t: MockTransport) -> Blk {
    open_played_by(t, MockHost::new()).0
}

/// A host whose first `n` wakes come with no completion posted.
fn spurious_host(n: usize) -> MockHost {
    let host = MockHost::new();
    host.script_waits(core::iter::repeat_n(MockWait::Spurious { after_ns: 0 }, n));
    host
}

/// A handful of early/spurious wakes before the completion lands must
/// not defeat a request: the driver re-scans the used ring after each
/// advisory wake and succeeds once the completion is posted, rather than
/// concluding `Busy`/`DeviceFault` from a single empty poll (the boot
/// root-unlock reliability fix).
#[test]
fn spurious_wakes_before_completion_still_succeed() {
    let (t, backing) = build_device();
    backing.borrow_mut()[7 * SECTOR_SIZE..8 * SECTOR_SIZE].fill(0x5A);
    // Five leading wakes deliver no completion; the sixth drains it.
    let (mut blk, _, _) = open_played_by(t, spurious_host(5));
    let mut buf = vec![0u8; SECTOR_SIZE];
    blk.read_blocks(7, &mut buf)
        .expect("read survives spurious wakes");
    assert!(buf.iter().all(|b| *b == 0x5A));
}

/// A device that wakes forever without ever posting a completion must
/// fail **closed** at the `MAX_COMPLETION_WAKES` bound with a typed
/// `DeviceFault`, never loop forever — the fail-closed guarantee a stuck
/// or mis-routed shared interrupt must produce.
#[test]
fn a_never_completing_device_fails_closed() {
    let (t, _backing) = build_device();
    // More spurious wakes than the driver will tolerate: the completion
    // never lands, so the bounded loop gives up.
    let wakes = usize::try_from(MAX_COMPLETION_WAKES).unwrap_or(usize::MAX) + 1;
    let (mut blk, _, _) = open_played_by(t, spurious_host(wakes));
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(
        blk.read_blocks(0, &mut buf),
        Err(DriverError::DeviceFault),
        "a wake-storm with no completion must fail closed, not hang"
    );
}

/// A device that never signals at all — no wake, fired or otherwise —
/// must fail **closed** with `DeviceOffline` once its per-request deadline
/// elapses, distinct from the wake-storm's `DeviceFault` above: here the
/// wait itself times out rather than firing without a completion.
#[test]
fn a_silent_device_fails_closed_with_device_offline() {
    let (t, _backing) = build_device();
    let (mut blk, _, _) = open_played_by(t, MockHost::silent());
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(
        blk.read_blocks(0, &mut buf),
        Err(DriverError::DeviceOffline),
        "a device that never signals must fail closed once its deadline elapses"
    );
}

#[test]
fn bring_up_declares_the_device_quiesced_once_its_reset_confirms() {
    let (t, _backing) = build_device();
    let host = MockHost::new();
    let _blk = VirtioBlk::open(t, &host).expect("open");
    assert_eq!(host.quiesced_calls(), 1);
}

#[test]
fn a_device_whose_reset_never_confirms_is_refused_before_it_is_given_memory() {
    let (mut t, _backing) = build_device();
    t.refuse_resets_after(0);
    let host = MockHost::new();
    assert!(matches!(
        VirtioBlk::open(t, &host),
        Err(VirtioError::DeviceFault)
    ));
    assert_eq!(host.quiesced_calls(), 0);
    assert_eq!(host.bytes_allocated(), 0);
}

#[test]
fn a_dropped_device_that_confirms_its_reset_releases_every_region() {
    let (t, _backing) = build_device();
    let host = MockHost::new();
    drop(VirtioBlk::open(t, &host).expect("open"));
    assert_eq!(host.slabs_outstanding(), 0);
}

#[test]
fn a_dropped_device_whose_reset_never_confirms_releases_nothing() {
    let (t, _backing) = build_device();
    let (blk, device, host) = open_played_by(t, MockHost::new());
    let held = host.slabs_outstanding();
    assert!(held > 0);
    device.borrow_mut().refuse_resets_after(0);
    drop(blk);
    assert_eq!(host.slabs_outstanding(), held);
}

/// Open a driver whose host lets the first request's wait time out with the
/// device still holding the chain.
fn open_abandoning_first_request(t: MockTransport) -> (Blk, Device) {
    let host = MockHost::new();
    host.script_waits([MockWait::Silent]);
    let (blk, device, _) = open_played_by(t, host);
    (blk, device)
}

/// What a device that filled every device-write segment of `chain` reports.
fn written_by(chain: &ChainView<'_>) -> Result<u32, VirtioError> {
    let bytes: usize = chain.device_write.iter().map(|segment| segment.len()).sum();
    u32::try_from(bytes).map_err(|_| VirtioError::DeviceFault)
}

#[test]
fn a_read_whose_completion_does_not_cover_its_payload_hands_back_nothing() {
    // The data staging is reused, so bytes the device does not report writing
    // are an earlier request's.
    let mut t = MockTransport::new(1, 8, 0, 8);
    t.set_config(0, &SECTORS.to_le_bytes());
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            let last = chain.device_write.len() - 1;
            chain.device_write[0].fill(0x3C);
            chain.device_write[last][0] = wire::STATUS_OK;
            Ok(1)
        }),
    );
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(blk.read_blocks(0, &mut buf), Err(DriverError::DeviceFault));
    assert!(buf.iter().all(|b| *b == 0), "nothing handed back");
}

#[test]
fn a_requestq_too_shallow_for_one_request_is_refused_before_it_is_programmed() {
    let mut t = MockTransport::new(1, 2, 0, 8);
    t.set_config(0, &SECTORS.to_le_bytes());
    let device = t.into_shared();
    let host = MockHost::new();
    assert!(matches!(
        VirtioBlk::open(Rc::clone(&device), &host),
        Err(VirtioError::QueueTooShallow)
    ));
    assert_eq!(host.slabs_outstanding(), 0);
    assert_eq!(
        device.borrow_mut().publish_raw_used(0, 0, 0),
        Err(VirtioError::DeviceFault),
        "the device was never given the ring"
    );
}

#[test]
fn a_sensitive_payload_the_device_held_is_scrubbed_when_the_driver_is_dropped() {
    let (t, _backing) = build_device();
    let (mut blk, _device) = open_abandoning_first_request(t);
    let payload = vec![0xC7u8; SECTOR_SIZE];
    assert_eq!(
        blk.write_blocks_with_class(4, &payload, BufferClass::Sensitive),
        Err(DriverError::DeviceOffline)
    );
    let (phys, len) = blk
        .data
        .as_ref()
        .map(|data| (data.phys(), data.len()))
        .expect("the staging is put back");
    drop(blk);
    // SAFETY: the mock host leaks every slab's storage, so these bytes
    // outlive the driver that freed them.
    let staging = unsafe { core::slice::from_raw_parts(phys as *const u8, len) };
    assert!(
        staging.iter().all(|b| *b == 0),
        "the confirmed reset took it back"
    );
}

#[test]
fn a_completion_that_wrote_no_status_is_refused() {
    // The status staging is reused, so without a fresh sentinel a device
    // that completes a read without answering it reads as the last one's OK.
    let mut t = MockTransport::new(1, 8, 0, 8);
    t.set_config(0, &SECTORS.to_le_bytes());
    let answered = Rc::new(core::cell::Cell::new(0u32));
    let answered_by_shim = Rc::clone(&answered);
    t.install_shim(
        0,
        Box::new(move |chain: &mut ChainView<'_>| {
            let last = chain.device_write.len() - 1;
            chain.device_write[0].fill(0x3C);
            if answered_by_shim.get() == 0 {
                chain.device_write[last][0] = wire::STATUS_OK;
            }
            answered_by_shim.set(answered_by_shim.get() + 1);
            written_by(chain)
        }),
    );
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    blk.read_blocks(0, &mut buf).expect("answered");
    buf.fill(0);
    assert_eq!(blk.read_blocks(1, &mut buf), Err(DriverError::DeviceFault));
    assert_eq!(answered.get(), 2);
    assert!(buf.iter().all(|b| *b == 0), "nothing handed back");
}

#[test]
fn a_late_completion_is_never_returned_as_a_later_reads_data() {
    // The device answers the first read only after its deadline: its data
    // must never be handed to the next read, which asked for another block.
    let (t, backing) = build_device();
    backing.borrow_mut()[3 * SECTOR_SIZE..4 * SECTOR_SIZE].fill(0xAA);
    backing.borrow_mut()[5 * SECTOR_SIZE..6 * SECTOR_SIZE].fill(0x55);
    let (mut blk, device) = open_abandoning_first_request(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(
        blk.read_blocks(3, &mut buf),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(device.borrow_mut().drain_queue(0), Ok(1), "answered late");
    blk.read_blocks(5, &mut buf)
        .expect("the device answers again");
    assert!(buf.iter().all(|b| *b == 0x55), "block 5's own data");
}

#[test]
fn a_request_is_refused_while_the_device_still_holds_an_abandoned_one() {
    let (t, _backing) = build_device();
    let (mut blk, device) = open_abandoning_first_request(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(
        blk.read_blocks(0, &mut buf),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(
        blk.write_blocks(1, &buf),
        Err(DriverError::DeviceOffline),
        "its staging is the device's, so nothing is published over it"
    );
    assert_eq!(
        device.borrow_mut().drain_queue(0),
        Ok(1),
        "only the abandoned chain was ever published"
    );
}

#[test]
fn a_sensitive_payload_the_device_held_is_scrubbed_when_it_comes_back() {
    let (t, backing) = build_device();
    backing.borrow_mut()[2 * SECTOR_SIZE..3 * SECTOR_SIZE].fill(0x5E);
    let (mut blk, device) = open_abandoning_first_request(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(
        blk.read_blocks_with_class(2, &mut buf, BufferClass::Sensitive),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(device.borrow_mut().drain_queue(0), Ok(1));
    blk.reclaim_staging().expect("the chain came back");
    let staging = blk
        .data
        .as_ref()
        .expect("the staging is the driver's again");
    assert!(staging.as_bytes().iter().all(|b| *b == 0));
}

#[test]
fn an_abandoned_sensitive_write_still_carries_its_payload_to_the_device() {
    // Scrubbing staging the device has yet to read would write zeros over
    // the block the caller meant to write.
    let (t, backing) = build_device();
    let (mut blk, device) = open_abandoning_first_request(t);
    let payload = vec![0xC7u8; SECTOR_SIZE];
    assert_eq!(
        blk.write_blocks_with_class(4, &payload, BufferClass::Sensitive),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(device.borrow_mut().drain_queue(0), Ok(1));
    assert!(backing.borrow()[4 * SECTOR_SIZE..5 * SECTOR_SIZE]
        .iter()
        .all(|b| *b == 0xC7));
    blk.reclaim_staging().expect("the chain came back");
    let staging = blk
        .data
        .as_ref()
        .expect("the staging is the driver's again");
    assert!(staging.as_bytes().iter().all(|b| *b == 0), "then scrubbed");
}

#[test]
fn open_reads_geometry_from_device_config() {
    let (t, _backing) = build_device();
    let blk = open_with_autodrain(t);
    assert_eq!(
        blk.geometry().unwrap(),
        BlockGeometry {
            block_size: 512,
            block_count: SECTORS,
        }
    );
}

#[test]
fn read_returns_planted_pattern() {
    let (t, backing) = build_device();
    backing.borrow_mut()[3 * SECTOR_SIZE..4 * SECTOR_SIZE].fill(0xA5);
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    blk.read_blocks(3, &mut buf).expect("read");
    assert!(buf.iter().all(|b| *b == 0xA5));
}

#[test]
fn write_then_read_round_trip() {
    let (t, _backing) = build_device();
    let mut blk = open_with_autodrain(t);
    let payload = vec![0xC3u8; SECTOR_SIZE];
    blk.write_blocks(5, &payload).expect("write");
    let mut readback = vec![0u8; SECTOR_SIZE];
    blk.read_blocks(5, &mut readback).expect("read");
    assert_eq!(readback, payload);
}

#[test]
fn validate_block_op_rejects_unaligned_lengths() {
    let (t, _backing) = build_device();
    let mut blk = open_with_autodrain(t);
    let mut tiny = vec![0u8; 100];
    assert_eq!(
        blk.read_blocks(0, &mut tiny),
        Err(DriverError::BufferTooSmall)
    );
    let mut empty: Vec<u8> = Vec::new();
    assert_eq!(
        blk.read_blocks(0, &mut empty),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn validate_block_op_rejects_out_of_range() {
    let (t, _backing) = build_device();
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(
        blk.read_blocks(100, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        blk.read_blocks(u64::MAX, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn sensitive_class_completes_round_trip() {
    let (t, backing) = build_device();
    let mut blk = open_with_autodrain(t);
    let payload = vec![0xDEu8; SECTOR_SIZE];
    blk.write_blocks_with_class(2, &payload, BufferClass::Sensitive)
        .expect("write");
    assert!(backing.borrow()[2 * SECTOR_SIZE..3 * SECTOR_SIZE]
        .iter()
        .all(|b| *b == 0xDE));
    let mut readback = vec![0u8; SECTOR_SIZE];
    blk.read_blocks_with_class(2, &mut readback, BufferClass::Sensitive)
        .expect("read");
    assert_eq!(readback, payload);
}

#[test]
fn multi_block_read_concatenates_sectors() {
    let (t, backing) = build_device();
    backing.borrow_mut()[0..SECTOR_SIZE].fill(0xAA);
    backing.borrow_mut()[SECTOR_SIZE..2 * SECTOR_SIZE].fill(0xBB);
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE * 2];
    blk.read_blocks(0, &mut buf).expect("read");
    assert!(buf[..SECTOR_SIZE].iter().all(|b| *b == 0xAA));
    assert!(buf[SECTOR_SIZE..].iter().all(|b| *b == 0xBB));
}

#[test]
fn steady_state_io_allocates_no_new_dma() {
    // The header/data/status staging is carved once at open; reads and
    // writes must all reuse it. The per-request `dma_alloc`/`dma_free`
    // churn (and the audit-log entry it emits every request) is exactly
    // the defect this driver must not reintroduce.
    let (t, _backing) = build_device();
    let (mut blk, _, host) = open_played_by(t, MockHost::new());
    let after_open = host.bytes_allocated();
    let mut buf = vec![0u8; SECTOR_SIZE];
    for lba in 0..8u64 {
        let tag = u8::try_from(lba & 0xff).unwrap_or(0);
        let payload = vec![tag; SECTOR_SIZE];
        blk.write_blocks(lba, &payload).expect("write");
        blk.read_blocks(lba, &mut buf).expect("read");
        assert!(buf.iter().all(|b| *b == tag));
    }
    assert_eq!(
        host.bytes_allocated(),
        after_open,
        "steady-state I/O must not allocate DMA"
    );
}

#[test]
fn transfer_larger_than_staging_window_chunks_and_round_trips() {
    // A transfer bigger than the fixed staging window is split into
    // block-aligned chunks that reuse the same buffers; the bytes must
    // still round-trip end to end and land at the right sectors.
    let bytes = wire::MAX_TRANSFER_LEN * 2 + SECTOR_SIZE; // 2.5 chunks.
    let blocks = bytes / SECTOR_SIZE;
    let sectors = u64::try_from(blocks).unwrap() + 4;
    let (t, _backing) = build_device_with_sectors(sectors);
    let (mut blk, _, host) = open_played_by(t, MockHost::new());
    let after_open = host.bytes_allocated();
    // A recognisable per-block pattern so a mis-chunked copy is caught.
    let mut payload = vec![0u8; bytes];
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte = u8::try_from((i / SECTOR_SIZE) & 0xff).unwrap_or(0);
    }
    blk.write_blocks(2, &payload).expect("chunked write");
    let mut readback = vec![0u8; bytes];
    blk.read_blocks(2, &mut readback).expect("chunked read");
    assert_eq!(readback, payload);
    assert_eq!(
        host.bytes_allocated(),
        after_open,
        "chunked transfers must reuse the staging buffers"
    );
}

/// Shared log of the `(sector, num_sectors)` pairs a discard shim records.
type DiscardLog = Rc<RefCell<Vec<(u64, u32)>>>;

/// Build a discard-capable virtio-blk `MockTransport`: it offers
/// `VIRTIO_BLK_F_DISCARD`, advertises a discard granularity of
/// `align` sectors and a `max` per-request limit in its config window,
/// and its shim records every `VIRTIO_BLK_T_DISCARD` descriptor's
/// `(sector, num_sectors)` into the returned log.
fn build_discard_device(align: u32, max: u32) -> (MockTransport, DiscardLog) {
    // Config window must be large enough to hold the discard fields
    // (`discard_sector_alignment` ends at offset 48).
    let mut t = MockTransport::new(1, 8, wire::VIRTIO_BLK_F_DISCARD, 64);
    t.set_config(wire::CONFIG_CAPACITY_OFFSET, &SECTORS.to_le_bytes());
    t.set_config(wire::CONFIG_MAX_DISCARD_SECTORS_OFFSET, &max.to_le_bytes());
    t.set_config(
        wire::CONFIG_DISCARD_SECTOR_ALIGNMENT_OFFSET,
        &align.to_le_bytes(),
    );
    let log: DiscardLog = Rc::new(RefCell::new(Vec::new()));
    let log_for_shim = Rc::clone(&log);
    t.install_shim(
        0,
        Box::new(move |chain: &mut ChainView<'_>| {
            let header = *chain.device_read.first().ok_or(VirtioError::DeviceFault)?;
            if header.len() < wire::HEADER_LEN {
                return Err(VirtioError::DeviceFault);
            }
            let req_type = u32::from_le_bytes(header[0..4].try_into().unwrap_or([0; 4]));
            if req_type != wire::VIRTIO_BLK_T_DISCARD {
                if let Some(last) = chain.device_write.last_mut() {
                    last[0] = 2; // VIRTIO_BLK_S_UNSUPP.
                }
                return Ok(1);
            }
            if chain.device_read.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            let desc = chain.device_read[1];
            if desc.len() < wire::DISCARD_DESCRIPTOR_LEN {
                return Err(VirtioError::DeviceFault);
            }
            let sector = u64::from_le_bytes(desc[0..8].try_into().unwrap_or([0; 8]));
            let num = u32::from_le_bytes(desc[8..12].try_into().unwrap_or([0; 4]));
            log_for_shim.borrow_mut().push((sector, num));
            if let Some(last) = chain.device_write.last_mut() {
                last[0] = wire::STATUS_OK;
            }
            Ok(1)
        }),
    );
    (t, log)
}

#[test]
fn discard_capability_unsupported_without_feature() {
    let (t, _backing) = build_device();
    let blk = open_with_autodrain(t);
    assert_eq!(
        blk.discard_capability().unwrap(),
        DiscardCapability::unsupported()
    );
}

#[test]
fn discard_unsupported_device_refuses() {
    let (t, _backing) = build_device();
    let mut blk = open_with_autodrain(t);
    assert_eq!(blk.discard(0, 4), Err(DriverError::Unsupported));
}

#[test]
fn discard_capable_device_reports_negotiated_limits() {
    let (t, _log) = build_discard_device(8, 64);
    let (blk, device, _) = open_played_by(t, MockHost::new());
    assert_eq!(
        blk.discard_capability().unwrap(),
        DiscardCapability {
            supported: true,
            granularity_blocks: 8,
            max_blocks_per_request: 64,
        }
    );
    assert_eq!(
        device.borrow().negotiated_driver_features(),
        wire::VIRTIO_BLK_F_DISCARD
    );
}

#[test]
fn discard_capable_device_records_descriptor() {
    let (t, log) = build_discard_device(1, 0);
    let mut blk = open_with_autodrain(t);
    blk.discard(4, 3).expect("discard");
    assert_eq!(log.borrow().as_slice(), &[(4, 3)]);
}

#[test]
fn discard_rejects_out_of_range_and_oversized() {
    let (t, _log) = build_discard_device(1, 4);
    let mut blk = open_with_autodrain(t);
    assert_eq!(
        blk.discard(SECTORS - 1, 4),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(blk.discard(0, 5), Err(DriverError::LengthOutOfRange));
    // A zero-length discard is a no-op success.
    assert!(blk.discard(0, 0).is_ok());
}

/// Shared flush counter a flush-capable shim increments.
type FlushLog = Rc<RefCell<usize>>;

/// Build a flush-capable virtio-blk `MockTransport`: it offers
/// `VIRTIO_BLK_F_FLUSH`, and its shim counts every `VIRTIO_BLK_T_FLUSH`
/// request into the returned log, answering each with `flush_status`. Any
/// other request is answered `VIRTIO_BLK_S_UNSUPP`.
fn build_flush_device_with_status(flush_status: u8) -> (MockTransport, FlushLog) {
    let mut t = MockTransport::new(1, 8, wire::VIRTIO_BLK_F_FLUSH, 64);
    t.set_config(wire::CONFIG_CAPACITY_OFFSET, &SECTORS.to_le_bytes());
    let log: FlushLog = Rc::new(RefCell::new(0));
    let log_for_shim = Rc::clone(&log);
    t.install_shim(
        0,
        Box::new(move |chain: &mut ChainView<'_>| {
            let header = *chain.device_read.first().ok_or(VirtioError::DeviceFault)?;
            if header.len() < wire::HEADER_LEN {
                return Err(VirtioError::DeviceFault);
            }
            let req_type = u32::from_le_bytes(header[0..4].try_into().unwrap_or([0; 4]));
            if req_type == wire::VIRTIO_BLK_T_FLUSH {
                *log_for_shim.borrow_mut() += 1;
                if let Some(last) = chain.device_write.last_mut() {
                    last[0] = flush_status;
                }
                return Ok(1);
            }
            if let Some(last) = chain.device_write.last_mut() {
                last[0] = wire::STATUS_UNSUPP;
            }
            Ok(1)
        }),
    );
    (t, log)
}

/// A flush-capable device that answers every flush `STATUS_OK`.
fn build_flush_device() -> (MockTransport, FlushLog) {
    build_flush_device_with_status(wire::STATUS_OK)
}

#[test]
fn flush_without_feature_is_a_noop_success() {
    // A write-through device (no `VIRTIO_BLK_F_FLUSH`) has no volatile
    // cache: flush succeeds without issuing any request.
    let (t, _backing) = build_device();
    let (mut blk, device, _) = open_played_by(t, MockHost::new());
    assert_eq!(device.borrow().negotiated_driver_features(), 0);
    blk.flush()
        .expect("flush is a no-op success when write-through");
}

#[test]
fn flush_capable_device_issues_a_flush_command() {
    let (t, log) = build_flush_device();
    let (mut blk, device, _) = open_played_by(t, MockHost::new());
    assert_eq!(
        device.borrow().negotiated_driver_features(),
        wire::VIRTIO_BLK_F_FLUSH
    );
    blk.flush().expect("flush");
    assert_eq!(
        *log.borrow(),
        1,
        "one VIRTIO_BLK_T_FLUSH reached the device"
    );
}

#[test]
fn register_requires_drv_load() {
    struct H {
        grant: bool,
    }
    impl DriverHost for H {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            cap == CapabilityId::DRV_LOAD && self.grant
        }
        fn kind(&self) -> tairix_abi::driver::DriverKind {
            tairix_abi::driver::DriverKind::UserSpace
        }
    }
    assert_eq!(
        register(&H { grant: false }),
        Err(DriverError::PermissionDenied)
    );
    assert!(register(&H { grant: true }).is_ok());
}

/// Build a virtio-blk `MockTransport` whose shim answers every
/// `VIRTIO_BLK_T_IN` read with the caller-chosen device status byte
/// (filling the data buffer with a recognisable non-zero pattern first),
/// so the driver's status decode can be exercised for every outcome.
fn build_device_returning_read_status(status: u8) -> MockTransport {
    let mut t = MockTransport::new(1, 8, 0, 8);
    t.set_config(wire::CONFIG_CAPACITY_OFFSET, &SECTORS.to_le_bytes());
    t.install_shim(
        0,
        Box::new(move |chain: &mut ChainView<'_>| {
            // Plant a non-zero pattern in the data buffer so a (buggy)
            // copy-on-error would be visible to the caller; a correct
            // decode never copies it on a non-OK status.
            if let Some(data) = chain.device_write.first_mut() {
                data.fill(0x5A);
            }
            if let Some(last) = chain.device_write.last_mut() {
                last[0] = status;
            }
            Ok(1)
        }),
    );
    t
}

#[test]
fn status_to_result_maps_every_device_status() {
    assert_eq!(status_to_result(wire::STATUS_OK), Ok(()));
    // A device-reported I/O error is a per-request medium error, not a
    // whole-device fault, so a consumer recovers around it and repairs
    // the block rather than dropping the device.
    assert_eq!(
        status_to_result(wire::STATUS_IOERR),
        Err(DriverError::MediumError)
    );
    // The device does not implement this request type: a request-level
    // refusal, not a device fault.
    assert_eq!(
        status_to_result(wire::STATUS_UNSUPP),
        Err(DriverError::Unsupported)
    );
    // Any undefined status byte fails closed: the device is not speaking
    // the protocol we trust, so it is a fault, never the benign
    // `Unsupported`.
    assert_eq!(status_to_result(3), Err(DriverError::DeviceFault));
    assert_eq!(status_to_result(0x7F), Err(DriverError::DeviceFault));
    assert_eq!(status_to_result(0xFF), Err(DriverError::DeviceFault));
}

#[test]
fn read_ioerror_surfaces_as_medium_error_not_device_fault() {
    let t = build_device_returning_read_status(wire::STATUS_IOERR);
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    // A single bad-sector I/O error is a per-request `MediumError`: a
    // consumer (e.g. a RAID member) recovers around it and repairs the
    // block, rather than dropping the whole device as a `DeviceFault`
    // would force.
    assert_eq!(blk.read_blocks(0, &mut buf), Err(DriverError::MediumError));
    // Fail closed: no device-written bytes are copied back on an error,
    // so the caller never sees the shim's `0x5A` pattern.
    assert!(
        buf.iter().all(|b| *b == 0),
        "no data must be copied back on a failed read"
    );
}

#[test]
fn read_unknown_status_fails_closed_as_device_fault() {
    let t = build_device_returning_read_status(0x7F);
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(blk.read_blocks(0, &mut buf), Err(DriverError::DeviceFault));
    assert!(buf.iter().all(|b| *b == 0));
}

#[test]
fn read_unsupported_status_is_request_level_not_a_fault() {
    let t = build_device_returning_read_status(wire::STATUS_UNSUPP);
    let mut blk = open_with_autodrain(t);
    let mut buf = vec![0u8; SECTOR_SIZE];
    assert_eq!(blk.read_blocks(0, &mut buf), Err(DriverError::Unsupported));
    assert!(buf.iter().all(|b| *b == 0));
}

#[test]
fn flush_ioerror_fails_closed_as_medium_error() {
    // A flush the device cannot commit is a genuine I/O failure the
    // caller must see (data may not be durable), never a silent success.
    let (t, log) = build_flush_device_with_status(wire::STATUS_IOERR);
    let mut blk = open_with_autodrain(t);
    assert_eq!(blk.flush(), Err(DriverError::MediumError));
    assert_eq!(*log.borrow(), 1, "the flush reached the device");
}

#[test]
fn bind_table_matches_a_virtio_block_node() {
    use tairix_abi::HwMatchKey;

    // One entry at the declared exact-match priority, matching a
    // discovered virtio node whose probed device id is `virtio-blk`.
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let blk = HwMatchKey::virtio(VIRTIO_BLK_DEVICE_ID);
    assert!(BIND_KEYS[0].key.matches(&blk));

    // A different virtio device (e.g. virtio-net, device id 1) and a
    // non-virtio node both fail the match — the caller leaves them
    // unbound rather than guessing.
    let net = HwMatchKey::virtio(1);
    assert!(!BIND_KEYS[0].key.matches(&net));
    let pci_storage = HwMatchKey::pci(0x8086, 0x2922, 0x01_06_01);
    assert!(!BIND_KEYS[0].key.matches(&pci_storage));
}
