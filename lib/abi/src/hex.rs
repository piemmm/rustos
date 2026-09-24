//! Lowercase hexadecimal: the one spelling every identifier this crate
//! renders as text, and reads back, uses.

/// The digits, in value order.
pub(crate) const LOWER: &[u8; 16] = b"0123456789abcdef";

/// Render `bytes` into `out` — exactly twice as long — and borrow it as text.
pub(crate) fn encode<'a>(bytes: &[u8], out: &'a mut [u8]) -> &'a str {
    for (pair, byte) in out.as_chunks_mut::<2>().0.iter_mut().zip(bytes) {
        *pair = [
            LOWER[usize::from(byte >> 4)],
            LOWER[usize::from(byte & 0x0F)],
        ];
    }
    // Every byte written is an ASCII digit; an `out` of the wrong length
    // renders as nothing rather than as a partial identifier.
    if out.len() == bytes.len() * 2 {
        core::str::from_utf8(out).unwrap_or("")
    } else {
        ""
    }
}

/// The `N` bytes `text` spells, or `None` for any other length or any byte
/// that is not a lowercase digit.
pub(crate) fn decode<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let value = |digit: u8| LOWER.iter().position(|&d| d == digit);
    let mut out = [0u8; N];
    for (byte, [high, low]) in out.iter_mut().zip(text.as_chunks::<2>().0) {
        let high = u8::try_from(value(*high)?).ok()?;
        let low = u8::try_from(value(*low)?).ok()?;
        *byte = (high << 4) | low;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn every_byte_round_trips_and_only_lowercase_digits_read() {
        let bytes: [u8; 4] = [0x00, 0x7F, 0xA5, 0xFF];
        let mut out = [0u8; 8];
        assert_eq!(encode(&bytes, &mut out), "007fa5ff");
        assert_eq!(decode::<4>(b"007fa5ff"), Some(bytes));
        assert_eq!(
            decode::<4>(b"007FA5FF"),
            None,
            "uppercase is another spelling"
        );
        assert_eq!(decode::<4>(b"007fa5f"), None);
        assert_eq!(decode::<4>(b"007fa5fg"), None);
        assert_eq!(encode(&bytes, &mut [0u8; 7]), "");
    }
}
