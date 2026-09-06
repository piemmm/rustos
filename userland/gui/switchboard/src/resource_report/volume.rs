//! One pane per mounted volume: how fast it is serving, what it holds, what
//! medium it is on, and every completion bucketed
//! (`plans/switchboard/04-disk.png`).
//!
//! Nothing here is served pre-derived: throughput, IOPS, utilisation, await
//! and mean queue depth are all deltas of the volume's cumulative counters
//! over this sample's own interval, folded once in the rolling meters. A
//! first sample, an unmeasurable interval, and an interval in which nothing
//! completed each yield no reading rather than a nought that would read as an
//! idle device.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::sysinfo::{MountRecord, MOUNT_VOLUME_ID_LEN};
use tairix_controls::PressureKind;

use super::{
    availability_name, health_state, health_text, medium_name, used_permille, volume_bytes,
};
use crate::format::{format_bytes, format_latency, format_rate, percent};
use crate::model::{RollingMeters, VolumeService};
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{absence_statement, HealthSeverity, Reading, ReadingFact, Unmeasured};
use crate::view::resources::{
    BlockBody, DeviceAction, DeviceGroup, DeviceId, HeroInstrument, PaneBlock, PaneHero,
    ResourceControl, ResourceDevice, TaskCostColumn,
};

/// One volume's rail entry and pane.
pub(super) fn device(
    sample: &Sample,
    meters: &RollingMeters,
    mount: &MountRecord,
) -> ResourceDevice {
    let id = DeviceId::Volume(mount.volume_id());
    let held = volume_bytes(mount);
    let share = held.map(|held| used_permille(held.total, held.available));
    let service = meters.devices.volume_service(id);
    ResourceDevice {
        id,
        group: DeviceGroup::Storage,
        // The rail states how full the volume is, not how fast: a reader
        // scanning the rail is choosing which volume to look at, and its
        // trace beside this already carries the rate.
        reading: share.map_or_else(
            || Reading::Absent(Unmeasured::Unavailable),
            |permille| Reading::measured(percent(permille)),
        ),
        name: name_of(mount),
        kind: PressureKind::Disk,
        trend: meters.devices.primary_history(id).to_vec(),
        hero: hero(sample, meters, id, &service),
        blocks: blocks(sample, meters, mount, &service),
        banner: None,
        actions: actions(),
    }
}

/// The reading that earns the pane: how much the volume is moving, read above
/// the line and written below, so a read-heavy and a write-heavy volume never
/// look alike.
fn hero(
    sample: &Sample,
    meters: &RollingMeters,
    id: DeviceId,
    service: &VolumeService,
) -> PaneHero {
    let total = service
        .read_bps
        .zip(service.write_bps)
        .map(|(read, write)| read.saturating_add(write));
    PaneHero {
        value: super::reading(sample, DegradedField::VolumeIoStats, total, format_rate),
        unit: String::new(),
        context: context(service),
        instrument: HeroInstrument::Trend {
            samples: meters.devices.primary_history(id).to_vec(),
            opposing: Some(meters.devices.opposing_history(id).to_vec()),
        },
        caption: String::from("read above the line, write below"),
    }
}

/// The volume's name: what is behind it, and where it is mounted.
fn name_of(mount: &MountRecord) -> String {
    let point = String::from_utf8_lossy(mount.target_bytes()).into_owned();
    let source = String::from_utf8_lossy(mount.source_bytes()).into_owned();
    if source.is_empty() {
        return point;
    }
    format!("{source} · {point}")
}

/// How the throughput splits by direction, and what it cost the device.
fn context(service: &VolumeService) -> Vec<String> {
    let mut lines = Vec::new();
    if let (Some(read), Some(write)) = (service.read_bps, service.write_bps) {
        lines.push(format!(
            "{} read · {} write",
            format_rate(read),
            format_rate(write)
        ));
    }
    let iops = service
        .read_iops
        .zip(service.write_iops)
        .map(|(read, write)| read.saturating_add(write));
    match (iops, service.utilisation_permille) {
        (Some(iops), Some(utilisation)) => {
            lines.push(format!("{iops} IOPS · {} utilised", percent(utilisation)));
        }
        (Some(iops), None) => lines.push(format!("{iops} IOPS")),
        (None, Some(utilisation)) => lines.push(format!("{} utilised", percent(utilisation))),
        (None, None) => {}
    }
    lines
}

/// The service-and-queue block, the capacity and medium block, the bucketed
/// health block, and the tasks transferring the most.
fn blocks(
    sample: &Sample,
    meters: &RollingMeters,
    mount: &MountRecord,
    service: &VolumeService,
) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::half(
            "SERVICE & QUEUE",
            BlockBody::Facts(service_facts(sample, service))
        )
        .with_note(
            "Every figure here is a two-sample delta over this pane's own interval: utilisation is busy time over the interval, await is wait time over the requests that completed in it.",
        ),
        PaneBlock::half(
            "CAPACITY & MEDIUM",
            BlockBody::Facts(capacity_facts(mount, service))
        ),
        PaneBlock::half(
            "HEALTH — EVERY COMPLETION, BUCKETED",
            health(sample, &mount.volume_id()),
        ),
        PaneBlock::half(
            "TOP CONSUMERS — DISK",
            BlockBody::Consumers(super::consumers::by_disk(sample, &meters.tasks)),
        )
        .with_note(super::consumers::NOT_A_TOTAL),
    ]
}

/// How hard the volume is being worked, and how deep its queue stands.
///
/// The service rows degrade with the ungated counters and the queue rows with
/// the separately-gated ones, so a caller without the kernel scope still sees
/// its utilisation and await.
fn service_facts(sample: &Sample, service: &VolumeService) -> Vec<ReadingFact> {
    alloc::vec![
        derived(
            sample,
            "Utilisation",
            DegradedField::VolumeIoStats,
            service.utilisation_permille.map(percent),
        ),
        derived(
            sample,
            "Queue depth",
            DegradedField::VolumeIoQueue,
            service.mean_depth_centi.map(|centi| format!(
                "{}.{:02} mean",
                centi / 100,
                centi % 100
            )),
        ),
        derived(
            sample,
            "Await, read",
            DegradedField::VolumeIoStats,
            service.read_await_ns.map(format_latency),
        ),
        derived(
            sample,
            "Await, write",
            DegradedField::VolumeIoStats,
            service.write_await_ns.map(format_latency),
        ),
        derived(
            sample,
            "Service time",
            DegradedField::VolumeIoStats,
            service.service_ns.map(format_latency),
        ),
        derived(
            sample,
            "In-flight requests",
            DegradedField::VolumeIoQueue,
            service
                .in_flight
                .map(|in_flight| match service.budget_depth {
                    Some(depth) => format!("{in_flight} of {depth}"),
                    None => in_flight.to_string(),
                }),
        ),
    ]
}

/// A derived reading, or the sample's own explanation for why `field` could
/// not supply it.
///
/// A derivation with no denominator is genuinely unmeasured this interval
/// even where the query answered, which is why the absence is stated rather
/// than shown as nought.
fn derived(
    sample: &Sample,
    label: &str,
    field: DegradedField,
    text: Option<String>,
) -> ReadingFact {
    match text {
        Some(text) => ReadingFact::text(label, text),
        None => ReadingFact::absent(label, Unmeasured::from_absence(sample.absence(field))),
    }
}

/// What the volume holds, what it is, and the envelope its device is served
/// with.
fn capacity_facts(mount: &MountRecord, service: &VolumeService) -> Vec<ReadingFact> {
    let mut facts = alloc::vec![
        ReadingFact::text(
            "Volume",
            String::from_utf8_lossy(mount.target_bytes()).into_owned(),
        ),
        ReadingFact::text(
            "Filesystem",
            String::from_utf8_lossy(mount.fstype_bytes()).into_owned(),
        ),
        ReadingFact::text("Medium", medium_name(mount.medium())),
        ReadingFact::text("Availability", availability_name(mount.availability())),
    ];
    match volume_bytes(mount) {
        Some(held) => {
            facts.push(ReadingFact::text(
                "Capacity",
                format!(
                    "{} of {}",
                    format_bytes(held.used()),
                    format_bytes(held.total)
                ),
            ));
            facts.push(ReadingFact::text("Free", format_bytes(held.available)));
        }
        None => facts.push(ReadingFact::absent("Capacity", Unmeasured::Unavailable)),
    }
    facts.push(ReadingFact::text(
        "Block size",
        format_bytes(u64::from(mount.usage().block_size)),
    ));
    facts.push(ReadingFact::text("Mount flags", flag_text(mount)));
    if let (Some(depth), Some(deadline)) = (service.budget_depth, service.budget_deadline_ns) {
        facts.push(ReadingFact::text(
            "Device class budget",
            format!("{depth} deep · {} deadline", format_latency(deadline)),
        ));
    }
    facts
}

/// The mount's own permission policy, in the words the mount table uses.
///
/// Rendered through the shared mount-option spelling, so a volume's flags
/// read the same here as they do wherever else the mount table is shown.
fn flag_text(mount: &MountRecord) -> String {
    tairix_procinfo::render_options(mount.flags())
}

/// Every completion, bucketed, with the status the buckets resolve to.
fn health(sample: &Sample, volume_id: &[u8; MOUNT_VOLUME_ID_LEN]) -> BlockBody {
    let Some(records) = sample.volume_health.as_ref() else {
        return BlockBody::Absence(absence_statement(
            "this volume's I/O health",
            Unmeasured::from_absence(sample.absence(DegradedField::VolumeHealth)),
        ));
    };
    let Some(record) = super::find_volume(Some(records.as_slice()), volume_id) else {
        return BlockBody::Absence(absence_statement(
            "this volume's I/O health",
            Unmeasured::Unavailable,
        ));
    };
    let counters = record.counters();
    let severity = health_state(record.availability());
    BlockBody::Health {
        pill: String::from(pill_of(severity)),
        severity,
        facts: alloc::vec![
            ReadingFact::text("Completions", counters.completions.to_string()),
            ReadingFact::text("Answered healthy", counters.ok.to_string()),
            ReadingFact::text("Reissued", counters.reissues.to_string()),
            ReadingFact::text("Transient errors", counters.transient.to_string()),
            ReadingFact::text("Timeouts", counters.timeouts.to_string()),
            ReadingFact::text("Device resets", counters.resets.to_string()),
            ReadingFact::text("Medium errors", counters.medium_errors.to_string()),
            ReadingFact::text("Offline / removed", counters.offline.to_string()),
            ReadingFact::text("Unclassified faults", counters.faults.to_string()),
            ReadingFact::text("Summary", health_text(record)),
        ],
    }
}

/// The pill a severity reads as.
const fn pill_of(severity: HealthSeverity) -> &'static str {
    match severity {
        HealthSeverity::Healthy => "Healthy",
        HealthSeverity::Degraded => "Degraded",
        HealthSeverity::Failing => "Failing",
    }
}

/// The commands the rail offers for a volume.
fn actions() -> Vec<DeviceAction> {
    alloc::vec![
        DeviceAction::ready(
            ResourceControl::SortTasksBy(TaskCostColumn::Disk),
            "Sort tasks by disk",
        ),
        DeviceAction::absent(ResourceControl::Scrub, "Scrub now"),
        DeviceAction::absent(ResourceControl::Trim, "Trim"),
        DeviceAction::absent(ResourceControl::Unmount, "Unmount"),
        DeviceAction::absent(ResourceControl::CopyReadings, "Copy readings"),
    ]
}
