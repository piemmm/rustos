//! The `Machine` group's three panes: what this machine is, who is on it,
//! and what this session may do (`plans/NEW-SWITCHBOARD.md` S4).
//!
//! These are the reports that are genuinely fact lists, so they keep that
//! shape while the resource panes lead with instruments. Their rail entries
//! carry no trace: they are facts, not rates, and the absence of an
//! instrument is what says so.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::{CapabilityId, CapabilityQuery};
use tairix_controls::PressureKind;

use super::{authority_facts, bound, limit_name, machine_facts, reading};
use crate::format::format_duration;
use crate::model::display_name;
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{absence_statement, Reading, ReadingFact, Unmeasured};
use crate::view::resources::{
    BlockBody, DeviceAction, DeviceGroup, DeviceId, PaneBlock, PaneHero, ResourceControl,
    ResourceDevice,
};

/// The machine's identity and how long it has been up.
pub(super) fn identity(sample: &Sample) -> ResourceDevice {
    let uptime = reading(sample, DegradedField::Uptime, sample.uptime, |uptime| {
        format_duration(uptime.since_boot)
    });
    ResourceDevice {
        id: DeviceId::Identity,
        group: DeviceGroup::Machine,
        name: String::from("Identity & uptime"),
        kind: PressureKind::Cpu,
        reading: uptime.clone(),
        trend: Vec::new(),
        hero: PaneHero::facts(hostname(sample), "")
            .with_context(alloc::vec![crate::view::reading::reading_text(&uptime)]),
        blocks: alloc::vec![PaneBlock::full(
            "MACHINE",
            BlockBody::Facts(machine_facts(sample)),
        )],
        banner: None,
        actions: Vec::new(),
    }
}

/// The machine's hostname, as the pane's headline.
fn hostname(sample: &Sample) -> Reading {
    reading(
        sample,
        DegradedField::Identity,
        sample.identity.as_ref(),
        |id| display_name(id.hostname_bytes()),
    )
}

/// The machine's seats and its logged-in census.
pub(super) fn sessions(sample: &Sample) -> ResourceDevice {
    let seats = seat_count(sample);
    ResourceDevice {
        id: DeviceId::Sessions,
        group: DeviceGroup::Machine,
        name: String::from("Sessions & seats"),
        kind: PressureKind::Network,
        reading: seats.clone(),
        trend: Vec::new(),
        hero: PaneHero::facts(seats, "seats"),
        blocks: alloc::vec![
            PaneBlock::half("SEATS", seat_block(sample)),
            PaneBlock::half("CENSUS", BlockBody::Facts(census_facts(sample))),
        ],
        banner: None,
        actions: session_actions(),
    }
}

/// How many seats this machine is configured with.
fn seat_count(sample: &Sample) -> Reading {
    reading(
        sample,
        DegradedField::Seats,
        sample.seats.as_ref(),
        |seats| seats.len().to_string(),
    )
}

/// One row per configured seat, naming who holds it.
fn seat_block(sample: &Sample) -> BlockBody {
    let Some(seats) = sample.seats.as_ref() else {
        return BlockBody::Absence(absence_statement(
            "the seat list",
            Unmeasured::from_absence(sample.absence(DegradedField::Seats)),
        ));
    };
    if seats.is_empty() {
        return BlockBody::Absence(String::from("No seat is configured at this machine."));
    }
    BlockBody::Facts(
        seats
            .iter()
            .map(|seat| {
                ReadingFact::text(
                    format!("Seat {}", seat.seat_id),
                    format!(
                        "console {} · owner task {}",
                        seat.foreground_console, seat.owner_task
                    ),
                )
            })
            .collect(),
    )
}

/// The logged-in census the load reading carries.
fn census_facts(sample: &Sample) -> Vec<ReadingFact> {
    alloc::vec![
        ReadingFact::new(
            "Live tasks",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| load.total_tasks.to_string(),
            ),
        ),
        ReadingFact::new(
            "Runnable",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| load.runnable.to_string(),
            ),
        ),
        ReadingFact::new(
            "Users with a task",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| load.users.to_string(),
            ),
        ),
    ]
}

/// The session and power commands the quick-actions menu offers.
///
/// Each is refused for want of an interface rather than of authority: no
/// power, lock or session-control endpoint exists for this service to drive,
/// so the rail states that plainly instead of offering a button that would
/// do nothing.
fn session_actions() -> Vec<DeviceAction> {
    alloc::vec![
        DeviceAction::absent(ResourceControl::Lock, "Lock"),
        DeviceAction::absent(ResourceControl::LogOut, "Log Out"),
        DeviceAction::absent(ResourceControl::Restart, "Restart"),
        DeviceAction::absent(ResourceControl::ShutDown, "Shut Down")
            .with_role(tairix_controls::ControlRole::Destructive),
    ]
}

/// What this session may do, and the limits it runs under.
pub(super) fn authority(sample: &Sample, caps: &dyn CapabilityQuery) -> ResourceDevice {
    let held = u32::from(caps.holds(CapabilityId::PROC_CONTROL))
        + u32::from(sample.scopes.global_process_scope)
        + u32::from(sample.scopes.memory_pressure)
        + u32::from(sample.scopes.hardware_scope);
    ResourceDevice {
        id: DeviceId::Authority,
        group: DeviceGroup::Machine,
        name: String::from("Permissions & limits"),
        kind: PressureKind::Memory,
        reading: Reading::measured(held.to_string()),
        trend: Vec::new(),
        hero: PaneHero::facts(Reading::measured(held.to_string()), "of 4 held"),
        blocks: alloc::vec![
            PaneBlock::half("AUTHORITY", BlockBody::Facts(authority_facts(sample, caps))),
            PaneBlock::half("RESOURCE LIMITS", limit_block(sample)),
        ],
        banner: None,
        actions: Vec::new(),
    }
}

/// This session's effective limits and its live usage against them.
fn limit_block(sample: &Sample) -> BlockBody {
    let Some(limits) = sample.resource_limits.as_ref() else {
        return BlockBody::Absence(absence_statement(
            "this session's resource limits",
            Unmeasured::from_absence(sample.absence(DegradedField::ResourceLimits)),
        ));
    };
    BlockBody::Facts(
        limits
            .iter()
            .map(|record| {
                ReadingFact::text(
                    limit_name(record.kind),
                    format!(
                        "{} used · {} soft · {} hard",
                        record.usage,
                        bound(record.kind, record.limit.soft),
                        bound(record.kind, record.limit.hard)
                    ),
                )
            })
            .collect(),
    )
}
