//! No-allocation numeric formatters used to attach numeric identifiers
//! to structured log records.
//!
//! These helpers were extracted from `kernel/sec` once a second
//! consumer (`kernel/ipc`) needed them.
//! Both crates render task / port / capability identifiers into
//! `lib/log`'s structured field values without touching an
//! allocator on the hot path; the helpers therefore live here so the
//! code exists in exactly one place. `lib/util` deliberately has no
//! direct dependency on `lib/log` (none of its items take a
//! `tairix_log::Field`), so the cross-crate type is named in prose
//! rather than as an intra-doc link.
//!
//! Every function is total, panic-free, and writes only ASCII bytes
//! into the caller's stack buffer.

/// Render `value` into `buf` as decimal text and return the populated
/// sub-slice.
///
/// `buf` is sized so the largest `i32` plus its sign fits without
/// panicking. The function never panics — including for `i32::MIN`,
/// whose magnitude is handled by widening to `i64` before negation.
#[must_use]
pub fn format_i32(value: i32, buf: &mut [u8; 12]) -> &str {
    let negative = value < 0;
    // Use the `u32` unsigned magnitude so `i32::MIN.abs()` does not
    // overflow. Two-step widening keeps the bit pattern intact.
    let mut n: u32 = if negative {
        // `value as i64` widens losslessly; negating the widened value
        // and casting back to `u32` yields the absolute magnitude.
        let widened = -i64::from(value);
        // The magnitude of any `i32` (including `i32::MIN`) fits in a
        // `u32` exactly: `-i32::MIN as i64 == 2^31` is representable.
        // The truncation and sign-loss are therefore provably lossless.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            widened as u32
        }
    } else {
        // A non-negative `i32` fits in `u32` by construction.
        #[allow(clippy::cast_sign_loss)]
        {
            value as u32
        }
    };
    let mut tmp = [0u8; 10];
    let mut pos = tmp.len();
    if n == 0 {
        pos -= 1;
        tmp[pos] = b'0';
    } else {
        while n > 0 {
            pos -= 1;
            // `n % 10` is in `0..=9`, the cast to `u8` is lossless.
            #[allow(clippy::cast_possible_truncation)]
            {
                tmp[pos] = b'0' + (n % 10) as u8;
            }
            n /= 10;
        }
    }
    let mut out_pos = 0;
    if negative {
        buf[out_pos] = b'-';
        out_pos += 1;
    }
    let digits = &tmp[pos..];
    buf[out_pos..out_pos + digits.len()].copy_from_slice(digits);
    out_pos += digits.len();
    // SAFETY-INVARIANT: every byte written above is ASCII (`'-'` or
    // `b'0'..=b'9'`), so the resulting slice is valid UTF-8 by
    // construction. Confirmed by the unit tests below.
    core::str::from_utf8(&buf[..out_pos]).unwrap_or("?")
}

/// Saturating decimal formatter for `usize`.
///
/// Counts greater than `i32::MAX` are clamped to `i32::MAX` rather
/// than overflowing or panicking. Used for "how many capabilities did
/// we install" / "how many bytes are buffered" style audit fields.
#[must_use]
pub fn format_usize(value: usize, buf: &mut [u8; 12]) -> &str {
    let clamped = i32::try_from(value).unwrap_or(i32::MAX);
    format_i32(clamped, buf)
}

/// Render `value` into `buf` as decimal text and return the populated
/// sub-slice.
///
/// Full-range and total: every `u64` — including `u64::MAX`
/// (`"18446744073709551615"`, 20 digits) — renders without clamping,
/// panicking, or allocating. Unlike [`format_usize`] this never saturates
/// and is `usize`-width-independent, so a 64-bit quantity (a duration in
/// milliseconds, a byte count) is rendered faithfully on every target,
/// including the 32-bit `usize` of `wasm32`.
#[must_use]
pub fn format_u64(value: u64, buf: &mut [u8; 20]) -> &str {
    let mut n = value;
    let mut pos = buf.len();
    if n == 0 {
        pos -= 1;
        buf[pos] = b'0';
    } else {
        while n > 0 {
            pos -= 1;
            // `n % 10` is in `0..=9`, the cast to `u8` is lossless.
            #[allow(clippy::cast_possible_truncation)]
            {
                buf[pos] = b'0' + (n % 10) as u8;
            }
            n /= 10;
        }
    }
    // SAFETY-INVARIANT: every byte written above is `b'0'..=b'9'`, so the
    // slice is valid UTF-8 by construction. Confirmed by the unit tests.
    core::str::from_utf8(&buf[pos..]).unwrap_or("?")
}

/// Render `value` into `buf` as a fixed-width 16-nibble lowercase hex
/// string and return the populated sub-slice.
///
/// Used to attach opaque numeric identifiers (task ids, port ids,
/// signer key fingerprints) to audit records without revealing
/// structure that would help an attacker correlate them.
#[must_use]
pub fn format_hex_u64(value: u64, buf: &mut [u8; 16]) -> &str {
    for (i, slot) in buf.iter_mut().enumerate() {
        // `(value >> shift) & 0xF` is in `0..=15`; cast to `u8` is lossless.
        #[allow(clippy::cast_possible_truncation)]
        let nibble = ((value >> ((15 - i) * 4)) & 0xF) as u8;
        *slot = hex_nibble(nibble);
    }
    core::str::from_utf8(&buf[..]).unwrap_or("?")
}

/// Bytes [`format_hex_offset`] writes: `+0x` plus 16 nibbles.
pub const HEX_OFFSET_LEN: usize = 19;

/// Render `offset` into `buf` as `+0x` followed by fixed-width 16-nibble
/// lowercase hex, and return the populated sub-slice.
///
/// The leading `+` is what stops a reader mistaking a diagnostic's code
/// address for an absolute runtime one: every address on a kernel or user
/// post-mortem record is expressed relative to a load base, so the marker
/// is the difference between an offline `addr2line` input and a disclosed
/// load address.
#[must_use]
pub fn format_hex_offset(offset: u64, buf: &mut [u8; HEX_OFFSET_LEN]) -> &str {
    buf[0] = b'+';
    buf[1] = b'0';
    buf[2] = b'x';
    let mut hex = [0u8; 16];
    let rendered = format_hex_u64(offset, &mut hex);
    buf[3..].copy_from_slice(rendered.as_bytes());
    core::str::from_utf8(&buf[..]).unwrap_or("+0x")
}

/// Bytes [`format_hex_offset_list`] needs per rendered offset: one offset
/// plus its separating comma. The trailing comma is never written, so a
/// buffer of `n * HEX_OFFSET_STRIDE` always holds `n` offsets.
pub const HEX_OFFSET_STRIDE: usize = HEX_OFFSET_LEN + 1;

/// Render `offsets` into `buf` as a comma-separated list of
/// [`format_hex_offset`] values, and return the populated sub-slice.
///
/// The one spelling of a rendered backtrace, so a kernel post-mortem and a
/// user-space stall report cannot disagree about how a frame chain reads.
/// Renders as many whole offsets as `buf` holds and stops, so a caller with
/// a fixed field buffer gets a prefix rather than nothing.
#[must_use]
pub fn format_hex_offset_list<'b>(offsets: &[u64], buf: &'b mut [u8]) -> &'b str {
    let mut used = 0;
    for &offset in offsets {
        let separator = usize::from(used > 0);
        if used + separator + HEX_OFFSET_LEN > buf.len() {
            break;
        }
        if separator == 1 {
            buf[used] = b',';
            used += 1;
        }
        let mut one = [0u8; HEX_OFFSET_LEN];
        let text = format_hex_offset(offset, &mut one);
        buf[used..used + HEX_OFFSET_LEN].copy_from_slice(text.as_bytes());
        used += HEX_OFFSET_LEN;
    }
    core::str::from_utf8(&buf[..used]).unwrap_or("")
}

/// Render `bytes` into `buf` as lowercase hex, two characters per byte, and
/// return the populated sub-slice.
///
/// Used to attach an opaque byte blob — a device descriptor, a wire header —
/// to a diagnostic record. Renders as many whole bytes as `buf` holds and
/// stops, so a caller with a fixed field buffer gets a prefix rather than
/// nothing; the caller chunks a longer blob across records.
#[must_use]
pub fn format_hex_bytes<'b>(bytes: &[u8], buf: &'b mut [u8]) -> &'b str {
    // The two-character slots number `buf.len() / 2`, so the zip stops at
    // whichever of the two runs out and `rendered` is exactly its length.
    let rendered = bytes.len().min(buf.len() / 2);
    let (slots, _odd_trailing_byte) = buf.as_chunks_mut::<2>();
    for (byte, slot) in bytes.iter().zip(slots.iter_mut()) {
        slot[0] = hex_nibble(byte >> 4);
        slot[1] = hex_nibble(byte & 0xF);
    }
    core::str::from_utf8(&buf[..rendered * 2]).unwrap_or("?")
}

/// One nibble as its lowercase hex digit — the single hex spelling both
/// renderers above share.
///
/// A value above `0xF` cannot reach here from a masked shift; answering `?`
/// rather than indexing past the digits keeps the function total.
const fn hex_nibble(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        0xA..=0xF => b'a' + (nibble - 0xA),
        _ => b'?',
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_hex_bytes, format_hex_offset, format_hex_offset_list, format_hex_u64, format_i32,
        format_u64, format_usize, HEX_OFFSET_LEN, HEX_OFFSET_STRIDE,
    };

    #[test]
    fn format_i32_zero() {
        let mut buf = [0u8; 12];
        assert_eq!(format_i32(0, &mut buf), "0");
    }

    #[test]
    fn format_i32_positive() {
        let mut buf = [0u8; 12];
        assert_eq!(format_i32(12_345, &mut buf), "12345");
    }

    #[test]
    fn format_i32_negative() {
        let mut buf = [0u8; 12];
        assert_eq!(format_i32(-7, &mut buf), "-7");
    }

    #[test]
    fn format_i32_min_does_not_panic() {
        let mut buf = [0u8; 12];
        assert_eq!(format_i32(i32::MIN, &mut buf), "-2147483648");
    }

    #[test]
    fn format_i32_max() {
        let mut buf = [0u8; 12];
        assert_eq!(format_i32(i32::MAX, &mut buf), "2147483647");
    }

    #[test]
    fn format_usize_saturates_above_i32_max() {
        let mut buf = [0u8; 12];
        // Anything above `i32::MAX` clamps; the renderer must not panic.
        assert_eq!(
            format_usize(usize::MAX, &mut buf),
            format_i32(i32::MAX, &mut [0u8; 12]),
        );
    }

    #[test]
    fn format_usize_normal_value() {
        let mut buf = [0u8; 12];
        assert_eq!(format_usize(42, &mut buf), "42");
    }

    #[test]
    fn format_u64_zero() {
        let mut buf = [0u8; 20];
        assert_eq!(format_u64(0, &mut buf), "0");
    }

    #[test]
    fn format_u64_normal_value() {
        let mut buf = [0u8; 20];
        assert_eq!(format_u64(1_234_567_890, &mut buf), "1234567890");
    }

    #[test]
    fn format_u64_above_usize32_and_i32_max_is_not_clamped() {
        let mut buf = [0u8; 20];
        // Well beyond `i32::MAX`/`u32::MAX`, so it would clamp or truncate
        // through the `usize`/`i32` formatters: the full-range renderer
        // must reproduce it exactly.
        assert_eq!(format_u64(10_000_000_000, &mut buf), "10000000000");
    }

    #[test]
    fn format_u64_max() {
        let mut buf = [0u8; 20];
        assert_eq!(format_u64(u64::MAX, &mut buf), "18446744073709551615");
    }

    #[test]
    fn format_hex_u64_zero_is_sixteen_zeros() {
        let mut buf = [0u8; 16];
        assert_eq!(format_hex_u64(0, &mut buf), "0000000000000000");
    }

    #[test]
    fn format_hex_u64_full_range() {
        let mut buf = [0u8; 16];
        assert_eq!(
            format_hex_u64(0x0123_4567_89ab_cdef, &mut buf),
            "0123456789abcdef"
        );
    }

    #[test]
    fn format_hex_u64_high_bit_set() {
        let mut buf = [0u8; 16];
        assert_eq!(format_hex_u64(u64::MAX, &mut buf), "ffffffffffffffff");
    }

    #[test]
    fn format_hex_bytes_renders_two_lowercase_characters_per_byte() {
        let mut buf = [0u8; 8];
        assert_eq!(
            format_hex_bytes(&[0x05, 0x01, 0x09, 0xff], &mut buf),
            "050109ff"
        );
    }

    #[test]
    fn format_hex_bytes_of_nothing_is_empty() {
        let mut buf = [0u8; 8];
        assert_eq!(format_hex_bytes(&[], &mut buf), "");
    }

    #[test]
    fn format_hex_bytes_renders_whole_bytes_only_when_the_buffer_runs_out() {
        // Five characters hold two whole bytes; the third is not half-rendered.
        let mut buf = [0u8; 5];
        assert_eq!(format_hex_bytes(&[0xde, 0xad, 0xbe], &mut buf), "dead");
    }

    #[test]
    fn format_hex_bytes_covers_every_nibble() {
        let mut buf = [0u8; 16];
        assert_eq!(
            format_hex_bytes(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef], &mut buf),
            "0123456789abcdef"
        );
    }

    #[test]
    fn format_hex_bytes_into_an_unusable_buffer_is_empty() {
        let mut buf = [0u8; 1];
        assert_eq!(format_hex_bytes(&[0xaa], &mut buf), "");
    }

    #[test]
    fn format_hex_offset_marks_the_value_as_relative() {
        let mut buf = [0u8; HEX_OFFSET_LEN];
        assert_eq!(
            format_hex_offset(0x1234_5678, &mut buf),
            "+0x0000000012345678"
        );
    }

    #[test]
    fn format_hex_offset_renders_the_extremes_full_width() {
        let mut buf = [0u8; HEX_OFFSET_LEN];
        assert_eq!(format_hex_offset(0, &mut buf), "+0x0000000000000000");
        assert_eq!(format_hex_offset(u64::MAX, &mut buf), "+0xffffffffffffffff");
    }

    #[test]
    fn an_offset_list_joins_with_commas_and_no_trailing_one() {
        let mut buf = [0u8; 3 * HEX_OFFSET_STRIDE];
        assert_eq!(
            format_hex_offset_list(&[1, 2], &mut buf),
            "+0x0000000000000001,+0x0000000000000002"
        );
    }

    #[test]
    fn an_empty_offset_list_renders_nothing() {
        let mut buf = [0u8; HEX_OFFSET_STRIDE];
        assert_eq!(format_hex_offset_list(&[], &mut buf), "");
    }

    #[test]
    fn an_offset_list_renders_a_prefix_rather_than_overrunning_its_buffer() {
        // Room for two offsets and one separator exactly; the third is
        // dropped rather than truncated mid-value.
        let mut buf = [0u8; 2 * HEX_OFFSET_LEN + 1];
        assert_eq!(
            format_hex_offset_list(&[1, 2, 3], &mut buf),
            "+0x0000000000000001,+0x0000000000000002"
        );
        // A buffer too small for even one offset renders nothing.
        let mut tiny = [0u8; HEX_OFFSET_LEN - 1];
        assert_eq!(format_hex_offset_list(&[1], &mut tiny), "");
    }
}
