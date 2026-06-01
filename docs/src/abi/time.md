# 64-bit-native time (`abi-v1`)

RustOS is 64-bit-time-native (`AGENTS.md` §21). No kernel ABI, userland
ABI, IPC type, log format, native filesystem, or persistent OS metadata
may store absolute time as 32-bit seconds. The canonical time types live
in `lib/abi/src/time.rs` (`rustos_abi::time`).

## The types

- [`Time64`] — an absolute instant: signed 64-bit seconds since the Unix
  epoch plus a nanosecond field in `0..`[`NANOS_PER_SEC`]. It is RustOS's
  equivalent of Linux's `timespec64` (seconds *and* nanoseconds), not a
  seconds-only `time64_t`.
- [`Duration64`] — a span of time: signed 64-bit seconds plus the same
  canonical nanosecond field. [`Duration64::from_nanos`] splits a
  monotonic nanosecond count (e.g. nanoseconds since boot) exactly.

Both keep the sub-second component canonical (`0..NANOS_PER_SEC`), so the
derived ordering is chronological and the 12-byte little-endian wire
encoding (8-byte seconds + 4-byte nanoseconds) is unambiguous. Pointer
width is not time width: neither type is ever a `usize`, `u32`, or
`time_t` on the wire.

## Checked narrowing

Converting to a narrower representation is always *checked*. A legacy
on-disk timestamp field calls [`Time64::secs_i32`] or
[`Time64::secs_u32`] rather than casting; a value outside the target
range fails with [`Errno::TimestampOutOfRange`]. Silent truncation,
wrapping, saturation, or timezone guessing is forbidden (§21). Filesystem
drivers therefore preserve the widest range and precision their on-disk
format supports and surface an out-of-range exact-preservation write as
an error rather than corrupting the stored time.

## Tests

Unit tests cover the epoch, dates before 1970, dates past the 2038 (`i32`)
and 2106 (`u32`) boundaries, non-canonical nanosecond rejection, and the
checked narrowing at both ends. Both decoders are also driven by the
`lib/abi` fuzz harness (`AGENTS.md` §19.6).

[`Time64`]: ../../rustos_abi/time/struct.Time64.html
[`Duration64`]: ../../rustos_abi/time/struct.Duration64.html
[`Duration64::from_nanos`]: ../../rustos_abi/time/struct.Duration64.html#method.from_nanos
[`Time64::secs_i32`]: ../../rustos_abi/time/struct.Time64.html#method.secs_i32
[`Time64::secs_u32`]: ../../rustos_abi/time/struct.Time64.html#method.secs_u32
[`NANOS_PER_SEC`]: ../../rustos_abi/time/constant.NANOS_PER_SEC.html
[`Errno::TimestampOutOfRange`]: ../../rustos_abi/error/enum.Errno.html
