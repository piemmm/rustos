//! Device-health baselines and health-triggered scrub
//! (`docs/src/filesystem/arxfs-spec.md` §11, §15.11).
//!
//! `ARXFS` keeps a notion of the volume's health so it can decide *when* a
//! scrub is worth running, rather than only running one on demand. It leans
//! on the seams the earlier stages built and never adds a
//! second integrity or scrub path:
//!
//! * **The baseline.** A self-identifying [`BlockType::HealthBaseline`] block
//!   reached from the transaction root (like the Stage-8 scrub-progress
//!   record) stores the **last clean device-health snapshot** the next health
//!   pass compares against, plus the volume's **accumulated
//!   filesystem-observed fault counters** — metadata copy-repairs and
//!   unrepairable blocks (the Stage-3 companion-repair seam) and per-class
//!   data faults ([`crate::integrity::DataFault`], the Stage-5 seam). The
//!   device snapshot is *the last clean state* and the accumulated counters
//!   are a durable history, so both are **persisted**, not rebuildable:
//!   a transient fault that was repaired leaves no trace in the live trees.
//!   The block is the single source of truth; a crash mid-update leaves the
//!   previously committed baseline (or none) selected and never blocks a mount.
//! * **The report + thresholds.** [`ARXFS::health`] returns a structured
//!   [`HealthReport`] (mirroring [`crate::ScrubReport`] / [`crate::CheckReport`]
//!   / [`crate::TrimReport`]) that classifies the volume against the documented
//!   [`HealthThresholds`] — healthy / degraded / failing — with no magic
//!   numbers buried in code.
//! * **Health-triggered scrub.** A rise in the device's unsafe-shutdown or
//!   media-error counters since the baseline triggers a scrub through the
//!   Stage-8 [`ARXFS::scrub`] machinery — its `CAP_FS_MOUNT` gate, its budget,
//!   its resumable/interrupt-safe core — never a parallel verifier. The triggered scrub's findings fold back into the
//!   accumulated counters.
//! * **Capability-gated + logged.** The whole operation is gated on
//!   [`CapabilityId::FS_MOUNT`] (the mount-management capability that already
//!   gates scrub/check/trim) and fails closed and logged otherwise. Health-relevant decisions are logged through
//!   `lib/log` with stable event IDs in the reserved `arxfs`
//!   `12000..13000` range.
//!
//! If the device exposes no telemetry the baseline records
//! [`DeviceHealth::Unavailable`]; the health subsystem stays enabled and
//! classifies from the filesystem-observed counters alone.

use rustos_abi::driver::block::{Block, DeviceHealth, HealthSnapshot};
use rustos_abi::{CapabilityId, CapabilityQuery, DriverError};
use rustos_log::{log, Event, EventId, Level, Sink};

use crate::header::{BlockType, HEADER_LEN};
use crate::scrub::{ScrubBudget, ScrubReport, ARXFS_RANGE_END, ARXFS_RANGE_START};
use crate::{rd_u64, wr_u64, ARXFS, MAX_BLOCK_SIZE};

/// A health pass classified the volume healthy.
pub const HEALTH_OK: EventId = EventId(12_040);
/// A health pass classified the volume degraded (a watch-level finding).
pub const HEALTH_DEGRADED: EventId = EventId(12_041);
/// A health pass classified the volume failing (an act-now finding).
pub const HEALTH_FAILING: EventId = EventId(12_042);
/// A health pass triggered a scrub after a device-health delta crossed a
/// threshold.
pub const HEALTH_SCRUB_TRIGGERED: EventId = EventId(12_043);
/// A health pass was refused because the caller lacks `CAP_FS_MOUNT`.
pub const HEALTH_DENIED: EventId = EventId(12_044);

/// Every health event identifier falls inside the reserved `arxfs` range so
/// the stable IDs audit-log consumers rely on never collide with another
/// subsystem.
const _: () = {
    assert!(HEALTH_OK.0 >= ARXFS_RANGE_START && HEALTH_OK.0 < ARXFS_RANGE_END);
    assert!(HEALTH_DEGRADED.0 >= ARXFS_RANGE_START && HEALTH_DEGRADED.0 < ARXFS_RANGE_END);
    assert!(HEALTH_FAILING.0 >= ARXFS_RANGE_START && HEALTH_FAILING.0 < ARXFS_RANGE_END);
    assert!(
        HEALTH_SCRUB_TRIGGERED.0 >= ARXFS_RANGE_START && HEALTH_SCRUB_TRIGGERED.0 < ARXFS_RANGE_END
    );
    assert!(HEALTH_DENIED.0 >= ARXFS_RANGE_START && HEALTH_DENIED.0 < ARXFS_RANGE_END);
};

/// Owner object stamped in the health-baseline block header; a reserved
/// sentinel distinct from any inode number and from the chunk
/// (`u64::MAX - 1`), reverse-reference (`u64::MAX - 2`), and scrub-progress
/// (`u64::MAX - 3`) owners.
const HEALTH_BASELINE_OWNER: u64 = u64::MAX - 4;

/// Magic in the health-baseline payload: `"RFSHLTH1"`.
const HEALTH_MAGIC: u64 = 0x5246_5348_4c54_4831;

// Health-baseline payload field offsets, relative to the end of the header.
// All fields are stored as little-endian `u64`.
const HB_MAGIC: usize = HEADER_LEN;
const HB_DEVICE_PRESENT: usize = HEADER_LEN + 8;
/// First of the [`SNAPSHOT_FIELDS`] device-snapshot `u64`s.
const HB_SNAPSHOT: usize = HEADER_LEN + 16;
/// Number of device-snapshot fields persisted, in the order encoded by
/// [`snapshot_to_array`].
const SNAPSHOT_FIELDS: usize = 11;
/// First of the [`COUNTER_FIELDS`] accumulated fault-counter `u64`s.
const HB_COUNTERS: usize = HB_SNAPSHOT + SNAPSHOT_FIELDS * 8;
/// Number of accumulated filesystem-observed counter fields persisted.
const COUNTER_FIELDS: usize = 6;

/// The volume's health classification (`docs/src/filesystem/arxfs-spec.md`
/// §11). Ordered worst-last so the worse of two signals can be taken with a
/// simple `max`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HealthState {
    /// No threshold crossed: nothing to act on.
    Healthy,
    /// A watch-level threshold crossed: degradation is under way but the
    /// volume is still fully serviceable.
    Degraded,
    /// An act-now threshold crossed: the device or the data is failing.
    Failing,
}

/// The explicit, documented thresholds that classify a volume's health
/// (`docs/src/filesystem/arxfs-spec.md` §11; — no magic
/// numbers buried in code). [`ARXFS::health`] classifies against
/// [`HealthThresholds::DEFAULT`]; the type is public so the chosen values are
/// inspectable and testable. The thresholds are a fixed, non-tunable part of
/// the one mandatory profile.
///
/// A signal at or above a `failing_*` threshold is [`HealthState::Failing`];
/// at or above a `degraded_*` threshold (but below failing) it is
/// [`HealthState::Degraded`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HealthThresholds {
    /// Accumulated metadata copy-repairs (a good copy restored a bad one)
    /// that mark the volume degraded.
    pub degraded_metadata_repairs: u64,
    /// Accumulated both-copies-bad metadata blocks that mark the volume
    /// failing.
    pub failing_metadata_unrepairable: u64,
    /// Accumulated per-class data faults that mark the volume degraded.
    pub degraded_data_faults: u64,
    /// Accumulated per-class data faults that mark the volume failing.
    pub failing_data_faults: u64,
    /// Device media/data-integrity errors that mark the volume degraded.
    pub degraded_media_errors: u64,
    /// Device media/data-integrity errors that mark the volume failing.
    pub failing_media_errors: u64,
    /// Device reallocated/pending sectors that mark the volume degraded.
    pub degraded_reallocated_sectors: u64,
    /// Device uncorrectable sectors that mark the volume failing.
    pub failing_uncorrectable_sectors: u64,
    /// Device wear (percentage of rated endurance used) that marks the
    /// volume degraded.
    pub degraded_wear_pct: u16,
    /// Device wear that marks the volume failing.
    pub failing_wear_pct: u16,
    /// Remaining device spare percentage at or below which the volume is
    /// degraded.
    pub degraded_available_spare_pct: u16,
    /// Remaining device spare percentage at or below which the volume is
    /// failing.
    pub failing_available_spare_pct: u16,
}

impl HealthThresholds {
    /// The fixed thresholds [`ARXFS::health`] classifies against. Chosen so
    /// that a single repaired metadata block or data fault, or any device
    /// media error, raises a watch-level signal, while accumulated faults, a
    /// device critical warning, exhausted spare, or worn-out media raise an
    /// act-now signal.
    pub const DEFAULT: Self = Self {
        degraded_metadata_repairs: 1,
        failing_metadata_unrepairable: 1,
        degraded_data_faults: 1,
        failing_data_faults: 8,
        degraded_media_errors: 1,
        failing_media_errors: 16,
        degraded_reallocated_sectors: 1,
        failing_uncorrectable_sectors: 1,
        degraded_wear_pct: 80,
        failing_wear_pct: 100,
        degraded_available_spare_pct: 20,
        failing_available_spare_pct: 10,
    };
}

/// The structured outcome of a [`ARXFS::health`] pass
/// (`docs/src/filesystem/arxfs-spec.md` §11), mirroring [`ScrubReport`] /
/// [`crate::CheckReport`] / [`crate::TrimReport`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HealthReport {
    /// The volume's classification against [`HealthThresholds::DEFAULT`].
    pub state: HealthState,
    /// The current device snapshot, or `None` when the device exposed no
    /// telemetry this pass (the classification then rests on the
    /// filesystem-observed counters alone; the subsystem stays enabled).
    pub device: Option<HealthSnapshot>,
    /// Rise in the device's unsafe-shutdown counter since the last clean
    /// baseline (saturating). A non-zero delta schedules a metadata scrub.
    pub unsafe_shutdown_delta: u64,
    /// Rise in the device's media-error counter since the last clean
    /// baseline (saturating). A non-zero delta schedules a deep scrub.
    pub media_error_delta: u64,
    /// Accumulated metadata copy-repairs over the volume's lifetime.
    pub metadata_repaired: u64,
    /// Accumulated both-copies-bad metadata blocks over the volume's
    /// lifetime.
    pub metadata_unrepairable: u64,
    /// Accumulated data blocks that failed the physical checksum.
    pub data_physical_faults: u64,
    /// Accumulated data blocks whose AEAD tag failed.
    pub data_aead_faults: u64,
    /// Accumulated data blocks whose plaintext failed its logical hash.
    pub data_logical_faults: u64,
    /// Number of scrubs health monitoring has triggered over the volume's
    /// lifetime.
    pub scrubs_triggered: u64,
    /// `true` when an unsafe-shutdown delta scheduled a metadata scrub.
    pub metadata_scrub_recommended: bool,
    /// `true` when a media-error delta scheduled a deep scrub.
    pub deep_scrub_recommended: bool,
    /// The triggered scrub's report, or `None` when no threshold was crossed
    /// and no scrub ran (the recommendation, when present, was acted on
    /// through the Stage-8 machinery).
    pub scrub: Option<ScrubReport>,
    /// `true` when device health is critical enough that the volume should be
    /// mounted read-only (critical single-device health).
    pub read_only_recommended: bool,
}

impl HealthReport {
    /// Emit the closing health event for this report to `sink` with a stable
    /// event ID.
    fn log_outcome(&self, sink: &dyn Sink) {
        if self.scrub.is_some() {
            log(
                sink,
                &Event {
                    level: Level::Info,
                    id: HEALTH_SCRUB_TRIGGERED,
                    message: "arxfs health triggered a scrub",
                    fields: &[],
                },
            );
        }
        let (level, id, message) = match self.state {
            HealthState::Healthy => (Level::Info, HEALTH_OK, "arxfs health: volume healthy"),
            HealthState::Degraded => (
                Level::Warn,
                HEALTH_DEGRADED,
                "arxfs health: volume degraded",
            ),
            HealthState::Failing => (Level::Error, HEALTH_FAILING, "arxfs health: volume failing"),
        };
        log(
            sink,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }
}

/// The volume's accumulated filesystem-observed fault history. Persisted in
/// the baseline block (not rebuildable): a transient fault that was
/// repaired leaves no trace in the live trees, so the count is only durable
/// if it is written down.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct FaultCounters {
    metadata_repaired: u64,
    metadata_unrepairable: u64,
    data_physical_faults: u64,
    data_aead_faults: u64,
    data_logical_faults: u64,
    scrubs_triggered: u64,
}

impl FaultCounters {
    /// The six persisted counters in their on-disk order.
    fn to_array(self) -> [u64; COUNTER_FIELDS] {
        [
            self.metadata_repaired,
            self.metadata_unrepairable,
            self.data_physical_faults,
            self.data_aead_faults,
            self.data_logical_faults,
            self.scrubs_triggered,
        ]
    }

    /// Reconstruct from the six persisted counters in their on-disk order.
    fn from_array(a: [u64; COUNTER_FIELDS]) -> Self {
        Self {
            metadata_repaired: a[0],
            metadata_unrepairable: a[1],
            data_physical_faults: a[2],
            data_aead_faults: a[3],
            data_logical_faults: a[4],
            scrubs_triggered: a[5],
        }
    }

    /// Fold one scrub's findings into the lifetime counters (saturating, so a
    /// pathological volume can never wrap a counter past the threshold and
    /// look healthy again).
    fn fold_scrub(&mut self, report: &ScrubReport) {
        self.metadata_repaired = self
            .metadata_repaired
            .saturating_add(report.metadata_repaired);
        self.metadata_unrepairable = self
            .metadata_unrepairable
            .saturating_add(report.metadata_unrepairable);
        self.data_physical_faults = self
            .data_physical_faults
            .saturating_add(report.data_physical_faults);
        self.data_aead_faults = self
            .data_aead_faults
            .saturating_add(report.data_aead_faults);
        self.data_logical_faults = self
            .data_logical_faults
            .saturating_add(report.data_logical_faults);
        self.scrubs_triggered = self.scrubs_triggered.saturating_add(1);
    }

    /// Total data faults across every class (saturating).
    fn total_data_faults(self) -> u64 {
        self.data_physical_faults
            .saturating_add(self.data_aead_faults)
            .saturating_add(self.data_logical_faults)
    }
}

/// A decoded health baseline: the last clean device snapshot and the
/// accumulated filesystem-observed counters.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Baseline {
    device: DeviceHealth,
    counters: FaultCounters,
}

/// The eleven device-snapshot fields in their on-disk order. `bool` and
/// `u16` fields widen to `u64`; decode narrows them back, clamping rather
/// than truncating so a corrupt over-wide value never silently wraps.
fn snapshot_to_array(s: &HealthSnapshot) -> [u64; SNAPSHOT_FIELDS] {
    [
        s.power_on_hours,
        s.unsafe_shutdowns,
        s.media_errors,
        s.reallocated_sectors,
        s.pending_sectors,
        s.uncorrectable_sectors,
        s.crc_errors,
        u64::from(s.percentage_used),
        u64::from(s.available_spare),
        u64::from(s.temperature_kelvin),
        u64::from(s.critical_warning),
    ]
}

/// Reconstruct a [`HealthSnapshot`] from its eleven persisted fields,
/// clamping the narrow fields into range (a corrupt baseline is rebuilt at
/// the next clean health pass, so it never blocks a mount).
fn snapshot_from_array(a: [u64; SNAPSHOT_FIELDS]) -> HealthSnapshot {
    HealthSnapshot {
        power_on_hours: a[0],
        unsafe_shutdowns: a[1],
        media_errors: a[2],
        reallocated_sectors: a[3],
        pending_sectors: a[4],
        uncorrectable_sectors: a[5],
        crc_errors: a[6],
        percentage_used: u16::try_from(a[7]).unwrap_or(u16::MAX),
        available_spare: u16::try_from(a[8]).unwrap_or(u16::MAX),
        temperature_kelvin: u16::try_from(a[9]).unwrap_or(u16::MAX),
        critical_warning: a[10] != 0,
    }
}

/// Classify a volume against `thresholds` from its accumulated
/// filesystem-observed counters and its current device snapshot. The worse of
/// the two signals wins (`HealthState` is ordered worst-last).
fn classify(
    counters: &FaultCounters,
    device: DeviceHealth,
    thresholds: &HealthThresholds,
) -> HealthState {
    let mut state = HealthState::Healthy;
    let data_faults = counters.total_data_faults();
    if counters.metadata_repaired >= thresholds.degraded_metadata_repairs
        || data_faults >= thresholds.degraded_data_faults
    {
        state = state.max(HealthState::Degraded);
    }
    if counters.metadata_unrepairable >= thresholds.failing_metadata_unrepairable
        || data_faults >= thresholds.failing_data_faults
    {
        state = state.max(HealthState::Failing);
    }
    if let DeviceHealth::Available(s) = device {
        let reallocated = s.reallocated_sectors.saturating_add(s.pending_sectors);
        if s.media_errors >= thresholds.degraded_media_errors
            || reallocated >= thresholds.degraded_reallocated_sectors
            || s.percentage_used >= thresholds.degraded_wear_pct
            || s.available_spare <= thresholds.degraded_available_spare_pct
        {
            state = state.max(HealthState::Degraded);
        }
        if device_is_critical(&s, thresholds) {
            state = state.max(HealthState::Failing);
        }
    }
    state
}

/// Whether a device snapshot is critical enough to warrant a read-only mount
/// (critical single-device health, `docs/src/filesystem/arxfs-spec.md` §11).
fn device_is_critical(s: &HealthSnapshot, thresholds: &HealthThresholds) -> bool {
    s.critical_warning
        || s.media_errors >= thresholds.failing_media_errors
        || s.uncorrectable_sectors >= thresholds.failing_uncorrectable_sectors
        || s.percentage_used >= thresholds.failing_wear_pct
        || s.available_spare <= thresholds.failing_available_spare_pct
}

impl<B: Block> ARXFS<B> {
    /// Run a device-health pass (`docs/src/filesystem/arxfs-spec.md` §11).
    ///
    /// `health` reads the backing device's current telemetry, compares it
    /// with the last clean baseline, classifies the volume against
    /// [`HealthThresholds::DEFAULT`], and — when the device's unsafe-shutdown
    /// or media-error counters have risen since the baseline — **triggers a
    /// scrub** through the Stage-8 [`Self::scrub`] machinery (its
    /// `CAP_FS_MOUNT` gate, its budget, its resumable core), folding the
    /// scrub's findings into the volume's accumulated fault history. It then
    /// stores the current telemetry as the new baseline so the next pass
    /// compares against it, and returns a structured [`HealthReport`]. The
    /// closing classification is logged to `sink` with a stable event ID.
    ///
    /// A device that exposes no telemetry yields a report whose
    /// classification rests on the accumulated filesystem-observed counters
    /// alone; the subsystem stays enabled regardless.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if `caps` does not grant
    ///   [`CapabilityId::FS_MOUNT`] (fail-closed).
    /// * [`DriverError::DeviceFault`] / [`DriverError::NoSpace`] on an
    ///   unrecoverable device error while reading telemetry, scrubbing, or
    ///   persisting the baseline (never a panic).
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`].
    pub fn health(
        &mut self,
        caps: &dyn CapabilityQuery,
        sink: &dyn Sink,
    ) -> Result<HealthReport, DriverError> {
        if !caps.holds(CapabilityId::FS_MOUNT) {
            log(
                sink,
                &Event {
                    level: Level::Warn,
                    id: HEALTH_DENIED,
                    message: "arxfs health denied: missing CAP_FS_MOUNT",
                    fields: &[],
                },
            );
            return Err(DriverError::PermissionDenied);
        }
        let thresholds = HealthThresholds::DEFAULT;
        let current = self.block.device_health()?;
        let baseline = self.load_health_baseline().unwrap_or(Baseline {
            device: DeviceHealth::Unavailable,
            counters: FaultCounters::default(),
        });

        let (unsafe_shutdown_delta, media_error_delta) = match (baseline.device, current) {
            (DeviceHealth::Available(prev), DeviceHealth::Available(now)) => (
                now.unsafe_shutdowns.saturating_sub(prev.unsafe_shutdowns),
                now.media_errors.saturating_sub(prev.media_errors),
            ),
            _ => (0, 0),
        };
        let metadata_scrub_recommended = unsafe_shutdown_delta > 0;
        let deep_scrub_recommended = media_error_delta > 0;

        let mut counters = baseline.counters;
        let mut scrub = None;
        if metadata_scrub_recommended || deep_scrub_recommended {
            // Reuse the Stage-8 scrub: the gate is already satisfied, so it
            // verifies the whole volume and folds its findings into the
            // lifetime counters (no parallel verifier).
            let report = self.scrub(caps, sink, ScrubBudget::Unlimited)?;
            counters.fold_scrub(&report);
            scrub = Some(report);
        }

        let state = classify(&counters, current, &thresholds);
        let read_only_recommended = match current {
            DeviceHealth::Available(s) => device_is_critical(&s, &thresholds),
            DeviceHealth::Unavailable => false,
        };

        // Persist the new baseline: the current telemetry becomes the next
        // "last clean state" and the folded counters become the new history.
        // A crash mid-update leaves the previous committed baseline selected
        // and never loses live data.
        self.begin();
        let new_baseline = Baseline {
            device: current,
            counters,
        };
        if let Err(err) = self.store_health_baseline(&new_baseline) {
            self.rollback();
            return Err(err);
        }
        self.commit()?;

        let device = match current {
            DeviceHealth::Available(s) => Some(s),
            DeviceHealth::Unavailable => None,
        };
        let report = HealthReport {
            state,
            device,
            unsafe_shutdown_delta,
            media_error_delta,
            metadata_repaired: counters.metadata_repaired,
            metadata_unrepairable: counters.metadata_unrepairable,
            data_physical_faults: counters.data_physical_faults,
            data_aead_faults: counters.data_aead_faults,
            data_logical_faults: counters.data_logical_faults,
            scrubs_triggered: counters.scrubs_triggered,
            metadata_scrub_recommended,
            deep_scrub_recommended,
            scrub,
            read_only_recommended,
        };
        report.log_outcome(sink);
        Ok(report)
    }

    /// Store the initial mkfs baseline: the device's current telemetry with a
    /// zeroed fault history (`docs/src/filesystem/arxfs-spec.md` §11 mkfs
    /// flow). Called inside `format`'s transaction, so it is published by the
    /// same commit that lays down the root.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] / [`DriverError::NoSpace`] on an
    ///   unrecoverable device error.
    pub(crate) fn store_initial_health_baseline(&mut self) -> Result<(), DriverError> {
        let device = self.block.device_health()?;
        let baseline = Baseline {
            device,
            counters: FaultCounters::default(),
        };
        self.store_health_baseline(&baseline)
    }

    /// Load the persisted baseline from the health-baseline block. Returns
    /// `None` when no baseline exists or the record is unreadable or its magic
    /// is wrong: the baseline is rebuildable at the next clean health pass, so a corrupt one is simply re-established rather than failing the
    /// mount.
    fn load_health_baseline(&mut self) -> Option<Baseline> {
        if self.health_baseline_root == 0 {
            return None;
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(
            self.health_baseline_root,
            BlockType::HealthBaseline,
            &mut buf,
        )
        .ok()?;
        if rd_u64(&buf, HB_MAGIC) != HEALTH_MAGIC {
            return None;
        }
        let device = if rd_u64(&buf, HB_DEVICE_PRESENT) == 0 {
            DeviceHealth::Unavailable
        } else {
            let mut fields = [0u64; SNAPSHOT_FIELDS];
            for (i, slot) in fields.iter_mut().enumerate() {
                *slot = rd_u64(&buf, HB_SNAPSHOT + i * 8);
            }
            DeviceHealth::Available(snapshot_from_array(fields))
        };
        let mut counts = [0u64; COUNTER_FIELDS];
        for (i, slot) in counts.iter_mut().enumerate() {
            *slot = rd_u64(&buf, HB_COUNTERS + i * 8);
        }
        Some(Baseline {
            device,
            counters: FaultCounters::from_array(counts),
        })
    }

    /// Persist `baseline` to the health-baseline block, copy-on-writing it
    /// (and updating [`Self::health_baseline_root`]) so a crash mid-update
    /// still leaves a mountable volume.
    fn store_health_baseline(&mut self, baseline: &Baseline) -> Result<(), DriverError> {
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let bs = self.block_size;
        for byte in &mut buf[HEADER_LEN..bs] {
            *byte = 0;
        }
        wr_u64(&mut buf, HB_MAGIC, HEALTH_MAGIC);
        match baseline.device {
            DeviceHealth::Unavailable => wr_u64(&mut buf, HB_DEVICE_PRESENT, 0),
            DeviceHealth::Available(snapshot) => {
                wr_u64(&mut buf, HB_DEVICE_PRESENT, 1);
                for (i, value) in snapshot_to_array(&snapshot).iter().enumerate() {
                    wr_u64(&mut buf, HB_SNAPSHOT + i * 8, *value);
                }
            }
        }
        for (i, value) in baseline.counters.to_array().iter().enumerate() {
            wr_u64(&mut buf, HB_COUNTERS + i * 8, *value);
        }
        let old = self.health_baseline_root;
        let new = self.cow_meta(
            old,
            &mut buf,
            BlockType::HealthBaseline,
            HEALTH_BASELINE_OWNER,
            0,
        )?;
        self.health_baseline_root = new;
        Ok(())
    }
}
