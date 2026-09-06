//! Fixtures shared by the screen's per-section render tests.
//!
//! One font, one model, one window rect and one click helper serve every
//! section's tests, so no section carries its own copy of the fixture the
//! others already use.

use tairix_abi::{ProcId, PROC_ID_LEN};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    damage, ActivityState, PressureKind, PressureState, ProgressValue, RecoveryState,
};
use tairix_icon::IconKind;

use super::resources::{
    BlockBody, CompositionPart, ConsumerRow, CoreCell, DeviceAction, DeviceGroup, DeviceId,
    HeroInstrument, PaneBlock, PaneHero, PressureBanner, ResourceControl, ResourceDevice,
    ResourceReport, TaskCostColumn,
};
use super::{
    ActionVerdict, CrashSnapshot, FaultImpact, FaultMark, HealthSeverity, Reading, ReadingFact,
    RecoveryItem, SectionOutcome, Switchboard, SwitchboardAction, SwitchboardModel, TaskAuthority,
    TaskSummary, Unmeasured,
};

pub(super) fn font() -> BitmapFont {
    BitmapFont::console()
}

pub(super) const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

pub(super) const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

pub(super) fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

/// A populated model with enough items to overflow a modest viewport.
///
/// Its Resources report deliberately spans the surface's honest range: the
/// CPU pane leads with a plotted trend, Memory with a measured track, and a
/// volume's rate block is left honestly unmeasured — exactly the "quiet
/// instrument" default a pane with no wired query falls back to.
pub(super) fn model() -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for i in 0..50 {
        m.tasks.push(TaskSummary {
            proc_id: task_id(i),
            name: alloc::format!("task {i}"),
            cpu_permille: Some(u16::try_from(i).unwrap_or(0) * 10),
            pressure: if i % 3 == 0 {
                PressureState::Under(PressureKind::Cpu)
            } else {
                PressureState::None
            },
            activity: ActivityState::Progress(ProgressValue::new(500)),
            recovery: RecoveryState::None,
            authority: TaskAuthority {
                switch: ActionVerdict::Ready,
                pause: ActionVerdict::Ready,
                resume: ActionVerdict::DisabledByState,
                lower_priority: ActionVerdict::Ready,
                force_quit: ActionVerdict::Ready,
            },
            ..TaskSummary::default()
        });
    }
    for i in 0..6 {
        m.recovery.push(recovery_item(i, RecoveryState::Hung));
    }
    m.resources = resource_report();
    m
}

/// A Resources report spanning every state a pane must draw: a trended
/// rate, a measured track, a composition, a per-core grid, a health block,
/// a consumers block, an absence stated in words, a pressure banner, and a
/// fact pane with no instrument at all.
///
/// One device per rail group, so a test can select across groups and assert
/// the heading each group's first entry carries.
pub(super) fn resource_report() -> ResourceReport {
    ResourceReport {
        devices: alloc::vec![
            cpu_device(),
            memory_device(),
            volume_device(),
            machine_device(),
        ],
        volumes_absent: None,
        interfaces_absent: Some(Unmeasured::NotPermitted),
    }
}

/// The CPU device: a trended hero, a per-core grid and a consumers block.
fn cpu_device() -> ResourceDevice {
    ResourceDevice {
        id: DeviceId::Cpu,
        group: DeviceGroup::Resources,
        name: alloc::string::String::from("CPU"),
        kind: PressureKind::Cpu,
        reading: Reading::measured("18%"),
        trend: (0..24).map(|i| i * 40).collect(),
        hero: PaneHero {
            value: Reading::measured("18"),
            unit: alloc::string::String::from("% busy"),
            context: alloc::vec![alloc::string::String::from("2.2 of 12 cores-equivalent")],
            instrument: HeroInstrument::Trend {
                samples: (0..24).map(|i| i * 40).collect(),
                opposing: None,
            },
            caption: alloc::string::String::from("busy share, all cores"),
        },
        blocks: alloc::vec![
            PaneBlock::full(
                "PER-CORE BUSY",
                BlockBody::Cores(
                    (0..4)
                        .map(|i| CoreCell {
                            label: alloc::format!("core {i}"),
                            badge: alloc::string::String::from("P"),
                            busy: Reading::measured("41%"),
                            clock: Reading::measured("3.9 GHz"),
                            trend: alloc::vec![300, 500, 400],
                        })
                        .collect(),
                ),
            ),
            PaneBlock::half(
                "TOP CONSUMERS — CPU",
                BlockBody::Consumers(alloc::vec![ConsumerRow {
                    name: alloc::string::String::from("task 3"),
                    icon: IconKind::Executable,
                    amount: alloc::string::String::from("9.7%"),
                    share: 970,
                }]),
            )
            .with_note("A sum of tasks is not the device's total."),
        ],
        banner: None,
        actions: alloc::vec![
            DeviceAction::ready(
                ResourceControl::SortTasksBy(TaskCostColumn::Cpu),
                "Sort tasks by CPU",
            ),
            DeviceAction::absent(ResourceControl::CopyReadings, "Copy readings"),
        ],
    }
}

/// The memory device: a tracked hero, a composition, and the banner a
/// resource under pressure wears with its recommended relief.
fn memory_device() -> ResourceDevice {
    ResourceDevice {
        id: DeviceId::Memory,
        group: DeviceGroup::Resources,
        name: alloc::string::String::from("Memory"),
        kind: PressureKind::Memory,
        reading: Reading::measured("53%"),
        trend: alloc::vec![],
        hero: PaneHero {
            value: Reading::measured("8.6"),
            unit: alloc::string::String::from("of 16 GB"),
            context: alloc::vec![alloc::string::String::from("53% committed")],
            instrument: HeroInstrument::Track(Some(530)),
            caption: alloc::string::String::new(),
        },
        blocks: alloc::vec![PaneBlock::full(
            "COMPOSITION",
            BlockBody::Composition(alloc::vec![
                CompositionPart {
                    label: alloc::string::String::from("Processes"),
                    amount: alloc::string::String::from("4.1 GB"),
                    share: 300,
                    remainder: false,
                },
                CompositionPart {
                    label: alloc::string::String::from("Free"),
                    amount: alloc::string::String::from("7.4 GB"),
                    share: 700,
                    remainder: true,
                },
            ]),
        )],
        banner: Some(PressureBanner {
            band: alloc::string::String::from("elevated"),
            summary: alloc::string::String::from("Memory pressure has stood in the elevated band"),
            detail: alloc::string::String::from("Recommended relief: compress inactive pages"),
            relief: Some(DeviceAction::absent(
                ResourceControl::Relieve,
                "Reclaim now",
            )),
        }),
        actions: alloc::vec![DeviceAction::absent(
            ResourceControl::Relieve,
            "Reclaim now"
        )],
    }
}

/// A volume: a health block, and a rate block stated absent in words.
fn volume_device() -> ResourceDevice {
    ResourceDevice {
        id: DeviceId::Volume([7; 16]),
        group: DeviceGroup::Storage,
        name: alloc::string::String::from("nvme0 · System:"),
        kind: PressureKind::Disk,
        reading: Reading::measured("72%"),
        trend: alloc::vec![],
        hero: PaneHero {
            value: Reading::measured("812 GB"),
            unit: alloc::string::String::from("of 1.10 TB"),
            context: alloc::vec![],
            instrument: HeroInstrument::Track(Some(720)),
            caption: alloc::string::String::new(),
        },
        blocks: alloc::vec![
            PaneBlock::half(
                "HEALTH",
                BlockBody::Health {
                    pill: alloc::string::String::from("Healthy"),
                    severity: HealthSeverity::Healthy,
                    facts: alloc::vec![ReadingFact::text("Completions", "4,182,904")],
                },
            ),
            PaneBlock::half(
                "SERVICE & QUEUE",
                BlockBody::Facts(alloc::vec![ReadingFact::absent(
                    "Utilisation",
                    Unmeasured::NoInterface
                )]),
            ),
        ],
        banner: None,
        actions: alloc::vec![DeviceAction::absent(ResourceControl::Scrub, "Scrub now")],
    }
}

/// A `Machine` fact pane: no trace, no instrument, facts only.
fn machine_device() -> ResourceDevice {
    ResourceDevice {
        id: DeviceId::Identity,
        group: DeviceGroup::Machine,
        name: alloc::string::String::from("Identity & uptime"),
        kind: PressureKind::Cpu,
        reading: Reading::measured("2h 12m"),
        trend: alloc::vec![],
        hero: PaneHero::facts(Reading::measured("tairix"), ""),
        blocks: alloc::vec![PaneBlock::full(
            "MACHINE",
            BlockBody::Facts(alloc::vec![
                ReadingFact::text("Hostname", "tairix"),
                ReadingFact::absent("Machine id", Unmeasured::Unavailable),
            ]),
        )],
        banner: None,
        actions: alloc::vec![],
    }
}

/// The fault at `index` of the populated model: a task with a measured age
/// and impact readings, no crash record, and both commands permitted.
///
/// The identity is derived from `index` so every fixture fault has its own
/// stable one — which is what lets a test move a fault in the list and
/// still assert the selection followed it.
pub(super) fn recovery_item(index: usize, recovery: RecoveryState) -> RecoveryItem {
    RecoveryItem {
        proc_id: fault_id(index),
        pid: 400 + index as u64,
        name: alloc::format!("hung {index}"),
        detail: alloc::string::String::from("not responding"),
        since: Reading::measured("4m"),
        recovery,
        impact: FaultImpact::of(recovery),
        status: alloc::string::String::from("The task has stopped answering."),
        recommendation: alloc::string::String::from("Restart it."),
        marks: alloc::vec![FaultMark {
            stamp: alloc::string::String::from("4m ago"),
            text: alloc::string::String::from("Stopped answering its seat"),
            is_fault: true,
        }],
        crash: None,
        cpu: Reading::measured("3%"),
        memory: Reading::measured("64.0 MiB"),
        disk: Reading::measured("0 B/s"),
        network: Reading::Absent(Unmeasured::NoInterface),
        can_restart: true,
        can_force: true,
    }
}

/// The stable identity the fixture fault at `index` carries.
pub(super) fn fault_id(index: usize) -> ProcId {
    let mut bytes = [0u8; PROC_ID_LEN];
    bytes[0] = 0x9f;
    bytes[1] = u8::try_from(index).unwrap_or(u8::MAX);
    ProcId::from_raw(bytes)
}

/// The identity of the fixture task at `index`, distinct from every
/// [`fault_id`], so a test can move a task in the list and still assert the
/// selection followed it rather than the position it used to hold.
pub(super) fn task_id(index: usize) -> ProcId {
    let mut bytes = [0u8; PROC_ID_LEN];
    bytes[0] = 0x5a;
    bytes[1] = u8::try_from(index).unwrap_or(u8::MAX);
    ProcId::from_raw(bytes)
}

/// A crash record a test can hang on a fixture fault, so the Crash
/// Snapshot page can be asserted against real recorded values.
pub(super) fn fault_crash() -> CrashSnapshot {
    CrashSnapshot {
        cause: alloc::string::String::from("outside every mapping the task owns"),
        location: alloc::string::String::from("8 bytes into the null page"),
        access: alloc::string::String::from("write"),
        owner: alloc::string::String::from("uid 1000, gid 1000"),
        pc: alloc::string::String::from("0x0000000000401234 (program-relative)"),
        sp: alloc::string::String::from("0x00007ffe0000f000"),
        fp: alloc::string::String::from("not meaningful for this frame"),
        registers: alloc::vec![(alloc::string::String::from("x0"), 0xdead_beef_u64)],
        frames: alloc::vec![0x0040_1234, 0x0040_5678],
    }
}

pub(super) fn bounds() -> Rect {
    Rect::new(0, 0, 600, 400)
}

pub(super) fn centre(rect: Rect) -> (i32, i32) {
    (
        rect.left() + i32::try_from(rect.width).unwrap_or(0) / 2,
        rect.top() + i32::try_from(rect.height).unwrap_or(0) / 2,
    )
}

/// The rectangle the master card in list slot `index` occupies.
///
/// This is the very rectangle the section hit-tests its cards against, read
/// from the section's own list geometry, so a test aims where the screen
/// really seats the card rather than at a rectangle of its own invention.
pub(super) fn card_slot(sb: &Switchboard, b: Rect, theme: &Theme, index: usize) -> Rect {
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let info = sb.list_info(&layout, Scale::ONE, theme);
    info.item_rect(u32::try_from(index).unwrap_or(0))
}

/// A point inside master card `item`'s own body, clear of every footer
/// button in `footer`, so a click there is a body press and nothing else.
///
/// The footer sits along the card's bottom edge, so the upper quarter is
/// body; the assertion is what keeps that true if the footer layout ever
/// changes, rather than leaving a test quietly clicking a button.
pub(super) fn card_body_centre(item: Rect, footer: &[Rect]) -> (i32, i32) {
    let x = item.left() + i32::try_from(item.width).unwrap_or(0) / 2;
    let y = item.top() + i32::try_from(item.height).unwrap_or(0) / 4;
    assert!(
        footer.iter().all(|rect| !rect.contains(Point::new(x, y))),
        "the body point must miss every footer button"
    );
    (x, y)
}

/// The first pixel of `area` where `before` and `after` differ that `damage`
/// does not name, or `None` when the report covered every change.
///
/// This is the sound half of a damage proof: whatever a round reported, the
/// present copies only that, so every pixel the round *moved* has to be
/// inside it or the screen keeps a stale one.
pub(super) fn unreported_change(
    before: &Surface,
    after: &Surface,
    area: Rect,
    damage: &Region,
) -> Option<Point> {
    (area.top()..area.bottom()).find_map(|y| {
        (area.left()..area.right()).find_map(|x| {
            let (xu, yu) = (u32::try_from(x).ok()?, u32::try_from(y).ok()?);
            (before.get(xu, yu) != after.get(xu, yu) && !damage.contains(Point::new(x, y)))
                .then_some(Point::new(x, y))
        })
    })
}

/// Paint the screen into a fresh surface the size of the fixture window.
pub(super) fn shot(sb: &mut Switchboard) -> Surface {
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &Theme::dark(), font());
    surface
}

pub(super) fn has_ink(surface: &Surface, rect: Rect) -> bool {
    (rect.left()..rect.right()).any(|x| {
        (rect.top()..rect.bottom()).any(|y| {
            let (xu, yu) = (u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0));
            surface.get(xu, yu).is_some_and(|p| p.a > 0)
        })
    })
}

/// Feed one pointer event to the screen and answer the action it produced,
/// discarding the round's report.
///
/// The one place a test that is not about damage builds a sink, so no test
/// file carries its own.
pub(super) fn pointer(
    sb: &mut Switchboard,
    b: Rect,
    scale: Scale,
    theme: &Theme,
    event: &InputEvent,
) -> Option<SwitchboardAction> {
    sb.on_pointer(event, b, scale, theme, font(), &mut damage::sink())
}

/// Feed a full click (move, press, release) at `(x, y)` and collect the
/// actions produced.
pub(super) fn click(
    sb: &mut Switchboard,
    b: Rect,
    scale: Scale,
    theme: &Theme,
    x: i32,
    y: i32,
) -> alloc::vec::Vec<SwitchboardAction> {
    let mut out = alloc::vec::Vec::new();
    for event in [moved(x, y), PRESS, RELEASE] {
        if let Some(action) = pointer(sb, b, scale, theme, &event) {
            out.push(action);
        }
    }
    out
}

/// Feed one key to the screen laid out in the fixture window for the dark
/// theme — the same screen [`bounds`] and [`click`] use, which is what gives
/// every control the key reaches the rectangle it is drawn in.
pub(super) fn key(sb: &mut Switchboard, key: Key) -> Option<SwitchboardAction> {
    sb.on_key(
        key,
        bounds(),
        Scale::ONE,
        &Theme::dark(),
        font(),
        &mut damage::sink(),
    )
}

/// Feed one pointer event to the screen laid out in the fixture window,
/// answering the rectangles the round reported.
///
/// The report is what a damage test is about, so it comes back rather than
/// being discarded as [`click`] discards it.
pub(super) fn report(sb: &mut Switchboard, event: &InputEvent) -> Region {
    let mut damage = damage::sink();
    sb.on_pointer(
        event,
        bounds(),
        Scale::ONE,
        &Theme::dark(),
        font(),
        &mut damage,
    );
    damage
}

/// Commit `key` on the section the screen is showing, laid out in the
/// fixture window for the dark theme.
///
/// The section's own activation path, reached without the screen's Tab cycle
/// in the way, so a test can put the cursor on one stop and press it.
pub(super) fn activate(sb: &mut Switchboard, key: Key) -> Option<SectionOutcome> {
    let theme = Theme::dark();
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    sb.active_mut()
        .activate_focused(key, ctx, &mut damage::sink())
}

/// The Tasks section's command-rail item rectangles, in rail order.
///
/// Read from the rail's own layout — the very rectangles the render path
/// paints into — rather than re-derived, so a test aims at exactly what a
/// reader sees.
pub(super) fn task_rail_rects(
    sb: &Switchboard,
    b: Rect,
    scale: Scale,
    theme: &Theme,
) -> alloc::vec::Vec<Rect> {
    let layout = sb.compute_layout(b, scale, theme);
    let ctx = sb.section_ctx(&layout, b, scale, theme, font());
    sb.tasks.rail_item_rects(&ctx)
}

/// The window point that hits shown task row `row`.
pub(super) fn task_row_point(
    sb: &Switchboard,
    b: Rect,
    scale: Scale,
    theme: &Theme,
    row: usize,
) -> (i32, i32) {
    let layout = sb.compute_layout(b, scale, theme);
    let info = sb.list_info(&layout, scale, theme);
    centre(info.item_rect(u32::try_from(row).unwrap_or(0)))
}

/// Select shown task row `row` with the pointer, which is what gives the
/// command rail its subject.
pub(super) fn select_task_row(
    sb: &mut Switchboard,
    b: Rect,
    scale: Scale,
    theme: &Theme,
    row: usize,
) {
    let (x, y) = task_row_point(sb, b, scale, theme, row);
    assert!(
        click(sb, b, scale, theme, x, y).is_empty(),
        "selecting a row emits nothing of its own"
    );
}

/// Put the Tasks section's content cursor on shown row `row`.
///
/// The section's cursor spans its header controls before its rows, so a
/// test that wants a row walks down to it exactly as a reader would rather
/// than assuming row zero is the first stop.
pub(super) fn focus_task_row(sb: &mut Switchboard, row: usize) {
    let target = sb.tasks.focus_index_for_row(row);
    for _ in 0..target {
        assert_eq!(key(sb, Key::Named(NamedKey::Down)), None);
    }
    assert_eq!(sb.active().content_focus(), target);
}
