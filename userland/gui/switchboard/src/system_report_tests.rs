//! Unit tests for the System screen's report builder: that every fact
//! derives from the sample it claims, that the capacity arithmetic is
//! right, and that an absent reading names the reason the sample resolved.

use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
use tairix_abi::net_ipc::{
    NetAddrFamily, NetAddrState, NetIfAddr, NetIfKind, NetInterfaceFactsRecord,
    NetInterfaceRatesRecord, NetInterfaceStateRecord, IF_NAME_LEN, NET_IF_MAX_ADDRS,
};
use tairix_abi::rlimit::{LimitKind, ResourceLimit, RLIMIT_INFINITY};
use tairix_abi::switchboard_ipc::FrameReport;
use tairix_abi::sysinfo::{
    CpuCoreClass, CpuInfoRecord, LoadAverage, MemoryTotal, MountAvailability, MountRecord,
    MountVolumeState, ResourceLimitRecord, SeatRecord, SystemIdentity, Uptime, SEAT_FLAG_OWNED,
};
use tairix_abi::{Duration64, Time64};

use super::{build_system_report, used_permille, HeadlinePressure};
use crate::sample::{MemoryPressureSample, Sample, ScopeVerdicts};
use crate::test_host::NO_AUTHORITY as NONE;
use crate::view::{HealthSeverity, Reading, TileInstrument, Unmeasured};

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

/// The report for `sample`, with no history, no pressure latched and no
/// frame reported.
fn report_of(sample: &Sample) -> crate::view::SystemReport {
    build_system_report(sample, &[], HeadlinePressure::default(), None, &NONE)
}

/// The Desktop block's facts for `frame`, with nothing else measured.
fn compositor_facts(frame: Option<FrameReport>) -> Vec<crate::view::SystemFact> {
    build_system_report(&permitted(), &[], HeadlinePressure::default(), frame, &NONE).compositor
}

/// The reading of the fact named `label` in `facts`.
fn fact<'a>(facts: &'a [crate::view::SystemFact], label: &str) -> &'a Reading {
    &facts
        .iter()
        .find(|fact| fact.label == label)
        .expect("the page must carry this fact")
        .value
}

/// A mount record with `total`/`avail` blocks of `block` bytes each.
fn mount(target: &str, block: u32, total: u64, avail: u64) -> MountRecord {
    MountRecord::new(
        b"/dev/vda1",
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
        [0u8; 16],
    )
    .expect("a valid mount record")
}

/// An interface name padded into its fixed-width wire field.
fn if_name(name: &str) -> [u8; IF_NAME_LEN] {
    let mut field = [0u8; IF_NAME_LEN];
    for (slot, byte) in field.iter_mut().zip(name.bytes()) {
        *slot = byte;
    }
    field
}

// --- The machine's facts -----------------------------------------------

#[test]
fn the_machine_facts_derive_from_the_identity_the_sample_carries() {
    let sample = Sample {
        identity: Some(
            SystemIdentity::new([0xab; 16], 1, 2, 3, b"tairix").expect("a valid identity"),
        ),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        fact(&report.machine, "Hostname"),
        &Reading::measured("tairix")
    );
    assert_eq!(
        fact(&report.machine, "OS version"),
        &Reading::measured("TAIRiX 1.2.3")
    );
    assert!(
        matches!(fact(&report.machine, "Machine id"), Reading::Measured(id) if id.starts_with("abab")),
        "the machine id is an identifier, spelled in hexadecimal"
    );
}

#[test]
fn the_uptime_fact_derives_from_the_uptime_reading() {
    let sample = Sample {
        uptime: Some(Uptime {
            since_boot: Duration64::from_secs(7_260),
            boot_time: Time64::from_secs(1_700_000_000),
        }),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(fact(&report.machine, "Uptime"), &Reading::measured("2h 1m"));
    assert_eq!(
        fact(&report.machine, "Booted"),
        &Reading::measured("1700000000 s since the epoch")
    );
}

#[test]
fn the_core_count_names_the_performance_split_only_when_there_is_one() {
    let one_class = Sample {
        cpu_info: Some(alloc::vec![
            cpu(0, CpuCoreClass::Performance),
            cpu(1, CpuCoreClass::Performance)
        ]),
        ..permitted()
    };
    assert_eq!(
        fact(&report_of(&one_class).machine, "Cores"),
        &Reading::measured("2")
    );
    let mixed = Sample {
        cpu_info: Some(alloc::vec![
            cpu(0, CpuCoreClass::Performance),
            cpu(1, CpuCoreClass::Efficiency)
        ]),
        ..permitted()
    };
    assert_eq!(
        fact(&report_of(&mixed).machine, "Cores"),
        &Reading::measured("2 (1 performance, 1 efficiency)")
    );
}

/// One CPU inventory record.
fn cpu(index: u32, class: CpuCoreClass) -> CpuInfoRecord {
    CpuInfoRecord::new(index, class, 0, 0, 0, 0, 0, b"Test Core").expect("a valid CPU record")
}

#[test]
fn the_load_average_reads_as_its_fixed_point_value() {
    let sample = Sample {
        load_average: Some(LoadAverage {
            load1: 150,
            load5: 75,
            load15: 0,
            runnable: 3,
            total_tasks: 42,
            users: 2,
        }),
        ..permitted()
    };
    let report = report_of(&sample);
    assert!(
        matches!(fact(&report.machine, "Load average"), Reading::Measured(text) if text.contains('.')),
        "a load average is a decimal, not a raw fixed-point integer"
    );
    assert_eq!(fact(&report.census, "Logged in"), &Reading::measured("2"));
    assert_eq!(
        fact(&report.census, "Runnable tasks"),
        &Reading::measured("3")
    );
    assert_eq!(
        fact(&report.census, "Total tasks"),
        &Reading::measured("42")
    );
}

// --- Storage capacity arithmetic ---------------------------------------

#[test]
fn a_volumes_capacity_comes_from_its_block_counts() {
    // 200 GiB total, 140 GiB available, in 4 KiB blocks.
    let block = 4096u32;
    let total = 200u64 * 1024 * 1024 * 1024 / u64::from(block);
    let avail = 140u64 * 1024 * 1024 * 1024 / u64::from(block);
    let sample = Sample {
        mounts: Some(alloc::vec![mount("System:", block, total, avail)]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        report.volumes[0].capacity,
        Reading::measured("60.0 GiB of 200.0 GiB used")
    );
    assert_eq!(report.volumes[0].mount_point, "System:");
    assert_eq!(report.volumes[0].filesystem, "arxfs");
}

#[test]
fn the_disk_reading_is_the_used_fraction_across_every_measured_volume() {
    let block = 1024u32;
    let sample = Sample {
        mounts: Some(alloc::vec![
            mount("System:", block, 100, 25),
            mount("Backup:", block, 100, 75),
        ]),
        ..permitted()
    };
    let report = report_of(&sample);
    // 200 blocks total, 100 available, so half is used.
    assert_eq!(report.headline[2].value, Reading::measured("50%"));
    assert_eq!(
        report.headline[2].instrument,
        TileInstrument::Track(Some(500))
    );
    assert_eq!(
        report.headline[2].detail,
        Reading::measured("100.0 KiB free")
    );
}

#[test]
fn a_volume_with_no_fixed_capacity_says_so_rather_than_reading_as_full() {
    let sample = Sample {
        mounts: Some(alloc::vec![mount("System:", 4096, 0, 0)]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        report.volumes[0].capacity,
        Reading::measured("no fixed capacity")
    );
    assert_eq!(
        report.headline[2].instrument,
        TileInstrument::Track(None),
        "a table with no measurable volume leaves the tile unmeasured, not at nought"
    );
}

#[test]
fn a_volume_reporting_more_available_than_total_does_not_underflow() {
    // The wire record refuses to carry more available than total, so the
    // guarantee is asserted on the arithmetic a corrupt or future record
    // would still reach.
    assert_eq!(used_permille(10, 1_000), 0);
    assert_eq!(used_permille(0, 0), 0);
    assert_eq!(used_permille(u64::MAX, 0), 1_000);
    assert_eq!(used_permille(200, 100), 500);
}

#[test]
fn a_volume_with_no_health_record_is_absent_rather_than_reported_healthy() {
    let sample = Sample {
        mounts: Some(alloc::vec![mount("System:", 1024, 10, 5)]),
        volume_health: Some(Vec::new()),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        report.volumes[0].health,
        Reading::Absent(Unmeasured::Unavailable),
        "\"no faults measured\" and \"no measurement\" are different statements"
    );
    assert_eq!(report.volumes[0].health_state, HealthSeverity::Healthy);
}

// --- Network -----------------------------------------------------------

#[test]
fn an_interface_joins_its_facts_state_and_rates_by_name() {
    let sample = Sample {
        net_facts: Some(alloc::vec![NetInterfaceFactsRecord {
            name: if_name("eth0"),
            kind: NetIfKind::Ethernet,
            mac: [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01],
            mtu: 1500,
            offloads: 0,
            rx_queues: 1,
        }]),
        net_state: Some(alloc::vec![NetInterfaceStateRecord {
            name: if_name("eth0"),
            link_up: true,
            addr_count: 1,
            addrs: addrs_with(NetIfAddr {
                family: NetAddrFamily::V4,
                prefix: 24,
                state: NetAddrState::Preferred,
                addr: v4(10, 0, 2, 15),
            }),
        }]),
        net_rates: Some(alloc::vec![NetInterfaceRatesRecord {
            name: if_name("eth0"),
            window: Duration64::from_secs(1),
            rx_pps: 0,
            rx_bps: 2048,
            tx_pps: 0,
            tx_bps: 1024,
        }]),
        ..permitted()
    };
    let report = report_of(&sample);
    let iface = &report.interfaces[0];
    assert_eq!(iface.name, "eth0");
    assert_eq!(iface.link, Reading::measured("up"));
    assert_eq!(iface.addresses, alloc::vec!["10.0.2.15/24"]);
    assert_eq!(iface.rx, Reading::measured("2.0 KiB/s"));
    assert_eq!(iface.tx, Reading::measured("1.0 KiB/s"));
    assert_eq!(
        fact(&iface.facts, "Hardware address"),
        &Reading::measured("de:ad:be:ef:00:01")
    );
}

#[test]
fn an_interface_with_no_addresses_reports_an_empty_list_not_an_absence() {
    let sample = Sample {
        net_facts: Some(alloc::vec![NetInterfaceFactsRecord {
            name: if_name("eth1"),
            kind: NetIfKind::Ethernet,
            mac: [0; 6],
            mtu: 1500,
            offloads: 0,
            rx_queues: 1,
        }]),
        net_state: Some(alloc::vec![NetInterfaceStateRecord {
            name: if_name("eth1"),
            link_up: false,
            addr_count: 0,
            addrs: addrs_with(unset_addr()),
        }]),
        ..permitted()
    };
    let report = report_of(&sample);
    let iface = &report.interfaces[0];
    assert!(iface.addresses.is_empty());
    assert_eq!(
        iface.addresses_absent, None,
        "an interface with no address has been measured; it is not an absence"
    );
    assert_eq!(iface.link, Reading::measured("down"));
}

/// The address array with `first` in slot nought.
fn addrs_with(first: NetIfAddr) -> [NetIfAddr; NET_IF_MAX_ADDRS] {
    let mut addrs = [unset_addr(); NET_IF_MAX_ADDRS];
    addrs[0] = first;
    addrs
}

/// An address slot the record's count does not reach.
fn unset_addr() -> NetIfAddr {
    NetIfAddr {
        family: NetAddrFamily::V4,
        prefix: 0,
        state: NetAddrState::Preferred,
        addr: [0u8; 16],
    }
}

/// An IPv4 address in a wire address slot.
fn v4(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
    let mut addr = [0u8; 16];
    addr[0] = a;
    addr[1] = b;
    addr[2] = c;
    addr[3] = d;
    addr
}

// --- Seats and limits --------------------------------------------------

#[test]
fn a_seat_states_its_owner_or_that_it_is_unowned() {
    let sample = Sample {
        seats: Some(alloc::vec![
            SeatRecord {
                seat_id: 0,
                owner_task: 7,
                generation: 1,
                foreground_console: 1,
                flags: SEAT_FLAG_OWNED,
            },
            SeatRecord {
                seat_id: 1,
                owner_task: 0,
                generation: 0,
                foreground_console: 2,
                flags: 0,
            },
        ]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(report.seats[0].owner, Reading::measured("task 7"));
    assert_eq!(report.seats[1].owner, Reading::measured("unowned"));
}

#[test]
fn a_limit_is_spelled_in_the_unit_its_kind_is_denominated_in() {
    let sample = Sample {
        resource_limits: Some(alloc::vec![
            ResourceLimitRecord {
                kind: LimitKind::OpenStreams,
                reserved: 0,
                limit: ResourceLimit {
                    soft: 64,
                    hard: RLIMIT_INFINITY,
                },
                usage: 9,
            },
            ResourceLimitRecord {
                kind: LimitKind::StackBytes,
                reserved: 0,
                limit: ResourceLimit {
                    soft: 8 * 1024 * 1024,
                    hard: 8 * 1024 * 1024,
                },
                usage: 4096,
            },
        ]),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(report.limits[0].soft, "64");
    assert_eq!(
        report.limits[0].hard, "unlimited",
        "the unbounded sentinel is spelled out, not shown as a huge number"
    );
    assert_eq!(report.limits[1].soft, "8.0 MiB");
    assert_eq!(report.limits[1].usage, Reading::measured("4.0 KiB"));
}

// --- Honest absence ----------------------------------------------------

#[test]
fn a_reading_outside_the_ceiling_is_not_permitted() {
    let report = report_of(&Sample::default());
    assert_eq!(
        report.headline[1].value,
        Reading::Absent(Unmeasured::NotPermitted),
        "the memory gauge needs a capability this sample's ceiling withholds"
    );
    assert_eq!(
        fact(&report.memory, "Kernel heap"),
        &Reading::Absent(Unmeasured::NotPermitted)
    );
}

#[test]
fn a_permitted_reading_the_service_could_not_answer_is_unavailable() {
    let report = report_of(&permitted());
    assert_eq!(
        report.headline[1].value,
        Reading::Absent(Unmeasured::Unavailable),
        "with the capability granted, a missing figure is a failure to answer"
    );
    assert_eq!(
        fact(&report.machine, "Hostname"),
        &Reading::Absent(Unmeasured::Unavailable)
    );
}

#[test]
fn an_absent_list_carries_the_reason_the_sample_resolved() {
    let denied = report_of(&Sample::default());
    assert_eq!(denied.seats_absent, Some(Unmeasured::NotPermitted));
    assert_eq!(
        denied.volumes_absent,
        Some(Unmeasured::Unavailable),
        "the mount table needs no optional scope, so its absence is a fault"
    );
    let permitted = report_of(&permitted());
    assert_eq!(permitted.seats_absent, Some(Unmeasured::Unavailable));
}

#[test]
fn a_present_list_carries_no_absence_at_all() {
    let sample = Sample {
        mounts: Some(Vec::new()),
        seats: Some(Vec::new()),
        net_facts: Some(Vec::new()),
        resource_limits: Some(Vec::new()),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(report.volumes_absent, None);
    assert_eq!(report.seats_absent, None);
    assert_eq!(report.interfaces_absent, None);
    assert_eq!(report.limits_absent, None);
}

// --- The rail ----------------------------------------------------------

#[test]
fn every_system_action_is_refused_for_want_of_an_interface() {
    let report = report_of(&permitted());
    assert_eq!(report.actions.len(), 4);
    for action in &report.actions {
        assert!(!action.allowed, "{} must not look available", action.label);
        assert_eq!(
            action.refusal,
            Some(Unmeasured::NoInterface),
            "no power, lock, or session endpoint exists for this service to drive"
        );
    }
}

// --- The header --------------------------------------------------------

#[test]
fn the_header_carries_the_latches_the_service_reached() {
    let sample = Sample {
        cpu_busy_permille: Some(900),
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 950,
            total_bytes: 0,
        }),
        ..permitted()
    };
    let report = build_system_report(
        &sample,
        &[900],
        HeadlinePressure {
            cpu: true,
            memory: false,
        },
        None,
        &NONE,
    );
    assert!(report.headline[0].pressured);
    assert!(!report.headline[1].pressured);
    assert!(
        !report.headline[2].pressured && !report.headline[3].pressured,
        "no disk or network strain latch exists, so neither claims one"
    );
}

#[test]
fn the_memory_detail_states_the_used_and_installed_totals() {
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 0,
            used_permille: 500,
            total_bytes: 0,
        }),
        memory_total: Some(MemoryTotal {
            total_bytes: 16 * 1024 * 1024 * 1024,
        }),
        ..permitted()
    };
    let report = report_of(&sample);
    assert_eq!(
        report.headline[1].detail,
        Reading::measured("8.0 GiB of 16.0 GiB")
    );
}

/// A frame that changed 3 200 of a 1920x1080 screen's pixels and blended
/// 42 000 layer contributions to do it.
const OVERDRAWN: FrameReport = FrameReport {
    screen_px: 1920 * 1080,
    damaged_px: 3_200,
    blended_px: 42_000,
    opaque_px: 1_100,
    dirty_rects: 3,
    present_calls: 1,
    chrome_hits: 12,
    chrome_misses: 1,
};

#[test]
fn a_reported_frame_states_the_damage_against_the_screen_and_the_overdraw() {
    let facts = compositor_facts(Some(OVERDRAWN));
    assert_eq!(
        fact(&facts, "Last frame"),
        &Reading::measured("3.2k px of 2.0M px recomposed")
    );
    assert_eq!(
        fact(&facts, "Blended"),
        &Reading::measured("42.0k px, 13.1x damaged"),
        "the blend against the damage is the reading this block exists for"
    );
    assert_eq!(fact(&facts, "Opaque copies"), &Reading::measured("1.1k px"));
    assert_eq!(fact(&facts, "Rectangles"), &Reading::measured("3"));
    assert_eq!(fact(&facts, "Present calls"), &Reading::measured("1"));
    assert_eq!(
        fact(&facts, "Window furniture"),
        &Reading::measured("12 cached, 1 rendered")
    );
}

#[test]
fn an_idle_frame_reads_idle_rather_than_a_row_of_zeros() {
    let idle = FrameReport {
        screen_px: 1920 * 1080,
        damaged_px: 0,
        blended_px: 0,
        opaque_px: 0,
        dirty_rects: 0,
        present_calls: 0,
        chrome_hits: 0,
        chrome_misses: 0,
    };
    let facts = compositor_facts(Some(idle));
    assert_eq!(
        facts.len(),
        1,
        "a frame that recomposed nothing must not lay out a row of zeros"
    );
    assert_eq!(
        fact(&facts, "Last frame"),
        &Reading::measured("idle, nothing recomposed")
    );
}

#[test]
fn an_unreported_frame_is_absent_rather_than_nought() {
    let facts = compositor_facts(None);
    assert_eq!(facts.len(), 1);
    assert_eq!(
        fact(&facts, "Last frame"),
        &Reading::Absent(Unmeasured::Unavailable),
        "only the desktop can count this, and it has not reported yet"
    );
}
