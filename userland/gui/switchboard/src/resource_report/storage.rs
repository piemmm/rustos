//! One pane per storage *device*: how fast it is serving, what it holds,
//! what medium it is on, and every completion bucketed
//! (`plans/switchboard/04-disk.png`).
//!
//! The service readings are the device's, not a mount's: every volume on one
//! disk reads the same fold, and one volume is projected at as many mount
//! points as the namespace needs. So the rail groups the mount table into
//! devices before it draws anything — otherwise a disk's throughput is
//! reported once per partition and once per projection, and a volume's
//! counters are deltaed against themselves.
//!
//! Nothing here is served pre-derived: throughput, IOPS, utilisation, await
//! and mean queue depth are all deltas of the device's cumulative counters
//! over this sample's own interval, folded once in the rolling meters. A
//! first sample, an unmeasurable interval, and an interval in which nothing
//! completed each yield no reading rather than a nought that would read as an
//! idle device.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::sysinfo::{MountRecord, VolumeIoStatsRecord, MOUNT_VOLUME_ID_LEN};
use tairix_controls::PressureKind;

use super::{
    availability_name, health_state, health_text, medium_name, used_permille, volume_bytes,
    VolumeBytes,
};
use crate::format::{format_bytes, format_latency, format_rate, percent};
use crate::model::{OwnerBundles, RollingMeters, VolumeService};
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{absence_statement, HealthSeverity, Reading, ReadingFact, Unmeasured};
use crate::view::resources::{
    BlockBody, DeviceAction, DeviceId, HeroInstrument, PaneBlock, PaneHero, RailGroup,
    ResourceControl, ResourceDevice, StorageId, TaskCostColumn,
};

/// The nil volume identity: what the mount table reports for a mount with no
/// backing volume at all — the in-RAM layout directories.
const NO_VOLUME: [u8; MOUNT_VOLUME_ID_LEN] = [0; MOUNT_VOLUME_ID_LEN];

/// One volume of a storage device, and every mount projecting it.
struct StorageVolume<'a> {
    /// The volume's durable identity, which the per-volume queries key on.
    id: [u8; MOUNT_VOLUME_ID_LEN],
    /// Every mount projecting it, in the mount table's own order. A volume
    /// reachable at six paths is one volume, not six.
    mounts: Vec<&'a MountRecord>,
}

/// One storage device the rail lists, and what is on it.
pub(crate) struct StorageSubject<'a> {
    /// Its own identity, which the selection remembers.
    id: StorageId,
    /// The volumes on it, in the mount table's own order. Never empty: a
    /// subject is only ever built from a mount.
    volumes: Vec<StorageVolume<'a>>,
}

impl<'a> StorageSubject<'a> {
    /// A subject identified as `id`, carrying `mount`'s volume.
    fn of(id: StorageId, volume: [u8; MOUNT_VOLUME_ID_LEN], mount: &'a MountRecord) -> Self {
        Self {
            id,
            volumes: alloc::vec![StorageVolume {
                id: volume,
                mounts: alloc::vec![mount],
            }],
        }
    }

    /// Add `mount` to this subject, under `volume`.
    fn add(&mut self, volume: [u8; MOUNT_VOLUME_ID_LEN], mount: &'a MountRecord) {
        match self.volumes.iter_mut().find(|held| held.id == volume) {
            Some(held) => held.mounts.push(mount),
            None => self.volumes.push(StorageVolume {
                id: volume,
                mounts: alloc::vec![mount],
            }),
        }
    }

    /// The volume id the device's own readings are looked up by.
    ///
    /// The three per-volume queries key on a volume but report the
    /// *device's* counters, so any volume on the subject answers for it.
    /// The list is non-empty by construction; the nil identity matches no
    /// record, so a subject that somehow held none would read unmeasured
    /// rather than borrow another device's figures.
    pub(crate) fn key(&self) -> [u8; MOUNT_VOLUME_ID_LEN] {
        self.volumes.first().map_or(NO_VOLUME, |volume| volume.id)
    }

    /// This device's rail entry identity.
    pub(crate) const fn device_id(&self) -> DeviceId {
        DeviceId::Storage(self.id)
    }

    /// What the device's volumes hold between them, or [`None`] where none
    /// of them tracks a fixed capacity.
    ///
    /// Summed over the *volumes*, so a volume projected at several mount
    /// points is counted once rather than once per projection.
    fn held(&self) -> Option<VolumeBytes> {
        self.heads()
            .filter_map(|(_, mount)| volume_bytes(mount))
            .reduce(|held, next| VolumeBytes {
                total: held.total.saturating_add(next.total),
                available: held.available.saturating_add(next.available),
            })
    }

    /// The medium the device declares, taking the first mount that names one
    /// so an unclassified projection cannot hide a classified sibling.
    fn medium(&self) -> Option<BlkDeviceClass> {
        self.volumes
            .iter()
            .flat_map(|volume| volume.mounts.iter())
            .find_map(|mount| mount.medium())
    }

    /// Each volume's identity, in rail order: its own record and the first
    /// mount projecting it, which carries the volume's driver-reported
    /// accounting (every projection reports the same).
    fn heads(&self) -> impl Iterator<Item = (&[u8; MOUNT_VOLUME_ID_LEN], &MountRecord)> {
        self.volumes
            .iter()
            .filter_map(|volume| volume.mounts.first().map(|mount| (&volume.id, *mount)))
    }

    /// The device's own name, as its driver declares it, or [`None`] where
    /// it declares none.
    ///
    /// Read from the ungated service query, which is where the name rides
    /// precisely because that is the query the grouping key comes from: a
    /// session that may read no queue depth and no health can still name what
    /// it lists.
    fn device_name(&self, sample: &Sample) -> Option<String> {
        let name = super::find_volume_stats(sample.volume_io_stats.as_deref(), &self.key())
            .map(VolumeIoStatsRecord::device)?;
        name.is_named().then(|| name.as_str().to_string())
    }

    /// The volumes on the device, in rail order. A volume the mount table
    /// gives no source for falls back to its first mount point, so no volume
    /// is ever nameless.
    fn volume_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (_, mount) in self.heads() {
            let source = String::from_utf8_lossy(mount.source_bytes()).into_owned();
            let name = if source.is_empty() {
                String::from_utf8_lossy(mount.target_bytes()).into_owned()
            } else {
                source
            };
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// The rail entry's name: the **device** first, then the volumes on it.
    ///
    /// A device is not at a path and is not a filesystem, so naming the entry
    /// after the volumes that happen to sit on it names a filesystem to a
    /// reader who is choosing which disk to look at. The device leads, and its
    /// volumes follow so two disks of the same kind are still told apart by
    /// what is on them. A device whose driver declares no name is named by its
    /// volumes alone rather than by an invented identity.
    fn name(&self, sample: &Sample) -> String {
        let volumes = self.volume_names();
        match self.device_name(sample) {
            Some(device) if volumes.is_empty() => device,
            Some(device) => format!("{device} · {}", volumes.join(" · ")),
            None => volumes.join(" · "),
        }
    }
}

/// Group the mount table into the storage devices the rail lists.
///
/// A volume projected at several mount points is one volume of one device,
/// and every volume served over one block endpoint is one device — so a
/// disk's throughput is folded and drawn once rather than once per
/// projection or once per partition. Where the kernel publishes no serving
/// device for a volume there are no shared counters to collapse, so the
/// volume stands as its own subject with its capacity alone. A mount with no
/// backing volume at all (the in-RAM layout directories) names no storage
/// device and is not one.
///
/// The order is the mount table's, which is the registry's own stable
/// registration order, so the rail does not reorder itself between samples.
pub(crate) fn subjects(sample: &Sample) -> Vec<StorageSubject<'_>> {
    let mut subjects: Vec<StorageSubject<'_>> = Vec::new();
    for mount in sample.mounts.iter().flatten() {
        let volume = mount.volume_id();
        if volume == NO_VOLUME {
            continue;
        }
        let id = subject_of(sample, &volume);
        match subjects.iter_mut().find(|subject| subject.id == id) {
            Some(subject) => subject.add(volume, mount),
            None => subjects.push(StorageSubject::of(id, volume, mount)),
        }
    }
    subjects
}

/// The subject a volume belongs to: the device serving it where the kernel
/// publishes one, else the volume itself.
///
/// The service query is the ungated one of the three, so this grouping holds
/// for a session that may read no queue depth and no health.
fn subject_of(sample: &Sample, volume: &[u8; MOUNT_VOLUME_ID_LEN]) -> StorageId {
    super::find_volume_stats(sample.volume_io_stats.as_deref(), volume)
        .map(VolumeIoStatsRecord::dev)
        .filter(|dev| *dev != 0)
        .map_or(StorageId::Volume(*volume), StorageId::Device)
}

/// One storage device's rail entry and pane.
pub(super) fn device(
    sample: &Sample,
    meters: &RollingMeters,
    subject: &StorageSubject<'_>,
    bundles: &OwnerBundles,
) -> ResourceDevice {
    let id = subject.device_id();
    let share = subject
        .held()
        .map(|held| used_permille(held.total, held.available));
    let service = meters.devices.volume_service(id);
    ResourceDevice {
        id,
        group: RailGroup::Storage,
        // The rail states how full the device is, not how fast: a reader
        // scanning the rail is choosing which device to look at, and its
        // trace beside this already carries the rate.
        reading: share.map_or_else(
            || Reading::Absent(Unmeasured::Unavailable),
            |permille| Reading::measured(percent(permille)),
        ),
        name: subject.name(sample),
        kind: PressureKind::Disk,
        trend: meters.devices.primary_history(id).to_vec(),
        hero: hero(sample, meters, id, &service),
        blocks: blocks(sample, meters, subject, &service, bundles),
        banner: None,
        actions: actions(),
    }
}

/// The reading that earns the pane: how much the device is moving, read above
/// the line and written below, so a read-heavy and a write-heavy device never
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
        instrument: HeroInstrument::trend(meters.devices.primary_history(id).to_vec())
            .with_opposing(meters.devices.opposing_history(id).to_vec()),
        caption: String::from("read above the line, write below"),
    }
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

/// The service-and-queue block, the capacity and medium block, the volumes
/// on the device, the bucketed health block, and the tasks transferring the
/// most.
fn blocks(
    sample: &Sample,
    meters: &RollingMeters,
    subject: &StorageSubject<'_>,
    service: &VolumeService,
    bundles: &OwnerBundles,
) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::half(
            "SERVICE & QUEUE",
            BlockBody::Facts(service_facts(sample, service))
        ),
        PaneBlock::half(
            "CAPACITY & MEDIUM",
            BlockBody::Facts(capacity_facts(sample, subject, service))
        ),
        PaneBlock::half("VOLUMES & MOUNTS", BlockBody::Facts(volume_facts(subject))),
        PaneBlock::half(
            "HEALTH — EVERY COMPLETION, BUCKETED",
            health(sample, subject)
        ),
        PaneBlock::half(
            "TOP CONSUMERS — DISK",
            BlockBody::Consumers(super::consumers::by_disk(sample, &meters.tasks, bundles)),
        ),
    ]
}

/// How hard the device is being worked, and how deep its queue stands.
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

/// What the device holds between its volumes, what it is, and the envelope it
/// is served with.
fn capacity_facts(
    sample: &Sample,
    subject: &StorageSubject<'_>,
    service: &VolumeService,
) -> Vec<ReadingFact> {
    let mut facts = alloc::vec![
        match subject.device_name(sample) {
            Some(device) => ReadingFact::text("Device", device),
            // Stated rather than invented: nothing above the driver knows
            // what the device is, and a fabricated identity would read like a
            // measurement.
            None => ReadingFact::absent("Device", Unmeasured::Unavailable),
        },
        ReadingFact::text("Volumes", subject.volumes.len().to_string()),
        ReadingFact::text("Medium", medium_name(subject.medium())),
    ];
    match subject.held() {
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
    // Each distinct block size the device's volumes are formatted with, so a
    // disk carrying two differently-formatted volumes names both rather than
    // reporting whichever came first as the device's.
    let mut blocks: Vec<String> = Vec::new();
    for (_, mount) in subject.heads() {
        let size = format_bytes(u64::from(mount.usage().block_size));
        if !blocks.contains(&size) {
            blocks.push(size);
        }
    }
    facts.push(ReadingFact::text("Block size", blocks.join(" · ")));
    if let (Some(depth), Some(deadline)) = (service.budget_depth, service.budget_deadline_ns) {
        facts.push(ReadingFact::text(
            "Device class budget",
            format!("{depth} deep · {} deadline", format_latency(deadline)),
        ));
    }
    facts
}

/// Each volume on the device, then the paths it is reachable at.
///
/// This is where the mount table's own detail lives now that the rail is per
/// device: a volume's filesystem, its own capacity and its availability, then
/// one row per projection carrying that mount's permission policy.
fn volume_facts(subject: &StorageSubject<'_>) -> Vec<ReadingFact> {
    let mut facts = Vec::new();
    for volume in &subject.volumes {
        let Some(head) = volume.mounts.first() else {
            continue;
        };
        let source = String::from_utf8_lossy(head.source_bytes()).into_owned();
        let fstype = String::from_utf8_lossy(head.fstype_bytes()).into_owned();
        let capacity = volume_bytes(head).map_or_else(
            || String::from("capacity unavailable"),
            |bytes| {
                format!(
                    "{} of {}",
                    format_bytes(bytes.used()),
                    format_bytes(bytes.total)
                )
            },
        );
        facts.push(ReadingFact::text(
            if source.is_empty() { "Volume" } else { &source },
            format!(
                "{fstype} · {capacity} · {}",
                availability_name(head.availability())
            ),
        ));
        for mount in &volume.mounts {
            facts.push(ReadingFact::text(
                String::from_utf8_lossy(mount.target_bytes()).into_owned(),
                tairix_procinfo::render_options(mount.flags()),
            ));
        }
    }
    facts
}

/// Every completion, bucketed, with the status the buckets resolve to.
///
/// The counters are the device's own fold, so any of its volumes' records
/// carries them; the pill is the *worst* availability across those volumes,
/// because a volume that has gone unavailable overrides the device's live
/// overlay in its own record alone and reading only the first would report a
/// healthy device with a dirty volume on it.
fn health(sample: &Sample, subject: &StorageSubject<'_>) -> BlockBody {
    let Some(records) = sample.volume_health.as_ref() else {
        return BlockBody::Absence(absence_statement(
            "this device's I/O health",
            Unmeasured::from_absence(sample.absence(DegradedField::VolumeHealth)),
        ));
    };
    let Some(record) = super::find_volume_stats(Some(records.as_slice()), &subject.key()) else {
        return BlockBody::Absence(absence_statement(
            "this device's I/O health",
            Unmeasured::Unavailable,
        ));
    };
    let counters = record.counters();
    let severity = subject
        .heads()
        .filter_map(|(volume, _)| super::find_volume_stats(Some(records.as_slice()), volume))
        .map(|record| health_state(record.availability()))
        .max()
        .unwrap_or_else(|| health_state(record.availability()));
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

/// The commands the rail offers for a storage device.
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
