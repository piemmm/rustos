//! The Graphics pane: what the last composited frame cost, the compositing
//! path, and the device behind it (`plans/switchboard/06-graphics.png`).
//!
//! Named for the display path, not for a GPU: a framebuffer-only or headless
//! machine has no GPU and would read an empty *GPU* pane — but it still
//! composites, and that work is what a reader needs. Counts of work only; no
//! wall-clock figure rides this path, because a duration is neither
//! reproducible nor assertable.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::display_ipc::DisplayStats;
use tairix_abi::hwtree::{HwDeviceClass, HwNode};
use tairix_abi::switchboard_ipc::FrameReport;
use tairix_controls::PressureKind;

use crate::format::{format_bytes, format_pixels, percent};
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{absence_statement, Reading, ReadingFact, Unmeasured};
use crate::view::resources::{
    BlockBody, DeviceAction, DeviceGroup, DeviceId, HeroInstrument, PaneBlock, PaneHero,
    ResourceControl, ResourceDevice,
};

/// The display path's rail entry and pane.
pub(super) fn device(
    sample: &Sample,
    frame: Option<FrameReport>,
    stats: Option<&DisplayStats>,
    busy_permille: Option<u16>,
    history: &[u16],
) -> ResourceDevice {
    // The rail states the frame's *damage* — what changed on screen — where
    // the hero states the contributions blended to resolve it. The two are
    // orders of magnitude apart, so showing the hero's figure here would say
    // one reading twice at two magnitudes.
    let damaged = frame.map(|frame| frame.damaged_px);
    ResourceDevice {
        id: DeviceId::Graphics,
        group: DeviceGroup::Graphics,
        name: String::from("Compositor"),
        kind: PressureKind::Gpu,
        reading: damaged.map_or_else(
            || Reading::Absent(Unmeasured::Unavailable),
            |px| Reading::measured(format_pixels(px)),
        ),
        trend: history.to_vec(),
        hero: hero(frame, history),
        blocks: blocks(sample, frame, stats, busy_permille),
        banner: None,
        actions: actions(),
    }
}

/// The reading that earns the pane: what a frame blended to change what it
/// changed.
fn hero(frame: Option<FrameReport>, history: &[u16]) -> PaneHero {
    let Some(frame) = frame else {
        // Only the desktop can count this, and it has not spoken yet: an
        // absent reading, never a zero that would read as an idle frame.
        return PaneHero::facts(Reading::Absent(Unmeasured::Unavailable), "")
            .with_context(alloc::vec![String::from(
                "No frame has been reported yet. Only the session that owns the compositor can count one.",
            )]);
    };
    if frame.is_idle() {
        return PaneHero::facts(Reading::measured("idle"), "")
            .with_context(alloc::vec![String::from("Nothing was recomposed.")]);
    }
    PaneHero {
        value: Reading::measured(format_pixels(frame.blended_px)),
        unit: String::from("px blended"),
        context: alloc::vec![
            format!(
                "to recompose {} of {} on screen",
                format_pixels(frame.damaged_px),
                format_pixels(frame.screen_px)
            ),
            overdraw_line(&frame),
        ],
        instrument: HeroInstrument::Trend {
            samples: history.to_vec(),
            opposing: None,
        },
        caption: String::from("damaged pixels per frame"),
    }
}

/// How many layer contributions the frame paid for every pixel it changed.
///
/// Derived from the two counts beside it rather than sent, so the line can
/// never disagree with them. A frame with no damage to divide by says so.
fn overdraw_line(frame: &FrameReport) -> String {
    match frame
        .blended_px
        .saturating_mul(10)
        .checked_div(frame.damaged_px)
    {
        Some(tenths) => format!("{}.{}x overdraw", tenths / 10, tenths % 10),
        None => String::from("nothing damaged to divide by"),
    }
}

/// The frame-work breakdown, the compositing path, and the device.
fn blocks(
    sample: &Sample,
    frame: Option<FrameReport>,
    stats: Option<&DisplayStats>,
    busy_permille: Option<u16>,
) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::half("FRAME WORK — COUNTS ONLY", frame_block(frame)).with_note(
            "No wall-clock figure rides this path: a duration is neither reproducible nor assertable, so the compositor reports work and the reader draws the conclusion.",
        ),
        PaneBlock::half(
            "COMPOSITING PATH",
            BlockBody::Facts(path_facts(sample, stats)),
        ),
        PaneBlock::full("GRAPHICS DEVICE", device_block(sample, stats, busy_permille)).with_note(
            "Identity comes from the hardware tree; the device's own occupancy, memory and compositor capability come from the display service that drives it. A per-engine breakdown awaits a device that reports its engines separately.",
        ),
    ]
}

/// What the last frame actually did.
fn frame_block(frame: Option<FrameReport>) -> BlockBody {
    let Some(frame) = frame else {
        return BlockBody::Absence(absence_statement(
            "the desktop's last frame",
            Unmeasured::Unavailable,
        ));
    };
    if frame.is_idle() {
        return BlockBody::Absence(String::from(
            "The last frame was idle: nothing was recomposed.",
        ));
    }
    BlockBody::Facts(alloc::vec![
        ReadingFact::text("Damaged", format_pixels(frame.damaged_px)),
        ReadingFact::text("Blended contributions", format_pixels(frame.blended_px)),
        ReadingFact::text("Opaque copies", format_pixels(frame.opaque_px)),
        ReadingFact::text("Screen", format_pixels(frame.screen_px)),
        ReadingFact::text("Dirty rectangles", frame.dirty_rects.to_string()),
        ReadingFact::text("Present calls", frame.present_calls.to_string()),
        ReadingFact::text(
            "Window furniture",
            format!(
                "{} cached · {} rendered",
                frame.chrome_hits, frame.chrome_misses
            ),
        ),
    ])
}

/// How the desktop composites, and what the device it composites onto can do
/// in hardware.
fn path_facts(sample: &Sample, stats: Option<&DisplayStats>) -> Vec<ReadingFact> {
    let mut facts = alloc::vec![ReadingFact::text(
        "Compositing path",
        "software · lib/raster",
    )];
    match stats.map(|stats| stats.device.accel) {
        // A device with a hardware compositor still composites in software
        // here: the desktop's layer path is a separate stage, so the row
        // states what the device offers and that the desktop is not taking
        // it — one row, because a reader asking either question wants both.
        Some(Some(caps)) => {
            facts.push(ReadingFact::text(
                "Accelerated layers",
                "available · not in use",
            ));
            facts.push(ReadingFact::text(
                "Max hardware layers",
                caps.max_layers.to_string(),
            ));
            facts.push(ReadingFact::text(
                "Max layer size",
                format!("{}×{}", caps.max_width_px, caps.max_height_px),
            ));
            facts.push(ReadingFact::text(
                "Per-layer opacity",
                if caps.per_layer_opacity { "yes" } else { "no" },
            ));
        }
        Some(None) => facts.push(ReadingFact::text(
            "Accelerated layers",
            "none · the device has no hardware compositor",
        )),
        None => {
            let absence = Unmeasured::from_absence(sample.absence(DegradedField::GpuDeviceStats));
            facts.push(ReadingFact::absent("Accelerated layers", absence));
        }
    }
    // Which flip model the scan-out uses is the driver's own business and no
    // reading publishes it, so it is marked rather than guessed.
    facts.push(ReadingFact::absent(
        "Vsync / flip model",
        Unmeasured::NoInterface,
    ));
    facts
}

/// The graphics device the display path runs on: what discovery reports about
/// the node, and what the service driving it measured.
fn device_block(
    sample: &Sample,
    stats: Option<&DisplayStats>,
    busy_permille: Option<u16>,
) -> BlockBody {
    let Some(nodes) = sample.hardware.as_ref() else {
        return BlockBody::Absence(absence_statement(
            "the graphics device",
            Unmeasured::from_absence(sample.absence(DegradedField::HardwareTree)),
        ));
    };
    let Some(node) = nodes
        .iter()
        .find(|node| node.class() == Some(HwDeviceClass::Display))
    else {
        return BlockBody::Absence(String::from(
            "Discovery reports no display device: this machine composites to a framebuffer the firmware handed over.",
        ));
    };
    let mut facts = alloc::vec![
        ReadingFact::text("Hardware node", format!("node {}", node.id())),
        ReadingFact::text("Device class", "display"),
        ReadingFact::text("Match keys", match_keys(node)),
        ReadingFact::text(
            "Resource requests",
            format!("{} declared", node.resources().len()),
        ),
    ];
    // Which driver bound to the node is the device manager's own record, and
    // no query publishes it, so the pane states that rather than naming a
    // driver it inferred from the class.
    facts.push(ReadingFact::absent("Bound driver", Unmeasured::NoInterface));
    let absence = Unmeasured::from_absence(sample.absence(DegradedField::GpuDeviceStats));
    if let Some(stats) = stats {
        facts.push(ReadingFact::text(
            "Scan-out",
            format!(
                "{}×{} · {}",
                stats.mode.width_px,
                stats.mode.height_px,
                stats.mode.format.name()
            ),
        ));
        // The share of the *interval*, not of the service's lifetime: a
        // cumulative total presented as utilisation would read as busy long
        // after the work stopped. A first sample has nothing to delta against
        // and says so.
        facts.push(match busy_permille {
            Some(permille) => ReadingFact::text("Device utilisation", percent(permille)),
            None => ReadingFact::absent("Device utilisation", Unmeasured::Unavailable),
        });
        facts.push(ReadingFact::text("Video memory", memory_line(stats)));
    } else {
        facts.push(ReadingFact::absent("Scan-out", absence));
        facts.push(ReadingFact::absent("Device utilisation", absence));
        facts.push(ReadingFact::absent("Video memory", absence));
    }
    // A per-engine split needs a device that reports its engines separately;
    // no display driver does, so the row states the absence rather than
    // folding one device's occupancy into an invented engine.
    facts.push(ReadingFact::absent(
        "Decode / encode engines",
        Unmeasured::NoInterface,
    ));
    BlockBody::Facts(facts)
}

/// The device's own memory, or the statement that it has none.
///
/// `0` total means the device owns no memory — a firmware framebuffer scans
/// out of system RAM — which is a different statement from none being free,
/// so it is spelled in words rather than as `0 B of 0 B`.
fn memory_line(stats: &DisplayStats) -> String {
    if stats.device.mem_total_bytes == 0 {
        return String::from("none of its own · scans out of system RAM");
    }
    format!(
        "{} of {}",
        format_bytes(stats.device.mem_resident_bytes),
        format_bytes(stats.device.mem_total_bytes)
    )
}

/// The keys the device manager binds this node against.
fn match_keys(node: &HwNode) -> String {
    let keys = node.match_keys();
    if keys.is_empty() {
        return String::from("none declared");
    }
    format!("{} declared", keys.len())
}

/// The commands the rail offers for the display path.
fn actions() -> Vec<DeviceAction> {
    alloc::vec![
        DeviceAction::absent(ResourceControl::DamageOverlay, "Damage overlay"),
        DeviceAction::absent(ResourceControl::CopyReadings, "Copy readings"),
    ]
}
