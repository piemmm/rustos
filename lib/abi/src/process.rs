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

/// Validate the argument and environment counts against the frozen `abi-v1`
/// limits and return the total number of [`StringSlot`] records the block
/// will carry.
///
/// Each of `args` and `env`, and their sum, must be within
/// [`PROCESS_START_MAX_STRINGS`].
fn checked_slot_count(args: &[&[u8]], env: &[&[u8]]) -> Result<usize, Errno> {
    let max = PROCESS_START_MAX_STRINGS as usize;
    if args.len() > max || env.len() > max {
        return Err(Errno::LengthOutOfRange);
    }
    let slot_count = args
        .len()
        .checked_add(env.len())
        .ok_or(Errno::LengthOutOfRange)?;
    if slot_count > max {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(slot_count)
}

/// The exact encoded length, in bytes, of the startup-vector block that
/// [`write_into`] produces for `args` and `env`.
///
/// This is the buffer size the kernel loader must allocate before calling
/// [`write_into`]. It is computed with the same checked arithmetic and the
/// same frozen `abi-v1` limits the builder enforces, so a successful
/// [`encoded_len`] guarantees [`write_into`] will not reject the same inputs
/// for a size reason.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] if the argument or environment count exceeds
/// [`PROCESS_START_MAX_STRINGS`], a single string is longer than
/// [`PROCESS_START_MAX_STRING_LEN`], or the whole block would exceed
/// [`PROCESS_START_MAX_TOTAL_LEN`].
pub fn encoded_len(args: &[&[u8]], env: &[&[u8]]) -> Result<usize, Errno> {
    let slot_count = checked_slot_count(args, env)?;
    let slots_bytes = slot_count
        .checked_mul(StringSlot::WIRE_LEN)
        .ok_or(Errno::LengthOutOfRange)?;
    let strings_base = ProcessStartHeader::WIRE_LEN
        .checked_add(slots_bytes)
        .ok_or(Errno::LengthOutOfRange)?;
    let mut total = strings_base;
    for s in args.iter().chain(env.iter()) {
        if s.len() > PROCESS_START_MAX_STRING_LEN as usize {
            return Err(Errno::LengthOutOfRange);
        }
        total = total.checked_add(s.len()).ok_or(Errno::LengthOutOfRange)?;
    }
    if total as u64 > PROCESS_START_MAX_TOTAL_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(total)
}

/// Build a startup-vector block for `args` / `env` with the per-process
/// stack-canary seed `canary` into `buf`, returning the number of bytes
/// written.
///
/// This is the production builder the §16.5 loader uses: the kernel sizes a
/// buffer with [`encoded_len`], calls this to serialise the block, and copies
/// the written bytes into the new process's address space.
///
/// `lib/abi` performs no allocation, so the caller owns the buffer; `buf` must
/// be at least [`encoded_len`] bytes. The produced block round-trips through
/// [`ProcessStart::parse`].
///
/// # Errors
///
/// * any error from [`encoded_len`] (counts, string length, or total size
///   over the frozen `abi-v1` limits);
/// * [`Errno::BufferTooSmall`] if `buf` is shorter than [`encoded_len`];
/// * [`Errno::OutOfRange`] if any string contains an embedded NUL byte (which
///   would make it unrepresentable as a C string — the same rule
///   [`ProcessStart::parse`] enforces).
pub fn write_into(
    buf: &mut [u8],
    args: &[&[u8]],
    env: &[&[u8]],
    canary: u64,
) -> Result<usize, Errno> {
    let total_len = encoded_len(args, env)?;
    if buf.len() < total_len {
        return Err(Errno::BufferTooSmall);
    }
    for s in args.iter().chain(env.iter()) {
        if s.contains(&0) {
            return Err(Errno::OutOfRange);
        }
    }

    let slot_count = args.len() + env.len();
    let strings_base = ProcessStartHeader::WIRE_LEN + slot_count * StringSlot::WIRE_LEN;

    let header = ProcessStartHeader {
        magic: PROCESS_START_MAGIC,
        abi_version: crate::ABI_VERSION_CURRENT,
        arg_count: u32::try_from(args.len()).map_err(|_| Errno::LengthOutOfRange)?,
        env_count: u32::try_from(env.len()).map_err(|_| Errno::LengthOutOfRange)?,
        total_len: total_len as u64,
        canary,
    };
    buf[..ProcessStartHeader::WIRE_LEN].copy_from_slice(&header.to_le_bytes());

    let mut slot_at = ProcessStartHeader::WIRE_LEN;
    let mut string_off = strings_base;
    for s in args.iter().chain(env.iter()) {
        let slot = StringSlot {
            offset: u32::try_from(string_off).map_err(|_| Errno::LengthOutOfRange)?,
            len: u32::try_from(s.len()).map_err(|_| Errno::LengthOutOfRange)?,
        };
        buf[slot_at..slot_at + StringSlot::WIRE_LEN].copy_from_slice(&slot.to_le_bytes());
        buf[string_off..string_off + s.len()].copy_from_slice(s);
        slot_at += StringSlot::WIRE_LEN;
        string_off += s.len();
    }
    Ok(total_len)
}

/// The four standard file descriptors every process inherits at spawn
/// (`AGENTS.md` §20).
///
/// A program performs **all** of its text I/O over these inherited
/// descriptors and never over a kernel-discovered device: fd 0 reads
/// input, fd 1 writes data, fd 2 writes diagnostics, and fd 3 carries
/// optional structured advisory metadata ([`crate::stdinfo`]). Which
/// kernel *stream backing* each descriptor resolves to is decided by the
/// spawner, never hard-coded into the program (§20 — device
/// independence is a property of the stream layer, not the program).
pub const STDIN: u32 = 0;
/// Primary data output (`AGENTS.md` §20). See [`STDIN`].
pub const STDOUT: u32 = 1;
/// Errors, warnings, and diagnostics (`AGENTS.md` §20). See [`STDIN`].
pub const STDERR: u32 = 2;
/// Optional structured advisory metadata (`AGENTS.md` §20, [`crate::stdinfo`]).
/// See [`STDIN`].
pub const STDINFO: u32 = 3;

/// Number of standard file descriptors the process ABI reserves
/// (`AGENTS.md` §20): exactly fd 0/1/2/3.
pub const STD_STREAM_COUNT: usize = 4;

/// The access a single inherited descriptor grants its process.
///
/// A descriptor is established at spawn and points at a kernel *stream
/// backing* object (`AGENTS.md` §20). [`StreamMode`] records the
/// direction that backing supports for the owning process; a
/// [`stream_read`](crate::SyscallNumber::STREAM_READ) /
/// [`stream_write`](crate::SyscallNumber::STREAM_WRITE) against a
/// descriptor whose mode does not permit the direction fails closed
/// (§5.4) rather than reaching a device the program was never granted.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StreamMode {
    /// No backing is attached to this descriptor: every access denies
    /// (`AGENTS.md` §5.4 — fail closed; §20 — no fallback to a device).
    Closed = 0,
    /// The descriptor is readable (a `stream_read` source). Writes deny.
    Read = 1,
    /// The descriptor is writable (a `stream_write` sink). Reads deny.
    Write = 2,
}

/// One process's standard-stream descriptor table (`AGENTS.md` §20).
///
/// A fixed table of [`STD_STREAM_COUNT`] entries, one per standard
/// descriptor (fd 0/1/2/3), recording the access each inherited stream
/// grants. The spawner establishes it when it creates the process; the
/// kernel consults it to resolve a `stream_read` / `stream_write` fd to
/// its backing's direction (`AGENTS.md` §20 — the descriptor table, not
/// an ambient device, is the authority). The table is small and `Copy`;
/// the backing objects themselves live kernel-side.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DescriptorTable {
    modes: [StreamMode; STD_STREAM_COUNT],
}

impl DescriptorTable {
    /// A table with every standard descriptor [`Closed`](StreamMode::Closed).
    ///
    /// The fail-closed default (`AGENTS.md` §5.4): a process with no
    /// inherited streams can reach no backing until the spawner attaches
    /// one. This is also what an unregistered task resolves to.
    #[must_use]
    pub const fn closed() -> Self {
        Self {
            modes: [StreamMode::Closed; STD_STREAM_COUNT],
        }
    }

    /// The standard text-I/O table: fd 0 readable, fd 1/2/3 writable.
    ///
    /// The shape every bootstrap-session process inherits (`AGENTS.md`
    /// §20): `stdin` is the input source, `stdout`/`stderr`/`stdinfo` are
    /// output sinks. The spawner backs these descriptors with the
    /// discovered console during early bring-up (`plans/PI.md` P6e-3a),
    /// but the program only ever names the fd numbers.
    #[must_use]
    pub const fn standard() -> Self {
        let mut modes = [StreamMode::Closed; STD_STREAM_COUNT];
        modes[STDIN as usize] = StreamMode::Read;
        modes[STDOUT as usize] = StreamMode::Write;
        modes[STDERR as usize] = StreamMode::Write;
        modes[STDINFO as usize] = StreamMode::Write;
        Self { modes }
    }

    /// The [`StreamMode`] of descriptor `fd`, or
    /// [`StreamMode::Closed`] when `fd` is not one of the standard
    /// descriptors (`fd >= STD_STREAM_COUNT`).
    ///
    /// An out-of-range descriptor resolves to `Closed` so the kernel
    /// fails it closed exactly as it would a closed standard descriptor,
    /// without leaking that the index was out of range (`AGENTS.md`
    /// §5.4).
    #[must_use]
    pub fn mode(&self, fd: u32) -> StreamMode {
        let index = fd as usize;
        if index < STD_STREAM_COUNT {
            self.modes[index]
        } else {
            StreamMode::Closed
        }
    }
}

impl Default for DescriptorTable {
    fn default() -> Self {
        Self::closed()
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::{
        DescriptorTable, ProcessStart, ProcessStartHeader, StreamMode, StringSlot,
        PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS, PROCESS_START_MAX_STRING_LEN,
        PROCESS_START_MAX_TOTAL_LEN, STDERR, STDIN, STDINFO, STDOUT, STD_STREAM_COUNT,
    };
    use crate::{Errno, ABI_VERSION_CURRENT};
    use alloc::vec::Vec;

    #[test]
    fn standard_fd_numbers_are_frozen() {
        // The fd numbers are part of the process ABI (`AGENTS.md` §20).
        assert_eq!(STDIN, 0);
        assert_eq!(STDOUT, 1);
        assert_eq!(STDERR, 2);
        assert_eq!(STDINFO, 3);
        assert_eq!(STD_STREAM_COUNT, 4);
    }

    #[test]
    fn closed_table_denies_every_descriptor() {
        let table = DescriptorTable::closed();
        assert_eq!(table, DescriptorTable::default());
        for fd in [STDIN, STDOUT, STDERR, STDINFO] {
            assert_eq!(table.mode(fd), StreamMode::Closed);
        }
    }

    #[test]
    fn standard_table_reads_stdin_and_writes_the_rest() {
        let table = DescriptorTable::standard();
        assert_eq!(table.mode(STDIN), StreamMode::Read);
        assert_eq!(table.mode(STDOUT), StreamMode::Write);
        assert_eq!(table.mode(STDERR), StreamMode::Write);
        assert_eq!(table.mode(STDINFO), StreamMode::Write);
    }

    #[test]
    fn out_of_range_descriptor_is_closed() {
        // A descriptor past the standard set resolves to Closed so the
        // kernel fails it closed without leaking the out-of-range case.
        let first_out_of_range = u32::try_from(STD_STREAM_COUNT).expect("fits in u32");
        assert_eq!(
            DescriptorTable::standard().mode(first_out_of_range),
            StreamMode::Closed
        );
        assert_eq!(
            DescriptorTable::standard().mode(u32::MAX),
            StreamMode::Closed
        );
    }

    /// Build a valid startup-vector block from argument and environment
    /// strings, mirroring what the kernel loader will write.
    fn build(args: &[&[u8]], env: &[&[u8]]) -> Vec<u8> {
        build_with_canary(args, env, 0xDEAD_BEEF_F00D_CAFE)
    }

    fn build_with_canary(args: &[&[u8]], env: &[&[u8]], canary: u64) -> Vec<u8> {
        // The tests drive the very same production builder the kernel loader
        // uses (`AGENTS.md` §2.2 — one definition), proving the writer and the
        // parser agree end-to-end rather than re-implementing the layout.
        let total_len = super::encoded_len(args, env).expect("within abi-v1 limits");
        let mut block = alloc::vec![0u8; total_len];
        let written = super::write_into(&mut block, args, env, canary).expect("fits the buffer");
        assert_eq!(written, total_len);
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
        // Build a clean block, then poke a NUL into the string region so the
        // parser sees an embedded NUL (the builder itself refuses NULs, so we
        // cannot ask it to produce this block — see `write_into_rejects_nul`).
        let mut block = build(&[b"aXb"], &[]);
        let string_at = ProcessStartHeader::WIRE_LEN + StringSlot::WIRE_LEN + 1;
        block[string_at] = 0;
        assert_eq!(ProcessStart::parse(&block), Err(Errno::OutOfRange));
    }

    #[test]
    fn encoded_len_matches_the_written_block() {
        let args: &[&[u8]] = &[b"prog", b"--flag"];
        let env: &[&[u8]] = &[b"PATH=/Apps"];
        let len = super::encoded_len(args, env).expect("within limits");
        let block = build(args, env);
        assert_eq!(block.len(), len);
        // Header(32) + 3 slots(24) + strings("prog"+"--flag"+"PATH=/Apps").
        assert_eq!(len, 32 + 3 * 8 + 4 + 6 + 10);
    }

    #[test]
    fn write_into_round_trips_through_parse() {
        let args: &[&[u8]] = &[b"a", b"bb", b"ccc"];
        let env: &[&[u8]] = &[b"K=v"];
        let len = super::encoded_len(args, env).expect("len");
        let mut buf = alloc::vec![0u8; len];
        let written = super::write_into(&mut buf, args, env, 0x1122_3344_5566_7788).expect("write");
        assert_eq!(written, len);
        let view = ProcessStart::parse(&buf).expect("round-trips");
        assert_eq!(view.arg_count(), 3);
        assert_eq!(view.env_count(), 1);
        assert_eq!(view.arg(2), Some(&b"ccc"[..]));
        assert_eq!(view.env(0), Some(&b"K=v"[..]));
        assert_eq!(view.canary(), 0x1122_3344_5566_7788);
    }

    #[test]
    fn write_into_rejects_a_short_buffer() {
        let args: &[&[u8]] = &[b"prog"];
        let len = super::encoded_len(args, &[]).expect("len");
        let mut buf = alloc::vec![0u8; len - 1];
        assert_eq!(
            super::write_into(&mut buf, args, &[], 0),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn write_into_rejects_nul() {
        let args: &[&[u8]] = &[b"a\0b"];
        let mut buf = alloc::vec![0u8; 64];
        assert_eq!(
            super::write_into(&mut buf, args, &[], 0),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn encoded_len_rejects_too_many_strings() {
        let one: &[u8] = b"x";
        let args: Vec<&[u8]> = alloc::vec![one; PROCESS_START_MAX_STRINGS as usize + 1];
        assert_eq!(super::encoded_len(&args, &[]), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn encoded_len_rejects_oversized_string() {
        let big = alloc::vec![b'a'; PROCESS_START_MAX_STRING_LEN as usize + 1];
        let args: &[&[u8]] = &[&big];
        assert_eq!(super::encoded_len(args, &[]), Err(Errno::LengthOutOfRange));
    }
}
