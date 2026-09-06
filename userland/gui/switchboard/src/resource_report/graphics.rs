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

use tairix_abi::hwtree::{HwDeviceClass, HwNode};
use tairix_abi::switchboard_ipc::FrameReport;
use tairix_controls::PressureKind;

use crate::format::format_pixels;
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
    history: &[u16],
) -> ResourceDevice {
    let blended = frame.map(|frame| frame.blended_px);
    ResourceDevice {
        id: DeviceId::Graphics,
        group: DeviceGroup::Graphics,
        name: String::from("Compositor"),
        kind: PressureKind::Gpu,
        reading: blended.map_or_else(
            || Reading::Absent(Unmeasured::Unavailable),
            |px| Reading::measured(format_pixels(px)),
        ),
        trend: history.to_vec(),
        hero: hero(frame, history),
        blocks: blocks(sample, frame),
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
fn blocks(sample: &Sample, frame: Option<FrameReport>) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::half("FRAME WORK — COUNTS ONLY", frame_block(frame)).with_note(
            "No wall-clock figure rides this path: a duration is neither reproducible nor assertable, so the compositor reports work and the reader draws the conclusion.",
        ),
        PaneBlock::half("COMPOSITING PATH", BlockBody::Facts(path_facts())),
        PaneBlock::full("GRAPHICS DEVICE", device_block(sample)).with_note(
            "Identity comes from the hardware tree; engine utilisation and device memory need a per-device graphics statistics query.",
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

/// How the desktop composites, and what it cannot yet report about hardware
/// layers.
fn path_facts() -> Vec<ReadingFact> {
    alloc::vec![
        ReadingFact::text("Compositing path", "software · lib/raster"),
        // The accelerated-layer capabilities exist in the display driver ABI
        // but no query publishes them, so each is marked rather than guessed.
        ReadingFact::absent("Accelerated layers", Unmeasured::NoInterface),
        ReadingFact::absent("Max hardware layers", Unmeasured::NoInterface),
        ReadingFact::absent("Per-layer opacity", Unmeasured::NoInterface),
        ReadingFact::absent("Vsync / flip model", Unmeasured::NoInterface),
    ]
}

/// The graphics device the display path runs on, as discovery reports it.
fn device_block(sample: &Sample) -> BlockBody {
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
    facts.push(ReadingFact::absent("Scan-out", Unmeasured::NoInterface));
    facts.push(ReadingFact::absent(
        "Engine utilisation",
        Unmeasured::NoInterface,
    ));
    facts.push(ReadingFact::absent("Video memory", Unmeasured::NoInterface));
    BlockBody::Facts(facts)
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
