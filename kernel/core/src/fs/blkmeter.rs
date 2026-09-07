//! One block device's live I/O readings, and the fold that produces them
//! (`plans/FIX-IO.md` IO2/IO3/IO5).
//!
//! Every per-volume reading the `sysinfo` health, service and queue queries
//! report is a property of the **device**, not of a mount: the volumes on one
//! disk share one fold, and one volume is projected at as many mount points
//! as the namespace needs. This module is that fold and nothing else, so the
//! two paths a kernel-side consumer reaches a disk through cannot measure it
//! differently:
//!
//! * a device served by a user-space driver over a block-service endpoint,
//!   folded by [`super::BlkClient`] as it posts and reaps each attempt;
//! * a device the kernel drives itself — the bootstrap-floor disk, which has
//!   no serving endpoint to fold at — wrapped in [`MeteredBlock`].
//!
//! [`MeteredBlock`] sits directly over the device it measures and *under* any
//! cache above it: a cache hit never reaches the medium, so counting one as
//! device work would report a busy disk that is in fact idle.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use tairix_abi::blkio::{
    BlkDeviceClass, BlkDeviceName, BlkHealthCounters, BlkIoCounters, BlkOp, BlkQueueCounters,
    BlkStatus, IoBudget,
};
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use tairix_abi::driver::{BufferClass, DriverError};
use tairix_abi::sysinfo::{BlkHealthTransition, MountAvailability};
use tairix_log::{Field, FieldValue, Level, Sink};

use crate::audit::AuditEvent;
use crate::waitq::{wait_arch, WaitQueueArch};

/// Lock-free cumulative I/O-health tallies for one served block device.
///
/// The atomic mirror of [`BlkHealthCounters`]: the fold runs on the I/O path
/// (never a lock — a driver may park across a completion), and the mount
/// registry snapshots them for the `sysinfo` volume-health query. The status
/// → bucket assignment is *not* duplicated here: it is the one shared
/// [`BlkHealthCounters::bucket_index`] mapping, so the atomic tallies and the
/// pure value type can never disagree on what a "reset" or a "medium error"
/// counts as.
#[derive(Debug)]
pub struct BlkHealthCountersAtomic {
    fields: [AtomicU64; BlkHealthCounters::FIELD_COUNT],
}

impl Default for BlkHealthCountersAtomic {
    fn default() -> Self {
        Self {
            fields: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl BlkHealthCountersAtomic {
    /// Fold one device-level completion of `status`: bump the completion
    /// total and the single bucket `status` maps to, through the shared
    /// [`BlkHealthCounters::bucket_index`]. A plain `fetch_add` — a `u64`
    /// completion tally cannot wrap on any real device lifetime.
    fn fold(&self, status: BlkStatus) {
        self.fields[BlkHealthCounters::COMPLETIONS].fetch_add(1, Ordering::Relaxed);
        self.fields[BlkHealthCounters::bucket_index(status)].fetch_add(1, Ordering::Relaxed);
    }

    /// Record one consumer reissue (retry) of a reissuable completion.
    fn note_reissue(&self) {
        self.fields[BlkHealthCounters::REISSUES].fetch_add(1, Ordering::Relaxed);
    }

    /// A consistent-enough point-in-time snapshot of the tallies, rebuilt
    /// through the shared [`BlkHealthCounters::from_fields`]. The reads are
    /// individually atomic and observability-only, so a snapshot taken during
    /// a concurrent fold may straddle a single increment — never a torn or
    /// invalid value.
    #[must_use]
    pub fn snapshot(&self) -> BlkHealthCounters {
        let mut fields = [0u64; BlkHealthCounters::FIELD_COUNT];
        for (slot, atomic) in fields.iter_mut().zip(self.fields.iter()) {
            *slot = atomic.load(Ordering::Relaxed);
        }
        BlkHealthCounters::from_fields(fields)
    }
}

/// Lock-free cumulative service and queue counters for one served block
/// device.
///
/// The atomic mirror of [`BlkIoCounters`] and [`BlkQueueCounters`]: the fold
/// runs on the I/O path (never a lock — a driver may park across a
/// completion), and the mount registry snapshots them for the `sysinfo`
/// per-volume service and queue queries. The two counter blocks live in one
/// type because one attempt touches both: issuing it samples the queue depth
/// and may open the device-busy interval, and its end closes that interval
/// and folds its bytes and wait. The field order and the read/write field
/// mapping are *not* duplicated here — they are the shared
/// [`BlkIoCounters::direction`] and `from_fields` definitions, so the atomic
/// tallies and the pure value types cannot disagree.
#[derive(Debug, Default)]
pub struct BlkIoStatsAtomic {
    /// The cumulative service tallies, in [`BlkIoCounters`] wire order.
    io: [AtomicU64; BlkIoCounters::FIELD_COUNT],
    /// The queue gauge and its mean-depth accumulators, in
    /// [`BlkQueueCounters`] wire order.
    queue: [AtomicU64; BlkQueueCounters::FIELD_COUNT],
    /// The monotonic reading the open device-busy interval started at, read
    /// only on the edge that closes it.
    busy_since_ns: AtomicU64,
}

impl BlkIoStatsAtomic {
    /// Record one attempt being issued at `now_ns`: take the queue-depth
    /// observation the arriving request sees (itself included, so a device
    /// served one request at a time reads a mean depth of `1` rather than
    /// `0`) and open the device-busy interval if the device was idle.
    fn note_issue(&self, now_ns: u64) {
        let depth = self.queue[BlkQueueCounters::IN_FLIGHT].fetch_add(1, Ordering::AcqRel) + 1;
        add(&self.queue[BlkQueueCounters::QUEUE_DEPTH_SUM], depth);
        add(&self.queue[BlkQueueCounters::QUEUE_SAMPLES], 1);
        if depth == 1 {
            self.busy_since_ns.store(now_ns, Ordering::Release);
        }
    }

    /// Record one attempt leaving the device at `now_ns`, answered or not:
    /// drop it from the in-flight count and, if it was the last outstanding
    /// request, close the device-busy interval into `busy_ns`.
    ///
    /// A timed-out or cancelled attempt reaches here too — the device held it
    /// for the whole deadline, and utilisation has to show that dead time.
    ///
    /// The interval is exact while the device is served serially, which the
    /// shared-data-window discipline guarantees: were two attempts ever to
    /// overlap, an interval could be attributed slightly short. That is
    /// observability drift, never a torn or invalid value.
    fn note_done(&self, now_ns: u64) {
        // Saturating: were a completion ever folded without its issue, a
        // wrapping decrement would report an absurd queue depth to a reader
        // rather than merely losing one interval.
        let outstanding = self.queue[BlkQueueCounters::IN_FLIGHT].fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |depth| Some(depth.saturating_sub(1)),
        );
        if outstanding != Ok(1) {
            return;
        }
        let since = self.busy_since_ns.load(Ordering::Acquire);
        add(
            &self.io[BlkIoCounters::BUSY_NS],
            now_ns.saturating_sub(since),
        );
    }

    /// Fold one attempt the device **answered**: its direction's completed
    /// count, the `bytes` its completion actually moved, and the `wait_ns` it
    /// spent between issue and completion.
    ///
    /// A data-less operation folds nothing — it belongs to neither direction
    /// — and an attempt the device never answered never reaches here, so
    /// await stays a mean over requests that have a latency at all.
    fn note_answered(&self, op: BlkOp, bytes: u64, wait_ns: u64) {
        let Some(direction) = BlkIoCounters::direction(op) else {
            return;
        };
        add(&self.io[direction.bytes], bytes);
        add(&self.io[direction.ops], 1);
        add(&self.io[direction.wait_ns], wait_ns);
    }

    /// A consistent-enough point-in-time snapshot of the service tallies,
    /// rebuilt through the shared [`BlkIoCounters::from_fields`]. The reads
    /// are individually atomic and observability-only, so a snapshot taken
    /// during a concurrent fold may straddle a single increment — never a
    /// torn or invalid value.
    #[must_use]
    pub fn io_snapshot(&self) -> BlkIoCounters {
        BlkIoCounters::from_fields(snapshot_fields(&self.io))
    }

    /// The same for the queue gauge and its accumulators, through
    /// [`BlkQueueCounters::from_fields`].
    #[must_use]
    pub fn queue_snapshot(&self) -> BlkQueueCounters {
        BlkQueueCounters::from_fields(snapshot_fields(&self.queue))
    }
}

/// Add `delta` to `counter`, saturating: a tally is operational
/// observability, so an implausibly-long-lived device pins at [`u64::MAX`]
/// rather than wrapping to a smaller, misleading value — the same discipline
/// the pure value types' folds keep.
fn add(counter: &AtomicU64, delta: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
    });
}

/// Read `counters` into a plain array in the same order, for the shared
/// `from_fields` rebuild.
fn snapshot_fields<const N: usize>(counters: &[AtomicU64; N]) -> [u64; N] {
    core::array::from_fn(|i| counters[i].load(Ordering::Relaxed))
}

/// One served block device's live I/O readings, shared by [`Arc`] with the
/// mount registry so the `sysinfo` per-volume health, service and queue
/// queries and the mount snapshot can read a live device without holding any
/// I/O-path lock.
///
/// It identifies and names the device, carries the availability overlay the
/// fold updates, the cumulative [`BlkHealthCountersAtomic`] outcome tallies,
/// the [`BlkIoStatsAtomic`] service and queue counters, and the [`IoBudget`]
/// those are read against. It carries no capability token and no secret.
///
/// It is cloneable because every reading here is a property of the *device*,
/// not of a mount: every volume on one disk registers the same handles, so
/// they all read one fold rather than a divergent copy each.
#[derive(Clone)]
pub struct VolumeIoSource {
    /// The device's identity: the block-service call-endpoint id serving it,
    /// or its
    /// [`kernel_block_device`](tairix_abi::blkio::kernel_block_device)
    /// identity where the kernel drives the device itself. The two spaces are
    /// disjoint, so one identity always names one device.
    pub dev: u64,
    /// The device's own name, as its driver declares it, or the unnamed
    /// device.
    pub device: BlkDeviceName,
    /// The volume-availability overlay (a [`MountAvailability`] wire byte).
    pub availability: Arc<AtomicU8>,
    /// The cumulative I/O-health tallies folded from every completion.
    pub counters: Arc<BlkHealthCountersAtomic>,
    /// The cumulative service tallies and live queue occupancy folded from
    /// every attempt.
    pub stats: Arc<BlkIoStatsAtomic>,
    /// The per-device budget in force, derived once from the device's
    /// declared class, so a reported depth is read against the ceiling that
    /// applies to that medium.
    pub budget: IoBudget,
}

impl VolumeIoSource {
    /// The live health state the fold last reflected onto this device's
    /// overlay, or [`None`] when the byte names no live state.
    #[must_use]
    pub fn live_availability(&self) -> Option<MountAvailability> {
        live_availability(&self.availability)
    }
}

/// The live health state an availability `overlay` byte names, or [`None`]
/// when it names none.
///
/// Only the overlay's own three live states are live readings; the
/// authoritative vanish states belong to the surprise-removal path and never
/// travel through here. This is the one decode of that byte, read by the
/// mount snapshot and by a metered device's own
/// [`Block::backing_availability`] answer, so the two cannot report one
/// device's health differently.
fn live_availability(overlay: &AtomicU8) -> Option<MountAvailability> {
    match MountAvailability::from_u8(overlay.load(Ordering::Relaxed)) {
        Ok(
            live @ (MountAvailability::Available
            | MountAvailability::Degraded
            | MountAvailability::Recovering),
        ) => Some(live),
        _ => None,
    }
}

/// How one attempt left the device.
///
/// The distinction is the difference between a latency and no latency, so it
/// is a type rather than a flag a caller can transpose: a device that answers
/// a read with a bad sector took time to say so and belongs in the await
/// average, while one that consumed its whole deadline in silence has no
/// latency to average at all and would otherwise drag the mean toward the
/// timeout budget.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Attempt {
    /// The device answered, carrying `status`, having moved `bytes` — nought
    /// for a data-less operation and for a completion whose payload did not
    /// survive.
    Answered {
        /// The health/outcome axis the completion carried.
        status: BlkStatus,
        /// What the completion actually moved.
        bytes: u64,
    },
    /// The attempt left the device without an answer: a deadline the device
    /// consumed whole, or an endpoint torn down under it. It cost the device
    /// time and it says something about the device's health, but it has no
    /// latency and moved nothing.
    Unanswered {
        /// The health/outcome axis the failure classifies to.
        status: BlkStatus,
    },
}

impl Attempt {
    /// The health/outcome axis this attempt carries either way.
    #[must_use]
    pub const fn status(self) -> BlkStatus {
        match self {
            Self::Answered { status, .. } | Self::Unanswered { status } => status,
        }
    }
}

/// The fold behind one block device's [`VolumeIoSource`]: the counters, the
/// health overlay, and the audit trail of the device's availability edges.
///
/// One meter per device, however that device is reached. Both kernel-side
/// consumers bracket each attempt with [`issued`](Self::issued) and
/// [`completed`](Self::completed), so the queue gauge, the busy interval, the
/// service tallies, the health buckets and the overlay can never be advanced
/// in two different orders or for two different sets of attempts.
pub struct DeviceIoMeter {
    /// The device's identity — see [`VolumeIoSource::dev`].
    dev: u64,
    /// The device's declared name, adopted once the device has answered for
    /// itself ([`adopt_device`](Self::adopt_device)).
    device: BlkDeviceName,
    /// The availability overlay, shared with the mount registry.
    availability: Arc<AtomicU8>,
    /// The cumulative outcome tallies, shared with the mount registry.
    counters: Arc<BlkHealthCountersAtomic>,
    /// The cumulative service and queue counters, shared with the mount
    /// registry.
    stats: Arc<BlkIoStatsAtomic>,
    /// The budget the queue depth is read against, adopted with the name.
    budget: IoBudget,
    /// The sink each availability edge is audited through.
    audit: &'static (dyn Sink + Sync),
}

impl DeviceIoMeter {
    /// A meter for the device identified as `dev`, before the device has
    /// said anything about itself.
    ///
    /// Until [`adopt_device`](Self::adopt_device) lands, the device is
    /// unnamed and served the bounded unclassified envelope: a device that
    /// has not answered yet must not be granted a slow medium's patience on
    /// nothing but hope.
    #[must_use]
    pub fn new(dev: u64, audit: &'static (dyn Sink + Sync)) -> Self {
        Self {
            dev,
            device: BlkDeviceName::UNNAMED,
            availability: Arc::new(AtomicU8::new(MountAvailability::Available.as_u8())),
            counters: Arc::new(BlkHealthCountersAtomic::default()),
            stats: Arc::new(BlkIoStatsAtomic::default()),
            budget: BlkDeviceClass::served_as(None).budget(),
            audit,
        }
    }

    /// Adopt what the device declares about itself: its `name`, and the
    /// budget its `class` earns through the one shared
    /// [`BlkDeviceClass::served_as`] policy.
    ///
    /// A class word this build does not recognise is served exactly the
    /// bounded unclassified envelope, so an unknown buys no patience.
    pub fn adopt_device(&mut self, class: Option<BlkDeviceClass>, name: BlkDeviceName) {
        self.budget = BlkDeviceClass::served_as(class).budget();
        self.device = name;
    }

    /// The budget in force for this device.
    #[must_use]
    pub const fn budget(&self) -> IoBudget {
        self.budget
    }

    /// The device's own name, or the unnamed device.
    #[must_use]
    pub const fn device_name(&self) -> BlkDeviceName {
        self.device
    }

    /// The shared live-reading handles the mount registry consults to build
    /// the mount snapshot and answer the `sysinfo` per-volume queries.
    #[must_use]
    pub fn io_source(&self) -> VolumeIoSource {
        VolumeIoSource {
            dev: self.dev,
            device: self.device,
            availability: Arc::clone(&self.availability),
            counters: Arc::clone(&self.counters),
            stats: Arc::clone(&self.stats),
            budget: self.budget,
        }
    }

    /// The live health state this device's overlay currently names.
    #[must_use]
    pub fn live_availability(&self) -> Option<MountAvailability> {
        live_availability(&self.availability)
    }

    /// Record one attempt becoming genuinely outstanding to the device at
    /// `issued_ns`.
    ///
    /// Called only once the attempt has actually reached the device: a
    /// refused post never reached it and must not read as queue occupancy or
    /// as busy time.
    pub fn issued(&self, issued_ns: u64) {
        self.stats.note_issue(issued_ns);
    }

    /// Record the attempt issued at `issued_ns` leaving the device at
    /// `done_ns` as `attempt` describes.
    ///
    /// This is the whole per-attempt fold, in one place: the queue gauge and
    /// the busy interval close, an answered attempt's bytes and latency fold
    /// into its direction, the outcome lands in its health bucket, and the
    /// availability overlay reflects what the outcome says about the device.
    pub fn completed(&self, op: BlkOp, issued_ns: u64, done_ns: u64, attempt: Attempt) {
        self.stats.note_done(done_ns);
        if let Attempt::Answered { bytes, .. } = attempt {
            self.stats
                .note_answered(op, bytes, done_ns.saturating_sub(issued_ns));
        }
        let status = attempt.status();
        self.counters.fold(status);
        self.note_health(status);
    }

    /// Record one consumer reissue of a reissuable completion.
    pub fn reissued(&self) {
        self.counters.note_reissue();
    }

    /// Reflect one completion's reported device-level health into the shared
    /// availability overlay, through the single shared status→availability
    /// mapping, and record a health-transition audit event on a genuine
    /// change of state. A status that carries no volume-availability signal
    /// (a per-request medium error, or a gone/dead device owned by the
    /// surprise-removal path) leaves the overlay unchanged and logs nothing.
    ///
    /// The overlay update is a single atomic swap that yields the prior
    /// state, so the transition is classified edge-triggered through the one
    /// shared [`MountAvailability::health_transition`] rule: a run of
    /// identical completions logs one event, not one per request, and a disk
    /// that comes back is logged exactly once as a recovery. The overlay byte
    /// is only ever a state this method stored, so its decode cannot fail;
    /// were it ever corrupt, it fails closed to "no transition" (no forged
    /// health event).
    fn note_health(&self, status: BlkStatus) {
        let Some(next) = MountAvailability::from_block_status(status) else {
            return;
        };
        let prev = self.availability.swap(next.as_u8(), Ordering::Relaxed);
        if prev == next.as_u8() {
            return;
        }
        if let Ok(prev) = MountAvailability::from_u8(prev) {
            if let Some(transition) = MountAvailability::health_transition(prev, next) {
                self.emit_health(transition);
            }
        }
    }

    /// Emit one storage-health audit record for a real availability edge,
    /// naming the device in the `dev` field (never a secret or a capability
    /// token). A degrade or entry into recovery is a recoverable anomaly
    /// ([`Level::Warn`]); a recovery — the disk came back — is a routine
    /// operational event ([`Level::Info`]).
    fn emit_health(&self, transition: BlkHealthTransition) {
        let (event, level) = match transition {
            BlkHealthTransition::Degraded => (AuditEvent::VolumeDegraded, Level::Warn),
            BlkHealthTransition::Recovering => (AuditEvent::VolumeRecovering, Level::Warn),
            BlkHealthTransition::Recovered => (AuditEvent::VolumeRecovered, Level::Info),
        };
        crate::audit::emit(
            self.audit,
            level,
            event,
            &[Field {
                key: "dev",
                value: FieldValue::UnsignedInt(self.dev),
            }],
        );
    }
}

/// A [`Block`] device the kernel drives itself, with every device operation
/// folded into a [`DeviceIoMeter`] (`plans/FIX-IO.md` IO5).
///
/// The bootstrap-floor disk has no serving block-service endpoint, so
/// nothing on its path would otherwise fold the counters the three
/// per-volume `sysinfo` queries report — and the disk the machine runs from
/// is precisely the one whose service and health a reader needs. This
/// wrapper is that fold, and it is deliberately the *innermost* layer above
/// the device: a cache above it answers hits without touching the medium, and
/// counting those as device work would report throughput and utilisation the
/// disk never did.
///
/// It measures and reports; it decides nothing. Every operation, its result
/// and its error are the wrapped device's, unchanged.
pub struct MeteredBlock<B: Block> {
    /// The device being measured.
    device: B,
    /// The fold every operation passes through.
    meter: DeviceIoMeter,
}

impl<B: Block> MeteredBlock<B> {
    /// Wrap `device` as the device identified as `dev`, adopting the class
    /// and name it declares for itself.
    ///
    /// Unlike the block-service path there is no round trip to learn them:
    /// the device is right here, so the meter is fully informed before the
    /// first operation.
    pub fn new(device: B, dev: u64, audit: &'static (dyn Sink + Sync)) -> Self {
        let mut meter = DeviceIoMeter::new(dev, audit);
        meter.adopt_device(Some(device.device_class()), device.device_name());
        Self { device, meter }
    }

    /// The shared live-reading handles for this device, for the mount
    /// registry to attach to every volume on it.
    #[must_use]
    pub fn io_source(&self) -> VolumeIoSource {
        self.meter.io_source()
    }

    /// Run one device operation of `op`, folding the attempt.
    ///
    /// `moved` is how many bytes the operation transfers when it succeeds;
    /// a failed attempt moved none. A direct device call always answers, so
    /// every attempt here is an [`Attempt::Answered`] one — including a
    /// failure, which the device took time to report. The outcome is
    /// classified through the shared `BlkStatus` mappings — the device's own
    /// promise on success, the health signal its error carries otherwise — so
    /// an in-kernel device's buckets mean exactly what a served device's do.
    fn measured<T>(
        &mut self,
        op: BlkOp,
        moved: u64,
        run: impl FnOnce(&mut B) -> Result<T, DriverError>,
    ) -> Result<T, DriverError> {
        let issued_ns = now_ns();
        self.meter.issued(issued_ns);
        let outcome = run(&mut self.device);
        let done_ns = now_ns();
        let attempt = match &outcome {
            Ok(_) => Attempt::Answered {
                status: BlkStatus::for_backing(self.device.backing_availability()),
                bytes: moved,
            },
            Err(err) => Attempt::Answered {
                status: BlkStatus::for_errno(err.as_errno()),
                bytes: 0,
            },
        };
        self.meter.completed(op, issued_ns, done_ns, attempt);
        outcome
    }
}

impl<B: Block> Block for MeteredBlock<B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        self.device.geometry()
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let moved = buf.len() as u64;
        self.measured(BlkOp::Read, moved, |device| device.read_blocks(lba, buf))
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let moved = buf.len() as u64;
        self.measured(BlkOp::Write, moved, |device| device.write_blocks(lba, buf))
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.measured(BlkOp::Flush, 0, Block::flush)
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let moved = buf.len() as u64;
        self.measured(BlkOp::Read, moved, |device| {
            device.read_blocks_with_class(lba, buf, class)
        })
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let moved = buf.len() as u64;
        self.measured(BlkOp::Write, moved, |device| {
            device.write_blocks_with_class(lba, buf, class)
        })
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        self.device.discard_capability()
    }

    /// A discard moves no bytes of the caller's, but it occupies the device
    /// like any other operation, so it is folded as one.
    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        self.measured(BlkOp::Flush, 0, |device| device.discard(lba, blocks))
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        self.device.device_health()
    }

    fn device_class(&self) -> BlkDeviceClass {
        self.device.device_class()
    }

    fn device_name(&self) -> BlkDeviceName {
        self.device.device_name()
    }

    /// The worse of what the wrapped device promises and what this device's
    /// own folded health says, so a consumer deciding whether to spend
    /// discretionary I/O sees the same reading the mount snapshot does.
    fn backing_availability(&self) -> MountAvailability {
        let device = self.device.backing_availability();
        match self.meter.live_availability() {
            Some(folded) => MountAvailability::worse_of(device, folded),
            None => device,
        }
    }
}

/// The monotonic clock the fold timestamps attempts with, or `0` where no
/// clock is installed (a host test of this path). A meter with no clock folds
/// zero-length intervals — no rate, never a fabricated one.
fn now_ns() -> u64 {
    wait_arch().map_or(0, WaitQueueArch::now_ns)
}

#[cfg(test)]
#[path = "blkmeter_tests.rs"]
mod tests;
