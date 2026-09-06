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

/// A pixel count in the largest decimal unit that keeps it under four
/// digits, with one decimal place above a thousand (`"2.0M px"`) and whole
/// pixels below it (`"512 px"`).
///
/// Decimal rather than binary: a pixel count is a screen area, so a reader
/// compares it against a resolution they know in millions, not mebibytes.
/// A count beyond the last unit saturates in that unit rather than wrapping
/// to a smaller, misleading number.
#[must_use]
pub fn format_pixels(pixels: u64) -> String {
    let mut scale = 1u64;
    let mut unit = 0usize;
    while pixels / scale >= 1000 && unit + 1 < PIXEL_UNITS.len() {
        scale = scale.saturating_mul(1000);
        unit = unit.saturating_add(1);
    }
    let name = PIXEL_UNITS.get(unit).copied().unwrap_or("");
    if unit == 0 {
        return format!("{pixels} px");
    }
    let whole = pixels / scale;
    let tenths = (pixels % scale).saturating_mul(10) / scale;
    format!("{whole}.{tenths}{name} px")
}

/// A permille fraction as whole-percent display text (`"92%"`).
///
/// Whole percent is the precision a share sampled over one interval earns:
/// a tenth of a percent would imply an accuracy the counters behind it do
/// not have. A total summed across several tasks may legitimately exceed
/// `100%` on more than one core, so nothing is clamped here — a figure the
/// caller measured is shown as measured.
#[must_use]
pub fn percent(permille: u16) -> String {
    format!("{}%", permille / 10)
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
