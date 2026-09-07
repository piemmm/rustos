//! Unit tests for the per-device I/O fold and the metered in-kernel device.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;
use tairix_log::Event;

use super::*;

/// A sink that discards every event: these tests assert the counters and the
/// overlay, not the audit trail (`blkclient`'s own tests pin that).
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// The sink the tests audit health edges through.
static SINK: NullSink = NullSink;

/// Block size the fixture device is formatted with.
const BLOCK_SIZE: u32 = 512;

/// The same, as the width an index into the fixture's own bytes needs.
const BLOCK: usize = BLOCK_SIZE as usize;

/// A device identity in the reserved kernel-driven space.
const DEV: u64 = tairix_abi::blkio::kernel_block_device(0);

/// A tiny in-memory device whose next operation can be made to fail with a
/// chosen error, and whose promise to a consumer can be set independently of
/// its transfers succeeding.
struct FixtureBlock {
    data: Vec<u8>,
    fail: Option<DriverError>,
    promise: MountAvailability,
    name: BlkDeviceName,
    class: BlkDeviceClass,
    flushes: usize,
    discards: usize,
}

impl FixtureBlock {
    fn new(blocks: usize) -> Self {
        Self {
            data: vec![0u8; blocks * BLOCK],
            fail: None,
            promise: MountAvailability::Available,
            name: BlkDeviceName::new("fixture-blk"),
            class: BlkDeviceClass::Rotational,
            flushes: 0,
            discards: 0,
        }
    }

    fn check(&mut self) -> Result<(), DriverError> {
        match self.fail {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl FixtureBlock {
    /// Byte offset of `lba`, refused if it is not inside the fixture.
    fn offset(&self, lba: u64, len: usize) -> Result<usize, DriverError> {
        let block = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)?;
        let start = block
            .checked_mul(BLOCK)
            .ok_or(DriverError::LengthOutOfRange)?;
        if start.saturating_add(len) > self.data.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(start)
    }
}

impl Block for FixtureBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BLOCK_SIZE,
            block_count: (self.data.len() / BLOCK) as u64,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.check()?;
        let start = self.offset(lba, buf.len())?;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.check()?;
        let start = self.offset(lba, buf.len())?;
        self.data[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.check()?;
        self.flushes += 1;
        Ok(())
    }

    fn discard(&mut self, _lba: u64, _blocks: u64) -> Result<(), DriverError> {
        self.check()?;
        self.discards += 1;
        Ok(())
    }

    fn device_class(&self) -> BlkDeviceClass {
        self.class
    }

    fn device_name(&self) -> BlkDeviceName {
        self.name
    }

    fn backing_availability(&self) -> MountAvailability {
        self.promise
    }
}

/// A meter over one attempt, for the counter assertions.
fn meter() -> DeviceIoMeter {
    DeviceIoMeter::new(DEV, &SINK)
}

#[test]
fn a_new_meter_reports_its_identity_and_no_readings() {
    let source = meter().io_source();
    assert_eq!(source.dev, DEV);
    assert!(!source.device.is_named());
    let counters = source.counters.snapshot();
    assert_eq!(counters.completions, 0);
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, 0);
    assert_eq!(io.write_bytes, 0);
    // An unclassified device is served the bounded envelope, never a slow
    // medium's patience on nothing but hope.
    assert_eq!(
        source.budget,
        BlkDeviceClass::served_as(None).budget(),
        "an unanswered device buys no patience"
    );
}

#[test]
fn adopting_the_device_takes_its_name_and_its_class_budget() {
    let mut meter = meter();
    meter.adopt_device(
        Some(BlkDeviceClass::Rotational),
        BlkDeviceName::new("fixture-blk"),
    );
    let source = meter.io_source();
    assert_eq!(source.device.as_str(), "fixture-blk");
    assert_eq!(source.budget, BlkDeviceClass::Rotational.budget());
}

#[test]
fn an_answered_attempt_folds_its_bytes_latency_and_bucket() {
    let meter = meter();
    meter.issued(1_000);
    meter.completed(
        BlkOp::Read,
        1_000,
        3_000,
        Attempt::Answered {
            status: BlkStatus::Ok,
            bytes: 4_096,
        },
    );

    let source = meter.io_source();
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, 4_096);
    assert_eq!(io.read_ops, 1);
    assert_eq!(io.read_wait_ns, 2_000);
    assert_eq!(io.busy_ns, 2_000, "the device held the request throughout");

    let counters = source.counters.snapshot();
    assert_eq!(counters.completions, 1);
    assert_eq!(counters.ok, 1);

    let queue = source.stats.queue_snapshot();
    assert_eq!(queue.in_flight, 0, "the attempt has left the device");
    assert_eq!(queue.queue_samples, 1);
    assert_eq!(queue.queue_depth_sum, 1);
}

#[test]
fn a_write_folds_into_the_write_direction_only() {
    let meter = meter();
    meter.issued(0);
    meter.completed(
        BlkOp::Write,
        0,
        500,
        Attempt::Answered {
            status: BlkStatus::Ok,
            bytes: 1_024,
        },
    );

    let io = meter.io_source().stats.io_snapshot();
    assert_eq!(io.write_bytes, 1_024);
    assert_eq!(io.write_ops, 1);
    assert_eq!(io.read_bytes, 0);
    assert_eq!(io.read_ops, 0);
}

#[test]
fn an_unanswered_attempt_folds_no_latency_but_still_costs_device_time() {
    let meter = meter();
    meter.issued(100);
    meter.completed(
        BlkOp::Read,
        100,
        900,
        Attempt::Unanswered {
            status: BlkStatus::Timeout,
        },
    );

    let source = meter.io_source();
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, 0, "no payload survived");
    assert_eq!(io.read_ops, 0, "await averages answered attempts only");
    assert_eq!(io.read_wait_ns, 0);
    assert_eq!(
        io.busy_ns, 800,
        "the device consumed the whole deadline and utilisation must show it"
    );
    assert_eq!(
        source.counters.snapshot().timeouts,
        1,
        "a deadline the device consumed whole is a timeout, not an absence"
    );
}

#[test]
fn a_reissue_is_tallied_separately_from_the_completion_it_retries() {
    let meter = meter();
    meter.issued(0);
    meter.completed(
        BlkOp::Read,
        0,
        1,
        Attempt::Answered {
            status: BlkStatus::TransientError,
            bytes: 0,
        },
    );
    meter.reissued();

    let counters = meter.io_source().counters.snapshot();
    assert_eq!(counters.transient, 1);
    assert_eq!(counters.reissues, 1);
    assert_eq!(counters.completions, 1);
}

#[test]
fn the_overlay_follows_the_completion_health_and_recovers() {
    let meter = meter();
    assert_eq!(
        meter.live_availability(),
        Some(MountAvailability::Available),
        "a device with nothing said about it reads healthy"
    );

    meter.issued(0);
    meter.completed(
        BlkOp::Read,
        0,
        1,
        Attempt::Answered {
            status: BlkStatus::Degraded,
            bytes: 0,
        },
    );
    assert_eq!(meter.live_availability(), Some(MountAvailability::Degraded));

    meter.issued(1);
    meter.completed(
        BlkOp::Read,
        1,
        2,
        Attempt::Answered {
            status: BlkStatus::Ok,
            bytes: 4_096,
        },
    );
    assert_eq!(
        meter.live_availability(),
        Some(MountAvailability::Available),
        "a disk that comes back reads healthy again"
    );
}

#[test]
fn a_status_with_no_availability_signal_leaves_the_overlay_alone() {
    let meter = meter();
    meter.issued(0);
    meter.completed(
        BlkOp::Read,
        0,
        1,
        Attempt::Answered {
            status: BlkStatus::MediumError,
            bytes: 0,
        },
    );

    assert_eq!(
        meter.live_availability(),
        Some(MountAvailability::Available),
        "a bad sector is about the request, not the volume's availability"
    );
    assert_eq!(meter.io_source().counters.snapshot().medium_errors, 1);
}

#[test]
fn concurrent_attempts_read_a_deeper_queue() {
    let meter = meter();
    meter.issued(0);
    meter.issued(0);
    let queue = meter.io_source().stats.queue_snapshot();
    assert_eq!(queue.in_flight, 2);
    assert_eq!(queue.queue_depth_sum, 3, "one arriving, then two");

    meter.completed(
        BlkOp::Read,
        0,
        1,
        Attempt::Answered {
            status: BlkStatus::Ok,
            bytes: 0,
        },
    );
    assert_eq!(meter.io_source().stats.queue_snapshot().in_flight, 1);
}

#[test]
fn a_metered_device_names_and_classifies_itself_from_what_it_wraps() {
    let device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    assert_eq!(device.device_name().as_str(), "fixture-blk");
    assert_eq!(device.device_class(), BlkDeviceClass::Rotational);

    let source = device.io_source();
    assert_eq!(source.dev, DEV);
    assert_eq!(source.device.as_str(), "fixture-blk");
    assert_eq!(
        source.budget,
        BlkDeviceClass::Rotational.budget(),
        "the wrapped device's class earns the envelope"
    );
}

#[test]
fn a_metered_read_folds_the_bytes_it_moved() {
    let mut device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    let mut buf = [0u8; BLOCK];
    device.read_blocks(1, &mut buf).expect("the read succeeds");

    let source = device.io_source();
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, u64::from(BLOCK_SIZE));
    assert_eq!(io.read_ops, 1);
    assert_eq!(source.counters.snapshot().ok, 1);
}

#[test]
fn a_metered_write_folds_the_bytes_it_moved() {
    let mut device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    let buf = [0xABu8; BLOCK];
    device.write_blocks(0, &buf).expect("the write succeeds");

    let io = device.io_source().stats.io_snapshot();
    assert_eq!(io.write_bytes, u64::from(BLOCK_SIZE));
    assert_eq!(io.write_ops, 1);
}

#[test]
fn a_metered_flush_occupies_the_device_but_moves_nothing() {
    let mut device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    device.flush().expect("the flush succeeds");

    let source = device.io_source();
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, 0);
    assert_eq!(io.write_bytes, 0);
    assert_eq!(io.read_ops, 0);
    assert_eq!(io.write_ops, 0);
    assert_eq!(
        source.counters.snapshot().completions,
        1,
        "a data-less operation is still a completion the device answered"
    );
}

#[test]
fn a_metered_discard_is_folded_as_an_occupancy_not_a_transfer() {
    let mut device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    device.discard(0, 1).expect("the discard succeeds");

    let source = device.io_source();
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, 0);
    assert_eq!(io.write_bytes, 0);
    assert_eq!(source.counters.snapshot().completions, 1);
}

#[test]
fn a_failed_metered_operation_moves_no_bytes_and_lands_in_its_bucket() {
    let mut inner = FixtureBlock::new(4);
    inner.fail = Some(DriverError::MediumError);
    let mut device = MeteredBlock::new(inner, DEV, &SINK);

    let mut buf = [0u8; BLOCK];
    assert_eq!(
        device.read_blocks(0, &mut buf),
        Err(DriverError::MediumError),
        "the error is the device's, unchanged"
    );

    let source = device.io_source();
    let io = source.stats.io_snapshot();
    assert_eq!(io.read_bytes, 0, "a failed read moved nothing");
    assert_eq!(
        io.read_ops, 1,
        "the device answered — a bad sector is a completion, not silence"
    );
    let counters = source.counters.snapshot();
    assert_eq!(counters.medium_errors, 1);
    assert_eq!(counters.completions, 1);
}

#[test]
fn a_metered_device_offline_error_drives_the_health_bucket() {
    let mut inner = FixtureBlock::new(4);
    inner.fail = Some(DriverError::DeviceOffline);
    let mut device = MeteredBlock::new(inner, DEV, &SINK);

    let mut buf = [0u8; BLOCK];
    assert_eq!(
        device.read_blocks(0, &mut buf),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(device.io_source().counters.snapshot().offline, 1);
}

#[test]
fn a_successful_transfer_on_an_unwell_device_folds_degraded() {
    let mut inner = FixtureBlock::new(4);
    inner.promise = MountAvailability::Degraded;
    let mut device = MeteredBlock::new(inner, DEV, &SINK);

    let mut buf = [0u8; BLOCK];
    device.read_blocks(0, &mut buf).expect("the read succeeds");

    let source = device.io_source();
    assert_eq!(
        source.counters.snapshot().degraded,
        1,
        "a device serving perfectly well while short of redundancy is not `Ok`"
    );
    assert_eq!(
        source.live_availability(),
        Some(MountAvailability::Degraded)
    );
}

#[test]
fn a_metered_device_reports_the_worse_of_its_promise_and_its_fold() {
    let device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    assert_eq!(device.backing_availability(), MountAvailability::Available);

    // A transient error is a device riding out a blip; the metered device
    // must not go on promising a healthy backing to a layer deciding whether
    // to spend discretionary I/O on it.
    device.meter.issued(0);
    device.meter.completed(
        BlkOp::Read,
        0,
        1,
        Attempt::Answered {
            status: BlkStatus::TransientError,
            bytes: 0,
        },
    );
    assert_eq!(device.backing_availability(), MountAvailability::Recovering);
}

#[test]
fn a_metered_device_forwards_geometry_and_the_error_of_a_refused_request() {
    let device = MeteredBlock::new(FixtureBlock::new(4), DEV, &SINK);
    let geometry = device.geometry().expect("the geometry is the device's");
    assert_eq!(geometry.block_size, BLOCK_SIZE);
    assert_eq!(geometry.block_count, 4);

    // A geometry read is an observation, not an attempt: it must not appear
    // as device occupancy.
    assert_eq!(device.io_source().counters.snapshot().completions, 0);
}

#[test]
fn the_kernel_device_identity_space_is_disjoint_from_endpoint_ids() {
    assert!(tairix_abi::blkio::is_kernel_block_device(DEV));
    // The identities a served volume reports are endpoint ids, and the ones
    // the boot floor reports come from this block; nothing may be both.
    assert!(!tairix_abi::blkio::is_kernel_block_device(0));
    assert!(!tairix_abi::blkio::is_kernel_block_device(
        tairix_abi::sysinfo::SYSINFO_ENDPOINT
    ));
}
