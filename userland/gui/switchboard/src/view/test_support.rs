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
    damage, ActivityState, ControlRole, PressureKind, PressureState, ProgressValue, RecoveryState,
};

use super::{
    ActionVerdict, ActivityMember, ActivitySummary, CrashSnapshot, FaultImpact, FaultMark,
    HeadlineTile, HealthSeverity, JobSummary, LimitRow, NetworkInterface, PressureAction,
    PressureCause, PressureControl, Reading, RecoveryItem, SectionOutcome, SessionSeat,
    StorageVolume, Switchboard, SwitchboardAction, SwitchboardModel, SystemAction, SystemFact,
    SystemReport, TaskAuthority, TaskSummary, TileInstrument, Unmeasured,
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
/// Its System report deliberately spans the screen's honest range: CPU is
/// measured with a plotted trend, Memory and Disk are measured tracks, and
/// Network is left honestly unmeasured — exactly the "quiet instrument"
/// default a host with no wired query must fall back to.
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
            group: None,
            ..TaskSummary::default()
        });
    }
    for i in 0..8 {
        m.jobs.push(JobSummary {
            name: alloc::format!("job {i}"),
            detail: alloc::string::String::from("copying"),
            activity: ActivityState::Progress(ProgressValue::new(300)),
            can_pause: true,
            can_cancel: true,
        });
    }
    m.pressure = (0..PRESSURE_KINDS.len())
        .map(model_pressure_cause)
        .collect();
    m.activities = (0..6).map(model_activity).collect();
    for i in 0..6 {
        m.recovery.push(recovery_item(i, RecoveryState::Hung));
    }
    m.system = system_report();
    m
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
        write: true,
        owner: alloc::string::String::from("uid 1000, gid 1000"),
        pc: alloc::string::String::from("0x0000000000401234 (program-relative)"),
        sp: alloc::string::String::from("0x00007ffe0000f000"),
        fp: alloc::string::String::from("not meaningful for this frame"),
        registers: alloc::vec![(alloc::string::String::from("x0"), 0xdead_beef_u64)],
        frames: alloc::vec![0x0040_1234, 0x0040_5678],
    }
}

/// The resources the populated model flags, one cause each.
///
/// A cause is remembered by the resource it is about, so a fixture that
/// raised the same resource twice would carry two causes a selection
/// cannot tell apart — which the service never produces, since it raises
/// at most one cause per resource. One entry per kind keeps the fixture
/// honest about that.
pub(super) const PRESSURE_KINDS: [PressureKind; 6] = [
    PressureKind::Cpu,
    PressureKind::Memory,
    PressureKind::Disk,
    PressureKind::Network,
    PressureKind::Power,
    PressureKind::Thermal,
];

/// The pressure cause at `index` of the populated model: Working, blamed
/// on task `index`, offering a recommended Pause, Lower priority, and Show
/// tasks — all Ready, with both detail readings measured.
pub(super) fn model_pressure_cause(index: usize) -> PressureCause {
    let kind = PRESSURE_KINDS[index.min(PRESSURE_KINDS.len() - 1)];
    PressureCause {
        resource: alloc::format!("{kind:?}"),
        kind,
        culprit: alloc::format!("culprit {index}"),
        cause: alloc::string::String::from("busy loop"),
        activity: ActivityState::Working,
        task_index: Some(index),
        amount: Reading::measured("92%"),
        since: Reading::measured("4m"),
        actions: alloc::vec![
            PressureAction {
                label: alloc::string::String::from("Pause"),
                control: PressureControl::Pause,
                verdict: ActionVerdict::Ready,
                recommended: true,
            },
            PressureAction {
                label: alloc::string::String::from("Lower priority"),
                control: PressureControl::LowerPriority,
                verdict: ActionVerdict::Ready,
                recommended: false,
            },
            PressureAction {
                label: alloc::string::String::from("Show tasks"),
                control: PressureControl::ShowTasks,
                verdict: ActionVerdict::Ready,
                recommended: false,
            },
        ],
    }
}

/// The activity at `index` of the populated model: stable id `100 + index`,
/// controllable, accepting members, paused on the odd indices, with one
/// working and one idle member, and its three measurable totals measured.
pub(super) fn model_activity(index: u64) -> ActivitySummary {
    ActivitySummary {
        id: 100 + index,
        name: alloc::format!("activity {index}"),
        detail: alloc::string::String::from("2 tasks"),
        activity: ActivityState::Working,
        paused: index % 2 == 1,
        can_control: true,
        can_accept_member: true,
        cpu: Reading::measured("12%"),
        memory: Reading::measured("96.0 MiB"),
        disk: Reading::measured("0 B/s"),
        network: Reading::Absent(Unmeasured::NoInterface),
        members: alloc::vec![
            ActivityMember {
                name: alloc::format!("member {index}.0"),
                detail: alloc::string::String::from("running"),
                activity: ActivityState::Working,
                joined: true,
            },
            ActivityMember {
                name: alloc::format!("member {index}.1"),
                detail: alloc::string::String::from("idle"),
                activity: ActivityState::Idle,
                joined: true,
            },
        ],
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

/// A System report spanning every state the screen must draw: measured and
/// unmeasured header readings, a healthy volume and a failing one, an
/// interface with addresses and one without, a seat, a limit, and the rail's
/// refused actions.
pub(super) fn system_report() -> SystemReport {
    SystemReport {
        headline: headline_tiles(),
        machine: alloc::vec![
            SystemFact::new("Hostname", Reading::measured("tairix")),
            SystemFact::new("OS version", Reading::measured("TAIRiX 0.1.0")),
            SystemFact::new("Uptime", Reading::measured("2h 1m")),
            SystemFact::new("Machine id", Reading::Absent(Unmeasured::Unavailable)),
        ],
        authority: alloc::vec![
            SystemFact::new("Process control", Reading::measured("held")),
            SystemFact::new("Kernel readings", Reading::Absent(Unmeasured::NotPermitted)),
        ],
        cores: alloc::vec![
            SystemFact::new("Core 0", Reading::measured("performance, 3200 MHz")),
            SystemFact::new("Core 1", Reading::measured("performance, 3200 MHz")),
        ],
        memory: alloc::vec![
            SystemFact::new("Installed", Reading::measured("16.0 GiB")),
            SystemFact::new("Kernel heap", Reading::Absent(Unmeasured::NotPermitted)),
        ],
        compositor: alloc::vec![
            SystemFact::new(
                "Last frame",
                Reading::measured("3.2k px of 2.0M px recomposed")
            ),
            SystemFact::new("Blended", Reading::measured("42.0k px, 13.1x damaged")),
        ],
        volumes: alloc::vec![
            volume(
                "System:",
                HealthSeverity::Healthy,
                Reading::measured("no faults")
            ),
            volume(
                "Backup:",
                HealthSeverity::Failing,
                Reading::measured("3 medium errors"),
            ),
        ],
        volumes_absent: None,
        interfaces: alloc::vec![
            interface(
                "eth0",
                alloc::vec![alloc::string::String::from("10.0.2.15/24")]
            ),
            interface("eth1", alloc::vec![]),
        ],
        interfaces_absent: None,
        seats: alloc::vec![SessionSeat {
            name: alloc::string::String::from("Seat 0"),
            owner: Reading::measured("task 7"),
            console: Reading::measured("console 1"),
        }],
        seats_absent: None,
        census: alloc::vec![SystemFact::new("Logged in", Reading::measured("2"))],
        limits: alloc::vec![LimitRow {
            name: alloc::string::String::from("Open streams"),
            soft: alloc::string::String::from("64"),
            hard: alloc::string::String::from("unlimited"),
            usage: Reading::measured("9"),
        }],
        limits_absent: None,
        actions: alloc::vec![
            SystemAction {
                label: alloc::string::String::from("Lock"),
                role: ControlRole::System,
                allowed: true,
                refusal: None,
            },
            SystemAction {
                label: alloc::string::String::from("Shut Down"),
                role: ControlRole::Destructive,
                allowed: false,
                refusal: Some(Unmeasured::NoInterface),
            },
        ],
    }
}

/// The fixture's four header readings: three measured, one refused, across
/// both instrument shapes.
fn headline_tiles() -> alloc::vec::Vec<HeadlineTile> {
    alloc::vec![
        tile(
            "CPU",
            Reading::measured("62%"),
            Reading::measured("4 x Test Core"),
            PressureKind::Cpu,
            TileInstrument::Trend(alloc::vec![100, 300, 500, 620]),
        ),
        tile(
            "Memory",
            Reading::measured("54%"),
            Reading::measured("8.6 GiB of 16.0 GiB"),
            PressureKind::Memory,
            TileInstrument::Track(Some(538)),
        ),
        tile(
            "Disk",
            Reading::measured("72%"),
            Reading::measured("140.0 GiB free"),
            PressureKind::Disk,
            TileInstrument::Track(Some(720)),
        ),
        tile(
            "Network",
            Reading::Absent(Unmeasured::NotPermitted),
            Reading::Absent(Unmeasured::NotPermitted),
            PressureKind::Network,
            TileInstrument::Trend(alloc::vec![]),
        ),
    ]
}

/// One header reading for the fixture.
fn tile(
    name: &str,
    value: Reading,
    detail: Reading,
    kind: PressureKind,
    instrument: TileInstrument,
) -> HeadlineTile {
    HeadlineTile {
        name: alloc::string::String::from(name),
        value,
        unit: alloc::string::String::new(),
        detail,
        kind,
        pressured: matches!(kind, PressureKind::Cpu),
        instrument,
    }
}

/// One mounted volume for the fixture.
fn volume(mount_point: &str, health_state: HealthSeverity, health: Reading) -> StorageVolume {
    StorageVolume {
        source: alloc::string::String::from("/dev/vda1"),
        mount_point: alloc::string::String::from(mount_point),
        filesystem: alloc::string::String::from("arxfs"),
        medium: alloc::string::String::from("solid state"),
        availability: alloc::string::String::from("available"),
        capacity: Reading::measured("60.0 GiB of 200.0 GiB used"),
        health,
        health_state,
    }
}

/// One network interface for the fixture.
fn interface(name: &str, addresses: alloc::vec::Vec<alloc::string::String>) -> NetworkInterface {
    NetworkInterface {
        name: alloc::string::String::from(name),
        facts: alloc::vec![SystemFact::new("MTU", Reading::measured("1500 bytes"))],
        link: Reading::measured("up"),
        addresses,
        addresses_absent: None,
        rx: Reading::measured("1.0 KiB/s"),
        tx: Reading::Absent(Unmeasured::Unavailable),
    }
}
