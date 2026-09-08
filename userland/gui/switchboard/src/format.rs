//! The one place a figure becomes display text.
//!
//! Every byte count, throughput rate and elapsed duration this crate shows
//! is spelled here, so a size in the task table, a capacity on the storage
//! page and a memory total in a pressure card cannot drift into three
//! different renderings of the same number. A screen that needs a figure
//! written out calls one of these; it never writes its own.

use alloc::format;
use alloc::string::String;

use tairix_abi::Duration64;

/// The binary units a byte count is scaled through, smallest first.
const BYTE_UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/// A byte count in the largest binary unit that keeps it under four
/// digits, with one decimal place above a kibibyte (`"1.9 GiB"`) and whole
/// bytes below it (`"512 B"`).
///
/// One decimal is the most precision a scaled figure earns: a reader
/// comparing two volumes needs the magnitude and one significant place,
/// and more digits imply an accuracy the underlying block counts do not
/// have. A count beyond the last unit saturates in that unit rather than
/// wrapping to a smaller, misleading number.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    let mut scale = 1u64;
    let mut unit = 0usize;
    while bytes / scale >= 1024 && unit + 1 < BYTE_UNITS.len() {
        scale = scale.saturating_mul(1024);
        unit = unit.saturating_add(1);
    }
    let name = BYTE_UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        return format!("{bytes} {name}");
    }
    let whole = bytes / scale;
    let tenths = (bytes % scale).saturating_mul(10) / scale;
    format!("{whole}.{tenths} {name}")
}

/// A bytes-per-second rate in the same units as a byte count.
#[must_use]
pub fn format_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

/// The decimal units a pixel count is scaled through, smallest first.
const PIXEL_UNITS: [&str; 4] = ["", "k", "M", "G"];

/// Where `pixels` lands on the decimal ladder: the divisor that brings it
/// under four digits, and the magnitude prefix that divisor stands for.
///
/// Decimal rather than binary: a pixel count is a screen area, so a reader
/// compares it against a resolution they know in millions, not mebibytes.
/// A count beyond the last unit saturates in that unit rather than wrapping
/// to a smaller, misleading number.
fn pixel_scale(pixels: u64) -> (u64, &'static str) {
    let mut scale = 1u64;
    let mut unit = 0usize;
    while pixels / scale >= 1000 && unit + 1 < PIXEL_UNITS.len() {
        scale = scale.saturating_mul(1000);
        unit = unit.saturating_add(1);
    }
    (scale, PIXEL_UNITS.get(unit).copied().unwrap_or(""))
}

/// A pixel count in the largest decimal unit that keeps it under four
/// digits, with one decimal place above a thousand (`"2.0M px"`) and whole
/// pixels below it (`"512 px"`).
#[must_use]
pub fn format_pixels(pixels: u64) -> String {
    let (scale, name) = pixel_scale(pixels);
    if name.is_empty() {
        return format!("{pixels} px");
    }
    let whole = pixels / scale;
    let tenths = (pixels % scale).saturating_mul(10) / scale;
    format!("{whole}.{tenths}{name} px")
}

/// A pixel count as the figure a hero reads and the unit that trails it:
/// `4_200_000` → `("4.2", "M px")`, `512` → `("512", "px")`.
///
/// The magnitude prefix belongs to the unit, not the figure: a hero reads as
/// one number with its unit beside it, so a figure spelled `"4.2M px"` would
/// put two thirds of a unit in the headline and the rest beside it.
#[must_use]
pub fn pixel_parts(pixels: u64) -> (String, String) {
    let (scale, name) = pixel_scale(pixels);
    if name.is_empty() {
        return (format!("{pixels}"), String::from("px"));
    }
    let whole = pixels / scale;
    let tenths = (pixels % scale).saturating_mul(10) / scale;
    (format!("{whole}.{tenths}"), format!("{name} px"))
}

/// A permille fraction as whole-percent digits, with no unit (`"92"`).
///
/// What a hero's figure reads, because a pane's headline carries its unit
/// separately and beside it — a figure spelled with its own `%` would draw
/// `18% % busy`.
///
/// Whole percent is the precision a share sampled over one interval earns:
/// a tenth of a percent would imply an accuracy the counters behind it do
/// not have. A total summed across several tasks may legitimately exceed
/// `100%` on more than one core, so nothing is clamped here — a figure the
/// caller measured is shown as measured.
#[must_use]
pub fn whole_percent(permille: u16) -> String {
    format!("{}", permille / 10)
}

/// A permille fraction as whole-percent display text (`"92%"`).
///
/// What a reading that carries its own unit reads — a rail entry, a
/// per-core cell, a consumer row — at the precision
/// [`whole_percent`] states.
#[must_use]
pub fn percent(permille: u16) -> String {
    format!("{}%", whole_percent(permille))
}

/// The decimal units a latency is scaled through, smallest first.
const LATENCY_UNITS: [&str; 4] = ["ns", "us", "ms", "s"];

/// A nanosecond latency in the largest decimal unit that keeps it under four
/// digits, with one decimal place above nanoseconds (`"140.0 us"`).
///
/// Scaled like a byte count and for the same reason: a reader comparing two
/// volumes needs the magnitude and one significant place, and more digits
/// would imply an accuracy a figure derived from one interval's delta does
/// not have.
#[must_use]
pub fn format_latency(nanos: u64) -> String {
    let mut scale = 1u64;
    let mut unit = 0usize;
    while nanos / scale >= 1000 && unit + 1 < LATENCY_UNITS.len() {
        scale = scale.saturating_mul(1000);
        unit = unit.saturating_add(1);
    }
    let name = LATENCY_UNITS.get(unit).copied().unwrap_or("ns");
    if unit == 0 {
        return format!("{nanos} {name}");
    }
    let whole = nanos / scale;
    let tenths = (nanos % scale).saturating_mul(10) / scale;
    format!("{whole}.{tenths} {name}")
}

/// An elapsed duration in days, hours and minutes, dropping the units that
/// are nought so a machine up for four minutes does not read
/// `"0d 0h 4m"`.
///
/// Seconds appear only below a minute, where they are the whole reading:
/// an uptime measured to the second implies a precision that a figure
/// sampled seconds ago does not have. A negative duration — a clock that
/// moved backwards — reads as no elapsed time rather than as a wrapped
/// enormous one.
#[must_use]
pub fn format_duration(duration: Duration64) -> String {
    let seconds = duration.secs().max(0).unsigned_abs();
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        return format!("{days}d {hours}h {minutes}m");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    if minutes > 0 {
        return format!("{minutes}m");
    }
    format!("{seconds}s")
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
