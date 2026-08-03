//! Render the array and device reports as lines of text.
//!
//! These are pure functions from the ABI records to `String` lines, so every
//! shape a real machine can present — an optimal array, a degraded array with
//! an absent slot, a rebuild in progress, an empty machine, a listing of blank
//! candidate devices — is host-tested without a kernel. The layout tracks
//! Linux `mdadm --detail` / `--examine` where the concept matches, and names
//! the TAIRiX concepts (the array identity, the `node:<id>` device name)
//! where it differs.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_abi::raid::{ArrayHealth, RaidLevel};
use tairix_abi::raid_admin::{
    ArrayUuidBytes, RaidArrayRecord, RaidMemberDisposition, RaidMemberRecord, RAID_SLOT_NONE,
};

/// The tool's reported version, matching the bundle's `AppInfo`.
pub const VERSION: &str = "0.1.0";

/// Column label width for the `--detail` field block: the widest label
/// (`Active Devices`, `Rebuild Status`) right-aligned to the colon.
const DETAIL_LABEL_WIDTH: usize = 14;

/// Render the `--version` line.
#[must_use]
pub fn render_version() -> String {
    format!("mdadm (TAIRiX) {VERSION}")
}

/// Render every array's detail, an array's block separated from the next by a
/// blank line. An empty slice yields no lines (the caller advises the empty
/// machine on fd 3).
#[must_use]
pub fn render_detail(arrays: &[RaidArrayRecord]) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, array) in arrays.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        lines.extend(render_array_detail(array));
    }
    lines
}

/// Render one array's detail block: identity header, then the fields, then any
/// running rebuild or verification position.
#[must_use]
pub fn render_array_detail(array: &RaidArrayRecord) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("{}:", format_identity(&array.array())));
    push_field(&mut lines, "Raid Level", level_name(array.level()));
    push_field(&mut lines, "State", health_name(array.health()));
    push_field(
        &mut lines,
        "Raid Devices",
        &array.member_count().to_string(),
    );
    push_field(
        &mut lines,
        "Active Devices",
        &array.active_members().to_string(),
    );
    if array.level().is_striped() {
        push_field(
            &mut lines,
            "Chunk Size",
            &format!("{} blocks", array.chunk_blocks()),
        );
    }
    push_field(
        &mut lines,
        "Array Size",
        &format!("{} blocks x {} B", array.block_count(), array.block_size()),
    );
    push_field(
        &mut lines,
        "Published As",
        &format!("node:{}", array.node()),
    );
    push_field(&mut lines, "Endpoint", &array.endpoint().to_string());
    push_field(&mut lines, "Generation", &array.generation().to_string());
    if array.resyncing() {
        push_field(
            &mut lines,
            "Rebuild Status",
            &format!("{} / {} blocks", array.resync_cursor(), array.block_count()),
        );
    }
    if array.scrubbing() {
        push_field(
            &mut lines,
            "Scrub Status",
            &format!("{} / {} blocks", array.scrub_cursor(), array.block_count()),
        );
    }
    lines
}

/// Render the `--examine` device listing: a header row, then one row per device
/// the composer holds — array members with their slot and state, and the
/// unaffiliated candidates a new array can be created over. An empty slice
/// yields no lines (the caller advises the empty machine on fd 3).
#[must_use]
pub fn render_examine(members: &[RaidMemberRecord]) -> Vec<String> {
    if members.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(members.len() + 1);
    lines.push(examine_row("Device", "Array", "Slot", "State", "Blocks"));
    for member in members {
        let device = format!("node:{}", member.node());
        let array = if member.is_unaffiliated() {
            String::from("-")
        } else {
            format_identity(&member.array())
        };
        let slot = if member.slot() == RAID_SLOT_NONE {
            String::from("-")
        } else {
            member.slot().to_string()
        };
        lines.push(examine_row(
            &device,
            &array,
            &slot,
            disposition_name(member.disposition()),
            &member.block_count().to_string(),
        ));
    }
    lines
}

/// Format one `--examine` row with the fixed column widths.
fn examine_row(device: &str, array: &str, slot: &str, state: &str, blocks: &str) -> String {
    format!("{device:<14} {array:<32} {slot:>4} {state:<11} {blocks:>14}")
}

/// Append a right-aligned `label : value` field line to a detail block.
fn push_field(lines: &mut Vec<String>, label: &str, value: &str) {
    let mut line = String::new();
    let width = DETAIL_LABEL_WIDTH;
    // Writing into a `String` is infallible; the result is discarded on that
    // basis.
    let _ = write!(line, "{label:>width$} : {value}");
    lines.push(line);
}

/// The 128-bit array identity as 32 lower-case hexadecimal digits — the
/// spelling an array is named by, since there is no `/dev/md*`.
#[must_use]
pub fn format_identity(uuid: &ArrayUuidBytes) -> String {
    let mut out = String::with_capacity(32);
    for byte in uuid {
        // Writing into a `String` is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The Linux-`mdadm` level name for a [`RaidLevel`] (`raid-tp` for triple
/// parity, which has no numeric Linux spelling).
#[must_use]
pub const fn level_name(level: RaidLevel) -> &'static str {
    match level {
        RaidLevel::Mirror => "raid1",
        RaidLevel::Stripe => "raid0",
        RaidLevel::Parity => "raid5",
        RaidLevel::DualParity => "raid6",
        RaidLevel::TripleParity => "raid-tp",
        RaidLevel::Raid10 => "raid10",
    }
}

/// The reported name for an array's health.
#[must_use]
pub const fn health_name(health: ArrayHealth) -> &'static str {
    match health {
        ArrayHealth::Optimal => "optimal",
        ArrayHealth::Degraded => "degraded",
        ArrayHealth::Recovering => "recovering",
        ArrayHealth::Failed => "failed",
    }
}

/// The reported name for a device's disposition.
#[must_use]
pub const fn disposition_name(disposition: RaidMemberDisposition) -> &'static str {
    match disposition {
        RaidMemberDisposition::Candidate => "candidate",
        RaidMemberDisposition::Held => "held",
        RaidMemberDisposition::InSync => "in-sync",
        RaidMemberDisposition::Resyncing => "resyncing",
        RaidMemberDisposition::Faulted => "faulted",
    }
}

#[cfg(test)]
mod tests;
