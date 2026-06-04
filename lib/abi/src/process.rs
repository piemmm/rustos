//! The process startup vector: what the kernel hands a freshly spawned
//! program.
//!
//! When the loader (`AGENTS.md` §16.5, the `rxe` loader) drops into a freshly
//! created process it materialises a single contiguous *startup-vector block*
//! in the new address space and hands the program's entry trampoline (crt0,
//! `plans/CCOMPAT.md` CC3) a pointer to it. The block carries the program's
//! command-line arguments, its environment, and a per-process random seed for
//! the §19.2 stack canary. This module is the one definition both sides share
//! (`AGENTS.md` §2.2): the kernel *builds* the block and crt0 *parses* it.
//!
//! The block is **position-independent** — every string is referenced by an
//! offset relative to the block base, never an absolute pointer — so it works
//! unchanged wherever the loader places it in a PIE address space
//! (`AGENTS.md` §19.2). It is laid out as:
//!
//! ```text
//! +-----------------------------+  offset 0
//! | ProcessStartHeader          |  ProcessStartHeader::WIRE_LEN bytes
//! +-----------------------------+
//! | StringSlot[argc + envc]     |  StringSlot::WIRE_LEN bytes each
//! +-----------------------------+
//! | string data (no NUL)        |  referenced by the slots above
//! +-----------------------------+  offset total_len
//! ```
//!
//! The argument slots come first (`argc` of them), then the environment slots
//! (`envc` of them). Each [`StringSlot`] gives the byte offset and length of
//! one string in the trailing string region; the strings carry no NUL
//! terminator, so crt0 copies and NUL-terminates them when it builds the C
//! `argv` / `envp` vectors a hosted program expects.
//!
//! [`ProcessStart::parse`] treats the whole block as **untrusted input**
//! (`AGENTS.md` §19.5/§19.6): it bounds-checks every field against the frozen
//! `abi-v1` limits and the declared `total_len`, rejects an embedded NUL (so
//! every string is representable as a C string), and fails closed with an
//! [`Errno`] rather than ever indexing out of range (`AGENTS.md` §2.9).

use crate::le::{read_u32, read_u64};
use crate::Errno;

/// Magic number identifying an `abi-v1` startup-vector block (`"PSV1"`
/// little-endian).
pub const PROCESS_START_MAGIC: u32 = u32::from_le_bytes(*b"PSV1");

/// Maximum number of strings (arguments plus environment entries) a startup
/// vector may carry.
///
/// Bounded so a hostile or buggy spawner cannot ask crt0 to walk an
/// unbounded slot table; far larger than any sensible command line.
pub const PROCESS_START_MAX_STRINGS: u32 = 4096;

/// Maximum length, in bytes, of a single argument or environment string.
pub const PROCESS_START_MAX_STRING_LEN: u32 = 1 << 16;

/// Maximum total size, in bytes, of a startup-vector block.
///
/// Bounds the whole block (header, slot table, and string data together) so a
/// declared `total_len` can never exceed what the loader will materialise.
pub const PROCESS_START_MAX_TOTAL_LEN: u64 = 1 << 24;

/// One string's location within the startup-vector block.
///
/// Both fields are little-endian on the wire. `offset` is measured from the
/// block base (offset 0, the start of the [`ProcessStartHeader`]); `len` is
/// the string's length in bytes, excluding any terminator (the strings are
/// stored without a NUL).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StringSlot {
    /// Byte offset of the string from the block base.
    pub offset: u32,
    /// Length of the string in bytes (no NUL terminator).
    pub len: u32,
}

impl StringSlot {
    /// Encoded size of a [`StringSlot`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub const fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        let offset = self.offset.to_le_bytes();
        let len = self.len.to_le_bytes();
        out[0] = offset[0];
        out[1] = offset[1];
        out[2] = offset[2];
        out[3] = offset[3];
        out[4] = len[0];
        out[5] = len[1];
        out[6] = len[2];
        out[7] = len[3];
        out
    }

    /// Decode a [`StringSlot`] from the first [`StringSlot::WIRE_LEN`] bytes
    /// of `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            len: read_u32(bytes, 4),
        })
    }
}

/// Fixed prefix of a startup-vector block.
///
/// Total wire size is exactly [`ProcessStartHeader::WIRE_LEN`] bytes, encoded
/// little-endian. The header is followed by `arg_count + env_count`
/// [`StringSlot`] records and then the string data they reference; the whole
/// block is `total_len` bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProcessStartHeader {
    /// Must equal [`PROCESS_START_MAGIC`].
    pub magic: u32,
    /// ABI version of the block format; must be [`crate::ABI_VERSION_CURRENT`].
    pub abi_version: u32,
    /// Number of argument strings (the first `arg_count` slots).
    pub arg_count: u32,
    /// Number of environment strings (the slots after the arguments).
    pub env_count: u32,
    /// Total length of the whole block in bytes (header + slots + strings).
    pub total_len: u64,
    /// Per-process random seed for the §19.2 stack canary.
    ///
    /// The kernel fills this from the platform RNG (`AGENTS.md` §22) when it
    /// builds the block; crt0 installs it as the program's canary. It is not
    /// validated here — any value is structurally acceptable — but the kernel
    /// must supply real entropy.
    pub canary: u64,
}

impl ProcessStartHeader {
    /// Encoded size of a [`ProcessStartHeader`] on the wire.
    pub const WIRE_LEN: usize = 32;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub const fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        let magic = self.magic.to_le_bytes();
        let abi_version = self.abi_version.to_le_bytes();
        let arg_count = self.arg_count.to_le_bytes();
        let env_count = self.env_count.to_le_bytes();
        let total_len = self.total_len.to_le_bytes();
        let canary = self.canary.to_le_bytes();
        out[0] = magic[0];
        out[1] = magic[1];
        out[2] = magic[2];
        out[3] = magic[3];
        out[4] = abi_version[0];
        out[5] = abi_version[1];
        out[6] = abi_version[2];
        out[7] = abi_version[3];
        out[8] = arg_count[0];
        out[9] = arg_count[1];
        out[10] = arg_count[2];
        out[11] = arg_count[3];
        out[12] = env_count[0];
        out[13] = env_count[1];
        out[14] = env_count[2];
        out[15] = env_count[3];
        out[16] = total_len[0];
        out[17] = total_len[1];
        out[18] = total_len[2];
        out[19] = total_len[3];
        out[20] = total_len[4];
        out[21] = total_len[5];
        out[22] = total_len[6];
        out[23] = total_len[7];
        out[24] = canary[0];
        out[25] = canary[1];
        out[26] = canary[2];
        out[27] = canary[3];
        out[28] = canary[4];
        out[29] = canary[5];
        out[30] = canary[6];
        out[31] = canary[7];
        out
    }

    /// Decode a [`ProcessStartHeader`] from the first
    /// [`ProcessStartHeader::WIRE_LEN`] bytes of `bytes`, validating the
    /// magic and ABI version but **not** the trailing slot table or string
    /// data (use [`ProcessStart::parse`] to validate the whole block).
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match.
    /// * [`Errno::AbiVersionUnsupported`] if `abi_version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != PROCESS_START_MAGIC {
            return Err(Errno::BadMagic);
        }
        let abi_version = read_u32(bytes, 4);
        if abi_version != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        Ok(Self {
            magic,
            abi_version,
            arg_count: read_u32(bytes, 8),
            env_count: read_u32(bytes, 12),
            total_len: read_u64(bytes, 16),
            canary: read_u64(bytes, 24),
        })
    }
}

/// A validated, borrowed view over a whole startup-vector block.
///
/// [`ProcessStart::parse`] is the only constructor; it validates every field
/// of the [`ProcessStartHeader`], the `arg_count + env_count` [`StringSlot`]
/// records, and the bytes each slot points at, so the accessors below can
/// never index out of range (`AGENTS.md` §2.9). The view borrows the block; it
/// performs no allocation, so it runs unchanged inside the kernel builder's
/// self-check and inside crt0.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProcessStart<'a> {
    block: &'a [u8],
    header: ProcessStartHeader,
}

impl<'a> ProcessStart<'a> {
    /// Re-export of [`ProcessStartHeader::WIRE_LEN`] for terse internal use.
    const WIRE_LEN: usize = ProcessStartHeader::WIRE_LEN;

    /// Validate `block` as a startup-vector block and return a view over it.
    ///
    /// The block is treated as untrusted input: every offset and length is
    /// checked against the declared `total_len` and the frozen `abi-v1`
    /// limits, and an embedded NUL byte (which would make a string
    /// unrepresentable as a C string) is rejected.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `block` is shorter than the header or
    ///   than the declared `total_len`.
    /// * [`Errno::BadMagic`] / [`Errno::AbiVersionUnsupported`] from
    ///   [`ProcessStartHeader::from_bytes`].
    /// * [`Errno::LengthOutOfRange`] if a count, a string length, or
    ///   `total_len` exceeds its frozen maximum, or if the slot table does
    ///   not fit inside `total_len`.
    /// * [`Errno::OutOfRange`] if a slot points outside the string region,
    ///   its end overflows, or a string contains an embedded NUL byte.
    pub fn parse(block: &'a [u8]) -> Result<Self, Errno> {
        let header = ProcessStartHeader::from_bytes(block)?;

        if header.arg_count > PROCESS_START_MAX_STRINGS
            || header.env_count > PROCESS_START_MAX_STRINGS
        {
            return Err(Errno::LengthOutOfRange);
        }
        // `arg_count` and `env_count` are each <= 4096, so the sum is exact.
        let slot_count = header.arg_count + header.env_count;
        if slot_count > PROCESS_START_MAX_STRINGS {
            return Err(Errno::LengthOutOfRange);
        }

        if header.total_len > PROCESS_START_MAX_TOTAL_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let total_len = usize_or_range(header.total_len)?;
        if block.len() < total_len {
            return Err(Errno::BufferTooSmall);
        }

        // The slot table must fit inside the declared block.
        let slots_bytes = (slot_count as usize)
            .checked_mul(StringSlot::WIRE_LEN)
            .ok_or(Errno::LengthOutOfRange)?;
        let strings_base = Self::WIRE_LEN
            .checked_add(slots_bytes)
            .ok_or(Errno::LengthOutOfRange)?;
        if strings_base > total_len {
            return Err(Errno::LengthOutOfRange);
        }

        // Validate every slot's string lies in the string region, is within
        // the per-string limit, and is free of embedded NULs.
        for i in 0..slot_count as usize {
            let slot_at = Self::WIRE_LEN + i * StringSlot::WIRE_LEN;
            let slot = StringSlot::from_bytes(&block[slot_at..])?;
            if slot.len > PROCESS_START_MAX_STRING_LEN {
                return Err(Errno::LengthOutOfRange);
            }
            let start = slot.offset as usize;
            let end = start
                .checked_add(slot.len as usize)
                .ok_or(Errno::OutOfRange)?;
            if start < strings_base || end > total_len {
                return Err(Errno::OutOfRange);
            }
            if block[start..end].contains(&0) {
                return Err(Errno::OutOfRange);
            }
        }

        Ok(Self { block, header })
    }

    /// The validated header.
    #[must_use]
    pub const fn header(&self) -> ProcessStartHeader {
        self.header
    }

    /// The number of argument strings.
    #[must_use]
    pub const fn arg_count(&self) -> u32 {
        self.header.arg_count
    }

    /// The number of environment strings.
    #[must_use]
    pub const fn env_count(&self) -> u32 {
        self.header.env_count
    }

    /// The per-process stack-canary seed the kernel supplied.
    #[must_use]
    pub const fn canary(&self) -> u64 {
        self.header.canary
    }

    /// The argument string at index `index`, or `None` if out of range.
    ///
    /// The returned bytes carry no NUL terminator (see the module docs).
    #[must_use]
    pub fn arg(&self, index: u32) -> Option<&'a [u8]> {
        if index >= self.header.arg_count {
            return None;
        }
        self.string_at(index)
    }

    /// The environment string at index `index`, or `None` if out of range.
    ///
    /// The returned bytes carry no NUL terminator (see the module docs).
    #[must_use]
    pub fn env(&self, index: u32) -> Option<&'a [u8]> {
        if index >= self.header.env_count {
            return None;
        }
        self.string_at(self.header.arg_count + index)
    }

    /// Resolve the string referenced by slot `slot_index` (arguments first,
    /// then environment). The slot was bounds-checked in [`Self::parse`], so
    /// the indexing below cannot panic.
    fn string_at(&self, slot_index: u32) -> Option<&'a [u8]> {
        let slot_at = Self::WIRE_LEN + (slot_index as usize) * StringSlot::WIRE_LEN;
        let slot = StringSlot::from_bytes(&self.block[slot_at..]).ok()?;
        let start = slot.offset as usize;
        let end = start + slot.len as usize;
        self.block.get(start..end)
    }
}

/// Convert a `u64` byte count to `usize`, failing closed if it does not fit
/// the host word (a 32-bit target cannot address a 4 GiB+ block).
fn usize_or_range(value: u64) -> Result<usize, Errno> {
    usize::try_from(value).map_err(|_| Errno::LengthOutOfRange)
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::{
        ProcessStart, ProcessStartHeader, StringSlot, PROCESS_START_MAGIC,
        PROCESS_START_MAX_STRINGS, PROCESS_START_MAX_STRING_LEN, PROCESS_START_MAX_TOTAL_LEN,
    };
    use crate::{Errno, ABI_VERSION_CURRENT};
    use alloc::vec::Vec;

    /// Build a valid startup-vector block from argument and environment
    /// strings, mirroring what the kernel loader will write.
    fn build(args: &[&[u8]], env: &[&[u8]]) -> Vec<u8> {
        build_with_canary(args, env, 0xDEAD_BEEF_F00D_CAFE)
    }

    fn build_with_canary(args: &[&[u8]], env: &[&[u8]], canary: u64) -> Vec<u8> {
        let slot_count = args.len() + env.len();
        let strings_base = ProcessStartHeader::WIRE_LEN + slot_count * StringSlot::WIRE_LEN;

        let mut slots = Vec::new();
        let mut strings = Vec::new();
        for s in args.iter().chain(env.iter()) {
            let offset = strings_base + strings.len();
            slots.push(StringSlot {
                offset: u32::try_from(offset).expect("offset fits"),
                len: u32::try_from(s.len()).expect("len fits"),
            });
            strings.extend_from_slice(s);
        }
        let total_len = strings_base + strings.len();

        let header = ProcessStartHeader {
            magic: PROCESS_START_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            arg_count: u32::try_from(args.len()).expect("argc fits"),
            env_count: u32::try_from(env.len()).expect("envc fits"),
            total_len: u64::try_from(total_len).expect("total fits"),
            canary,
        };

        let mut block = Vec::new();
        block.extend_from_slice(&header.to_le_bytes());
        for slot in &slots {
            block.extend_from_slice(&slot.to_le_bytes());
        }
        block.extend_from_slice(&strings);
        assert_eq!(block.len(), total_len);
        block
    }

    #[test]
    fn wire_sizes_are_frozen() {
        assert_eq!(ProcessStartHeader::WIRE_LEN, 32);
        assert_eq!(core::mem::size_of::<ProcessStartHeader>(), 32);
        assert_eq!(core::mem::align_of::<ProcessStartHeader>(), 8);
        assert_eq!(StringSlot::WIRE_LEN, 8);
        assert_eq!(core::mem::size_of::<StringSlot>(), 8);
        assert_eq!(core::mem::align_of::<StringSlot>(), 4);
    }

    #[test]
    fn header_round_trips() {
        let header = ProcessStartHeader {
            magic: PROCESS_START_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            arg_count: 3,
            env_count: 2,
            total_len: 1234,
            canary: 0x0123_4567_89AB_CDEF,
        };
        let decoded = ProcessStartHeader::from_bytes(&header.to_le_bytes()).expect("valid");
        assert_eq!(decoded, header);
    }

    #[test]
    fn slot_round_trips() {
        let slot = StringSlot {
            offset: 0xABCD,
            len: 0x42,
        };
        let decoded = StringSlot::from_bytes(&slot.to_le_bytes()).expect("valid");
        assert_eq!(decoded, slot);
    }

    #[test]
    fn parses_args_and_env() {
        let block = build(&[b"prog", b"--flag", b"value"], &[b"PATH=/Apps", b"LANG=C"]);
        let view = ProcessStart::parse(&block).expect("valid block");
        assert_eq!(view.arg_count(), 3);
        assert_eq!(view.env_count(), 2);
        assert_eq!(view.arg(0), Some(&b"prog"[..]));
        assert_eq!(view.arg(1), Some(&b"--flag"[..]));
        assert_eq!(view.arg(2), Some(&b"value"[..]));
        assert_eq!(view.arg(3), None);
        assert_eq!(view.env(0), Some(&b"PATH=/Apps"[..]));
        assert_eq!(view.env(1), Some(&b"LANG=C"[..]));
        assert_eq!(view.env(2), None);
        assert_eq!(view.canary(), 0xDEAD_BEEF_F00D_CAFE);
    }

    #[test]
    fn parses_an_empty_vector() {
        let block = build(&[], &[]);
        let view = ProcessStart::parse(&block).expect("valid empty block");
        assert_eq!(view.arg_count(), 0);
        assert_eq!(view.env_count(), 0);
        assert_eq!(view.arg(0), None);
        assert_eq!(view.env(0), None);
    }

    #[test]
    fn parses_empty_strings() {
        let block = build(&[b""], &[b""]);
        let view = ProcessStart::parse(&block).expect("valid block with empty strings");
        assert_eq!(view.arg(0), Some(&b""[..]));
        assert_eq!(view.env(0), Some(&b""[..]));
    }

    #[test]
    fn preserves_the_canary() {
        let block = build_with_canary(&[b"x"], &[], 0xFEED_FACE_DEAD_C0DE);
        let view = ProcessStart::parse(&block).expect("valid");
        assert_eq!(view.canary(), 0xFEED_FACE_DEAD_C0DE);
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(ProcessStart::parse(&[0u8; 8]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut block = build(&[b"x"], &[]);
        block[0] ^= 0xFF;
        assert_eq!(ProcessStart::parse(&block), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_bad_abi_version() {
        let mut block = build(&[b"x"], &[]);
        block[4] = 99;
        assert_eq!(
            ProcessStart::parse(&block),
            Err(Errno::AbiVersionUnsupported)
        );
    }

    #[test]
    fn rejects_block_shorter_than_total_len() {
        let block = build(&[b"prog"], &[]);
        let truncated = &block[..block.len() - 1];
        assert_eq!(ProcessStart::parse(truncated), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn rejects_oversized_total_len() {
        let mut block = build(&[b"x"], &[]);
        let huge = (PROCESS_START_MAX_TOTAL_LEN + 1).to_le_bytes();
        block[16..24].copy_from_slice(&huge);
        assert_eq!(ProcessStart::parse(&block), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn rejects_too_many_strings() {
        let mut block = build(&[b"x"], &[]);
        let bad = (PROCESS_START_MAX_STRINGS + 1).to_le_bytes();
        block[8..12].copy_from_slice(&bad);
        assert_eq!(ProcessStart::parse(&block), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn rejects_slot_table_overflowing_total_len() {
        // Claim two args but give a total_len that only covers the header.
        let mut block = build(&[b"a", b"b"], &[]);
        let short = (ProcessStartHeader::WIRE_LEN as u64).to_le_bytes();
        block[16..24].copy_from_slice(&short);
        assert_eq!(ProcessStart::parse(&block), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn rejects_slot_pointing_into_the_slot_table() {
        let mut block = build(&[b"abcd"], &[]);
        // Slot 0's offset lives at bytes [WIRE_LEN .. WIRE_LEN+4].
        let slot_off = ProcessStartHeader::WIRE_LEN;
        block[slot_off..slot_off + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(ProcessStart::parse(&block), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_slot_running_past_the_block() {
        let mut block = build(&[b"abcd"], &[]);
        // Inflate slot 0's length so offset+len exceeds total_len.
        let len_off = ProcessStartHeader::WIRE_LEN + 4;
        block[len_off..len_off + 4].copy_from_slice(&0xFFFFu32.to_le_bytes());
        assert_eq!(ProcessStart::parse(&block), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_oversized_string_len() {
        let mut block = build(&[b"abcd"], &[]);
        let len_off = ProcessStartHeader::WIRE_LEN + 4;
        let bad = (PROCESS_START_MAX_STRING_LEN + 1).to_le_bytes();
        block[len_off..len_off + 4].copy_from_slice(&bad);
        assert_eq!(ProcessStart::parse(&block), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn rejects_embedded_nul() {
        let block = build(&[b"a\0b"], &[]);
        assert_eq!(ProcessStart::parse(&block), Err(Errno::OutOfRange));
    }
}
