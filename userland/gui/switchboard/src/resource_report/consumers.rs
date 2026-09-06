//! The top-consumers block the CPU, Memory and volume panes each carry
//! (`plans/NEW-SWITCHBOARD.md` S4).
//!
//! The five tasks costing a device most, from the per-task readings the
//! process record already provides — so a pane and the Tasks table can
//! never disagree about what a task costs.

use alloc::vec::Vec;

use tairix_icon::IconKind;

use crate::format::{format_bytes, format_rate, percent};
use crate::model::TaskMeters;
use crate::sample::{ProcessSummary, Sample};
use crate::view::resources::ConsumerRow;

/// How many consumers a block names.
///
/// A handful is what a reader can act on; the whole list is the Tasks
/// table's job, and the rail's "sort tasks by" command is how a reader gets
/// there.
const CONSUMERS: usize = 5;

/// What every top-consumers block says about itself.
///
/// Summing the tasks on a device is not the device's total: filesystem,
/// RAID and swap traffic belongs to no process, so a reader who added the
/// rows up would be reading a number the system never measured.
pub(super) const NOT_A_TOTAL: &str =
    "A sum of tasks is not the device's total: filesystem, RAID and swap work belongs to no process.";

/// The tasks costing the processor most.
pub(super) fn by_cpu(sample: &Sample) -> Vec<ConsumerRow> {
    rank(
        sample,
        |process| process.cpu_permille.map(u64::from),
        |value| percent(u16::try_from(value).unwrap_or(u16::MAX)),
    )
}

/// The tasks holding the most memory.
pub(super) fn by_memory(sample: &Sample) -> Vec<ConsumerRow> {
    rank(sample, |process| Some(process.mem_bytes), format_bytes)
}

/// The tasks transferring the most to and from storage.
///
/// The rate is the delta `meters` measured between this sample and the last,
/// so a task first seen this sample contributes no rate rather than its
/// whole-of-life total dressed as one.
pub(super) fn by_disk(sample: &Sample, meters: &TaskMeters) -> Vec<ConsumerRow> {
    rank(
        sample,
        |process| meters.disk_rate(process.proc_id),
        format_rate,
    )
}

/// The `CONSUMERS` largest tasks by `cost`, each with its share of the
/// largest so the track compares the tasks with one another.
///
/// A task with no measured cost is left out rather than ranked at nought: a
/// missing reading is not a small one.
fn rank(
    sample: &Sample,
    cost: impl Fn(&ProcessSummary) -> Option<u64>,
    text: impl Fn(u64) -> alloc::string::String,
) -> Vec<ConsumerRow> {
    let mut ranked: Vec<(&ProcessSummary, u64)> = sample
        .processes
        .iter()
        .filter_map(|process| {
            cost(process)
                .filter(|value| *value > 0)
                .map(|v| (process, v))
        })
        .collect();
    // Descending, so the largest consumer leads and sets the track's scale.
    ranked.sort_by_key(|(_, cost)| core::cmp::Reverse(*cost));
    ranked.truncate(CONSUMERS);
    let largest = ranked.first().map_or(0, |(_, value)| *value);
    ranked
        .into_iter()
        .map(|(process, value)| ConsumerRow {
            name: crate::model::display_name(&process.name),
            icon: IconKind::Executable,
            amount: text(value),
            share: share_of(value, largest),
        })
        .collect()
}

/// `value` as a permille of `largest`, full where the two are equal.
fn share_of(value: u64, largest: u64) -> u16 {
    if largest == 0 {
        return 0;
    }
    u16::try_from(value.saturating_mul(1_000) / largest)
        .unwrap_or(1_000)
        .min(1_000)
}
