//! Unit tests for the Resources report builder: that the rail's length is
//! discovered rather than declared, that every reading derives from the
//! sample it claims, and that an absent one names the reason the sample
//! itself resolved.

use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
use tairix_abi::net_ipc::{NetIfKind, NetInterfaceFactsRecord, IF_NAME_LEN};
use tairix_abi::sysinfo::{
    CpuCoreClass, CpuInfoRecord, KernelMemoryStats, MountAvailability, MountRecord,
    MountVolumeState, MOUNT_VOLUME_ID_LEN,
};
use tairix_abi::{CapabilityId, CapabilityQuery};

use super::{build_resource_report, used_permille};
use crate::model::{RollingMeters, SessionReport};
use crate::sample::{CoreBusy, MemoryPressureSample, Sample, ScopeVerdicts};
use crate::view::resources::{BlockBody, DeviceGroup, DeviceId, HeroInstrument};
use crate::view::{Reading, ReadingFact, ResourceDevice, ResourceReport, Unmeasured};

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
    build_resource_report(sample, &mut meters, &SessionReport::HEALTHY, &NoAuthority)
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

/// A mount record with `total`/`avail` blocks of `block` bytes each.
fn mount(target: &str, block: u32, total: u64, avail: u64) -> MountRecord {
    MountRecord::new(
        b"nvme0",
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
        [7; MOUNT_VOLUME_ID_LEN],
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
        .any(|device| device.group == DeviceGroup::Storage));
    assert!(!report
        .devices
        .iter()
        .any(|device| device.group == DeviceGroup::Network));
}

#[test]
fn the_rail_grows_one_entry_per_discovered_volume_and_interface() {
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount("System:", 4_096, 100, 40),
            mount("Backup:", 4_096, 200, 10),
        ]),
        net_facts: Some(alloc::vec![iface("eth0"), iface("eth1"), iface("lo")]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        report
            .devices
            .iter()
            .filter(|d| d.group == DeviceGroup::Storage)
            .count(),
        2
    );
    assert_eq!(
        report
            .devices
            .iter()
            .filter(|d| d.group == DeviceGroup::Network)
            .count(),
        3
    );
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
        assert!(matches!(machine.hero.instrument, HeroInstrument::None));
    }
}

#[test]
fn the_cpu_pane_leads_with_a_trend_and_the_memory_pane_with_a_track() {
    let sample = Sample {
        cpu_busy_permille: Some(180),
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 530,
            total_bytes: 16 * 1024 * 1024 * 1024,
        }),
        ..permitted()
    };
    let report = report_of(&sample);
    // The choice belongs to the reading: a rate has no ceiling to fill a bar
    // against, and a fraction of a measured whole has nothing to trend.
    assert!(matches!(
        device(&report, DeviceId::Cpu).hero.instrument,
        HeroInstrument::Trend { .. }
    ));
    assert!(matches!(
        device(&report, DeviceId::Memory).hero.instrument,
        HeroInstrument::Track(Some(530))
    ));
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
fn a_volumes_capacity_comes_from_its_block_counts() {
    let sample = Sample {
        mounts: Some(alloc::vec![mount("System:", 4_096, 100, 40)]),
        ..permitted()
    };
    let report = report_of(&sample);
    let volume = device(&report, DeviceId::Volume([7; MOUNT_VOLUME_ID_LEN]));
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

#[test]
fn a_volumes_service_block_states_the_absent_interface_in_every_row() {
    let sample = Sample {
        mounts: Some(alloc::vec![mount("System:", 4_096, 100, 40)]),
        ..permitted()
    };
    let report = report_of(&sample);
    let volume = device(&report, DeviceId::Volume([7; MOUNT_VOLUME_ID_LEN]));
    for label in [
        "Utilisation",
        "Queue depth",
        "Await, read",
        "In-flight requests",
    ] {
        assert_eq!(
            fact(volume, label),
            &Reading::Absent(Unmeasured::NoInterface),
            "{label} needs a per-volume I/O statistics query, and says so"
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
