//! Stable [`tairix_log::EventId`] constants the console-output path emits.
//!
//! Per the `lib/log` convention every subsystem owns a 1 000-wide reserved
//! range; the console-output queue occupies `18000..19000` (the next free range
//! after `fontd`'s `17000..18000`). Once shipped the numeric values must never
//! be re-used or re-numbered — external capture consumers rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for console-output event identifiers.
pub const CONSOLE_OUT_RANGE_START: u32 = 18_000;
/// Range end (exclusive) reserved for console-output event identifiers.
pub const CONSOLE_OUT_RANGE_END: u32 = 19_000;

/// Console output the queue could not accept was dropped.
///
/// Carries `records`, the whole lines lost, and `bytes`, the payload they
/// carried. Its position in the stream is the position of the gap: everything
/// before it was delivered, everything after it was admitted afterwards.
///
/// This is the record that makes a shed load *visible*. Without it a capture
/// truncated by a console slower than its producers would read as the complete
/// truth, which is how a flood becomes a way to hide what it buried.
pub const CONSOLE_OUTPUT_DROPPED: EventId = EventId(18_001);

// The reserved range is exactly one thousand wide, and every identifier above
// sits inside it. Checked at compile time rather than by a test, so a
// mis-numbered event cannot be built at all, let alone shipped.
const _: () = assert!(CONSOLE_OUT_RANGE_END - CONSOLE_OUT_RANGE_START == 1_000);
const _: () = assert!(CONSOLE_OUTPUT_DROPPED.0 >= CONSOLE_OUT_RANGE_START);
const _: () = assert!(CONSOLE_OUTPUT_DROPPED.0 < CONSOLE_OUT_RANGE_END);
