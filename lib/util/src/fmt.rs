//! No-allocation numeric formatters used to attach numeric identifiers
//! to structured log records.
//!
//! Per, these helpers were extracted from
//! `kernel/sec` once a second consumer (`kernel/ipc`) needed them.
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
        *slot = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
    }
    core::str::from_utf8(&buf[..]).unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::{format_hex_u64, format_i32, format_u64, format_usize};

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
}
