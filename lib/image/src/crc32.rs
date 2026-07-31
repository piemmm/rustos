//! PNG's own chunk-framing checksum.
//!
//! Every PNG chunk carries a trailing CRC-32 computed over its type and
//! payload bytes (W3C PNG §"CRC algorithm"), using the reflected
//! ISO-HDLC/IEEE-802.3 polynomial — the same algorithm `zip` and `gzip`
//! use, and the checksum PNG itself specifies (`0xEDB8_8320` reflected).
//! This is a **framing** checksum private to this crate's chunk reader: it
//! is unrelated to `lib/crc32c`'s CRC-32C (a different polynomial entirely,
//! used for TAIRiX's own on-disk formats) and to `lib/compress::zlib`'s
//! Adler-32 (zlib's own, distinct, container checksum). A first-party
//! implementation is legitimate here exactly as it is for `lib/crc32c`: it
//! is an error-detecting checksum, not a security primitive, so the
//! charter's "never hand-roll crypto" bar does not apply.

/// The 256-entry lookup table for the reflected, table-driven CRC-32,
/// generated at compile time so no table is hand-transcribed.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    const POLY: u32 = 0xEDB8_8320;
    let mut table = [0u32; 256];
    let mut n = 0u32;
    while n < 256 {
        let mut crc = n;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            k += 1;
        }
        table[n as usize] = crc;
        n += 1;
    }
    table
}

/// The CRC-32 of the concatenation of `parts` (reflected, table-driven,
/// init `0xFFFF_FFFF`, final XOR `0xFFFF_FFFF`) — PNG's chunk-framing
/// checksum, computed over a chunk's type and payload without needing to
/// materialise their concatenation.
pub(crate) fn crc32_of(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
            crc = (crc >> 8) ^ TABLE[index];
        }
    }
    !crc
}

#[cfg(test)]
/// The CRC-32 of a single slice — a thin wrapper over [`crc32_of`] used
/// only by this module's own known-answer tests.
fn crc32(data: &[u8]) -> u32 {
    crc32_of(&[data])
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn matches_the_known_answer_vector() {
        // The standard CRC-32/ISO-HDLC check value, the same algorithm
        // every PNG encoder and decoder computes chunk CRCs with.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn differs_from_a_single_bit_flip() {
        let a = crc32(b"a PNG chunk's type and payload");
        let b = crc32(b"a PNG chunk's Type and payload");
        assert_ne!(a, b);
    }
}
