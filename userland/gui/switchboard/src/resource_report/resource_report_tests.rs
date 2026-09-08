//! Unit tests for the Resources report builder: that the rail's length is
//! discovered rather than declared, that every reading derives from the
//! sample it claims, and that an absent one names the reason the sample
//! itself resolved.

use alloc::format;
use alloc::vec::Vec;

use tairix_abi::blkio::{
    BlkDeviceClass, BlkDeviceName, BlkHealthCounters, BlkIoCounters, BlkQueueCounters,
};
use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_abi::display_ipc::DisplayStats;
use tairix_abi::driver::display::{AccelCaps, DisplayDeviceReport, DisplayFormat, DisplayMode};
use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
use tairix_abi::hwtree::{HwDeviceClass, HwNode, HW_NODE_ROOT};
use tairix_abi::net_ipc::{
    NetCounters, NetIfKind, NetInterfaceCountersRecord, NetInterfaceFactsRecord, IF_NAME_LEN,
};
use tairix_abi::switchboard_ipc::FrameReport;
use tairix_abi::sysinfo::{
    CpuCoreClass, CpuInfoRecord, KernelMemoryStats, MountAvailability, MountRecord,
    MountVolumeState, VolumeIoHealthRecord, VolumeIoQueueRecord, VolumeIoStatsRecord,
    MOUNT_VOLUME_ID_LEN,
};
use tairix_abi::{CapabilityId, CapabilityQuery};

use super::{build_resource_report, used_permille};
use crate::derive::{derive_summary, Hysteresis};
use crate::model::{OwnerBundles, RollingMeters, SessionReport, VolumeService};
use crate::sample::{CoreBusy, MemoryPressureSample, Sample, ScopeVerdicts};
use crate::view::resources::{BlockBody, DeviceId, HeroInstrument, RailGroup, StorageId};
use crate::view::{
    HealthSeverity, Reading, ReadingFact, ResourceDevice, ResourceReport, Unmeasured,
};

/// A caller holding nothing, so a refusal is a refusal of authority.
struct NoAuthority;

impl CapabilityQuery for NoAuthority {
    fn holds(&self, _capability: CapabilityId) -> bool {
        false
    }
}

/// Every optional reading scope granted, so a test about a *missing*
/// reading is about a failure to answer rather than a refusal.
const PERMITTED: ScopeVerdicts = ScopeVerdicts {
    global_process_scope: true,
    memory_pressure: true,
    hardware_scope: true,
};

/// A sample with every scope granted and nothing measured.
fn permitted() -> Sample {
    Sample {
        scopes: PERMITTED,
        ..Sample::default()
    }
}

/// The report `sample` produces under no authority at all.
fn report_of(sample: &Sample) -> ResourceReport {
    let mut meters = RollingMeters::new();
    folded_report(sample, &mut meters, &SessionReport::HEALTHY)
}

/// Fold `sample` into `meters` and build the report from them, as the service
/// does: the meters are folded once per sample and the report only reads them,
/// so a test that builds twice does not advance a trace twice.
fn folded_report(
    sample: &Sample,
    meters: &mut RollingMeters,
    session: &SessionReport,
) -> ResourceReport {
    meters.record(sample, Hysteresis::new(), session);
    build_resource_report(sample, meters, &OwnerBundles::new(), session, &NoAuthority)
}

/// The device with `id`, which the report must carry.
fn device(report: &ResourceReport, id: DeviceId) -> &ResourceDevice {
    report
        .devices
        .iter()
        .find(|device| device.id == id)
        .expect("the rail must carry this device")
}

/// The reading of the fact named `label` among a pane's blocks.
fn fact<'a>(device: &'a ResourceDevice, label: &str) -> &'a Reading {
    for block in &device.blocks {
        let facts: &[ReadingFact] = match &block.body {
            BlockBody::Facts(facts) | BlockBody::Health { facts, .. } => facts,
            _ => continue,
        };
        if let Some(found) = facts.iter().find(|fact| fact.label == label) {
            return &found.value;
        }
    }
    panic!("no pane block carries a fact named {label}");
}

/// The volume identity the one-volume fixtures use.
const VOLUME: [u8; MOUNT_VOLUME_ID_LEN] = [7; MOUNT_VOLUME_ID_LEN];

/// The block-service endpoint the fixtures' service counters name.
const DEV: u64 = 0x5953_2001;

/// The name the fixtures' device declares for itself, so a test asserting a
/// rail entry's name proves the *device* leads it rather than its volumes.
const DEVICE_NAME: BlkDeviceName = BlkDeviceName::new("virtio-blk");

/// The rail id of the device serving [`VOLUME`], as a sample carrying its
/// service counters names it.
const SERVED: DeviceId = DeviceId::Storage(StorageId::Device(DEV));

/// The rail id of [`VOLUME`] where no sample publishes a serving device for
/// it, so the volume stands as its own subject.
const UNSERVED: DeviceId = DeviceId::Storage(StorageId::Volume(VOLUME));

/// A mount of `volume` at `target`, projected from `source`, with
/// `total`/`avail` blocks of `block` bytes each.
fn mount_of(
    source: &str,
    target: &str,
    volume: [u8; MOUNT_VOLUME_ID_LEN],
    block: u32,
    total: u64,
    avail: u64,
) -> MountRecord {
    MountRecord::new(
        source.as_bytes(),
        target.as_bytes(),
        b"arxfs",
        MountFlags::default(),
        MountVolumeState {
            usage: VolumeStats {
                block_size: block,
                total_blocks: total,
                free_blocks: avail,
                avail_blocks: avail,
                files: 0,
                files_free: 0,
            },
            availability: MountAvailability::Available,
            medium: None,
        },
        volume,
    )
    .expect("a valid mount record")
}

/// A mount of [`VOLUME`] at `target` with `total`/`avail` blocks of `block`
/// bytes each.
fn mount(target: &str, block: u32, total: u64, avail: u64) -> MountRecord {
    mount_of("nvme0", target, VOLUME, block, total, avail)
}

/// `record` with the live availability the mount snapshot would overlay.
fn with_availability(record: &MountRecord, availability: MountAvailability) -> MountRecord {
    MountRecord::new(
        record.source_bytes(),
        record.target_bytes(),
        record.fstype_bytes(),
        record.flags(),
        MountVolumeState {
            usage: record.usage(),
            availability,
            medium: record.medium(),
        },
        record.volume_id(),
    )
    .expect("a valid mount record")
}

/// An interface name, NUL-padded as the wire carries it.
fn if_name(name: &str) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name.as_bytes());
    out
}

/// One interface the inventory names.
fn iface(name: &str) -> NetInterfaceFactsRecord {
    NetInterfaceFactsRecord {
        name: if_name(name),
        mac: [0x52, 0x54, 0x00, 0xa3, 0x1f, 0x0b],
        mtu: 1_500,
        kind: NetIfKind::Ethernet,
        offloads: 0,
        rx_queues: 1,
    }
}

/// The board's own frame: 3,200 damaged pixels of a 2.07 M screen, resolved
/// by blending 4.2 M layer contributions.
fn frame_report() -> FrameReport {
    FrameReport {
        screen_px: 2_073_600,
        damaged_px: 3_200,
        blended_px: 4_203_904,
        opaque_px: 1_842_110,
        dirty_rects: 7,
        present_calls: 1,
        chrome_hits: 124,
        chrome_misses: 2,
    }
}

/// One accelerated graphics device with memory of its own.
fn graphics_device() -> DisplayStats {
    DisplayStats {
        seat_id: 1,
        busy_ns: 250_000_000,
        idle_ns: 750_000_000,
        device: DisplayDeviceReport {
            mem_resident_bytes: 8 << 20,
            mem_total_bytes: 256 << 20,
            accel: Some(AccelCaps {
                max_layers: 4,
                max_width_px: 1_920,
                max_height_px: 1_080,
                per_layer_opacity: true,
            }),
        },
        mode: DisplayMode {
            width_px: 1_920,
            height_px: 1_080,
            stride_bytes: 7_680,
            format: DisplayFormat::Bgra8888,
        },
    }
}

/// One CPU of `class`, whose clock the port does not measure.
fn cpu(index: u32) -> CpuInfoRecord {
    CpuInfoRecord::new(
        index,
        CpuCoreClass::Performance,
        0,
        0,
        0,
        0,
        1_000_000,
        b"Test Core",
    )
    .expect("a valid CPU record")
}

#[test]
fn the_rail_always_carries_the_processor_memory_graphics_and_machine_panes() {
    let report = report_of(&permitted());
    for id in [
        DeviceId::Cpu,
        DeviceId::Memory,
        DeviceId::Graphics,
        DeviceId::Identity,
        DeviceId::Sessions,
        DeviceId::Authority,
    ] {
        let _ = device(&report, id);
    }
    // A machine with nothing mounted and no interface has no `Storage` or
    // `Network` entry at all — the rail is what discovery found, never a
    // fixed set of classes with empty slots in it.
    assert!(!report
        .devices
        .iter()
        .any(|device| device.group == RailGroup::Storage));
    assert!(!report
        .devices
        .iter()
        .any(|device| device.group == RailGroup::Network));
}

/// The rail's `Storage` entries, in rail order, with their names.
fn storage(report: &ResourceReport) -> Vec<(DeviceId, alloc::string::String)> {
    report
        .devices
        .iter()
        .filter(|d| d.group == RailGroup::Storage)
        .map(|d| (d.id, d.name.clone()))
        .collect()
}

#[test]
fn the_rail_grows_one_entry_per_discovered_device_and_interface() {
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount_of("nvme0", "System:", [7; MOUNT_VOLUME_ID_LEN], 4_096, 100, 40),
            mount_of("sda", "Backup:", [9; MOUNT_VOLUME_ID_LEN], 4_096, 200, 10),
        ]),
        net_facts: Some(alloc::vec![iface("eth0"), iface("eth1"), iface("lo")]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(storage(&report).len(), 2);
    assert_eq!(
        report
            .devices
            .iter()
            .filter(|d| d.group == RailGroup::Network)
            .count(),
        3
    );
}

#[test]
fn a_volume_projected_at_many_paths_is_one_rail_entry() {
    // The boot namespace projects one writable volume at `/` and at every
    // flag-bearing subtree beneath it. Those are views of one volume, not
    // six devices, and drawing one per mount is the defect this grouping
    // exists to fix.
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount("/", 4_096, 100, 40),
            mount("/System/Logs", 4_096, 100, 40),
            mount("/System/Settings", 4_096, 100, 40),
            mount("/Users", 4_096, 100, 40),
            mount("/Apps", 4_096, 100, 40),
            mount("/Storage", 4_096, 100, 40),
        ]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        storage(&report),
        alloc::vec![(UNSERVED, alloc::string::String::from("nvme0"))]
    );
    // Every projection is still reachable from the pane, so collapsing the
    // rail loses nothing: each mount point states its own permission policy.
    let device = device(&report, UNSERVED);
    for target in [
        "/",
        "/System/Logs",
        "/System/Settings",
        "/Users",
        "/Apps",
        "/Storage",
    ] {
        let _ = fact(device, target);
    }
    // The capacity is the volume's, counted once rather than six times.
    assert_eq!(
        fact(device, "Capacity"),
        &Reading::measured("240.0 KiB of 400.0 KiB")
    );
    assert_eq!(fact(device, "Volumes"), &Reading::measured("1"));
}

#[test]
fn volumes_sharing_one_served_device_are_one_rail_entry() {
    // Two partitions of one disk report the *same* device fold, so drawing
    // one entry each would state that disk's throughput twice.
    let root = [7; MOUNT_VOLUME_ID_LEN];
    let system = [8; MOUNT_VOLUME_ID_LEN];
    let counters = BlkIoCounters {
        read_bytes: 4 << 20,
        write_bytes: 1 << 20,
        read_ops: 512,
        write_ops: 128,
        busy_ns: 500_000_000,
        read_wait_ns: 64_000_000,
        write_wait_ns: 32_000_000,
    };
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount_of("ARXFSRoot", "/", root, 4_096, 100, 40),
            mount_of("ARXFSSystem", "/System", system, 4_096, 50, 10),
        ]),
        volume_io_stats: Some(alloc::vec![
            VolumeIoStatsRecord::new(root, DEV, counters, DEVICE_NAME),
            VolumeIoStatsRecord::new(system, DEV, counters, DEVICE_NAME),
        ]),
        elapsed_ns: Some(1_000_000_000),
        ..permitted()
    };
    let report = report_of(&sample);
    // The **device** leads the entry, then the volumes on it: a reader
    // choosing which disk to look at must be told the disk, not a filesystem
    // that happens to sit on it.
    assert_eq!(
        storage(&report),
        alloc::vec![(
            SERVED,
            alloc::string::String::from("virtio-blk · ARXFSRoot · ARXFSSystem")
        )]
    );
    let device = device(&report, SERVED);
    assert_eq!(fact(device, "Device"), &Reading::measured("virtio-blk"));
    assert_eq!(fact(device, "Volumes"), &Reading::measured("2"));
    // Both volumes' capacities, each counted once: 240 KiB of 400 KiB and
    // 160 KiB of 200 KiB.
    assert_eq!(
        fact(device, "Capacity"),
        &Reading::measured("400.0 KiB of 600.0 KiB")
    );
    // Each volume names its own filesystem and capacity, then its mounts.
    let _ = fact(device, "ARXFSRoot");
    let _ = fact(device, "ARXFSSystem");
    let _ = fact(device, "/");
    let _ = fact(device, "/System");
}

#[test]
fn a_device_whose_driver_declares_no_name_is_named_by_its_volumes_alone() {
    // Nothing above the driver knows what the device is, so an unnamed one
    // states the absence rather than inventing an identity — and the entry
    // still reaches its volumes.
    let sample = Sample {
        mounts: Some(alloc::vec![mount_of(
            "Backup",
            "/Storage/Backup",
            VOLUME,
            4_096,
            100,
            40
        )]),
        volume_io_stats: Some(alloc::vec![VolumeIoStatsRecord::new(
            VOLUME,
            DEV,
            BlkIoCounters::default(),
            BlkDeviceName::UNNAMED,
        )]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        storage(&report),
        alloc::vec![(SERVED, alloc::string::String::from("Backup"))]
    );
    assert_eq!(
        fact(device(&report, SERVED), "Device"),
        &Reading::Absent(Unmeasured::Unavailable)
    );
}

#[test]
fn a_devices_health_pill_takes_the_worst_of_the_volumes_on_it() {
    // A volume that has gone unavailable overrides the device's live overlay
    // in *its own* record alone, so reading only the first volume would
    // report a healthy device with a dirty volume sitting on it. The
    // buckets are the device's own fold and identical in every record.
    let root = [7; MOUNT_VOLUME_ID_LEN];
    let system = [8; MOUNT_VOLUME_ID_LEN];
    let counters = BlkHealthCounters {
        completions: 12,
        ok: 12,
        ..BlkHealthCounters::default()
    };
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount_of("ARXFSRoot", "/", root, 4_096, 100, 40),
            with_availability(
                &mount_of("ARXFSSystem", "/System", system, 512, 50, 10),
                MountAvailability::UnavailableDirty,
            ),
        ]),
        volume_io_stats: Some(alloc::vec![
            VolumeIoStatsRecord::new(root, DEV, BlkIoCounters::default(), DEVICE_NAME),
            VolumeIoStatsRecord::new(system, DEV, BlkIoCounters::default(), DEVICE_NAME),
        ]),
        volume_health: Some(alloc::vec![
            VolumeIoHealthRecord::new(root, DEV, MountAvailability::Available, counters),
            VolumeIoHealthRecord::new(system, DEV, MountAvailability::UnavailableDirty, counters),
        ]),
        ..permitted()
    };
    let report = report_of(&sample);
    let device = device(&report, SERVED);
    let health = device
        .blocks
        .iter()
        .find_map(|block| match &block.body {
            BlockBody::Health { pill, severity, .. } => Some((pill.clone(), *severity)),
            _ => None,
        })
        .expect("the pane carries a health block");
    assert_eq!(health.1, HealthSeverity::Failing);
    assert_eq!(health.0, "Failing");
    // Each volume's own availability is still stated beside it.
    assert_eq!(
        fact(device, "ARXFSSystem"),
        &Reading::measured("arxfs · 20.0 KiB of 25.0 KiB · unavailable (dirty)")
    );
    // Two volumes formatted differently name both block sizes rather than
    // reporting whichever came first as the device's.
    assert_eq!(
        fact(device, "Block size"),
        &Reading::measured("4.0 KiB · 512 B")
    );
}

#[test]
fn a_device_folds_its_counters_once_however_many_mounts_project_it() {
    // Folding per mount deltas a device's cumulative counters against
    // themselves: the second fold of one sample sees the first fold's own
    // reading as the interval's earlier end, derives a nought rate, and
    // plots it. The rail then shows an idle disk and a flat trace for the
    // volume the machine is actually running from.
    let mut meters = RollingMeters::new();
    let projected = |stats: VolumeIoStatsRecord| Sample {
        mounts: Some(alloc::vec![
            mount("/", 4_096, 100, 40),
            mount("/Users", 4_096, 100, 40),
            mount("/Apps", 4_096, 100, 40),
        ]),
        volume_io_stats: Some(alloc::vec![stats]),
        elapsed_ns: Some(1_000_000_000),
        ..permitted()
    };
    let first = projected(io_stats(0, 0, 0, 0, 0, 0, 0));
    let _ = folded_report(&first, &mut meters, &SessionReport::HEALTHY);
    let second = projected(io_stats(
        4 << 20,
        1 << 20,
        512,
        128,
        500_000_000,
        64_000_000,
        32_000_000,
    ));
    let report = folded_report(&second, &mut meters, &SessionReport::HEALTHY);
    let device = device(&report, SERVED);
    assert_eq!(device.hero.value, Reading::measured("5.0 MiB/s"));
    // One interval, one trace point — not one per projection.
    assert_eq!(device.trend.len(), 1);
    assert_eq!(meters.devices.primary_history(SERVED).len(), 1);
}

#[test]
fn a_mount_with_no_backing_volume_is_no_storage_device() {
    // The in-RAM layout directories the default namespace lays out carry no
    // volume, no capacity and no device. They are view plumbing, and
    // reporting them as disks is the per-mount defect wearing another hat.
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount_of("", "/System", [0; MOUNT_VOLUME_ID_LEN], 0, 0, 0),
            mount_of("", "/Users", [0; MOUNT_VOLUME_ID_LEN], 0, 0, 0),
        ]),
        ..permitted()
    };
    assert!(storage(&report_of(&sample)).is_empty());
}

#[test]
fn the_rail_states_a_refused_inventory_rather_than_reading_as_empty() {
    // An empty rail group and a refused one are different statements: with
    // the reading absent the report names the refusal, so the surface can
    // say so in words instead of showing nothing.
    let refused = Sample {
        scopes: ScopeVerdicts {
            hardware_scope: false,
            ..PERMITTED
        },
        ..Sample::default()
    };
    let report = report_of(&refused);
    assert_eq!(report.interfaces_absent, Some(Unmeasured::NotPermitted));

    let answered = Sample {
        net_facts: Some(alloc::vec![]),
        ..permitted()
    };
    assert_eq!(report_of(&answered).interfaces_absent, None);
}

#[test]
fn the_machine_group_carries_no_trace_because_its_readings_are_facts() {
    let report = report_of(&permitted());
    for id in [DeviceId::Identity, DeviceId::Sessions, DeviceId::Authority] {
        let machine = device(&report, id);
        assert!(
            machine.trend.is_empty(),
            "a fact pane has no rate to plot, and the absent instrument says so"
        );
        assert_eq!(machine.hero.instrument, HeroInstrument::default());
    }
}

#[test]
fn the_resource_panes_carry_both_a_trace_and_a_share_bar() {
    let sample = Sample {
        cpu_busy_permille: Some(180),
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 530,
            total_bytes: 16 * 1024 * 1024 * 1024,
        }),
        ..permitted()
    };
    // Recorded, because a trace is a history: a report built on fresh meters
    // has nothing to plot and would prove nothing about the instrument.
    let mut hysteresis = Hysteresis::new();
    let mut meters = RollingMeters::new();
    for _ in 0..2 {
        let _ = derive_summary(&sample, &mut hysteresis);
        meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
    }
    let report = folded_report(&sample, &mut meters, &SessionReport::HEALTHY);
    // The two answer different questions, and the boards draw both on each of
    // these panes: the trace says what the resource has been doing, the bar
    // how much of it is in use now.
    let cpu = &device(&report, DeviceId::Cpu).hero.instrument;
    assert!(
        !cpu.samples.is_empty(),
        "the processor plots its own history"
    );
    assert_eq!(cpu.track, Some(Some(180)));
    let memory = &device(&report, DeviceId::Memory).hero.instrument;
    assert!(
        !memory.samples.is_empty(),
        "memory plots its committed-share history too"
    );
    assert_eq!(memory.track, Some(Some(530)));
}

#[test]
fn a_core_whose_clock_is_unmeasured_reads_absent_never_a_nominal_figure() {
    let sample = Sample {
        cpu_info: Some(alloc::vec![cpu(0)]),
        core_busy: alloc::vec![CoreBusy {
            cpu: 0,
            permille: Some(410),
        }],
        ..permitted()
    };
    let report = report_of(&sample);
    let grid = device(&report, DeviceId::Cpu)
        .blocks
        .iter()
        .find_map(|block| match &block.body {
            BlockBody::Cores(cells) => Some(cells),
            _ => None,
        })
        .expect("the CPU pane must carry its per-core grid");
    assert_eq!(grid.len(), 1);
    assert_eq!(grid[0].busy, Reading::measured("41%"));
    assert_eq!(
        grid[0].clock,
        Reading::Absent(Unmeasured::Unavailable),
        "a port that measures no clock reports none, never an assumed nominal"
    );
}

#[test]
fn the_per_core_grid_states_its_absence_when_the_inventory_did_not_answer() {
    let report = report_of(&permitted());
    let cpu = device(&report, DeviceId::Cpu);
    assert!(cpu.blocks.iter().any(|block| matches!(
        &block.body,
        BlockBody::Absence(text) if text.contains("unavailable")
    )));
}

#[test]
fn a_storage_devices_capacity_comes_from_its_volumes_block_counts() {
    let sample = Sample {
        mounts: Some(alloc::vec![mount("System:", 4_096, 100, 40)]),
        ..permitted()
    };
    let report = report_of(&sample);
    // With no service counters published there is no serving device to
    // group on, so the volume stands as its own subject.
    let volume = device(&report, UNSERVED);
    // 60 of 100 blocks of 4 KiB used.
    assert_eq!(volume.reading, Reading::measured("60%"));
    assert_eq!(
        fact(volume, "Capacity"),
        &Reading::measured("240.0 KiB of 400.0 KiB")
    );
}

#[test]
fn a_volume_reporting_more_available_than_total_does_not_underflow() {
    // A service reporting more free than total yields nought used rather
    // than wrapping into a nearly-full disk.
    assert_eq!(used_permille(10, 1_000), 0);
    assert_eq!(used_permille(0, 0), 0);
    assert_eq!(used_permille(u64::MAX, 0), 1_000);
}

/// A volume's cumulative service counters, as one sample reports them.
fn io_stats(
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
    busy_ns: u64,
    read_wait_ns: u64,
    write_wait_ns: u64,
) -> VolumeIoStatsRecord {
    VolumeIoStatsRecord::new(
        VOLUME,
        DEV,
        BlkIoCounters {
            read_bytes,
            write_bytes,
            read_ops,
            write_ops,
            busy_ns,
            read_wait_ns,
            write_wait_ns,
        },
        DEVICE_NAME,
    )
}

/// A volume's queue occupancy on a solid-state device's budget.
fn io_queue(in_flight: u64, depth_sum: u64, samples: u64) -> VolumeIoQueueRecord {
    VolumeIoQueueRecord::new(
        VOLUME,
        DEV,
        BlkQueueCounters {
            in_flight,
            queue_depth_sum: depth_sum,
            queue_samples: samples,
        },
        BlkDeviceClass::SolidState.budget(),
    )
}

/// A one-volume sample carrying `stats` and `queue` over a one-second
/// interval.
fn volume_sample(stats: VolumeIoStatsRecord, queue: Option<VolumeIoQueueRecord>) -> Sample {
    Sample {
        mounts: Some(alloc::vec![mount("System:", 4_096, 100, 40)]),
        volume_io_stats: Some(alloc::vec![stats]),
        volume_io_queue: queue.map(|queue| alloc::vec![queue]),
        elapsed_ns: Some(1_000_000_000),
        ..permitted()
    }
}

#[test]
fn a_volumes_first_sample_yields_no_rate_at_all() {
    // A cumulative total is not a rate: with only one reading there is no
    // interval to divide by, so every derived row states its absence rather
    // than reading as an idle disk.
    let sample = volume_sample(
        io_stats(1 << 20, 0, 256, 0, 100_000_000, 40_000_000, 0),
        Some(io_queue(1, 256, 256)),
    );
    let report = report_of(&sample);
    let volume = device(&report, SERVED);
    for label in ["Utilisation", "Await, read", "Service time", "Queue depth"] {
        assert_eq!(
            fact(volume, label),
            &Reading::Absent(Unmeasured::Unavailable),
            "{label} has no interval to derive over on a first sample"
        );
    }
    assert_eq!(volume.hero.value, Reading::Absent(Unmeasured::Unavailable));
    // The instant gauge needs no interval, so it reads on the first sample —
    // against the ceiling the device's own class permits.
    assert_eq!(
        fact(volume, "In-flight requests"),
        &Reading::measured(format!(
            "1 of {}",
            BlkDeviceClass::SolidState.budget().queue_depth
        ))
    );
}

#[test]
fn a_volumes_service_block_derives_every_row_from_two_samples() {
    // Between the two samples: 4 MiB read in 512 ops, 1 MiB written in 128,
    // the device busy for half the second, reads waiting 64 ms in total and
    // writes 32 ms. Every row below is one of those deltas over another.
    let mut meters = RollingMeters::new();
    let first = volume_sample(io_stats(0, 0, 0, 0, 0, 0, 0), Some(io_queue(0, 0, 0)));
    let _ = folded_report(&first, &mut meters, &SessionReport::HEALTHY);
    let second = volume_sample(
        io_stats(
            4 << 20,
            1 << 20,
            512,
            128,
            500_000_000,
            64_000_000,
            32_000_000,
        ),
        Some(io_queue(3, 1_280, 640)),
    );
    let report = folded_report(&second, &mut meters, &SessionReport::HEALTHY);
    let volume = device(&report, SERVED);

    // busy_ns delta over the interval.
    assert_eq!(fact(volume, "Utilisation"), &Reading::measured("50%"));
    // wait_ns delta over the matching ops delta: 64 ms / 512 = 125 us.
    assert_eq!(fact(volume, "Await, read"), &Reading::measured("125.0 us"));
    // 32 ms / 128 = 250 us.
    assert_eq!(fact(volume, "Await, write"), &Reading::measured("250.0 us"));
    // busy_ns delta over every request that completed: 500 ms / 640.
    assert_eq!(fact(volume, "Service time"), &Reading::measured("781.2 us"));
    // depth sum delta over arrivals delta: 1280 / 640 = 2.00.
    assert_eq!(fact(volume, "Queue depth"), &Reading::measured("2.00 mean"));
    assert_eq!(
        fact(volume, "In-flight requests"),
        &Reading::measured("3 of 32")
    );
    // The queue record carries the budget, so the capacity block states the
    // envelope the device is really served with.
    assert_eq!(
        fact(volume, "Device class budget"),
        &Reading::measured("32 deep · 5.0 s deadline")
    );

    // The hero is the rate, split by direction and traced duplex.
    assert_eq!(volume.hero.value, Reading::measured("5.0 MiB/s"));
    assert!(volume
        .hero
        .context
        .iter()
        .any(|line| line.contains("4.0 MiB/s read") && line.contains("1.0 MiB/s write")));
    assert!(volume
        .hero
        .context
        .iter()
        .any(|line| line.contains("640 IOPS") && line.contains("50% utilised")));
    let instrument = &volume.hero.instrument;
    assert_eq!(instrument.samples.len(), 1);
    assert_eq!(instrument.opposing.as_ref().map(Vec::len), Some(1));
    // The rail states how full the volume is; its trace carries the rate.
    assert_eq!(volume.reading, Reading::measured("60%"));
    assert_eq!(volume.trend.len(), 1);
}

#[test]
fn a_denied_queue_scope_costs_the_queue_rows_alone() {
    // The service counters are ungated and the queue counters are not, so a
    // caller without the kernel scope still reads its utilisation and await
    // while the two queue rows state that they were not permitted.
    let mut meters = RollingMeters::new();
    let denied = ScopeVerdicts {
        memory_pressure: false,
        ..PERMITTED
    };
    let first = Sample {
        scopes: denied,
        ..volume_sample(io_stats(0, 0, 0, 0, 0, 0, 0), None)
    };
    let _ = folded_report(&first, &mut meters, &SessionReport::HEALTHY);
    let second = Sample {
        scopes: denied,
        ..volume_sample(
            io_stats(4 << 20, 0, 512, 0, 500_000_000, 64_000_000, 0),
            None,
        )
    };
    let report = folded_report(&second, &mut meters, &SessionReport::HEALTHY);
    let volume = device(&report, SERVED);
    assert_eq!(fact(volume, "Utilisation"), &Reading::measured("50%"));
    assert_eq!(fact(volume, "Await, read"), &Reading::measured("125.0 us"));
    for label in ["Queue depth", "In-flight requests"] {
        assert_eq!(
            fact(volume, label),
            &Reading::Absent(Unmeasured::NotPermitted),
            "{label} is gated on the kernel scope and says which refusal"
        );
    }
}

#[test]
fn a_sample_with_no_counters_breaks_the_series_rather_than_deltaing_over_the_gap() {
    // The counters are cumulative since attach, so deltaing a fresh reading
    // against a stale one — or against nought — would report a whole
    // lifetime's transfer as one interval's rate. A sample the query could
    // not answer therefore makes the next one a first sample again.
    let mut meters = RollingMeters::new();
    let low = io_stats(1 << 20, 0, 128, 0, 100_000_000, 16_000_000, 0);
    let high = io_stats(64 << 20, 0, 8_192, 0, 900_000_000, 512_000_000, 0);
    let steps = [
        volume_sample(low, None),
        // The query did not answer this cycle.
        Sample {
            volume_io_stats: None,
            ..volume_sample(low, None)
        },
        volume_sample(high, None),
    ];
    let mut last = None;
    for sample in steps {
        last = Some(folded_report(&sample, &mut meters, &SessionReport::HEALTHY));
    }
    let report = last.expect("three samples were folded");
    let volume = device(&report, SERVED);
    assert_eq!(volume.hero.value, Reading::Absent(Unmeasured::Unavailable));
    assert_eq!(
        fact(volume, "Utilisation"),
        &Reading::Absent(Unmeasured::Unavailable)
    );
    assert!(
        volume.trend.is_empty(),
        "no interval was measurable, so no point was plotted"
    );
}

#[test]
fn the_graphics_utilisation_is_an_interval_share_and_breaks_on_a_gap() {
    // `busy_ns` is cumulative since the display service started, so the share
    // must be a delta over the sample's own interval. A cycle the query could
    // not answer makes the next one a first sample again, rather than
    // reporting a whole service lifetime's occupancy as this interval's.
    let mut meters = RollingMeters::new();
    let graphics = |busy_ns: u64, elapsed_ns: Option<u64>, present: bool| Sample {
        hardware: Some(alloc::vec![HwNode::new(
            1,
            HW_NODE_ROOT,
            HwDeviceClass::Display
        )]),
        gpu_stats: present.then(|| {
            alloc::vec![DisplayStats {
                busy_ns,
                ..graphics_device()
            }]
        }),
        elapsed_ns,
        ..permitted()
    };
    let step = |meters: &mut RollingMeters, sample: &Sample| {
        folded_report(sample, meters, &SessionReport::HEALTHY)
    };

    let _ = step(&mut meters, &graphics(1_000_000_000, None, true));
    // A quarter of a one-second interval spent busy.
    let report = step(
        &mut meters,
        &graphics(1_250_000_000, Some(1_000_000_000), true),
    );
    assert_eq!(
        fact(device(&report, DeviceId::Graphics), "Device utilisation"),
        &Reading::measured("25%")
    );

    // The query does not answer, then answers a far larger total: the share
    // must be absent both times rather than deltaing over the gap.
    let report = step(&mut meters, &graphics(0, Some(1_000_000_000), false));
    assert_eq!(
        fact(device(&report, DeviceId::Graphics), "Device utilisation"),
        &Reading::Absent(Unmeasured::Unavailable),
        "permitted but unanswered: a fault to show, not a refusal"
    );
    let report = step(
        &mut meters,
        &graphics(9_000_000_000, Some(1_000_000_000), true),
    );
    assert_eq!(
        fact(device(&report, DeviceId::Graphics), "Device utilisation"),
        &Reading::Absent(Unmeasured::Unavailable),
        "the sample after an absent one is a first sample again"
    );
}

#[test]
fn an_unmounted_volume_leaks_neither_its_counters_nor_its_trace() {
    // Two samples give the volume a rate. It is then unmounted, and the same
    // id returns: its first sample after the return must again yield no rate,
    // which is only true if the fold dropped its counters and its history
    // with the mount.
    let mut meters = RollingMeters::new();
    let stats = io_stats(4 << 20, 0, 512, 0, 500_000_000, 64_000_000, 0);
    for sample in [
        volume_sample(io_stats(0, 0, 0, 0, 0, 0, 0), None),
        volume_sample(stats, None),
    ] {
        let _ = folded_report(&sample, &mut meters, &SessionReport::HEALTHY);
    }
    let id = SERVED;
    assert!(!meters.devices.primary_history(id).is_empty());

    // Unmounted: the sample names no volume at all.
    let _ = folded_report(&permitted(), &mut meters, &SessionReport::HEALTHY);
    assert!(meters.devices.primary_history(id).is_empty());
    assert_eq!(meters.devices.volume_service(id), VolumeService::default());

    // Back again, with the counters the departed volume left behind: the
    // first sample after the return is a first sample, so there is no rate.
    let report = folded_report(
        &volume_sample(stats, None),
        &mut meters,
        &SessionReport::HEALTHY,
    );
    let volume = device(&report, id);
    assert_eq!(volume.hero.value, Reading::Absent(Unmeasured::Unavailable));
    assert_eq!(
        fact(volume, "Utilisation"),
        &Reading::Absent(Unmeasured::Unavailable)
    );
}

/// A sample naming `eth0` with `rx`/`tx` cumulative bytes on it, over a
/// one-second interval.
fn interface_sample(rx: u64, tx: u64) -> Sample {
    Sample {
        net_facts: Some(alloc::vec![iface("eth0")]),
        net_counters: Some(alloc::vec![NetInterfaceCountersRecord {
            name: if_name("eth0"),
            counters: NetCounters {
                rx_bytes: rx,
                tx_bytes: tx,
                ..NetCounters::default()
            },
        }]),
        elapsed_ns: Some(1_000_000_000),
        ..permitted()
    }
}

#[test]
fn an_interface_entry_carries_the_trace_its_counters_derive() {
    // The rail folds every interface's cumulative counters; an entry that
    // then drew no trace would be measuring and discarding. A rate has no
    // ceiling to fill a bar against, so the hero trends duplex rather than
    // tracking.
    let mut meters = RollingMeters::new();
    let first = interface_sample(0, 0);
    let _ = folded_report(&first, &mut meters, &SessionReport::HEALTHY);
    let report = folded_report(
        &interface_sample(4 << 20, 1 << 20),
        &mut meters,
        &SessionReport::HEALTHY,
    );
    let eth0 = device(&report, DeviceId::Interface(if_name("eth0")));
    assert_eq!(eth0.trend.len(), 1);
    let instrument = &eth0.hero.instrument;
    assert_eq!(instrument.samples.len(), 1);
    assert_eq!(instrument.opposing.as_ref().map(Vec::len), Some(1));
}

#[test]
fn the_memory_entry_carries_its_own_committed_share_trace() {
    // Its trace is the memory reading's own history, never the CPU's and
    // never an empty vector dressed up as a decision.
    let mut hysteresis = Hysteresis::new();
    let mut meters = RollingMeters::new();
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 0,
            used_permille: 530,
            total_bytes: 16_000_000_000,
        }),
        cpu_busy_permille: Some(180),
        ..permitted()
    };
    for _ in 0..2 {
        let _ = derive_summary(&sample, &mut hysteresis);
        meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
    }
    // Built directly: the meters carry exactly the two samples folded above,
    // and building a report must not add a third point.
    let report = build_resource_report(
        &sample,
        &meters,
        &OwnerBundles::new(),
        &SessionReport::HEALTHY,
        &NoAuthority,
    );
    assert_eq!(
        device(&report, DeviceId::Memory).trend,
        alloc::vec![530, 530]
    );
}

/// Building a report reads the meters and folds nothing, so a trace's
/// horizontal axis is *time* — one slot per sample — and not "reports since I
/// started watching".
///
/// The session's frame report arrives several times a second while the
/// compositor is busy and not at all while it is quiet, and each one rebuilds
/// an open panel. Folding in the builder therefore advanced the display path's
/// trace in bursts and stalled it between them, and dragged the storage and
/// interface traces along on seat and owner-bundle reports too.
#[test]
fn rebuilding_a_report_never_advances_a_trace() {
    let mut meters = RollingMeters::new();
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 0,
            used_permille: 530,
            total_bytes: 16_000_000_000,
        }),
        cpu_busy_permille: Some(180),
        net_facts: Some(alloc::vec![iface("eth0")]),
        gpu_stats: Some(alloc::vec![graphics_device()]),
        elapsed_ns: Some(1_000_000_000),
        ..permitted()
    };
    // One sample, then a handful of rebuilds as reports would drive.
    meters.record(&sample, Hysteresis::new(), &SessionReport::HEALTHY);
    let after_fold = (
        device(
            &build_resource_report(
                &sample,
                &meters,
                &OwnerBundles::new(),
                &SessionReport::HEALTHY,
                &NoAuthority,
            ),
            DeviceId::Graphics,
        )
        .trend
        .len(),
        meters.system.cpu_history().len(),
        meters.system.memory_history().len(),
    );
    for _ in 0..5 {
        let report = build_resource_report(
            &sample,
            &meters,
            &OwnerBundles::new(),
            &SessionReport::HEALTHY,
            &NoAuthority,
        );
        assert_eq!(
            (
                device(&report, DeviceId::Graphics).trend.len(),
                meters.system.cpu_history().len(),
                meters.system.memory_history().len(),
            ),
            after_fold,
            "a rebuild advanced a trace"
        );
    }
}

#[test]
fn the_interface_pane_states_that_per_task_attribution_has_no_interface() {
    let sample = Sample {
        net_facts: Some(alloc::vec![iface("eth0")]),
        ..permitted()
    };
    let report = report_of(&sample);
    let iface = device(&report, DeviceId::Interface(if_name("eth0")));
    // An empty list would read as "none", so the absence is stated in words.
    assert!(iface.blocks.iter().any(|block| matches!(
        &block.body,
        BlockBody::Absence(text) if text.contains("per-process socket accounting")
    )));
}

#[test]
fn the_memory_composition_closes_on_the_whole_it_measures() {
    let sample = Sample {
        kernel_memory: Some(KernelMemoryStats {
            total_bytes: 16_000_000_000,
            free_bytes: 7_400_000_000,
            kernel_heap_bytes: 900_000_000,
            user_resident_bytes: 4_100_000_000,
            page_size: 4_096,
            reserved: 0,
        }),
        ..permitted()
    };
    let report = report_of(&sample);
    let parts = device(&report, DeviceId::Memory)
        .blocks
        .iter()
        .find_map(|block| match &block.body {
            BlockBody::Composition(parts) => Some(parts),
            _ => None,
        })
        .expect("the memory pane must carry its composition");
    // The shares must account for the whole exactly, or the bar would
    // under-report where the memory went.
    let total: u32 = parts.iter().map(|part| u32::from(part.share)).sum();
    assert_eq!(total, 1_000);
    assert!(parts.last().expect("a remainder").remainder);
}

#[test]
fn the_memory_composition_states_its_absence_without_the_kernel_reading() {
    let refused = Sample {
        scopes: ScopeVerdicts {
            memory_pressure: false,
            ..PERMITTED
        },
        ..Sample::default()
    };
    let report = report_of(&refused);
    assert!(device(&report, DeviceId::Memory)
        .blocks
        .iter()
        .any(|block| matches!(
            &block.body,
            BlockBody::Absence(text) if text.contains("not permitted")
        )));
}

#[test]
fn the_graphics_pane_reads_absent_until_the_session_reports_a_frame() {
    let report = report_of(&permitted());
    let graphics = device(&report, DeviceId::Graphics);
    // Only the session that owns the compositor can count a frame, and it
    // has not spoken: an absent reading, never a zero that would read as an
    // idle frame.
    assert_eq!(
        graphics.hero.value,
        Reading::Absent(Unmeasured::Unavailable)
    );
}

#[test]
fn the_graphics_rail_entry_reads_the_frames_damage_not_the_hero_figure() {
    // The board's `Compositor 3.2k px` is the damage; the hero's 4.2 M is the
    // contributions blended to resolve it. Showing the hero's figure in the
    // rail would state one reading twice, two magnitudes apart.
    let mut meters = RollingMeters::new();
    let session = SessionReport {
        frame: Some(frame_report()),
        ..SessionReport::HEALTHY
    };
    let report = folded_report(&permitted(), &mut meters, &session);
    let graphics = device(&report, DeviceId::Graphics);
    assert_eq!(graphics.reading, Reading::measured("3.2k px"));
    // The hero's figure carries no unit of its own — the unit trails it, so a
    // spelled-out one would read "4.2M px M px blended".
    assert_eq!(graphics.hero.value, Reading::measured("4.2"));
    assert_eq!(graphics.hero.unit, "M px blended");
    // And the trace now has a series behind it: the frame's damage as a
    // permille of its own screen.
    assert_eq!(graphics.trend, alloc::vec![1]);
}

#[test]
fn the_graphics_pane_publishes_the_devices_own_capability_and_memory() {
    let sample = Sample {
        hardware: Some(alloc::vec![HwNode::new(
            1,
            HW_NODE_ROOT,
            HwDeviceClass::Display
        )]),
        gpu_stats: Some(alloc::vec![graphics_device()]),
        ..permitted()
    };
    let report = report_of(&sample);
    let graphics = device(&report, DeviceId::Graphics);
    assert_eq!(
        fact(graphics, "Max hardware layers"),
        &Reading::measured("4")
    );
    assert_eq!(
        fact(graphics, "Per-layer opacity"),
        &Reading::measured("yes")
    );
    assert_eq!(
        fact(graphics, "Scan-out"),
        &Reading::measured("1920×1080 · BGRA8888")
    );
    assert_eq!(
        fact(graphics, "Video memory"),
        &Reading::measured("8.0 MiB of 256.0 MiB")
    );
    // A first sample has no interval to divide the cumulative busy time by,
    // so the utilisation is absent rather than a service lifetime's average
    // dressed as this moment.
    assert_eq!(
        fact(graphics, "Device utilisation"),
        &Reading::Absent(Unmeasured::Unavailable)
    );
    // A per-engine split still has no producer, and says so.
    assert_eq!(
        fact(graphics, "Decode / encode engines"),
        &Reading::Absent(Unmeasured::NoInterface)
    );
}

#[test]
fn a_device_with_no_memory_of_its_own_says_so_rather_than_reading_zero() {
    let sample = Sample {
        hardware: Some(alloc::vec![HwNode::new(
            1,
            HW_NODE_ROOT,
            HwDeviceClass::Display
        )]),
        gpu_stats: Some(alloc::vec![DisplayStats {
            seat_id: 0,
            busy_ns: 0,
            idle_ns: 0,
            device: DisplayDeviceReport::SOFTWARE,
            mode: DisplayMode {
                width_px: 800,
                height_px: 600,
                stride_bytes: 3_200,
                format: DisplayFormat::Bgra8888,
            },
        }]),
        ..permitted()
    };
    let report = report_of(&sample);
    let graphics = device(&report, DeviceId::Graphics);
    assert_eq!(
        fact(graphics, "Video memory"),
        &Reading::measured("none of its own · scans out of system RAM")
    );
    assert_eq!(
        fact(graphics, "Accelerated layers"),
        &Reading::measured("none · the device has no hardware compositor")
    );
}

#[test]
fn a_withheld_hardware_scope_marks_the_graphics_device_not_permitted() {
    let refused = Sample {
        hardware: None,
        scopes: ScopeVerdicts {
            global_process_scope: true,
            memory_pressure: true,
            hardware_scope: false,
        },
        ..Sample::default()
    };
    let report = report_of(&refused);
    let graphics = device(&report, DeviceId::Graphics);
    assert_eq!(
        fact(graphics, "Accelerated layers"),
        &Reading::Absent(Unmeasured::NotPermitted)
    );
}

#[test]
fn a_devices_commands_offer_only_what_the_service_can_carry_out() {
    let report = report_of(&permitted());
    let cpu = device(&report, DeviceId::Cpu);
    // "Sort tasks by CPU" is a view transition the surface performs itself;
    // every other command has no endpoint behind it and is drawn disabled
    // rather than marked for authority, since a grant would not conjure one.
    let ready = cpu
        .actions
        .iter()
        .filter(|action| action.verdict == crate::view::ActionVerdict::Ready)
        .count();
    assert_eq!(ready, 1);
    assert!(cpu
        .actions
        .iter()
        .all(|action| action.verdict != crate::view::ActionVerdict::DeniedByAuthority));
}

#[test]
fn the_authority_pane_names_a_withheld_scope_as_not_permitted() {
    let refused = Sample {
        scopes: ScopeVerdicts {
            global_process_scope: false,
            memory_pressure: false,
            hardware_scope: false,
        },
        ..Sample::default()
    };
    let report = report_of(&refused);
    let authority = device(&report, DeviceId::Authority);
    assert_eq!(
        fact(authority, "Kernel readings"),
        &Reading::Absent(Unmeasured::NotPermitted)
    );
}

/// One CPU of `index` reporting `features`, so a heterogeneous fixture can
/// give its cores different ISA sets.
fn cpu_with_features(index: u32, features: CpuFeatureSet) -> CpuInfoRecord {
    CpuInfoRecord::new(
        index,
        CpuCoreClass::Performance,
        0,
        features.bits(),
        0,
        0,
        1_000_000,
        b"Test Core",
    )
    .expect("a valid CPU record")
}

/// What a program may actually rely on is the intersection: a heterogeneous
/// machine schedules a task on whichever core is free, so an extension only
/// some cores implement is one no unpinned program may use.
#[test]
fn the_isa_fact_lists_the_features_every_core_implements() {
    let common = CpuFeatureSet::new()
        .with(CpuFeature::Asimd)
        .with(CpuFeature::Crc32);
    let sample = Sample {
        cpu_info: Some(alloc::vec![
            cpu_with_features(0, common.with(CpuFeature::Sha2)),
            cpu_with_features(1, common),
        ]),
        ..permitted()
    };
    let report = report_of(&sample);
    let cpu = device(&report, DeviceId::Cpu);

    assert_eq!(
        *fact(cpu, "ISA features"),
        Reading::measured(tairix_procinfo::cpu_feature_flags(common.bits())),
        "an extension only one core implements was reported as available"
    );
}

/// A port that reads no ISA features answers zero bits. That reads as
/// unmeasured, never as "this CPU implements none": the second would be a
/// claim about the silicon that nothing measured.
#[test]
fn a_processor_reporting_no_features_states_the_reading_is_unmeasured() {
    let sample = Sample {
        cpu_info: Some(alloc::vec![cpu(0)]),
        ..permitted()
    };
    let report = report_of(&sample);
    let cpu = device(&report, DeviceId::Cpu);

    assert_eq!(
        *fact(cpu, "ISA features"),
        Reading::Absent(Unmeasured::Unavailable)
    );
}

/// With no per-CPU inventory at all the fact states *why* it is absent, from
/// the verdict the sample already reached, rather than restating it.
#[test]
fn the_isa_fact_states_a_refused_inventory_as_refused() {
    let sample = Sample {
        cpu_info: None,
        ..permitted()
    };
    let report = report_of(&sample);
    let cpu = device(&report, DeviceId::Cpu);

    assert!(matches!(*fact(cpu, "ISA features"), Reading::Absent(_)));
}
