//! virtio-blk unit tests against the in-process [`MockTransport`].

use super::*;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use rustos_virtio::{ChainView, DmaHost, MockHost, MockTransport};

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

/// `VirtioHost` that auto-drains the transport's queue when
/// notified, so the driver's synchronous `kick → notify_wait →
/// poll_used` cycle completes inside one method call without test
/// scaffolding.
struct AutoDrainHost {
    inner: MockHost,
    transport: core::cell::UnsafeCell<*mut MockTransport>,
}

impl AutoDrainHost {
    fn new() -> Self {
        Self {
            inner: MockHost::new(),
            transport: core::cell::UnsafeCell::new(core::ptr::null_mut()),
        }
    }
    /// Plant the live transport's raw pointer so `notify_wait` can
    /// drain it. Called once per driver instance, while no other
    /// `&mut` to the transport is live.
    fn install_transport(&self, t: *mut MockTransport) {
        // SAFETY: tests are single-threaded with respect to a given
        // `AutoDrainHost`; `auto_host()` returns a freshly-leaked
        // instance per call, so no aliasing borrow of `self.transport`
        // can exist when we write to it.
        unsafe {
            *self.transport.get() = t;
        }
    }

    /// Total DMA bytes the underlying pool has handed out. Used to
    /// assert the data path reuses its staging rather than re-granting.
    fn bytes_allocated(&self) -> usize {
        self.inner.bytes_allocated()
    }
}

impl DmaHost for AutoDrainHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<rustos_virtio::DmaSlab, DriverError> {
        self.inner.alloc_dma_zeroed(size)
    }
}

impl rustos_virtio::VirtioHost for AutoDrainHost {
    fn notify_wait(&self, queue_index: u16) {
        // SAFETY: the pointer was installed by `install_transport`
        // while no other borrow of the transport was live; the
        // driver releases its `&mut self.transport` borrow between
        // `kick` and `notify_wait` (the `kick` call already
        // returned), so this `&mut *t_ptr` is the unique live
        // reference for the duration of `drain_queue`.
        let t_ptr = unsafe { *self.transport.get() };
        if !t_ptr.is_null() {
            let t = unsafe { &mut *t_ptr };
            let _ = t.drain_queue(queue_index);
        }
        self.inner.notify_wait(queue_index);
    }
}

fn auto_host() -> &'static AutoDrainHost {
    Box::leak(Box::new(AutoDrainHost::new()))
}

fn open_with_autodrain(t: MockTransport) -> Box<VirtioBlk<'static, MockTransport>> {
    open_with_autodrain_host(t).0
}

/// [`open_with_autodrain`] that also returns the leaked host, so a
/// test can observe its DMA-allocation counter.
fn open_with_autodrain_host(
    t: MockTransport,
) -> (
    Box<VirtioBlk<'static, MockTransport>>,
    &'static AutoDrainHost,
) {
    // Pin the driver behind a `Box` so the raw pointer we hand the
    // host stays valid across the test function's stack frame
    // (`Box` provides a stable heap address that `install_transport`
    // can record once and reuse for every `notify_wait`).
    let host = auto_host();
    let mut blk = Box::new(VirtioBlk::open(t, host).expect("open"));
    host.install_transport(blk.transport_mut() as *mut MockTransport);
    (blk, host)
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
    let (mut blk, host) = open_with_autodrain_host(t);
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
    let (mut blk, host) = open_with_autodrain_host(t);
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
    let blk = open_with_autodrain(t);
    assert_eq!(
        blk.discard_capability().unwrap(),
        DiscardCapability {
            supported: true,
            granularity_blocks: 8,
            max_blocks_per_request: 64,
        }
    );
    assert_eq!(
        blk.transport().negotiated_driver_features(),
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

#[test]
fn register_requires_drv_load() {
    struct H {
        grant: bool,
    }
    impl DriverHost for H {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            cap == CapabilityId::DRV_LOAD && self.grant
        }
        fn kind(&self) -> rustos_abi::driver::DriverKind {
            rustos_abi::driver::DriverKind::UserSpace
        }
    }
    assert_eq!(
        register(&H { grant: false }),
        Err(DriverError::PermissionDenied)
    );
    assert!(register(&H { grant: true }).is_ok());
}

#[test]
fn bind_table_matches_a_virtio_block_node() {
    use rustos_abi::HwMatchKey;

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
