//! The CPU pane: one busy trace for the machine, one per core
//! (`plans/switchboard/02-cpu.png`).
//!
//! Every figure is a two-sample delta. TAIRiX accounts busy and idle only,
//! so this is one busy trace rather than a stacked user/system/nice/iowait
//! area — the split it would need is not a reading the kernel takes.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::sysinfo::{CpuCoreClass, CpuInfoRecord, CpuLoadRecord, LoadAverage};
use tairix_controls::PressureKind;

use super::{consumers, reading as reading_of};
use crate::format::percent;
use crate::model::RollingMeters;
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{Reading, ReadingFact, Unmeasured};
use crate::view::resources::{BlockBody, CoreCell, HeroInstrument, PaneBlock, PaneHero};
use crate::view::resources::{
    DeviceAction, DeviceGroup, DeviceId, ResourceControl, ResourceDevice, TaskCostColumn,
};

/// The processor's rail entry and pane.
pub(super) fn device(sample: &Sample, meters: &RollingMeters) -> ResourceDevice {
    let busy = reading_of(
        sample,
        DegradedField::CpuTime,
        sample.cpu_busy_permille,
        percent,
    );
    let history = meters.system.cpu_history().to_vec();
    ResourceDevice {
        id: DeviceId::Cpu,
        group: DeviceGroup::Resources,
        name: String::from("CPU"),
        kind: PressureKind::Cpu,
        reading: busy.clone(),
        trend: history.clone(),
        hero: PaneHero {
            value: busy,
            unit: String::from("% busy"),
            context: context(sample),
            instrument: HeroInstrument::Trend {
                samples: history,
                opposing: None,
            },
            caption: String::from("busy share, all cores"),
        },
        blocks: blocks(sample, meters),
        banner: None,
        actions: actions(),
    }
}

/// What the hero's reading is worth: how many cores' work it amounts to,
/// and the scheduler's own load average beside it.
fn context(sample: &Sample) -> Vec<String> {
    let mut lines = Vec::new();
    if let (Some(busy), Some(count)) = (sample.cpu_busy_permille, core_count(sample)) {
        let equivalent = u64::from(busy) * u64::from(count);
        lines.push(format!(
            "{}.{} of {count} cores-equivalent",
            equivalent / 1_000,
            (equivalent % 1_000) / 100
        ));
    }
    if let Some(load) = sample.load_average {
        lines.push(format!(
            "Load average {} · {} · {}",
            fixed(load.load1),
            fixed(load.load5),
            fixed(load.load15)
        ));
    }
    lines
}

/// The per-core grid, the processor's own facts, and the tasks costing it
/// most.
fn blocks(sample: &Sample, meters: &RollingMeters) -> Vec<PaneBlock> {
    let cores = core_cells(sample, meters);
    let heading = match core_count(sample) {
        Some(count) => format!("PER-CORE BUSY — {count} LOGICAL CORES"),
        None => String::from("PER-CORE BUSY"),
    };
    alloc::vec![
        PaneBlock::full(
            &heading,
            if cores.is_empty() {
                BlockBody::Absence(crate::view::reading::absence_statement(
                    "the per-CPU inventory",
                    Unmeasured::from_absence(sample.absence(DegradedField::CpuInfo)),
                ))
            } else {
                BlockBody::Cores(cores)
            },
        ),
        PaneBlock::half("PROCESSOR", BlockBody::Facts(processor_facts(sample))).with_note(
            "TAIRiX accounts busy and idle only, so this is one busy trace, never a stacked area.",
        ),
        PaneBlock::half(
            "TOP CONSUMERS — CPU",
            BlockBody::Consumers(consumers::by_cpu(sample)),
        )
        .with_note(consumers::NOT_A_TOTAL),
    ]
}

/// One cell per logical CPU, each carrying its own core's trace.
fn core_cells(sample: &Sample, meters: &RollingMeters) -> Vec<CoreCell> {
    let Some(cpus) = sample.cpu_info.as_ref() else {
        return Vec::new();
    };
    cpus.iter()
        .map(|cpu| CoreCell {
            label: format!("core {}", cpu.cpu),
            badge: String::from(class_badge(cpu.class)),
            busy: sample
                .core_busy
                .iter()
                .find(|core| core.cpu == cpu.cpu)
                .and_then(|core| core.permille)
                .map_or_else(
                    || {
                        Reading::Absent(Unmeasured::from_absence(
                            sample.absence(DegradedField::CpuTime),
                        ))
                    },
                    |permille| Reading::measured(percent(permille)),
                ),
            clock: clock_of(cpu),
            trend: meters.devices.core_history(cpu.cpu).to_vec(),
        })
        .collect()
}

/// The processor's own facts: what it is, how it is arranged, and what the
/// scheduler is doing on it.
fn processor_facts(sample: &Sample) -> Vec<ReadingFact> {
    let mut facts = alloc::vec![
        ReadingFact::new("Model", model_reading(sample)),
        ReadingFact::new("Topology", topology_reading(sample)),
        ReadingFact::new("Live frequency", peak_clock(sample)),
    ];
    let load = sample.cpu_load.as_ref();
    facts.push(ReadingFact::new(
        "Context switches",
        reading_of(
            sample,
            DegradedField::CpuLoad,
            load.map(|loads| loads.iter().map(|entry| entry.switches).sum::<u64>()),
            |total| format!("{total} since boot"),
        ),
    ));
    facts.push(ReadingFact::new(
        "Involuntary preemptions",
        reading_of(
            sample,
            DegradedField::CpuLoad,
            load.map(|loads| loads.iter().map(|entry| entry.preemptions).sum::<u64>()),
            |total| format!("{total} since boot"),
        ),
    ));
    facts.push(ReadingFact::new(
        "Run-queue depth",
        reading_of(
            sample,
            DegradedField::CpuLoad,
            load.map(|loads| queue_summary(loads)),
            |text| text,
        ),
    ));
    // No sensor or power-supply interface exists, and no driver to serve
    // one, so the reading states its absence rather than a plausible number.
    facts.push(ReadingFact::absent(
        "Package temperature",
        Unmeasured::NoInterface,
    ));
    facts
}

/// The run queue's mean and peak depth across the cores that reported one.
fn queue_summary(loads: &[CpuLoadRecord]) -> String {
    let count = u64::try_from(loads.len()).unwrap_or(0);
    let peak = loads
        .iter()
        .map(|entry| entry.queue_depth)
        .max()
        .unwrap_or(0);
    let total: u64 = loads.iter().map(|entry| entry.queue_depth).sum();
    match count {
        0 => format!("{peak} peak"),
        count => format!(
            "{}.{} mean · {peak} peak",
            total / count,
            (total % count) * 10 / count
        ),
    }
}

/// How many logical CPUs the inventory reports.
fn core_count(sample: &Sample) -> Option<u32> {
    let cpus = sample.cpu_info.as_ref()?;
    u32::try_from(cpus.len()).ok().filter(|count| *count > 0)
}

/// The processor's model names, one per performance class present, so a
/// big.LITTLE machine names both parts rather than whichever core came
/// first.
fn model_reading(sample: &Sample) -> Reading {
    let Some(cpus) = sample.cpu_info.as_ref() else {
        return Reading::Absent(Unmeasured::from_absence(
            sample.absence(DegradedField::CpuInfo),
        ));
    };
    let mut names: Vec<String> = Vec::new();
    for cpu in cpus {
        let name = String::from_utf8_lossy(cpu.model_bytes()).into_owned();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return Reading::Absent(Unmeasured::Unavailable);
    }
    Reading::measured(names.join(" · "))
}

/// How the cores are arranged, by performance class.
fn topology_reading(sample: &Sample) -> Reading {
    let Some(cpus) = sample.cpu_info.as_ref() else {
        return Reading::Absent(Unmeasured::from_absence(
            sample.absence(DegradedField::CpuInfo),
        ));
    };
    let performance = cpus
        .iter()
        .filter(|cpu| cpu.class == CpuCoreClass::Performance)
        .count();
    let efficiency = cpus.len().saturating_sub(performance);
    if efficiency == 0 {
        return Reading::measured(format!("{performance} performance"));
    }
    Reading::measured(format!(
        "{performance} performance + {efficiency} efficiency"
    ))
}

/// The fastest clock any core is measured at, flagged as measured because
/// the record says so rather than because a figure was present.
fn peak_clock(sample: &Sample) -> Reading {
    let Some(cpus) = sample.cpu_info.as_ref() else {
        return Reading::Absent(Unmeasured::from_absence(
            sample.absence(DegradedField::CpuInfo),
        ));
    };
    match cpus
        .iter()
        .filter(|cpu| cpu.freq_measured())
        .map(|cpu| cpu.current_freq_hz)
        .max()
        .filter(|hz| *hz > 0)
    {
        Some(hz) => Reading::measured(format!("{} peak · measured", megahertz(hz))),
        None => Reading::Absent(Unmeasured::Unavailable),
    }
}

/// One core's live clock, or an absent reading where the port does not
/// measure one — never an assumed nominal figure.
fn clock_of(cpu: &CpuInfoRecord) -> Reading {
    if cpu.freq_measured() && cpu.current_freq_hz > 0 {
        Reading::measured(megahertz(cpu.current_freq_hz))
    } else {
        Reading::Absent(Unmeasured::Unavailable)
    }
}

/// A frequency in the unit a reader reads clocks in.
fn megahertz(hz: u64) -> String {
    let mhz = hz / 1_000_000;
    if mhz >= 1_000 {
        return format!("{}.{} GHz", mhz / 1_000, (mhz % 1_000) / 100);
    }
    format!("{mhz} MHz")
}

/// A load-average figure, which the wire carries in fixed point.
fn fixed(value: u32) -> String {
    format!(
        "{}.{:02}",
        LoadAverage::whole(value),
        LoadAverage::centis(value)
    )
}

/// The performance-class badge one core's cell wears.
const fn class_badge(class: CpuCoreClass) -> &'static str {
    match class {
        CpuCoreClass::Performance => "P",
        CpuCoreClass::Efficiency => "E",
    }
}

/// The commands the rail offers for the processor.
fn actions() -> Vec<DeviceAction> {
    alloc::vec![
        DeviceAction::ready(
            ResourceControl::SortTasksBy(TaskCostColumn::Cpu),
            "Sort tasks by CPU",
        ),
        DeviceAction::absent(ResourceControl::CopyReadings, "Copy readings"),
    ]
}
