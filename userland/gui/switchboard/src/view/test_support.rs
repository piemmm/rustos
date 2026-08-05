//! Fixtures shared by the screen's per-section render tests.
//!
//! One font, one model, one window rect and one click helper serve every
//! section's tests, so no section carries its own copy of the fixture the
//! others already use.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, ControlRole, MeterValue, PressureKind, PressureState, ProgressValue,
    RecoveryState,
};

use super::{
    ActionVerdict, ActivityMember, ActivitySummary, JobSummary, PressureAction, PressureCause,
    PressureControl, RecoveryItem, ResourceSummary, ServiceSummary, Switchboard, SwitchboardAction,
    SwitchboardModel, SystemAction, TaskSummary,
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
/// The three resources deliberately span the band's honest range: CPU is
/// measured with a plotted history, Memory is measured with a plain track (no
/// history), and Disk is left honestly unmeasured — exactly the "quiet meter"
/// default a host with no wired query must fall back to.
pub(super) fn model() -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for i in 0..50 {
        m.tasks.push(TaskSummary {
            name: alloc::format!("task {i}"),
            detail: alloc::format!("{i}%"),
            pressure: if i % 3 == 0 {
                PressureState::Under(PressureKind::Cpu)
            } else {
                PressureState::None
            },
            activity: ActivityState::Progress(ProgressValue::new(500)),
            recovery: RecoveryState::None,
            action: alloc::string::String::from("End"),
            action_allowed: true,
            group: None,
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
    m.pressure = (0..8).map(model_pressure_cause).collect();
    m.activities = (0..6).map(model_activity).collect();
    for i in 0..6 {
        m.recovery.push(RecoveryItem {
            name: alloc::format!("hung {i}"),
            detail: alloc::string::String::from("not responding"),
            recovery: RecoveryState::Hung,
            can_restart: true,
            can_force: true,
        });
    }
    m.resources.push(
        ResourceSummary::new(
            "CPU",
            "62%",
            PressureKind::Cpu,
            ActivityState::Progress(ProgressValue::new(620)),
        )
        .with_meter(
            MeterValue::Measured(ProgressValue::new(620)),
            PressureState::Under(PressureKind::Cpu),
            [100, 300, 500, 620],
        ),
    );
    m.resources.push(
        ResourceSummary::new(
            "Memory",
            "8.6 GB / 16 GB",
            PressureKind::Memory,
            ActivityState::Progress(ProgressValue::new(538)),
        )
        .with_meter(
            MeterValue::Measured(ProgressValue::new(538)),
            PressureState::None,
            [],
        ),
    );
    m.resources.push(ResourceSummary::new(
        "Disk",
        "72%",
        PressureKind::Disk,
        ActivityState::Progress(ProgressValue::new(720)),
    ));
    for i in 0..10 {
        m.services.push(ServiceSummary {
            name: alloc::format!("svc {i}"),
            detail: alloc::string::String::from("running"),
            recovery: RecoveryState::None,
            action: alloc::string::String::from("Restart"),
            action_allowed: true,
        });
    }
    m.system_actions.push(SystemAction {
        label: alloc::string::String::from("Lock"),
        role: ControlRole::System,
        allowed: true,
    });
    m.system_actions.push(SystemAction {
        label: alloc::string::String::from("Shut Down"),
        role: ControlRole::Destructive,
        allowed: true,
    });
    m
}

/// The CPU pressure cause at `index` of the populated model: Working, blamed
/// on task `index`, offering a recommended Pause, Lower priority, and Show
/// tasks — all Ready.
pub(super) fn model_pressure_cause(index: usize) -> PressureCause {
    PressureCause {
        resource: alloc::string::String::from("CPU"),
        kind: PressureKind::Cpu,
        culprit: alloc::format!("culprit {index}"),
        cause: alloc::string::String::from("busy loop"),
        activity: ActivityState::Working,
        task_index: Some(index),
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
/// working and one idle member.
pub(super) fn model_activity(index: u64) -> ActivitySummary {
    ActivitySummary {
        id: 100 + index,
        name: alloc::format!("activity {index}"),
        detail: alloc::string::String::from("2 tasks"),
        activity: ActivityState::Working,
        paused: index % 2 == 1,
        can_control: true,
        can_accept_member: true,
        members: alloc::vec![
            ActivityMember {
                name: alloc::format!("member {index}.0"),
                detail: alloc::string::String::from("running"),
                activity: ActivityState::Working,
            },
            ActivityMember {
                name: alloc::format!("member {index}.1"),
                detail: alloc::string::String::from("idle"),
                activity: ActivityState::Idle,
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

pub(super) fn has_ink(surface: &Surface, rect: Rect) -> bool {
    (rect.left()..rect.right()).any(|x| {
        (rect.top()..rect.bottom()).any(|y| {
            let (xu, yu) = (u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0));
            surface.get(xu, yu).is_some_and(|p| p.a > 0)
        })
    })
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
        if let Some(action) = sb.on_pointer(&event, b, scale, theme, font()) {
            out.push(action);
        }
    }
    out
}
