//! One pane per mounted volume: what it holds, what medium it is on, and
//! every completion bucketed (`plans/switchboard/04-disk.png`).
//!
//! Health is a real measurement today. Throughput, utilisation, queue depth
//! and await are exactly what a per-volume I/O statistics query would make
//! derivable; until one exists the block states the absence rather than a
//! plausible figure, and the layout already has the slot.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::sysinfo::{MountRecord, MOUNT_VOLUME_ID_LEN};
use tairix_controls::PressureKind;

use super::{
    availability_name, health_state, health_text, medium_name, used_permille, volume_bytes,
};
use crate::format::{format_bytes, percent};
use crate::model::RollingMeters;
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
    let held = volume_bytes(mount);
    let share = held.map(|held| used_permille(held.total, held.available));
    ResourceDevice {
        id: DeviceId::Volume(mount.volume_id()),
        group: DeviceGroup::Storage,
        name: name_of(mount),
        kind: PressureKind::Disk,
        reading: share.map_or_else(
            || Reading::Absent(Unmeasured::Unavailable),
            |permille| Reading::measured(percent(permille)),
        ),
        // A throughput trace needs a per-volume byte counter, which no query
        // serves; capacity is a level rather than a rate, so the rail entry
        // carries no trace instead of plotting one as the other.
        trend: Vec::new(),
        hero: PaneHero {
            value: held.map_or_else(
                || Reading::Absent(Unmeasured::Unavailable),
                |held| Reading::measured(format_bytes(held.used())),
            ),
            unit: held.map_or_else(String::new, |held| {
                format!("of {}", format_bytes(held.total))
            }),
            context: context(mount),
            instrument: HeroInstrument::Track(share),
            caption: String::new(),
        },
        blocks: blocks(sample, meters, mount),
        banner: None,
        actions: actions(),
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

/// What the volume is, and how much of it is left.
fn context(mount: &MountRecord) -> Vec<String> {
    let mut lines = alloc::vec![format!(
        "{} · {}",
        String::from_utf8_lossy(mount.fstype_bytes()),
        medium_name(mount.medium())
    )];
    if let Some(held) = volume_bytes(mount) {
        lines.push(format!("{} free", format_bytes(held.available)));
    }
    lines
}

/// The service-and-queue block, the capacity and medium block, the bucketed
/// health block, and the tasks transferring the most.
fn blocks(sample: &Sample, meters: &RollingMeters, mount: &MountRecord) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::half("SERVICE & QUEUE", BlockBody::Facts(service_facts())).with_note(
            "Every figure here needs a per-volume I/O statistics query: utilisation is a busy-time delta, await a wait-time delta over completions.",
        ),
        PaneBlock::half("CAPACITY & MEDIUM", BlockBody::Facts(capacity_facts(mount))),
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

/// The service block's readings, each awaiting the one query that would
/// make it derivable.
fn service_facts() -> Vec<ReadingFact> {
    [
        "Utilisation",
        "Queue depth",
        "Await, read",
        "Await, write",
        "Service time",
        "In-flight requests",
    ]
    .iter()
    .map(|label| ReadingFact::absent(*label, Unmeasured::NoInterface))
    .collect()
}

/// What the volume holds and what it is.
fn capacity_facts(mount: &MountRecord) -> Vec<ReadingFact> {
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
    let Some(record) = records
        .iter()
        .find(|record| &record.volume_id() == volume_id)
    else {
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
            ReadingFact::text("Reissued", counters.reissues.to_string()),
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
