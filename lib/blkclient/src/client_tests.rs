use alloc::vec;
use alloc::vec::Vec;

use super::*;

/// A scripted serving driver: a device whose block content is a
/// deterministic function of the byte address for reads, filled straight
/// into the shared window exactly as the real driver would, and which
/// records every accepted write's LBA and bytes so a test can assert what
/// actually reached the device.
struct MemDevice {
    block_size: u32,
    block_count: u64,
    flags: u32,
    class: BlkDeviceClass,
    calls: Vec<BlkRequest>,
    /// The per-request deadline each exchange was given, so a test can
    /// prove the wait bound follows the device's own class rather than a
    /// fixed value the transport chose for itself.
    deadlines: Vec<u64>,
    /// Every `BlkOp::Write` this device accepted, in arrival order: the
    /// LBA the request named, and the bytes copied out of the window at
    /// that moment.
    written: Vec<(u64, Vec<u8>)>,
}

/// The class the scripted device declares. Deliberately not the
/// unclassified default, so a test asserting the client's budget proves
/// the device was asked rather than assumed.
const DEVICE_CLASS: BlkDeviceClass = BlkDeviceClass::Removable;

fn fill(buf: &mut [u8], byte_base: u64) {
    for (i, out) in buf.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        {
            *out = ((byte_base + i as u64) % 251) as u8;
        }
    }
}

impl BlkCall for MemDevice {
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        window: &mut [u8],
        deadline_ns: u64,
    ) -> Result<usize, Errno> {
        let decoded = BlkRequest::decode(request)?;
        self.calls.push(decoded);
        self.deadlines.push(deadline_ns);
        match decoded.op {
            BlkOp::Geometry => BlkCompletion {
                block_size: self.block_size,
                block_count: self.block_count,
                flags: self.flags,
                class: self.class,
            }
            .encode(reply),
            BlkOp::Read => {
                let bytes = decoded.blocks as usize * self.block_size as usize;
                if bytes > window.len() {
                    return tairix_abi::blkio::encode_error_completion(
                        reply,
                        Errno::LengthOutOfRange,
                    );
                }
                fill(
                    &mut window[..bytes],
                    decoded.lba * u64::from(self.block_size),
                );
                BlkCompletion::default().encode(reply)
            }
            BlkOp::Write => {
                let bytes = decoded.blocks as usize * self.block_size as usize;
                if bytes > window.len() {
                    return tairix_abi::blkio::encode_error_completion(
                        reply,
                        Errno::LengthOutOfRange,
                    );
                }
                self.written.push((decoded.lba, window[..bytes].to_vec()));
                BlkCompletion::default().encode(reply)
            }
            BlkOp::Flush => BlkCompletion::default().encode(reply),
        }
    }
}

fn device(block_size: u32, block_count: u64, flags: u32) -> MemDevice {
    MemDevice {
        block_size,
        block_count,
        flags,
        class: DEVICE_CLASS,
        calls: Vec::new(),
        deadlines: Vec::new(),
        written: Vec::new(),
    }
}

#[test]
fn connect_adopts_the_devices_declared_class_and_budget() {
    // The client serves each device the patience its own class earns,
    // never one assumed envelope: a removable unit's budget here, and
    // the same class re-reported so a composition above inherits it.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let client = RemoteBlock::connect_read_only(device(512, 64, 0), &mut window).expect("connects");
    assert_eq!(client.device_class(), DEVICE_CLASS);
    assert_eq!(client.budget, DEVICE_CLASS.budget());
    assert_ne!(DEVICE_CLASS.budget(), BlkDeviceClass::Virtual.budget());
}

#[test]
fn every_request_after_connect_waits_the_devices_own_deadline() {
    // The wait bound is the *device's*, not a value the transport picked:
    // the geometry probe necessarily runs on the bounded unclassified
    // envelope (nothing has classified the device yet), and every request
    // after it waits exactly this class's deadline.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_only(device(512, 64, 0), &mut window).expect("connects");
    let mut buf = vec![0u8; 512];
    client.read_blocks(0, &mut buf).expect("read");

    let deadlines = &client.call.deadlines;
    assert_eq!(
        deadlines.first().copied(),
        Some(BlkDeviceClass::Virtual.budget().deadline_ns),
        "the geometry probe runs on the bounded unclassified envelope"
    );
    assert!(deadlines.len() > 1, "the read reached the wire");
    for deadline in &deadlines[1..] {
        assert_eq!(*deadline, DEVICE_CLASS.budget().deadline_ns);
    }
    assert_ne!(
        DEVICE_CLASS.budget().deadline_ns,
        BlkDeviceClass::Virtual.budget().deadline_ns,
        "the class must actually change the deadline for this to prove anything"
    );
}

#[test]
fn connect_validates_geometry_and_read_only_flag() {
    let mut window = vec![0u8; BLK_DATA_LEN];
    let client = RemoteBlock::connect_read_only(device(512, 64, BLK_FLAG_READ_ONLY), &mut window)
        .expect("connects");
    assert!(client.read_only());
    assert_eq!(
        client.geometry().expect("geometry"),
        BlockGeometry {
            block_size: 512,
            block_count: 64
        },
        "the validated geometry is cached"
    );
}

#[test]
fn hostile_geometries_are_refused_at_connect() {
    for (block_size, block_count) in [
        (0u32, 64u64),
        (513, 64),
        (256, 64),
        (8192, 64),
        (512, 0),
        (4096, u64::MAX),
    ] {
        let mut window = vec![0u8; BLK_DATA_LEN];
        assert!(
            RemoteBlock::connect_read_only(device(block_size, block_count, 0), &mut window)
                .is_err(),
            "{block_size}x{block_count} must be refused"
        );
    }
}

#[test]
fn a_window_smaller_than_one_block_is_refused() {
    let mut window = vec![0u8; 511];
    assert_eq!(
        RemoteBlock::connect_read_only(device(512, 64, 0), &mut window).err(),
        Some(Errno::OutOfRange)
    );
}

#[test]
fn reads_chunk_through_the_window_and_preserve_data() {
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_only(device(512, 1024, 0), &mut window).expect("connects");

    // Read a span larger than one window chunk so it must split.
    let blocks = (BLK_DATA_LEN / 512) as u64 + 3;
    let mut buf = vec![0u8; usize::try_from(blocks).expect("fits") * 512];
    client.read_blocks(5, &mut buf).expect("reads");

    let mut expected = vec![0u8; buf.len()];
    fill(&mut expected, 5 * 512);
    assert_eq!(buf, expected, "chunked data arrives in order, intact");

    // Geometry + two read chunks were issued.
    let reads: Vec<_> = client
        .call
        .calls
        .iter()
        .filter(|r| r.op == BlkOp::Read)
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0].lba, 5);
    assert_eq!(reads[1].lba, 5 + (BLK_DATA_LEN / 512) as u64);
}

#[test]
fn shape_and_extent_violations_fail_before_any_request() {
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_only(device(512, 8, 0), &mut window).expect("connects");

    let mut misaligned = [0u8; 100];
    assert_eq!(
        client.read_blocks(0, &mut misaligned),
        Err(DriverError::OutOfRange)
    );
    let mut empty: [u8; 0] = [];
    assert_eq!(
        client.read_blocks(0, &mut empty),
        Err(DriverError::OutOfRange)
    );
    let mut past_end = [0u8; 512];
    assert_eq!(
        client.read_blocks(8, &mut past_end),
        Err(DriverError::OutOfRange)
    );
    assert_eq!(
        client.read_blocks(u64::MAX, &mut past_end),
        Err(DriverError::OutOfRange)
    );
    let reads = client.call.calls.iter().filter(|r| r.op == BlkOp::Read);
    assert_eq!(reads.count(), 0, "no invalid request reached the wire");
}

#[test]
fn writes_are_refused_when_the_client_was_opened_read_only() {
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_only(device(512, 8, 0), &mut window).expect("connects");
    assert_eq!(
        client.write_blocks(0, &[0u8; 512]),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        client
            .call
            .calls
            .iter()
            .filter(|r| r.op == BlkOp::Write)
            .count(),
        0,
        "no write reached the wire"
    );
}

#[test]
fn an_error_completion_surfaces_as_a_typed_fault() {
    /// A device that refuses every read with a permission error.
    struct Refusing;
    impl BlkCall for Refusing {
        fn call(
            &mut self,
            request: &[u8],
            reply: &mut [u8],
            _window: &mut [u8],
            _deadline_ns: u64,
        ) -> Result<usize, Errno> {
            if BlkRequest::decode(request)?.op == BlkOp::Geometry {
                BlkCompletion {
                    block_size: 512,
                    block_count: 8,
                    flags: 0,
                    class: DEVICE_CLASS,
                }
                .encode(reply)
            } else {
                tairix_abi::blkio::encode_error_completion(reply, Errno::PermissionDenied)
            }
        }
    }
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_only(Refusing, &mut window).expect("connects");
    let mut buf = [0u8; 512];
    assert_eq!(
        client.read_blocks(0, &mut buf),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn a_truncated_or_corrupt_reply_fails_closed() {
    /// A device that replies with a truncated success frame.
    struct Truncating;
    impl BlkCall for Truncating {
        fn call(
            &mut self,
            request: &[u8],
            reply: &mut [u8],
            _window: &mut [u8],
            _deadline_ns: u64,
        ) -> Result<usize, Errno> {
            if BlkRequest::decode(request)?.op == BlkOp::Geometry {
                BlkCompletion {
                    block_size: 512,
                    block_count: 8,
                    flags: 0,
                    class: DEVICE_CLASS,
                }
                .encode(reply)
            } else {
                reply[..4].copy_from_slice(&0i32.to_le_bytes());
                Ok(4) // success status with the payload missing
            }
        }
    }
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_only(Truncating, &mut window).expect("connects");
    let mut buf = [0u8; 512];
    assert_eq!(
        client.read_blocks(0, &mut buf),
        Err(DriverError::DeviceFault)
    );
}

#[test]
fn a_truncated_or_corrupt_completion_on_a_write_fails_closed() {
    /// A device that replies with a truncated success frame, exactly as
    /// above but exercised on the write path.
    struct Truncating;
    impl BlkCall for Truncating {
        fn call(
            &mut self,
            request: &[u8],
            reply: &mut [u8],
            _window: &mut [u8],
            _deadline_ns: u64,
        ) -> Result<usize, Errno> {
            if BlkRequest::decode(request)?.op == BlkOp::Geometry {
                BlkCompletion {
                    block_size: 512,
                    block_count: 8,
                    flags: 0,
                    class: DEVICE_CLASS,
                }
                .encode(reply)
            } else {
                reply[..4].copy_from_slice(&0i32.to_le_bytes());
                Ok(4)
            }
        }
    }
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_write(Truncating, &mut window).expect("connects");
    assert_eq!(
        client.write_blocks(0, &[0u8; 512]),
        Err(DriverError::DeviceFault)
    );
}

/// A device that answers geometry, then reports a fixed health
/// [`BlkStatus`] for every data request — a stand-in for a disk that has
/// gone medium-error, offline/removed, or is mid-reset.
struct FaultingDevice {
    status: tairix_abi::blkio::BlkStatus,
}
impl BlkCall for FaultingDevice {
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        _window: &mut [u8],
        _deadline_ns: u64,
    ) -> Result<usize, Errno> {
        if BlkRequest::decode(request)?.op == BlkOp::Geometry {
            BlkCompletion {
                block_size: 512,
                block_count: 64,
                flags: 0,
                class: DEVICE_CLASS,
            }
            .encode(reply)
        } else {
            BlkCompletion::default().encode_status(self.status, reply)
        }
    }
}

#[test]
fn the_health_axis_surfaces_as_the_matching_typed_driver_error() {
    use tairix_abi::blkio::BlkStatus;
    // Each health outcome keeps its class through to the `Block`
    // consumer: a bad sector and a gone device are distinct hard errors,
    // the transient/reset classes are reissuable `Busy`, and a timeout or
    // an unclassified fatal fails closed as a device fault.
    for (status, expected) in [
        (BlkStatus::MediumError, DriverError::MediumError),
        (BlkStatus::Offline, DriverError::DeviceOffline),
        (BlkStatus::Removed, DriverError::DeviceOffline),
        (BlkStatus::TransientError, DriverError::Busy),
        (BlkStatus::Reset, DriverError::Busy),
        (BlkStatus::Timeout, DriverError::DeviceFault),
        (BlkStatus::Fatal, DriverError::DeviceFault),
    ] {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect_read_only(FaultingDevice { status }, &mut window)
            .expect("connects");
        let mut buf = [0u8; 512];
        assert_eq!(client.read_blocks(0, &mut buf), Err(expected), "{status:?}");
    }
}

#[test]
fn the_health_axis_surfaces_as_the_matching_typed_driver_error_on_write() {
    use tairix_abi::blkio::BlkStatus;
    // The same health axis, exercised through `write_blocks` on a
    // read/write client: each status keeps its own distinct class on the
    // write path exactly as it does on the read path.
    for (status, expected) in [
        (BlkStatus::MediumError, DriverError::MediumError),
        (BlkStatus::Offline, DriverError::DeviceOffline),
        (BlkStatus::Removed, DriverError::DeviceOffline),
        (BlkStatus::TransientError, DriverError::Busy),
        (BlkStatus::Reset, DriverError::Busy),
        (BlkStatus::Timeout, DriverError::DeviceFault),
        (BlkStatus::Fatal, DriverError::DeviceFault),
    ] {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut client = RemoteBlock::connect_read_write(FaultingDevice { status }, &mut window)
            .expect("connects");
        assert_eq!(
            client.write_blocks(0, &[0u8; 512]),
            Err(expected),
            "{status:?}"
        );
    }
}

#[test]
fn a_faulted_device_does_not_disturb_a_healthy_sibling() {
    use tairix_abi::blkio::BlkStatus;
    // Two independent served devices: one has gone offline, the other is
    // healthy. Each client owns its own transport and window, so a fault
    // on one is contained to its own caller — the head-of-line isolation
    // the strata depends on (one stalling disk never wedges another).
    let mut faulted_window = vec![0u8; BLK_DATA_LEN];
    let mut faulted = RemoteBlock::connect_read_only(
        FaultingDevice {
            status: BlkStatus::Offline,
        },
        &mut faulted_window,
    )
    .expect("connects");

    let mut healthy_window = vec![0u8; BLK_DATA_LEN];
    let mut healthy =
        RemoteBlock::connect_read_only(device(512, 64, 0), &mut healthy_window).expect("connects");

    let mut fbuf = [0u8; 512];
    let mut hbuf = [0u8; 512];
    for lba in 0..4u64 {
        // The faulted sibling fails every read closed...
        assert_eq!(
            faulted.read_blocks(lba, &mut fbuf),
            Err(DriverError::DeviceOffline)
        );
        // ...while the healthy one keeps serving correct data throughout.
        healthy
            .read_blocks(lba, &mut hbuf)
            .expect("healthy device unaffected by the faulted sibling");
        let mut expected = [0u8; 512];
        fill(&mut expected, lba * 512);
        assert_eq!(hbuf, expected, "the healthy sibling's data is intact");
    }
}

/// A device that answers geometry, then reports a reissuable `Reset` for
/// its first `faults` data requests before serving the real payload — a
/// stand-in for a disk that blips (a bus reset) and then recovers.
struct FlakyDevice {
    inner: MemDevice,
    faults: u32,
}
impl BlkCall for FlakyDevice {
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        window: &mut [u8],
        deadline_ns: u64,
    ) -> Result<usize, Errno> {
        let decoded = BlkRequest::decode(request)?;
        if decoded.op != BlkOp::Geometry && self.faults > 0 {
            self.faults -= 1;
            return BlkCompletion::default()
                .encode_status(tairix_abi::blkio::BlkStatus::Reset, reply);
        }
        self.inner.call(request, reply, window, deadline_ns)
    }
}

#[test]
fn a_transient_fault_is_reissued_and_then_succeeds() {
    // Two reissuable resets then a good read: within the rotational retry
    // budget, so the client rides out the blip and returns correct data
    // rather than failing the attempt for a device that was merely
    // recovering.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_only(
        FlakyDevice {
            inner: device(512, 64, 0),
            faults: 2,
        },
        &mut window,
    )
    .expect("connects");
    let mut buf = [0u8; 512];
    client.read_blocks(0, &mut buf).expect("read after reissue");
    let mut expected = [0u8; 512];
    fill(&mut expected, 0);
    assert_eq!(buf, expected, "the recovered read returns the real payload");
}

#[test]
fn a_device_that_keeps_reissuing_fails_closed_at_the_retry_budget() {
    // A device that resets on every attempt (one initial plus the
    // rotational budget of three reissues) fails closed as a reissuable
    // `Busy` rather than retrying forever.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_only(
        FlakyDevice {
            inner: device(512, 64, 0),
            faults: u32::MAX,
        },
        &mut window,
    )
    .expect("connects");
    let mut buf = [0u8; 512];
    assert_eq!(client.read_blocks(0, &mut buf), Err(DriverError::Busy));
}

#[test]
fn writes_are_refused_when_the_device_reports_read_only() {
    // Defence in depth: even a client opened read/write refuses to write
    // to a device that has declared itself write-protected, and nothing
    // reaches the wire before the refusal.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_write(device(512, 8, BLK_FLAG_READ_ONLY), &mut window)
            .expect("connects");
    assert_eq!(
        client.write_blocks(0, &[0u8; 512]),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        client
            .call
            .calls
            .iter()
            .filter(|r| r.op == BlkOp::Write)
            .count(),
        0,
        "no write reached the wire"
    );
}

#[test]
fn writes_chunk_through_the_window_and_arrive_intact_in_order() {
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_write(device(512, 1024, 0), &mut window).expect("connects");

    // A write larger than one window chunk, so it must split exactly like
    // a read does.
    let blocks = (BLK_DATA_LEN / 512) as u64 + 3;
    let mut buf = vec![0u8; usize::try_from(blocks).expect("fits") * 512];
    fill(&mut buf, 5 * 512);
    client.write_blocks(5, &buf).expect("writes");

    let writes = &client.call.written;
    assert_eq!(writes.len(), 2, "the write split into two chunks");
    assert_eq!(writes[0].0, 5, "the first chunk's LBA");
    assert_eq!(
        writes[1].0,
        5 + (BLK_DATA_LEN / 512) as u64,
        "the second chunk's LBA follows the first"
    );
    let mut arrived = Vec::new();
    arrived.extend_from_slice(&writes[0].1);
    arrived.extend_from_slice(&writes[1].1);
    assert_eq!(arrived, buf, "the chunked write arrives intact, in order");
}

#[test]
fn write_shape_and_extent_violations_fail_before_any_request() {
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client =
        RemoteBlock::connect_read_write(device(512, 8, 0), &mut window).expect("connects");

    assert_eq!(
        client.write_blocks(0, &[0u8; 100]),
        Err(DriverError::OutOfRange),
        "not a multiple of the block size"
    );
    assert_eq!(
        client.write_blocks(0, &[]),
        Err(DriverError::OutOfRange),
        "empty"
    );
    assert_eq!(
        client.write_blocks(8, &[0u8; 512]),
        Err(DriverError::OutOfRange),
        "past the end of the device"
    );
    assert_eq!(
        client.write_blocks(u64::MAX, &[0u8; 512]),
        Err(DriverError::OutOfRange),
        "lba overflow"
    );
    assert_eq!(
        client
            .call
            .calls
            .iter()
            .filter(|r| r.op == BlkOp::Write)
            .count(),
        0,
        "no invalid write reached the wire"
    );
    assert!(
        client.call.written.is_empty(),
        "nothing was ever accepted by the device"
    );
}

#[test]
fn flush_reaches_the_wire_on_a_read_write_client_and_not_on_a_read_only_one() {
    let mut writable_window = vec![0u8; BLK_DATA_LEN];
    let mut writable =
        RemoteBlock::connect_read_write(device(512, 8, 0), &mut writable_window).expect("connects");
    writable.flush().expect("flush succeeds");
    assert_eq!(
        writable
            .call
            .calls
            .iter()
            .filter(|r| r.op == BlkOp::Flush)
            .count(),
        1,
        "the read/write client's flush reached the wire"
    );

    let mut probe_window = vec![0u8; BLK_DATA_LEN];
    let mut probe =
        RemoteBlock::connect_read_only(device(512, 8, 0), &mut probe_window).expect("connects");
    probe.flush().expect("flush is a truthful no-op");
    assert_eq!(
        probe
            .call
            .calls
            .iter()
            .filter(|r| r.op == BlkOp::Flush)
            .count(),
        0,
        "a read-only client's flush never reaches the wire"
    );
}

#[test]
fn a_reissuable_write_is_reissued_and_then_succeeds() {
    // Two reissuable resets then a good write: within the rotational
    // retry budget, so the client rides out the blip rather than failing
    // an attempt for a device that was merely recovering, and the write
    // that finally lands is the one the device records.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_write(
        FlakyDevice {
            inner: device(512, 64, 0),
            faults: 2,
        },
        &mut window,
    )
    .expect("connects");
    let buf = [0xAAu8; 512];
    client
        .write_blocks(3, &buf)
        .expect("write succeeds after reissue");
    assert_eq!(
        client.call.inner.written,
        vec![(3u64, buf.to_vec())],
        "exactly the recovered write reached the device"
    );
}

#[test]
fn a_write_that_keeps_reissuing_fails_closed_at_the_retry_budget() {
    // A device that resets on every attempt fails the write closed as a
    // reissuable `Busy` rather than retrying forever, and never actually
    // commits anything while it does.
    let mut window = vec![0u8; BLK_DATA_LEN];
    let mut client = RemoteBlock::connect_read_write(
        FlakyDevice {
            inner: device(512, 64, 0),
            faults: u32::MAX,
        },
        &mut window,
    )
    .expect("connects");
    assert_eq!(client.write_blocks(0, &[0u8; 512]), Err(DriverError::Busy));
    assert!(
        client.call.inner.written.is_empty(),
        "a device that never actually accepts the write never records one"
    );
}
