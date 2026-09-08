//! The Memory pane: where the RAM went, the band it stands in, and the one
//! relief the model recommends (`plans/switchboard/03-memory.png`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::sysinfo::cache_class_name;
use tairix_abi::MemoryClass;
use tairix_controls::{ControlRole, PressureKind, MAX_COMPOSITION_SEGMENTS};

use super::{consumers, reading as reading_of};
use crate::format::{byte_parts, format_bytes, format_duration, percent};
use crate::model::{OwnerBundles, RollingMeters};
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{Reading, ReadingFact, Unmeasured};
use crate::view::resources::{
    BlockBody, CompositionPart, HeroInstrument, PaneBlock, PaneHero, Trace,
};
use crate::view::resources::{
    DeviceAction, DeviceId, PressureBanner, RailGroup, ResourceControl, ResourceDevice,
    TaskCostColumn,
};

/// The machine's memory: its rail entry and its pane.
pub(super) fn device(
    sample: &Sample,
    meters: &RollingMeters,
    bundles: &OwnerBundles,
) -> ResourceDevice {
    let committed = reading_of(
        sample,
        DegradedField::MemoryPressure,
        sample.memory_pressure,
        |memory| percent(memory.used_permille),
    );
    ResourceDevice {
        id: DeviceId::Memory,
        group: RailGroup::Resources,
        name: String::from("Memory"),
        kind: PressureKind::Memory,
        reading: committed.clone(),
        trend: Trace::single(
            PressureKind::Memory.signal_role(),
            meters.system.memory_history().to_vec(),
        ),
        hero: PaneHero {
            // The figure alone; its unit trails it at body size beside the
            // whole it is a share of, both scaled to that whole's unit.
            value: reading_of(
                sample,
                DegradedField::MemoryPressure,
                in_use_bytes(sample),
                |bytes| hero_parts(sample, bytes).0,
            ),
            unit: in_use_bytes(sample)
                .map(|bytes| hero_parts(sample, bytes).1)
                .unwrap_or_default(),
            context: context(sample, meters),
            // The committed share both ways, as the boards draw it: the trace
            // for what memory has been doing, the bar for how much is in use
            // now. The history is already measured for the rail's own entry.
            instrument: HeroInstrument::trend(Trace::single(
                PressureKind::Memory.signal_role(),
                meters.system.memory_history().to_vec(),
            ))
            .with_track(sample.memory_pressure.map(|m| m.used_permille)),
            caption: String::from("committed share"),
        },
        blocks: blocks(sample, bundles),
        banner: banner(sample, meters),
        actions: actions(),
    }
}

/// The bytes in use, from the total and the free reading behind it.
fn in_use_bytes(sample: &Sample) -> Option<u64> {
    let memory = sample.memory_pressure?;
    let used = u64::from(memory.used_permille);
    Some(memory.total_bytes.saturating_mul(used) / 1_000)
}

/// The hero's figure and the unit that trails it, both scaled to the machine's
/// own total so the pair reads as one quantity.
///
/// A machine whose total is unreadable has no whole to be a share of, so the
/// figure carries its own unit and nothing trails it.
fn hero_parts(sample: &Sample, bytes: u64) -> (String, String) {
    match sample.memory_total {
        Some(total) => byte_parts(bytes, total.total_bytes),
        None => (format_bytes(bytes), String::new()),
    }
}

/// What share is committed, what is reclaimable, and how long the band has
/// stood.
fn context(sample: &Sample, meters: &RollingMeters) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(memory) = sample.memory_pressure {
        lines.push(format!(
            "{} committed · {} available",
            percent(memory.used_permille),
            format_bytes(available_bytes(sample).unwrap_or(0))
        ));
    }
    if let Some(elapsed) = band_age(sample, meters) {
        lines.push(format!("Band held for {elapsed}"));
    }
    lines
}

/// How many bytes are free, from the kernel's own accounting where it is
/// readable and from the pressure reading otherwise.
fn available_bytes(sample: &Sample) -> Option<u64> {
    if let Some(kernel) = sample.kernel_memory {
        return Some(kernel.free_bytes);
    }
    let memory = sample.memory_pressure?;
    let free = 1_000u64.saturating_sub(u64::from(memory.used_permille));
    Some(memory.total_bytes.saturating_mul(free) / 1_000)
}

/// How long the memory band has stood, as the service's own clock measured
/// it — never a fabricated zero when no uptime reading anchors it.
fn band_age(sample: &Sample, meters: &RollingMeters) -> Option<String> {
    let now = sample.uptime.map(|uptime| uptime.since_boot);
    meters.pressure.memory_elapsed(now).map(format_duration)
}

/// The composition, the memory and kernel facts, the tasks holding the
/// most, and the bounded caches' own ledger.
fn blocks(sample: &Sample, bundles: &OwnerBundles) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::full("COMPOSITION", composition(sample)),
        PaneBlock::half("MEMORY", BlockBody::Facts(memory_facts(sample))),
        PaneBlock::half(
            "TOP CONSUMERS — MEMORY",
            BlockBody::Consumers(consumers::by_memory(sample, bundles)),
        ),
        PaneBlock::full("BOUNDED CACHES — RECLAIM LEDGER", ledger(sample)),
    ]
}

/// Where the RAM went: one part per memory class the kernel charges frames
/// to, and the free remainder.
///
/// The kernel charges every frame to exactly one class at allocation and
/// discharges it from the same one at free, so the parts partition the RAM in
/// use and the bar is valid by construction: the floored shares can never
/// exceed the whole, and `1000 - Σ named` closes it exactly.
///
/// That is why the classes are read rather than the per-process figure beside
/// them. `user_resident_bytes` counts *mappings*, so a frame shared between
/// address spaces counts once per space and a user driver's MMIO window counts
/// although it is not RAM — the named shares then summed past the whole, the
/// bar refused construction, and this block stated an absence under exactly
/// the load a reader most wants it.
fn composition(sample: &Sample) -> BlockBody {
    let Some(kernel) = sample.kernel_memory else {
        return BlockBody::Absence(crate::view::reading::absence_statement(
            "the memory composition",
            Unmeasured::from_absence(sample.absence(DegradedField::KernelMemory)),
        ));
    };
    let total = kernel.total_bytes;
    if total == 0 {
        return BlockBody::Absence(crate::view::reading::absence_statement(
            "the memory composition",
            Unmeasured::Unavailable,
        ));
    }
    // A class holding nothing is dropped rather than drawn as a nameable run
    // of no width, so a quiet machine shows the parts it genuinely has.
    let mut parts: Vec<CompositionPart> = MemoryClass::ALL
        .iter()
        .map(|class| (class_label(*class), kernel.class_bytes[class.index()]))
        .filter(|(_, bytes)| *bytes > 0)
        .map(|(label, bytes)| part(label, bytes, total))
        .collect();
    // The remainder closes the whole exactly, so the bar can never
    // under-report where the memory went: the shares of the named parts are
    // rounded down, and whatever that leaves is free.
    let named_share: u32 = parts.iter().map(|p| u32::from(p.share)).sum();
    let free_share = 1_000u32.saturating_sub(named_share);
    parts.push(CompositionPart {
        label: String::from("Free"),
        amount: format_bytes(kernel.free_bytes),
        share: u16::try_from(free_share).unwrap_or(0),
        remainder: true,
    });
    BlockBody::Composition(parts)
}

// Every class must be able to wear a hue of its own, or a part would be drawn
// in another part's colour and the bar would refuse construction outright. A
// class added past the ladder's length is therefore a compile error here
// rather than a composition that silently states an absence.
const _: () = assert!(tairix_abi::MEMORY_CLASS_COUNT <= MAX_COMPOSITION_SEGMENTS);

/// How a memory class reads to someone looking at their own machine, rather
/// than by the kernel's own charging vocabulary.
const fn class_label(class: MemoryClass) -> &'static str {
    match class {
        MemoryClass::UserAnon => "Processes",
        MemoryClass::UserFile => "File cache",
        MemoryClass::PageTable => "Page tables",
        MemoryClass::Kernel => "Kernel",
        MemoryClass::Dma => "Device buffers",
        MemoryClass::Compressed => "Compressed",
    }
}

/// One named part of the composition. The free remainder is built beside the
/// named parts, so this is never one.
fn part(label: &str, bytes: u64, total: u64) -> CompositionPart {
    CompositionPart {
        label: String::from(label),
        amount: format_bytes(bytes),
        share: u16::try_from(bytes.saturating_mul(1_000) / total.max(1)).unwrap_or(1_000),
        remainder: false,
    }
}

/// What the reclaimable caches hold across every reclaim class.
fn reclaimable_bytes(sample: &Sample) -> Option<u64> {
    let classes = sample.reclaim.as_ref()?;
    Some(
        classes
            .iter()
            .map(|class| class.payload_bytes.saturating_add(class.metadata_bytes))
            .sum(),
    )
}

/// The memory facts a reader reads beside the composition.
fn memory_facts(sample: &Sample) -> Vec<ReadingFact> {
    let kernel = sample.kernel_memory.as_ref();
    alloc::vec![
        ReadingFact::new(
            "Installed",
            reading_of(
                sample,
                DegradedField::MemoryTotal,
                sample.memory_total,
                |total| format_bytes(total.total_bytes),
            ),
        ),
        ReadingFact::new(
            "Committed",
            reading_of(
                sample,
                DegradedField::MemoryPressure,
                sample.memory_pressure,
                |memory| percent(memory.used_permille),
            ),
        ),
        ReadingFact::new(
            "Reclaimable",
            reading_of(
                sample,
                DegradedField::ReclaimStats,
                reclaimable_bytes(sample),
                format_bytes,
            ),
        ),
        ReadingFact::new("Pressure band", band_reading(sample)),
        ReadingFact::new(
            "Kernel heap",
            reading_of(sample, DegradedField::KernelMemory, kernel, |stats| {
                format_bytes(stats.kernel_heap_bytes)
            }),
        ),
        ReadingFact::new(
            "Page size",
            reading_of(sample, DegradedField::KernelMemory, kernel, |stats| {
                format_bytes(u64::from(stats.page_size))
            }),
        ),
        ReadingFact::new("Compressed tier", compressed_reading(sample)),
        // Swap being encrypted is a charter property rather than a setting,
        // so it is reported as one; the key is ephemeral and per boot, so
        // nothing paged out is recoverable at rest.
        ReadingFact::text("Swap", "encrypted · per-boot key"),
    ]
}

/// The band the pressure model reports, from the ungated reading so a
/// session without the kernel-statistics scope still sees it.
fn band_reading(sample: &Sample) -> Reading {
    reading_of(
        sample,
        DegradedField::MemoryPressureBand,
        sample.pressure_band,
        |band| String::from(band_name(band.band)),
    )
}

/// What the compressed tier holds, and what it holds it for.
fn compressed_reading(sample: &Sample) -> Reading {
    reading_of(
        sample,
        DegradedField::RamzipStats,
        sample.ramzip,
        |stats| match stats
            .logical_bytes
            .saturating_mul(10)
            .checked_div(stats.stored_bytes)
        {
            Some(tenths) => format!(
                "{} → {} · {}.{}x",
                format_bytes(stats.logical_bytes),
                format_bytes(stats.stored_bytes),
                tenths / 10,
                tenths % 10
            ),
            None => format_bytes(stats.stored_bytes),
        },
    )
}

/// Every declared bounded cache, what it holds, and how often it answers.
fn ledger(sample: &Sample) -> BlockBody {
    let Some(rows) = sample.cache_ledgers.as_ref() else {
        return BlockBody::Absence(crate::view::reading::absence_statement(
            "the bounded-cache ledger",
            Unmeasured::from_absence(sample.absence(DegradedField::CacheLedgers)),
        ));
    };
    if rows.is_empty() {
        return BlockBody::Absence(String::from("No bounded cache is declared."));
    }
    BlockBody::Facts(
        rows.iter()
            .map(|row| {
                let held = row.payload_bytes.saturating_add(row.metadata_bytes);
                let lookups = row.hits.saturating_add(row.misses);
                let hit = match lookups {
                    0 => String::from("no lookups yet"),
                    total => format!("{}% hit", row.hits.saturating_mul(100) / total),
                };
                ReadingFact::text(label_of(row), format!("{} · {hit}", format_bytes(held)))
            })
            .collect(),
    )
}

/// One ledger row's label, with the reclaim class it is reclaimed under.
fn label_of(row: &tairix_abi::sysinfo::CacheLedgerRecord) -> String {
    let name = String::from_utf8_lossy(row.label_bytes()).into_owned();
    match cache_class_name(row.class) {
        Some(class) => format!("{name} ({class})"),
        None => name,
    }
}

/// The banner this pane wears while memory pressure is latched.
///
/// The band, how long it has stood, and the relief the model recommends —
/// which has no endpoint behind it, so the banner offers it plainly
/// disabled rather than as a button that would do nothing.
fn banner(sample: &Sample, meters: &RollingMeters) -> Option<PressureBanner> {
    if !meters.system.memory_pressured() {
        return None;
    }
    let band = sample.pressure_band.map_or_else(
        || String::from("Under pressure"),
        |b| String::from(band_name(b.band)),
    );
    let held = band_age(sample, meters).map_or_else(
        || String::from("for an unmeasured time"),
        |elapsed| format!("for {elapsed}"),
    );
    Some(PressureBanner {
        band: band.clone(),
        summary: format!("Memory pressure has stood in the {band} band {held}"),
        detail: String::from(
            "Recommended relief: compress inactive anonymous pages. No endpoint drives reclaim from here.",
        ),
        relief: Some(
            DeviceAction::absent(ResourceControl::Relieve, "Reclaim now")
                .with_role(ControlRole::Primary),
        ),
    })
}

/// The pressure band's own name.
///
/// Read from the band byte the reading carries rather than derived from a
/// percentage, so the pane and the model can never disagree about which
/// band the machine is in. An unknown band is named as one instead of being
/// silently folded into a neighbour.
const fn band_name(band: u8) -> &'static str {
    match band {
        0 => "nominal",
        1 => "mild",
        2 => "elevated",
        3 => "severe",
        4 => "critical",
        _ => "unrecognised",
    }
}

/// The commands the rail offers for the machine's memory.
fn actions() -> Vec<DeviceAction> {
    alloc::vec![
        DeviceAction::absent(ResourceControl::Relieve, "Reclaim now"),
        DeviceAction::ready(
            ResourceControl::SortTasksBy(TaskCostColumn::Memory),
            "Sort tasks by memory",
        ),
        DeviceAction::absent(ResourceControl::CopyReadings, "Copy readings"),
    ]
}
