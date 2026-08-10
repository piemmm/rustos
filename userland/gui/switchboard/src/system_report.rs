//! Turn one [`Sample`] into the [`SystemReport`] the System screen draws.
//!
//! Every figure here comes from a reading the sampler actually took. Where
//! a reading is missing the report carries [`Reading::Absent`] with the
//! reason the sample itself resolved, so the screen states "not permitted"
//! where this session's authority stops and "unavailable" where the query
//! was permitted but unanswered. Nothing is inferred, defaulted, or
//! rounded up into a plausible number.

use core::fmt::Write;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::net_ipc::{NetAddrFamily, NetAddrState, NetIfAddr, NetIfKind};
use tairix_abi::rlimit::{LimitKind, RLIMIT_INFINITY};
use tairix_abi::switchboard_ipc::FrameReport;
use tairix_abi::sysinfo::{
    CpuCoreClass, LoadAverage, MountAvailability, MountRecord, VolumeIoHealthRecord,
};
use tairix_abi::{CapabilityId, CapabilityQuery};
use tairix_controls::{ControlRole, PressureKind};

use crate::format::{format_bytes, format_duration, format_pixels, format_rate, percent};
use crate::model::display_name;
use crate::sample::{DegradedField, Sample};
use crate::view::{
    HeadlineTile, HealthSeverity, LimitRow, NetworkInterface, Reading, SessionSeat, StorageVolume,
    SystemAction, SystemFact, SystemReport, TileInstrument, Unmeasured,
};

/// The pressure latches the service has already reached for the two
/// resources it measures strain on.
///
/// Passed in rather than re-derived so the header, the tray icon and the
/// Pressure section can never disagree about whether a resource is under
/// pressure.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct HeadlinePressure {
    /// Whether the CPU latch is under.
    pub cpu: bool,
    /// Whether the memory latch is under.
    pub memory: bool,
}

/// Build the System screen's whole report from this sample and the
/// caller's authority.
///
/// `cpu_history` is the rolling CPU series the header's trend plots and
/// `pressure` the latches the service has already reached; the caller owns
/// both because no single sample carries a history or a latch. `frame` is
/// what the desktop session last reported its composited frame cost, which
/// no kernel query can answer.
#[must_use]
pub fn build_system_report(
    sample: &Sample,
    cpu_history: &[u16],
    pressure: HeadlinePressure,
    frame: Option<FrameReport>,
    authority: &dyn CapabilityQuery,
) -> SystemReport {
    SystemReport {
        headline: headline(sample, cpu_history, pressure),
        machine: machine_facts(sample),
        authority: authority_facts(sample, authority),
        cores: core_facts(sample),
        memory: memory_facts(sample),
        compositor: compositor_facts(frame),
        volumes: storage_volumes(sample),
        volumes_absent: absent_unless(sample, DegradedField::Mounts, sample.mounts.is_some()),
        interfaces: network_interfaces(sample),
        interfaces_absent: absent_unless(
            sample,
            DegradedField::NetInterfaceFacts,
            sample.net_facts.is_some(),
        ),
        seats: session_seats(sample),
        seats_absent: absent_unless(sample, DegradedField::Seats, sample.seats.is_some()),
        census: census_facts(sample),
        limits: limit_rows(sample),
        limits_absent: absent_unless(
            sample,
            DegradedField::ResourceLimits,
            sample.resource_limits.is_some(),
        ),
        actions: system_actions(),
    }
}

/// Why `field` is missing, or [`None`] when `present` says it is not.
///
/// One place decides the shape of "absent, and here is why", so no page
/// can invent a different vocabulary for the same condition.
fn absent_unless(sample: &Sample, field: DegradedField, present: bool) -> Option<Unmeasured> {
    (!present).then(|| Unmeasured::from_absence(sample.absence(field)))
}

/// A reading built from an optional measurement, falling back to the
/// sample's own explanation for `field` when there is none.
///
/// The one place an absent measurement becomes an absent reading, so every
/// figure the product shows — a header tile, a page fact, a fault's age, a
/// pressure cause's amount, an activity's combined total — explains itself
/// with the verdict the service already reached, and no screen can invent a
/// second opinion about why a reading is missing.
pub(crate) fn reading<T>(
    sample: &Sample,
    field: DegradedField,
    value: Option<T>,
    text: impl Fn(T) -> String,
) -> Reading {
    value.map_or_else(
        || Reading::Absent(Unmeasured::from_absence(sample.absence(field))),
        |value| Reading::measured(text(value)),
    )
}

/// The four header readings, in the fixed order a reader learns once:
/// CPU, Memory, Disk, Network.
///
/// CPU and Network trend because their shape over time is the reading;
/// Memory and Disk track because each is a fraction of a fixed whole.
fn headline(sample: &Sample, cpu_history: &[u16], pressure: HeadlinePressure) -> Vec<HeadlineTile> {
    alloc::vec![
        cpu_tile(sample, cpu_history, pressure.cpu),
        memory_tile(sample, pressure.memory),
        disk_tile(sample),
        network_tile(sample),
    ]
}

/// The CPU tile: the aggregate busy fraction, trending over the recent
/// history, detailed by the core count and model the inventory reports.
fn cpu_tile(sample: &Sample, cpu_history: &[u16], pressured: bool) -> HeadlineTile {
    let detail = sample.cpu_info.as_ref().map_or_else(
        || {
            Reading::Absent(Unmeasured::from_absence(
                sample.absence(DegradedField::CpuInfo),
            ))
        },
        |cpus| {
            let model = cpus
                .first()
                .map(|cpu| display_name(cpu.model_bytes()))
                .filter(|model| !model.is_empty());
            Reading::measured(match model {
                Some(model) => format!("{} x {model}", cpus.len()),
                None => format!("{} cores", cpus.len()),
            })
        },
    );
    HeadlineTile {
        name: String::from("CPU"),
        value: reading(
            sample,
            DegradedField::CpuTime,
            sample.cpu_busy_permille,
            percent,
        ),
        unit: String::new(),
        detail,
        kind: PressureKind::Cpu,
        pressured,
        instrument: TileInstrument::Trend(cpu_history.to_vec()),
    }
}

/// The Memory tile: the used fraction as a track, detailed by the used
/// and installed byte totals when both are known.
fn memory_tile(sample: &Sample, pressured: bool) -> HeadlineTile {
    let used_permille = sample.memory_pressure.map(|memory| memory.used_permille);
    let detail = match (used_permille, sample.memory_total) {
        (Some(permille), Some(total)) => {
            let used = total
                .total_bytes
                .saturating_mul(u64::from(permille))
                .saturating_div(1000);
            Reading::measured(format!(
                "{} of {}",
                format_bytes(used),
                format_bytes(total.total_bytes)
            ))
        }
        (_, Some(total)) => {
            Reading::measured(format!("{} installed", format_bytes(total.total_bytes)))
        }
        (_, None) => Reading::Absent(Unmeasured::from_absence(
            sample.absence(DegradedField::MemoryTotal),
        )),
    };
    HeadlineTile {
        name: String::from("Memory"),
        value: reading(
            sample,
            DegradedField::MemoryPressure,
            used_permille,
            percent,
        ),
        unit: String::new(),
        detail,
        kind: PressureKind::Memory,
        pressured,
        instrument: TileInstrument::Track(used_permille),
    }
}

/// The Disk tile: the used fraction of every mounted volume that reports a
/// capacity, tracked together, detailed by the free space left across
/// them.
///
/// A volume whose format tracks no fixed capacity (`total_blocks == 0`)
/// contributes nothing rather than a fabricated zero, and a table with no
/// such volume at all leaves the tile honestly unmeasured.
fn disk_tile(sample: &Sample) -> HeadlineTile {
    let totals = sample.mounts.as_ref().map(|mounts| {
        mounts.iter().filter_map(volume_bytes).fold(
            (0u64, 0u64),
            |(total, avail), (mount_total, mount_avail)| {
                (
                    total.saturating_add(mount_total),
                    avail.saturating_add(mount_avail),
                )
            },
        )
    });
    let measured = totals.filter(|(total, _)| *total > 0);
    let permille = measured.map(|(total, avail)| used_permille(total, avail));
    let detail = match measured {
        Some((_, avail)) => Reading::measured(format!("{} free", format_bytes(avail))),
        None => Reading::Absent(Unmeasured::from_absence(
            sample.absence(DegradedField::Mounts),
        )),
    };
    HeadlineTile {
        name: String::from("Disk"),
        value: permille.map_or_else(
            || {
                Reading::Absent(Unmeasured::from_absence(
                    sample.absence(DegradedField::Mounts),
                ))
            },
            |permille| Reading::measured(percent(permille)),
        ),
        unit: String::new(),
        detail,
        kind: PressureKind::Disk,
        // No disk-pressure latch exists: the service measures capacity,
        // not strain, so the tile claims none rather than guessing one.
        pressured: false,
        instrument: TileInstrument::Track(permille),
    }
}

/// The Network tile: the summed receive and transmit throughput across
/// every interface that reports a rate.
fn network_tile(sample: &Sample) -> HeadlineTile {
    let summed = sample.net_rates.as_ref().map(|rates| {
        rates.iter().fold((0u64, 0u64), |(rx, tx), rate| {
            (
                rx.saturating_add(rate.rx_bps),
                tx.saturating_add(rate.tx_bps),
            )
        })
    });
    HeadlineTile {
        name: String::from("Network"),
        value: reading(
            sample,
            DegradedField::NetInterfaceRates,
            summed.map(|(rx, _)| rx),
            format_rate,
        ),
        unit: String::from("in"),
        detail: reading(
            sample,
            DegradedField::NetInterfaceRates,
            summed.map(|(_, tx)| tx),
            |tx| format!("{} out", format_rate(tx)),
        ),
        kind: PressureKind::Network,
        // No network-pressure latch exists either, for the same reason.
        pressured: false,
        // Throughput has no fixed ceiling to fill a bar against, so the
        // tile plots the shape of the traffic instead of a fraction it
        // would have to invent a denominator for.
        instrument: TileInstrument::Trend(Vec::new()),
    }
}

/// A volume's total and available capacity in bytes, or [`None`] when the
/// format tracks no fixed capacity.
fn volume_bytes(mount: &MountRecord) -> Option<(u64, u64)> {
    let stats = mount.usage();
    (stats.total_blocks > 0).then(|| {
        let block = u64::from(stats.block_size);
        (
            stats.total_blocks.saturating_mul(block),
            stats.avail_blocks.saturating_mul(block),
        )
    })
}

/// The used fraction of `total` given `avail` free, in permille.
///
/// Saturating throughout: a service reporting more available than total
/// yields nought used rather than an underflow. The scaling is done in
/// [`u128`] because a volume of the size TAIRiX must serve overflows a
/// [`u64`] once multiplied by a thousand, and a saturated numerator would
/// under-report a full disk as very nearly empty.
fn used_permille(total: u64, avail: u64) -> u16 {
    if total == 0 {
        return 0;
    }
    let used = u128::from(total.saturating_sub(avail));
    let permille = used.saturating_mul(1000) / u128::from(total);
    u16::try_from(permille).unwrap_or(1000).min(1000)
}

/// The machine's identity facts, in the order the Overview page reads
/// them.
fn machine_facts(sample: &Sample) -> Vec<SystemFact> {
    let identity = sample.identity.as_ref();
    alloc::vec![
        SystemFact::new(
            "Hostname",
            reading(sample, DegradedField::Identity, identity, |id| {
                display_name(id.hostname_bytes())
            }),
        ),
        SystemFact::new(
            "OS version",
            reading(sample, DegradedField::Identity, identity, |id| {
                format!(
                    "TAIRiX {}.{}.{}",
                    id.version_major, id.version_minor, id.version_patch
                )
            }),
        ),
        SystemFact::new(
            "Machine id",
            reading(sample, DegradedField::Identity, identity, |id| {
                hex(&id.machine_id)
            }),
        ),
        SystemFact::new(
            "Uptime",
            reading(sample, DegradedField::Uptime, sample.uptime, |uptime| {
                format_duration(uptime.since_boot)
            }),
        ),
        SystemFact::new(
            "Booted",
            reading(sample, DegradedField::Uptime, sample.uptime, |uptime| {
                format!("{} s since the epoch", uptime.boot_time.secs())
            }),
        ),
        SystemFact::new(
            "Processor",
            reading(
                sample,
                DegradedField::CpuInfo,
                sample.cpu_info.as_ref(),
                |cpus| {
                    cpus.first()
                        .map(|cpu| display_name(cpu.model_bytes()))
                        .filter(|model| !model.is_empty())
                        .unwrap_or_else(|| String::from("unnamed"))
                },
            ),
        ),
        SystemFact::new(
            "Cores",
            reading(
                sample,
                DegradedField::CpuInfo,
                sample.cpu_info.as_ref(),
                |cpus| core_census(cpus.iter().map(|cpu| cpu.class)),
            ),
        ),
        SystemFact::new(
            "Load average",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| {
                    format!(
                        "{} {} {}",
                        fixed(load.load1),
                        fixed(load.load5),
                        fixed(load.load15)
                    )
                },
            ),
        ),
        SystemFact::new(
            "Installed memory",
            reading(
                sample,
                DegradedField::MemoryTotal,
                sample.memory_total,
                |total| format_bytes(total.total_bytes),
            ),
        ),
    ]
}

/// A core inventory as text: the total, and the performance/efficiency
/// split where the machine reports one.
fn core_census(classes: impl Iterator<Item = CpuCoreClass>) -> String {
    let mut total = 0usize;
    let mut efficiency = 0usize;
    for class in classes {
        total = total.saturating_add(1);
        if class == CpuCoreClass::Efficiency {
            efficiency = efficiency.saturating_add(1);
        }
    }
    if efficiency == 0 {
        return format!("{total}");
    }
    format!(
        "{total} ({} performance, {efficiency} efficiency)",
        total.saturating_sub(efficiency)
    )
}

/// A load average's fixed-point value as decimal text.
fn fixed(value: u32) -> String {
    format!(
        "{}.{:02}",
        LoadAverage::whole(value),
        LoadAverage::centis(value)
    )
}

/// A byte string as lower-case hexadecimal — the machine id's own
/// spelling, which is an identifier rather than text.
///
/// A write into a growable string cannot fail, and the identifier is
/// display text rather than a decision, so a short spelling is preferable
/// to refusing to name the machine at all.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What the service can attest about this session's authority: the
/// capabilities it holds, and the optional reading scopes those resolved
/// to.
fn authority_facts(sample: &Sample, authority: &dyn CapabilityQuery) -> Vec<SystemFact> {
    alloc::vec![
        SystemFact::new(
            "Process control",
            held(authority.holds(CapabilityId::PROC_CONTROL)),
        ),
        SystemFact::new(
            "System-wide readings",
            granted(sample.scopes.global_process_scope),
        ),
        SystemFact::new("Kernel readings", granted(sample.scopes.memory_pressure)),
        SystemFact::new("Hardware inventory", granted(sample.scopes.hardware_scope)),
    ]
}

/// A capability verdict as a reading: held, or explicitly not permitted.
fn held(holds: bool) -> Reading {
    if holds {
        Reading::measured("held")
    } else {
        Reading::Absent(Unmeasured::NotPermitted)
    }
}

/// A reading scope's verdict, in the same shape as [`held`] so the page
/// reads uniformly.
fn granted(granted: bool) -> Reading {
    if granted {
        Reading::measured("granted")
    } else {
        Reading::Absent(Unmeasured::NotPermitted)
    }
}

/// Per-core facts: each core's class and measured frequency, and the
/// scheduler load beside it where the kernel scope permits reading it.
fn core_facts(sample: &Sample) -> Vec<SystemFact> {
    let Some(cpus) = sample.cpu_info.as_ref() else {
        return alloc::vec![SystemFact::new(
            "Cores",
            Reading::Absent(Unmeasured::from_absence(
                sample.absence(DegradedField::CpuInfo)
            )),
        )];
    };
    cpus.iter()
        .map(|cpu| {
            let class = match cpu.class {
                CpuCoreClass::Performance => "performance",
                CpuCoreClass::Efficiency => "efficiency",
            };
            let freq = if cpu.freq_measured() && cpu.current_freq_hz > 0 {
                format!(", {} MHz", cpu.current_freq_hz.saturating_div(1_000_000))
            } else {
                String::new()
            };
            let queue = sample
                .cpu_load
                .as_ref()
                .and_then(|loads| loads.iter().find(|load| load.cpu == cpu.cpu))
                .map(|load| format!(", {} queued", load.queue_depth));
            SystemFact::new(
                format!("Core {}", cpu.cpu),
                Reading::measured(match queue {
                    Some(queue) => format!("{class}{freq}{queue}"),
                    None => format!("{class}{freq}"),
                }),
            )
        })
        .collect()
}

/// The memory detail the Resources page carries: the installed total, the
/// pressure gauge, and the kernel's own accounting where permitted.
fn memory_facts(sample: &Sample) -> Vec<SystemFact> {
    let kernel = sample.kernel_memory.as_ref();
    alloc::vec![
        SystemFact::new(
            "Installed",
            reading(
                sample,
                DegradedField::MemoryTotal,
                sample.memory_total,
                |total| format_bytes(total.total_bytes),
            ),
        ),
        SystemFact::new(
            "In use",
            reading(
                sample,
                DegradedField::MemoryPressure,
                sample.memory_pressure,
                |memory| percent(memory.used_permille),
            ),
        ),
        SystemFact::new(
            "Kernel free",
            reading(sample, DegradedField::KernelMemory, kernel, |stats| {
                format_bytes(stats.free_bytes)
            }),
        ),
        SystemFact::new(
            "Kernel heap",
            reading(sample, DegradedField::KernelMemory, kernel, |stats| {
                format_bytes(stats.kernel_heap_bytes)
            }),
        ),
        SystemFact::new(
            "User resident",
            reading(sample, DegradedField::KernelMemory, kernel, |stats| {
                format_bytes(stats.user_resident_bytes)
            }),
        ),
        SystemFact::new(
            "Page size",
            reading(sample, DegradedField::KernelMemory, kernel, |stats| {
                format_bytes(u64::from(stats.page_size))
            }),
        ),
    ]
}

/// What the desktop's last composited frame cost, as the Resources page
/// states it.
///
/// The first row is the reading that matters: the pixels the frame
/// recomposed against the whole screen, with what resolving them blended
/// directly beneath. A frame that changes a few thousand pixels and blends
/// millions is paying for depth nobody can see, which is what turns "the
/// desktop feels slow" into a figure.
///
/// A session that has reported no frame is honestly absent rather than
/// zero: only the desktop can count this, and it has not spoken yet. A
/// frame that recomposed nothing says so in one line instead of laying out
/// a row of zeros as though a frame had been drawn.
fn compositor_facts(frame: Option<FrameReport>) -> Vec<SystemFact> {
    let Some(frame) = frame else {
        return alloc::vec![SystemFact::new(
            "Last frame",
            Reading::Absent(Unmeasured::Unavailable),
        )];
    };
    if frame.is_idle() {
        return alloc::vec![SystemFact::new(
            "Last frame",
            Reading::measured("idle, nothing recomposed"),
        )];
    }
    alloc::vec![
        SystemFact::new(
            "Last frame",
            Reading::measured(format!(
                "{} of {} recomposed",
                format_pixels(frame.damaged_px),
                format_pixels(frame.screen_px)
            )),
        ),
        SystemFact::new("Blended", Reading::measured(blend_text(&frame))),
        SystemFact::new(
            "Opaque copies",
            Reading::measured(format_pixels(frame.opaque_px)),
        ),
        SystemFact::new(
            "Rectangles",
            Reading::measured(frame.dirty_rects.to_string()),
        ),
        SystemFact::new(
            "Present calls",
            Reading::measured(frame.present_calls.to_string()),
        ),
        SystemFact::new(
            "Window furniture",
            Reading::measured(format!(
                "{} cached, {} rendered",
                frame.chrome_hits, frame.chrome_misses
            )),
        ),
    ]
}

/// The blended-contribution row: the count, and how many times over the
/// damage it is.
///
/// The multiplier is the legible form of overdraw — thirteen layer
/// contributions for every pixel that changed — and it is derived here from
/// the two counts beside it rather than sent, so the row can never disagree
/// with them. A frame with no damage to divide by shows the count alone.
fn blend_text(frame: &FrameReport) -> String {
    let pixels = format_pixels(frame.blended_px);
    match frame
        .blended_px
        .saturating_mul(10)
        .checked_div(frame.damaged_px)
    {
        Some(tenths) => format!("{pixels}, {}.{}x damaged", tenths / 10, tenths % 10),
        None => pixels,
    }
}

/// One [`StorageVolume`] per mounted volume, each carrying its real
/// capacity and the I/O health measured against it.
fn storage_volumes(sample: &Sample) -> Vec<StorageVolume> {
    let Some(mounts) = sample.mounts.as_ref() else {
        return Vec::new();
    };
    mounts
        .iter()
        .map(|mount| {
            let capacity = volume_bytes(mount).map_or_else(
                || Reading::measured("no fixed capacity"),
                |(total, avail)| {
                    Reading::measured(format!(
                        "{} of {} used",
                        format_bytes(total.saturating_sub(avail)),
                        format_bytes(total)
                    ))
                },
            );
            let health = volume_health(sample, &mount.volume_id());
            StorageVolume {
                source: display_name(mount.source_bytes()),
                mount_point: display_name(mount.target_bytes()),
                filesystem: display_name(mount.fstype_bytes()),
                medium: String::from(medium_name(mount.medium())),
                availability: String::from(availability_name(mount.availability())),
                capacity,
                health: health.0,
                health_state: health.1,
            }
        })
        .collect()
}

/// A volume's I/O health as a reading and the severity to draw it at.
///
/// A volume with no health record of its own is honestly absent rather
/// than reported healthy: "no faults measured" and "no measurement" are
/// different statements about a disk that may be dying.
fn volume_health(sample: &Sample, volume_id: &[u8; 16]) -> (Reading, HealthSeverity) {
    let Some(records) = sample.volume_health.as_ref() else {
        return (
            Reading::Absent(Unmeasured::from_absence(
                sample.absence(DegradedField::VolumeHealth),
            )),
            HealthSeverity::Healthy,
        );
    };
    let Some(record) = records
        .iter()
        .find(|record| &record.volume_id() == volume_id)
    else {
        return (
            Reading::Absent(Unmeasured::Unavailable),
            HealthSeverity::Healthy,
        );
    };
    (
        Reading::measured(health_text(record)),
        health_state(record.availability()),
    )
}

/// A volume's fault tallies as one line, naming only the buckets that
/// actually recorded something so a healthy disk reads as healthy rather
/// than as a wall of zeroes.
fn health_text(record: &VolumeIoHealthRecord) -> String {
    let counters = record.counters();
    let mut faults = Vec::new();
    for (label, count) in [
        ("timeouts", counters.timeouts),
        ("resets", counters.resets),
        ("medium errors", counters.medium_errors),
        ("offline", counters.offline),
        ("faults", counters.faults),
        ("degraded", counters.degraded),
    ] {
        if count > 0 {
            faults.push(format!("{count} {label}"));
        }
    }
    if faults.is_empty() {
        return format!("{} completions, no faults", counters.completions);
    }
    faults.join(", ")
}

/// The severity a volume's availability implies, so a failing disk is
/// drawn as a fault rather than as one more grey line.
const fn health_state(availability: MountAvailability) -> HealthSeverity {
    match availability {
        MountAvailability::Available => HealthSeverity::Healthy,
        MountAvailability::Degraded | MountAvailability::Recovering => HealthSeverity::Degraded,
        MountAvailability::UnavailableDirty
        | MountAvailability::UnavailableLost
        | MountAvailability::RecoveryConflict => HealthSeverity::Failing,
    }
}

/// A mount's availability in the words the mount table itself uses.
const fn availability_name(availability: MountAvailability) -> &'static str {
    match availability {
        MountAvailability::Available => "available",
        MountAvailability::UnavailableDirty => "unavailable (dirty)",
        MountAvailability::UnavailableLost => "unavailable (lost)",
        MountAvailability::RecoveryConflict => "recovery conflict",
        MountAvailability::Degraded => "degraded",
        MountAvailability::Recovering => "recovering",
    }
}

/// The medium a volume lives on, or that the mount table did not classify
/// it.
const fn medium_name(medium: Option<BlkDeviceClass>) -> &'static str {
    match medium {
        Some(BlkDeviceClass::Rotational) => "rotational",
        Some(BlkDeviceClass::SolidState) => "solid state",
        Some(BlkDeviceClass::Removable) => "removable",
        Some(BlkDeviceClass::Virtual) => "virtual",
        None => "unclassified",
    }
}

/// One [`NetworkInterface`] per interface the inventory names, joined to
/// its live state and throughput by name.
fn network_interfaces(sample: &Sample) -> Vec<NetworkInterface> {
    let Some(facts) = sample.net_facts.as_ref() else {
        return Vec::new();
    };
    facts
        .iter()
        .map(|iface| {
            let name = display_name(trim_nul(&iface.name));
            let state = sample.net_state.as_ref().and_then(|states| {
                states
                    .iter()
                    .find(|s| trim_nul(&s.name) == trim_nul(&iface.name))
            });
            let rate = sample.net_rates.as_ref().and_then(|rates| {
                rates
                    .iter()
                    .find(|r| trim_nul(&r.name) == trim_nul(&iface.name))
            });
            let addresses = state.map_or_else(Vec::new, |state| {
                state
                    .addrs
                    .iter()
                    .take(usize::from(state.addr_count).min(state.addrs.len()))
                    .map(format_addr)
                    .collect()
            });
            NetworkInterface {
                name,
                facts: alloc::vec![
                    SystemFact::new("Hardware address", Reading::measured(mac(iface.mac))),
                    SystemFact::new("MTU", Reading::measured(format!("{} bytes", iface.mtu))),
                    SystemFact::new("Kind", Reading::measured(kind_name(iface.kind))),
                ],
                link: reading(sample, DegradedField::NetInterfaceState, state, |state| {
                    String::from(if state.link_up { "up" } else { "down" })
                }),
                addresses,
                addresses_absent: absent_unless(
                    sample,
                    DegradedField::NetInterfaceState,
                    state.is_some(),
                ),
                rx: reading(sample, DegradedField::NetInterfaceRates, rate, |rate| {
                    format_rate(rate.rx_bps)
                }),
                tx: reading(sample, DegradedField::NetInterfaceRates, rate, |rate| {
                    format_rate(rate.tx_bps)
                }),
            }
        })
        .collect()
}

/// An interface name's bytes up to its first NUL — the wire carries a
/// fixed-width field, not a fixed-width name.
fn trim_nul(name: &[u8]) -> &[u8] {
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    name.get(..end).unwrap_or(name)
}

/// A hardware address in the conventional colon-separated hexadecimal.
fn mac(bytes: [u8; 6]) -> String {
    let mut out = String::new();
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// An interface's kind as display text.
const fn kind_name(kind: NetIfKind) -> &'static str {
    match kind {
        NetIfKind::Ethernet => "ethernet",
        NetIfKind::Loopback => "loopback",
        NetIfKind::Bond => "bond",
    }
}

/// One configured address with its prefix length, and its state where
/// that state is anything other than the ordinary preferred one.
fn format_addr(addr: &NetIfAddr) -> String {
    let text = match addr.family {
        NetAddrFamily::V4 => ipv4(&addr.addr),
        NetAddrFamily::V6 => ipv6(&addr.addr),
    };
    let state = match addr.state {
        NetAddrState::Preferred => "",
        NetAddrState::Tentative => " (tentative)",
        NetAddrState::Deprecated => " (deprecated)",
    };
    format!("{text}/{}{state}", addr.prefix)
}

/// The first four bytes of an address slot as dotted-quad text.
fn ipv4(addr: &[u8; 16]) -> String {
    let octet = |index: usize| addr.get(index).copied().unwrap_or(0);
    format!("{}.{}.{}.{}", octet(0), octet(1), octet(2), octet(3))
}

/// An address slot as the eight colon-separated hexadecimal groups of an
/// IPv6 address, written in full rather than with the `::` elision, so a
/// reader can compare two addresses character by character.
fn ipv6(addr: &[u8; 16]) -> String {
    let mut out = String::new();
    for group in 0..8usize {
        if group > 0 {
            out.push(':');
        }
        let high = addr.get(group.saturating_mul(2)).copied().unwrap_or(0);
        let low = addr
            .get(group.saturating_mul(2).saturating_add(1))
            .copied()
            .unwrap_or(0);
        let _ = write!(out, "{:x}", u16::from(high) << 8 | u16::from(low));
    }
    out
}

/// One [`SessionSeat`] per configured seat.
fn session_seats(sample: &Sample) -> Vec<SessionSeat> {
    let Some(seats) = sample.seats.as_ref() else {
        return Vec::new();
    };
    seats
        .iter()
        .map(|seat| SessionSeat {
            name: format!("Seat {}", seat.seat_id),
            owner: seat.owner().map_or_else(
                || Reading::measured("unowned"),
                |owner| Reading::measured(format!("task {owner}")),
            ),
            console: Reading::measured(format!("console {}", seat.foreground_console)),
        })
        .collect()
}

/// The logged-in census the load reading carries, alongside the task
/// counts measured with it.
fn census_facts(sample: &Sample) -> Vec<SystemFact> {
    alloc::vec![
        SystemFact::new(
            "Logged in",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| format!("{}", load.users),
            ),
        ),
        SystemFact::new(
            "Runnable tasks",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| format!("{}", load.runnable),
            ),
        ),
        SystemFact::new(
            "Total tasks",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| format!("{}", load.total_tasks),
            ),
        ),
    ]
}

/// One [`LimitRow`] per effective resource limit, with the live usage
/// measured against it.
fn limit_rows(sample: &Sample) -> Vec<LimitRow> {
    let Some(limits) = sample.resource_limits.as_ref() else {
        return Vec::new();
    };
    limits
        .iter()
        .map(|record| LimitRow {
            name: String::from(limit_name(record.kind)),
            soft: bound(record.kind, record.limit.soft),
            hard: bound(record.kind, record.limit.hard),
            usage: Reading::measured(bound(record.kind, record.usage)),
        })
        .collect()
}

/// A limit's name in the words the resource-limit facility uses.
const fn limit_name(kind: LimitKind) -> &'static str {
    match kind {
        LimitKind::AddressSpaceBytes => "Address space",
        LimitKind::OpenStreams => "Open streams",
        LimitKind::Processes => "Processes",
        LimitKind::StackBytes => "Stack",
        LimitKind::PinnedMemoryBytes => "Pinned memory",
    }
}

/// A limit bound in the unit its kind is denominated in, with the
/// unbounded sentinel spelled out rather than shown as a huge number.
fn bound(kind: LimitKind, value: u64) -> String {
    if value == RLIMIT_INFINITY {
        return String::from("unlimited");
    }
    match kind {
        LimitKind::AddressSpaceBytes | LimitKind::StackBytes | LimitKind::PinnedMemoryBytes => {
            format_bytes(value)
        }
        LimitKind::OpenStreams | LimitKind::Processes => value.to_string(),
    }
}

/// The system actions the rail offers.
///
/// Each is refused for want of an interface rather than for want of
/// authority: no power, lock, or session-control endpoint exists for this
/// service to drive, so the rail states that plainly instead of offering a
/// button that would do nothing.
fn system_actions() -> Vec<SystemAction> {
    alloc::vec![
        action("Lock", ControlRole::System),
        action("Log Out", ControlRole::System),
        action("Restart", ControlRole::System),
        action("Shut Down", ControlRole::Destructive),
    ]
}

/// One rail action, refused for want of an interface.
fn action(label: &str, role: ControlRole) -> SystemAction {
    SystemAction {
        label: String::from(label),
        role,
        allowed: false,
        refusal: Some(Unmeasured::NoInterface),
    }
}

#[cfg(test)]
#[path = "system_report_tests.rs"]
mod tests;
