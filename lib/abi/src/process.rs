//! The process startup vector: what the kernel hands a freshly spawned
//! program.
//!
//! When the loader (the `rxe` loader) drops into a freshly
//! created process it materialises a single contiguous *startup-vector block*
//! in the new address space and hands the program's entry trampoline (crt0,
//! `plans/CCOMPAT.md` CC3) a pointer to it. The block carries the program's
//! command-line arguments, its environment, and a per-process random seed for
//! the stack canary. This module is the one definition both sides share: the kernel *builds* the block and crt0 *parses* it.
//!
//! The block is **position-independent** — every string is referenced by an
//! offset relative to the block base, never an absolute pointer — so it works
//! unchanged wherever the loader places it in a PIE address space. It is laid out as:
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
//! [`ProcessStart::parse`] treats the whole block as **untrusted input**: it bounds-checks every field against the frozen
//! `abi-v1` limits and the declared `total_len`, rejects an embedded NUL (so
//! every string is representable as a C string), and fails closed with an
//! [`Errno`] rather than ever indexing out of range.

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
    /// Per-process random seed for the stack canary.
    ///
    /// The kernel fills this from the platform RNG when it
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
/// never index out of range. The view borrows the block; it
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
/// This is the production builder the loader uses: the kernel sizes a
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

/// The four standard file descriptors every process inherits at spawn.
///
/// A program performs **all** of its text I/O over these inherited
/// descriptors and never over a kernel-discovered device: fd 0 reads
/// input, fd 1 writes data, fd 2 writes diagnostics, and fd 3 carries
/// optional structured advisory metadata ([`crate::stdinfo`]). Which
/// kernel *stream backing* each descriptor resolves to is decided by the
/// spawner, never hard-coded into the program (device
/// independence is a property of the stream layer, not the program).
pub const STDIN: u32 = 0;
/// Primary data output. See [`STDIN`].
pub const STDOUT: u32 = 1;
/// Errors, warnings, and diagnostics. See [`STDIN`].
pub const STDERR: u32 = 2;
/// Optional structured advisory metadata ([`crate::stdinfo`]).
/// See [`STDIN`].
pub const STDINFO: u32 = 3;

/// Number of standard file descriptors the process ABI reserves: exactly fd 0/1/2/3.
pub const STD_STREAM_COUNT: usize = 4;

/// The `console` argument to [`crate::SyscallNumber::SPAWN`] that attaches
/// the child to the **caller's own** descriptor table instead of naming an
/// installed console index.
///
/// The all-ones sentinel: every real console index reported by
/// [`crate::SyscallNumber::CONSOLE_COUNT`] is small and unsigned, so the
/// sentinel can never collide with one. Inheriting is the default session
/// shape — a spawned child (login's shell, a shell's job) stays on the
/// console its parent was driving (the spawner decides
/// the backing, the program only ever names fd numbers).
pub const CONSOLE_INHERIT: u64 = u64::MAX;

/// The `target_uid` argument to [`crate::SyscallNumber::SPAWN`] that starts
/// the child under the **caller's own** kernel-attested credential (uid,
/// primary gid, supplementary groups) instead of switching to a different
/// user.
///
/// The all-ones sentinel: no real account bears uid [`u32::MAX`], so it can
/// never collide with a resolvable user. Inheriting is the default and needs
/// no capability — the child can only ever receive the credential its parent
/// already holds. A concrete uid, by contrast, asks the kernel to resolve
/// that user's full credential from the authoritative identity table and drop
/// the child into it, which requires the caller to hold
/// [`crate::CapabilityId::SPAWN_AS_USER`] and fails closed otherwise. A
/// running process can never change its *own* identity; the credential is
/// fixed at creation and only ever narrows or switches through a privileged
/// spawn (there is no setuid-self).
pub const SPAWN_UID_INHERIT: u32 = u32::MAX;

/// Highest console index a descriptor can record — the inclusive bound of
/// the [`DescriptorTable`] per-descriptor console field (`u8`).
///
/// An ABI field-width bound, not a capacity policy:
/// the number of consoles actually installed is discovered at boot and is
/// far below this; a spawn naming an index with no installed console fails
/// closed regardless.
pub const CONSOLE_INDEX_MAX: u8 = u8::MAX;

/// Version tag carried in the first field of an encoded [`SpawnAttach`]
/// block. A block bearing any other value is refused with
/// [`Errno::BadMagic`], so a stale or foreign encoding can never be
/// misread as wiring.
pub const SPAWN_ATTACH_VERSION: u32 = 2;

/// Exact byte length of an encoded [`SpawnAttach`] block: the version and
/// target-uid words, the console selector, the flags word, and one
/// `(kind, value)` pair per standard descriptor. The block is fixed-length
/// by design — the kernel bounds the copy before staging and refuses any
/// other length.
pub const SPAWN_ATTACH_LEN: usize = 4 + 4 + 8 + 8 + STD_STREAM_COUNT * 8;

/// [`SpawnAttach::flags`] bit: start the child as a **parser sandbox**
/// process (`docs/src/security/sandbox.md`).
///
/// A sandboxed child is the minimum-capability worker a program hands
/// untrusted bytes to: its effective capability set is forced empty
/// regardless of its manifest request or its user's grants, no capability
/// can ever be delegated to it, and the syscall dispatcher refuses every
/// syscall outside the closed sandbox allow-list (self-scoped and
/// descriptor-scoped operations only — no path-based filesystem access, no
/// IPC binding, no spawning). The only authority a sandboxed child holds is
/// the explicit descriptors its parent wired at spawn.
///
/// A sandbox block is canonical only when nothing ambient flows in: every
/// wire must be [`FdWire::Closed`] or [`FdWire::Handle`] (never an inherit
/// form), the credential must be inherited ([`SPAWN_UID_INHERIT`]), and the
/// console selector must be [`CONSOLE_INHERIT`] (no console index — a
/// sandbox never receives console-backed streams). [`SpawnAttach::parse`]
/// refuses any other shape, so the rule has exactly one definition shared
/// by the kernel and every userland encoder.
pub const SPAWN_FLAG_SANDBOX: u64 = 1;

/// Every [`SpawnAttach::flags`] bit with a defined meaning. A block
/// carrying any bit outside this mask is refused — reserved bits fail
/// closed instead of silently meaning nothing.
pub const SPAWN_FLAGS_ALL: u64 = SPAWN_FLAG_SANDBOX;

/// Reserved `spawn` path token: re-spawn the **caller's own program**.
///
/// A parser-sandbox worker is by definition the same binary as its parent
/// (`docs/src/security/sandbox.md`), but a program has no trustworthy
/// spelling of its own path — `argv[0]` is data its spawner chose, not
/// authority. Passing this token as the `spawn` path makes the kernel
/// substitute the exact registry or store-bundle path it admitted the
/// *caller* from (its own attested record), then run the ordinary
/// resolution and load gate over it — the token never bypasses a check,
/// it only supplies the path.
///
/// The token is honoured **only** for a sandbox spawn (an attach block
/// with [`SPAWN_FLAG_SANDBOX`]) — its one consumer — and only when the
/// caller's record carries a spawnable path; any other use fails closed
/// with `NotFound`. The leading `@` can never collide with a real
/// program: a registry path or `<Name>.app/Run` bundle path always
/// starts with `/`.
pub const SPAWN_SELF: &[u8] = b"@self";

/// How one of a spawned child's standard descriptors (fd 0–3) is backed —
/// one entry per slot in a [`SpawnAttach`] block (`plans/SPAWN.md` SP10).
///
/// The *base table* the wires refer to is the one the block's console
/// selector names: the parent's own descriptor table
/// ([`CONSOLE_INHERIT`]) or the standard shape on an installed console
/// index — exactly the two shapes `spawn` always offered.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FdWire {
    /// The base table's own slot for this descriptor (the default: the
    /// child sees exactly what an unwired spawn would give it).
    Inherit,
    /// The base table's slot `n` (`0..STD_STREAM_COUNT`) — how `2>&1`
    /// against an inherited console is spelled: the child's fd 2 becomes
    /// whatever backs the base table's fd 1.
    InheritSlot(u32),
    /// No backing: every access through this descriptor denies (fail
    /// closed), exactly like a [`DescriptorTable::closed`] slot.
    Closed,
    /// A descriptor of the **parent's own** open table — a file opened
    /// with `fs_open`, a resource opened with `resource_open`, or a pipe
    /// end minted by `pipe_create`. The kernel resolves it owner-checked
    /// against the kernel-trusted caller identity and clones the open
    /// description into the child; a forged or foreign number refuses the
    /// spawn.
    Handle(u32),
}

/// Wire discriminant for [`FdWire::Inherit`]. `0` is deliberately reserved
/// so an accidentally zeroed block fails closed rather than decoding as
/// all-inherit. Public (with its siblings) so the C-header generator emits
/// the one source-of-truth value.
pub const FD_WIRE_KIND_INHERIT: u32 = 1;
/// Wire discriminant for [`FdWire::InheritSlot`].
pub const FD_WIRE_KIND_INHERIT_SLOT: u32 = 2;
/// Wire discriminant for [`FdWire::Closed`].
pub const FD_WIRE_KIND_CLOSED: u32 = 3;
/// Wire discriminant for [`FdWire::Handle`].
pub const FD_WIRE_KIND_HANDLE: u32 = 4;

impl FdWire {
    /// The `(kind, value)` pair carried on the wire.
    #[must_use]
    const fn to_wire(self) -> (u32, u32) {
        match self {
            Self::Inherit => (FD_WIRE_KIND_INHERIT, 0),
            Self::InheritSlot(slot) => (FD_WIRE_KIND_INHERIT_SLOT, slot),
            Self::Closed => (FD_WIRE_KIND_CLOSED, 0),
            Self::Handle(fd) => (FD_WIRE_KIND_HANDLE, fd),
        }
    }

    /// Decode one `(kind, value)` pair, refusing every non-canonical shape:
    /// the reserved kind `0` and unknown kinds, a non-zero `value` on a
    /// kind that carries none, and an out-of-range slot reference.
    const fn from_wire(kind: u32, value: u32) -> Result<Self, Errno> {
        match kind {
            FD_WIRE_KIND_INHERIT => {
                if value != 0 {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::Inherit)
            }
            FD_WIRE_KIND_INHERIT_SLOT => {
                // Widening `u32 -> usize` is lossless on every target, so
                // the slot bound is compared without a truncating cast.
                if value as usize >= STD_STREAM_COUNT {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::InheritSlot(value))
            }
            FD_WIRE_KIND_CLOSED => {
                if value != 0 {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::Closed)
            }
            FD_WIRE_KIND_HANDLE => Ok(Self::Handle(value)),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The spawn *attach block*: how a child's credential and standard
/// descriptors are established (`plans/SPAWN.md` SP10). Carried by
/// [`crate::SyscallNumber::SPAWN`]'s `attach`/`attach_len` argument pair; a
/// zero `attach` pointer means "no block" — full inherit, exactly
/// [`SpawnAttach::INHERIT`].
///
/// The block carries only *selectors*, never authority: `target_uid` is
/// resolved and capability-gated kernel-side (`CAP_SPAWN_AS_USER`), the
/// console index is validated against the installed list, and every
/// [`FdWire::Handle`] is owner-checked against the kernel-trusted caller
/// identity before anything is built. The flags word only ever *narrows*
/// the child ([`SPAWN_FLAG_SANDBOX`]); no flag can widen authority.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SpawnAttach {
    /// The child's target user: [`SPAWN_UID_INHERIT`] or a concrete uid
    /// (kernel-gated on `CAP_SPAWN_AS_USER`).
    pub target_uid: u32,
    /// The base-table selector: [`CONSOLE_INHERIT`] or an installed
    /// console index.
    pub console: u64,
    /// Spawn-mode flags: zero or a combination of the defined
    /// [`SPAWN_FLAGS_ALL`] bits. Reserved bits are refused at parse.
    pub flags: u64,
    /// One wire per standard descriptor, indexed by fd number.
    pub wires: [FdWire; STD_STREAM_COUNT],
}

impl SpawnAttach {
    /// The full-inherit block: the caller's own credential and descriptor
    /// table, untouched — the semantics of passing no block at all.
    pub const INHERIT: Self = Self {
        target_uid: SPAWN_UID_INHERIT,
        console: CONSOLE_INHERIT,
        flags: 0,
        wires: [FdWire::Inherit; STD_STREAM_COUNT],
    };

    /// Build a canonical [`SPAWN_FLAG_SANDBOX`] block over `wires`.
    ///
    /// The credential and console selectors take the only values a sandbox
    /// block permits (inherit both); the caller supplies the explicit
    /// wires. The result still round-trips through [`Self::parse`], which
    /// refuses any inherit-form wire, so a non-canonical `wires` array is
    /// caught before it reaches the kernel.
    #[must_use]
    pub const fn sandbox(wires: [FdWire; STD_STREAM_COUNT]) -> Self {
        Self {
            target_uid: SPAWN_UID_INHERIT,
            console: CONSOLE_INHERIT,
            flags: SPAWN_FLAG_SANDBOX,
            wires,
        }
    }

    /// Whether this block requests the parser-sandbox spawn mode.
    #[must_use]
    pub const fn is_sandbox(&self) -> bool {
        self.flags & SPAWN_FLAG_SANDBOX != 0
    }

    /// Encode the block into its fixed-length wire form.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; SPAWN_ATTACH_LEN] {
        let mut out = [0u8; SPAWN_ATTACH_LEN];
        out[0..4].copy_from_slice(&SPAWN_ATTACH_VERSION.to_le_bytes());
        out[4..8].copy_from_slice(&self.target_uid.to_le_bytes());
        out[8..16].copy_from_slice(&self.console.to_le_bytes());
        out[16..24].copy_from_slice(&self.flags.to_le_bytes());
        for (index, wire) in self.wires.iter().enumerate() {
            let at = 24 + index * 8;
            let (kind, value) = wire.to_wire();
            out[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        out
    }

    /// Parse an encoded block, fail-closed.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for any length other than
    /// [`SPAWN_ATTACH_LEN`], [`Errno::BadMagic`] for a version other than
    /// [`SPAWN_ATTACH_VERSION`], and [`Errno::OutOfRange`] for any
    /// non-canonical wire (see [`FdWire`]), any reserved flag bit, or a
    /// non-canonical [`SPAWN_FLAG_SANDBOX`] block (an inherit-form wire, a
    /// uid switch, or a console index — nothing ambient may flow into a
    /// sandbox). A refused block wires nothing.
    pub fn parse(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != SPAWN_ATTACH_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if read_u32(bytes, 0) != SPAWN_ATTACH_VERSION {
            return Err(Errno::BadMagic);
        }
        let target_uid = read_u32(bytes, 4);
        let console = read_u64(bytes, 8);
        let flags = read_u64(bytes, 16);
        if flags & !SPAWN_FLAGS_ALL != 0 {
            return Err(Errno::OutOfRange);
        }
        let mut wires = [FdWire::Inherit; STD_STREAM_COUNT];
        for (index, wire) in wires.iter_mut().enumerate() {
            let at = 24 + index * 8;
            *wire = FdWire::from_wire(read_u32(bytes, at), read_u32(bytes, at + 4))?;
        }
        let block = Self {
            target_uid,
            console,
            flags,
            wires,
        };
        if block.is_sandbox() {
            let explicit = block
                .wires
                .iter()
                .all(|wire| matches!(wire, FdWire::Closed | FdWire::Handle(_)));
            if !explicit
                || block.target_uid != SPAWN_UID_INHERIT
                || block.console != CONSOLE_INHERIT
            {
                return Err(Errno::OutOfRange);
            }
        }
        Ok(block)
    }
}

/// A control signal delivered to a child process by
/// [`crate::SyscallNumber::SIGNAL`] or by the console line discipline
/// (`plans/SPAWN.md` SP9 — the `^C`/`^Z` foreground delivery).
///
/// The closed, minimal set job control needs (`plans/SPAWN.md` SP7/SP9), one
/// definition shared by the kernel, the C ABI view, and every first-party
/// caller so no consumer re-invents a parallel signal vocabulary. The
/// discriminant is the `u32` carried in the syscall's `signal` register;
/// `0` is reserved and never valid, so a zeroed register fails closed rather
/// than resolving to a real signal.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Signal {
    /// Resume a stopped child (the `bg`/`fg` continue).
    Continue = 1,
    /// Ask a child to terminate gracefully.
    Terminate = 2,
    /// Forcibly kill a child.
    Kill = 3,
    /// Interrupt the child — the console line discipline's `^C` delivery.
    /// The default (and, with no user-installed handlers in `abi-v1`, only)
    /// disposition terminates the child.
    Interrupt = 4,
    /// Stop the child without terminating it — the console line
    /// discipline's `^Z` delivery and the shell's stop request. The child
    /// is parked until a [`Continue`](Self::Continue) resumes it, and a
    /// parent waiting with [`crate::WaitFlags::STOPPED`] observes the stop.
    Stop = 5,
}

impl Signal {
    /// The discriminant carried on the wire.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a [`Signal`] from its wire discriminant.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] for any value that is not a defined
    /// signal (including the reserved `0`), so an unknown or zeroed register
    /// fails closed rather than being interpreted as a signal the caller did
    /// not name.
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::Continue),
            2 => Ok(Self::Terminate),
            3 => Ok(Self::Kill),
            4 => Ok(Self::Interrupt),
            5 => Ok(Self::Stop),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The exit status a [`wait`](crate::SyscallNumber::WAIT) reports for a
    /// child *terminated* by this signal, or `None` for a signal that does
    /// not end the child ([`Continue`](Self::Continue), [`Stop`](Self::Stop)).
    ///
    /// One definition shared by the kernel's signal producer (which records
    /// it as the terminated child's status) and every caller that reaps a
    /// signalled child (which recognises it), so the two can never disagree.
    /// It follows the long-standing Unix `128 + signal` convention with the
    /// signal numbers a shell user already scripts against — `Interrupt`
    /// surfaces as `130` (the `^C` code every POSIX shell reports),
    /// `Terminate` as `143` (SIGTERM's), and `Kill` as `137` (SIGKILL's) —
    /// rather than our own wire discriminants, so existing scripts keep
    /// their meaning. All are distinguishable from the small non-negative
    /// codes a program chooses for its own `exit`.
    #[must_use]
    pub const fn termination_status(self) -> Option<i32> {
        match self {
            Self::Continue | Self::Stop => None,
            Self::Interrupt => Some(130),
            Self::Kill => Some(137),
            Self::Terminate => Some(143),
        }
    }
}

/// The event a completed [`wait`](crate::SyscallNumber::WAIT) reports about
/// a child, decoded from the [`WaitStatusRecord`] the kernel wrote.
///
/// One definition shared by the kernel (which encodes it), `lib/rt` (which
/// decodes it), and every parent that waits, so the two sides can never
/// disagree about what a status means. A `Stopped` report never reaps the
/// child — it stays waitable and resumable ([`Signal::Continue`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum WaitStatus {
    /// The child terminated; the reap removed it. Carries the exit code the
    /// child passed to `exit`, or the [`Signal::termination_status`] code of
    /// the signal that ended it.
    Exited(i32),
    /// The child was stopped by this signal (requested with
    /// [`crate::WaitFlags::STOPPED`]); it was **not** reaped.
    Stopped(Signal),
}

/// Discriminant of a [`WaitStatusRecord`] naming an exited (reaped) child.
pub const WAIT_STATUS_KIND_EXITED: u32 = 1;

/// Discriminant of a [`WaitStatusRecord`] naming a stopped (unreaped) child.
pub const WAIT_STATUS_KIND_STOPPED: u32 = 2;

/// The wire record the [`wait`](crate::SyscallNumber::WAIT) syscall writes
/// through its `status` out-pointer (`plans/SPAWN.md` SP9).
///
/// A typed two-field record instead of a POSIX bit-packed status word:
/// `kind` names the event ([`WAIT_STATUS_KIND_EXITED`] /
/// [`WAIT_STATUS_KIND_STOPPED`]; `0` and every other value are reserved so a
/// zeroed or garbage record fails closed on decode) and `value` carries the
/// exit code or the stopping [`Signal`]'s discriminant. `#[repr(C)]` with
/// two fixed-width fields, so the C view (`ros_wait_status_t`) is the same
/// bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct WaitStatusRecord {
    /// The event kind: [`WAIT_STATUS_KIND_EXITED`] or
    /// [`WAIT_STATUS_KIND_STOPPED`]. `0` is reserved (the fail-closed
    /// default) and never written by a successful `wait`.
    pub kind: u32,
    /// The exit code (`kind` exited) or the stopping signal's wire
    /// discriminant (`kind` stopped).
    pub value: i32,
}

impl WaitStatusRecord {
    /// Byte length of the record as written through the `status` pointer.
    pub const WIRE_LEN: usize = 8;

    /// The bytes the kernel writes through the caller's `status` pointer:
    /// `kind` then `value`, native-endian (the record never leaves the
    /// machine — it crosses only the user/kernel boundary).
    #[must_use]
    pub const fn to_ne_bytes(self) -> [u8; Self::WIRE_LEN] {
        let kind = self.kind.to_ne_bytes();
        let value = self.value.to_ne_bytes();
        [
            kind[0], kind[1], kind[2], kind[3], value[0], value[1], value[2], value[3],
        ]
    }

    /// Rebuild the record from the bytes [`Self::to_ne_bytes`] produced.
    /// Shape only — [`Self::decode`] still validates the content.
    #[must_use]
    pub const fn from_ne_bytes(bytes: [u8; Self::WIRE_LEN]) -> Self {
        Self {
            kind: u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            value: i32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        }
    }

    /// Encode `status` as the wire record the kernel writes.
    #[must_use]
    // The stopped arm stores a signal discriminant (1..=5), far below
    // `i32::MAX`, so the widening cast can never wrap.
    #[allow(clippy::cast_possible_wrap)]
    pub const fn encode(status: WaitStatus) -> Self {
        match status {
            WaitStatus::Exited(code) => Self {
                kind: WAIT_STATUS_KIND_EXITED,
                value: code,
            },
            WaitStatus::Stopped(signal) => Self {
                kind: WAIT_STATUS_KIND_STOPPED,
                value: signal.as_u32() as i32,
            },
        }
    }

    /// Decode the record back into the typed [`WaitStatus`].
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] for a reserved `kind` (including the
    /// zeroed default) or a stopped record whose `value` is not a defined
    /// [`Signal`] — a malformed record is refused, never guessed at.
    // The stopped arm casts only after the negative guard, so the cast can
    // never lose a sign.
    #[allow(clippy::cast_sign_loss)]
    pub const fn decode(self) -> Result<WaitStatus, Errno> {
        match self.kind {
            WAIT_STATUS_KIND_EXITED => Ok(WaitStatus::Exited(self.value)),
            WAIT_STATUS_KIND_STOPPED => {
                if self.value < 0 {
                    return Err(Errno::OutOfRange);
                }
                match Signal::from_u32(self.value as u32) {
                    Ok(signal) => Ok(WaitStatus::Stopped(signal)),
                    Err(err) => Err(err),
                }
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The access a single inherited descriptor grants its process.
///
/// A descriptor is established at spawn and points at a kernel *stream
/// backing* object. [`StreamMode`] records the
/// direction that backing supports for the owning process; a
/// [`stream_read`](crate::SyscallNumber::STREAM_READ) /
/// [`stream_write`](crate::SyscallNumber::STREAM_WRITE) against a
/// descriptor whose mode does not permit the direction fails closed rather than reaching a device the program was never granted.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StreamMode {
    /// No backing is attached to this descriptor: every access denies
    /// (fail closed; — no fallback to a device).
    Closed = 0,
    /// The descriptor is readable (a `stream_read` source). Writes deny.
    Read = 1,
    /// The descriptor is writable (a `stream_write` sink). Reads deny.
    Write = 2,
}

/// One process's standard-stream descriptor table.
///
/// A fixed table of [`STD_STREAM_COUNT`] entries, one per standard
/// descriptor (fd 0/1/2/3), recording the access each inherited stream
/// grants **and which installed system console backs it** (the
/// per-descriptor console index). The spawner establishes it when it
/// creates the process; the kernel consults it to resolve a
/// `stream_read` / `stream_write` fd to its backing's direction *and*
/// device (the descriptor table, not an ambient device,
/// is the authority). Two processes on different consoles (the video
/// console and the UART, `plans/PI.md` P11) differ only in this table —
/// the programs themselves are identical. The table is small and `Copy`;
/// the backing objects themselves live kernel-side.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DescriptorTable {
    modes: [StreamMode; STD_STREAM_COUNT],
    consoles: [u8; STD_STREAM_COUNT],
}

impl DescriptorTable {
    /// A table with every standard descriptor [`Closed`](StreamMode::Closed).
    ///
    /// The fail-closed default: a process with no
    /// inherited streams can reach no backing until the spawner attaches
    /// one. This is also what an unregistered task resolves to.
    #[must_use]
    pub const fn closed() -> Self {
        Self {
            modes: [StreamMode::Closed; STD_STREAM_COUNT],
            consoles: [0; STD_STREAM_COUNT],
        }
    }

    /// The standard text-I/O table on the **primary** console (index 0):
    /// fd 0 readable, fd 1/2 writable, fd 3 (`stdinfo`) unattached.
    ///
    /// The shape every bootstrap-session process inherits: `stdin` is the
    /// input source, `stdout`/`stderr` are output sinks. The spawner backs
    /// these descriptors with the boot path's first installed console
    /// (`plans/PI.md` P6e-3a), but the program only ever names the fd
    /// numbers. `stdinfo` carries structured advisory records for tools
    /// that opt in — never terminal text — so a console session leaves it
    /// unattached: an unattached fd 3 write is discarded best-effort by the
    /// kernel rather than smeared over the terminal (where it would corrupt
    /// the primary output and every pipeline built on it).
    #[must_use]
    pub const fn standard() -> Self {
        Self::standard_on(0)
    }

    /// The standard text-I/O table attached to console `console`: fd 0
    /// readable, fd 1/2 writable, fd 3 (`stdinfo`) unattached, every
    /// attached descriptor backed by the named installed console
    /// (`plans/PI.md` P11 — one login session per discovered text console).
    ///
    /// The index is recorded verbatim; the kernel validates it against
    /// the installed console list when the table is established at spawn
    /// and fails closed on an index with no console. `stdinfo` is never
    /// backed by a console (see [`Self::standard`]).
    #[must_use]
    pub const fn standard_on(console: u8) -> Self {
        let mut modes = [StreamMode::Closed; STD_STREAM_COUNT];
        modes[STDIN as usize] = StreamMode::Read;
        modes[STDOUT as usize] = StreamMode::Write;
        modes[STDERR as usize] = StreamMode::Write;
        Self {
            modes,
            consoles: [console; STD_STREAM_COUNT],
        }
    }

    /// The [`StreamMode`] of descriptor `fd`, or
    /// [`StreamMode::Closed`] when `fd` is not one of the standard
    /// descriptors (`fd >= STD_STREAM_COUNT`).
    ///
    /// An out-of-range descriptor resolves to `Closed` so the kernel
    /// fails it closed exactly as it would a closed standard descriptor,
    /// without leaking that the index was out of range.
    #[must_use]
    pub fn mode(&self, fd: u32) -> StreamMode {
        let index = fd as usize;
        if index < STD_STREAM_COUNT {
            self.modes[index]
        } else {
            StreamMode::Closed
        }
    }

    /// The installed-console index backing descriptor `fd`, or `0` when
    /// `fd` is not one of the standard descriptors.
    ///
    /// Meaningful only when [`Self::mode`] is not
    /// [`StreamMode::Closed`]: the kernel resolves the direction first,
    /// so the index of a closed or out-of-range descriptor is never
    /// consulted — the out-of-range default exists so this accessor is
    /// total without leaking the range check.
    #[must_use]
    pub fn console(&self, fd: u32) -> u8 {
        let index = fd as usize;
        if index < STD_STREAM_COUNT {
            self.consoles[index]
        } else {
            0
        }
    }

    /// Point descriptor `fd` at console `console` with access `mode` — the
    /// spawn wiring's [`FdWire::InheritSlot`] application (`plans/SPAWN.md`
    /// SP10). An out-of-range `fd` is a no-op: the table has no such slot,
    /// so there is nothing to widen (fail closed).
    pub fn set_slot(&mut self, fd: u32, mode: StreamMode, console: u8) {
        let index = fd as usize;
        if index < STD_STREAM_COUNT {
            self.modes[index] = mode;
            self.consoles[index] = console;
        }
    }

    /// Close descriptor `fd`: every console access through it denies. Used
    /// by the spawn wiring for [`FdWire::Closed`] and for a slot whose
    /// backing is a wired open entry (exactly one authority per
    /// descriptor). An out-of-range `fd` is a no-op.
    pub fn close_slot(&mut self, fd: u32) {
        self.set_slot(fd, StreamMode::Closed, 0);
    }

    /// The single installed-console index backing **every attached**
    /// standard descriptor, or `None` when no descriptor is attached or
    /// the attached descriptors sit on different consoles.
    ///
    /// This is the honest answer to "which console is this process on?":
    /// the kernel attests it into the process's [`crate::Origin`] at spawn,
    /// so a per-console service can place a caller. A closed table (no
    /// streams) and a split table (streams on two consoles) both answer
    /// `None` — never a guess.
    #[must_use]
    pub fn session_console(&self) -> Option<u8> {
        let mut session: Option<u8> = None;
        for index in 0..STD_STREAM_COUNT {
            if self.modes[index] == StreamMode::Closed {
                continue;
            }
            match session {
                None => session = Some(self.consoles[index]),
                Some(console) if console == self.consoles[index] => {}
                Some(_) => return None,
            }
        }
        session
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
        DescriptorTable, FdWire, ProcessStart, ProcessStartHeader, Signal, SpawnAttach, StreamMode,
        StringSlot, WaitStatus, WaitStatusRecord, PROCESS_START_MAGIC, PROCESS_START_MAX_STRINGS,
        PROCESS_START_MAX_STRING_LEN, PROCESS_START_MAX_TOTAL_LEN, SPAWN_ATTACH_LEN,
        SPAWN_ATTACH_VERSION, SPAWN_FLAGS_ALL, SPAWN_FLAG_SANDBOX, STDERR, STDIN, STDINFO, STDOUT,
        STD_STREAM_COUNT, WAIT_STATUS_KIND_EXITED, WAIT_STATUS_KIND_STOPPED,
    };
    use crate::{Errno, ABI_VERSION_CURRENT};
    use alloc::vec::Vec;

    /// Every defined signal, for the exhaustive loops below.
    const ALL_SIGNALS: [Signal; 5] = [
        Signal::Continue,
        Signal::Terminate,
        Signal::Kill,
        Signal::Interrupt,
        Signal::Stop,
    ];

    #[test]
    fn spawn_attach_round_trips_every_wire_kind() {
        let attach = SpawnAttach {
            target_uid: 42,
            console: 1,
            flags: 0,
            wires: [
                FdWire::Handle(9),
                FdWire::InheritSlot(1),
                FdWire::Closed,
                FdWire::Inherit,
            ],
        };
        let bytes = attach.to_le_bytes();
        assert_eq!(bytes.len(), SPAWN_ATTACH_LEN);
        assert_eq!(SpawnAttach::parse(&bytes), Ok(attach));
        // The full-inherit block round-trips too and mirrors the no-block
        // semantics every pre-existing caller relies on.
        let inherit = SpawnAttach::INHERIT.to_le_bytes();
        assert_eq!(SpawnAttach::parse(&inherit), Ok(SpawnAttach::INHERIT));
    }

    #[test]
    fn spawn_attach_rejects_wrong_length_and_version() {
        let bytes = SpawnAttach::INHERIT.to_le_bytes();
        // Any length other than the fixed one is refused whole.
        assert_eq!(
            SpawnAttach::parse(&bytes[..SPAWN_ATTACH_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        let mut long = Vec::from(&bytes[..]);
        long.push(0);
        assert_eq!(SpawnAttach::parse(&long), Err(Errno::LengthOutOfRange));
        // A foreign version tag is refused before any wire is read.
        let mut wrong = bytes;
        wrong[0..4].copy_from_slice(&(SPAWN_ATTACH_VERSION + 1).to_le_bytes());
        assert_eq!(SpawnAttach::parse(&wrong), Err(Errno::BadMagic));
        // The all-zero block fails closed (kind 0 is reserved).
        assert_eq!(
            SpawnAttach::parse(&[0u8; SPAWN_ATTACH_LEN]),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn spawn_attach_rejects_non_canonical_wires() {
        // Kind 0 is reserved and unknown kinds are refused.
        for kind in [0u32, 5, u32::MAX] {
            let mut bytes = SpawnAttach::INHERIT.to_le_bytes();
            bytes[24..28].copy_from_slice(&kind.to_le_bytes());
            assert_eq!(SpawnAttach::parse(&bytes), Err(Errno::OutOfRange));
        }
        // A slot reference past the standard table is refused.
        let mut slot = SpawnAttach::INHERIT.to_le_bytes();
        slot[24..28].copy_from_slice(&2u32.to_le_bytes());
        let first_out_of_range = u32::try_from(STD_STREAM_COUNT).expect("small table");
        slot[28..32].copy_from_slice(&first_out_of_range.to_le_bytes());
        assert_eq!(SpawnAttach::parse(&slot), Err(Errno::OutOfRange));
        // A carried value on a kind that has none breaks canonical form.
        for kind in [1u32, 3] {
            let mut stray = SpawnAttach::INHERIT.to_le_bytes();
            stray[24..28].copy_from_slice(&kind.to_le_bytes());
            stray[28..32].copy_from_slice(&7u32.to_le_bytes());
            assert_eq!(SpawnAttach::parse(&stray), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn spawn_attach_rejects_reserved_flag_bits() {
        // Every undefined flag bit fails closed rather than silently
        // meaning nothing.
        for flags in [SPAWN_FLAGS_ALL + 1, 1u64 << 63, u64::MAX] {
            let mut bytes = SpawnAttach::INHERIT.to_le_bytes();
            bytes[16..24].copy_from_slice(&flags.to_le_bytes());
            assert_eq!(SpawnAttach::parse(&bytes), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn spawn_attach_sandbox_round_trips_and_reports_the_mode() {
        let attach = SpawnAttach::sandbox([
            FdWire::Handle(4),
            FdWire::Handle(5),
            FdWire::Closed,
            FdWire::Closed,
        ]);
        assert!(attach.is_sandbox());
        assert!(!SpawnAttach::INHERIT.is_sandbox());
        let parsed = SpawnAttach::parse(&attach.to_le_bytes());
        assert_eq!(parsed, Ok(attach));
    }

    #[test]
    fn spawn_attach_sandbox_refuses_every_ambient_shape() {
        let explicit = [
            FdWire::Handle(4),
            FdWire::Handle(5),
            FdWire::Closed,
            FdWire::Closed,
        ];
        // An inherit-form wire lets ambient backing flow in; refused.
        for ambient in [FdWire::Inherit, FdWire::InheritSlot(1)] {
            let mut wires = explicit;
            wires[2] = ambient;
            let block = SpawnAttach {
                flags: SPAWN_FLAG_SANDBOX,
                wires,
                ..SpawnAttach::INHERIT
            };
            assert_eq!(
                SpawnAttach::parse(&block.to_le_bytes()),
                Err(Errno::OutOfRange)
            );
        }
        // A credential switch inside a sandbox spawn is refused.
        let uid_switch = SpawnAttach {
            target_uid: 42,
            flags: SPAWN_FLAG_SANDBOX,
            wires: explicit,
            ..SpawnAttach::INHERIT
        };
        assert_eq!(
            SpawnAttach::parse(&uid_switch.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
        // A console index is a console-backed base table; refused.
        let console = SpawnAttach {
            console: 0,
            flags: SPAWN_FLAG_SANDBOX,
            wires: explicit,
            ..SpawnAttach::INHERIT
        };
        assert_eq!(
            SpawnAttach::parse(&console.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn signal_discriminants_are_frozen() {
        // The discriminants are the on-wire signal values; do not renumber.
        assert_eq!(Signal::Continue.as_u32(), 1);
        assert_eq!(Signal::Terminate.as_u32(), 2);
        assert_eq!(Signal::Kill.as_u32(), 3);
        assert_eq!(Signal::Interrupt.as_u32(), 4);
        assert_eq!(Signal::Stop.as_u32(), 5);
    }

    #[test]
    fn signal_round_trips_through_its_discriminant() {
        for signal in ALL_SIGNALS {
            assert_eq!(Signal::from_u32(signal.as_u32()), Ok(signal));
        }
    }

    #[test]
    fn signal_rejects_reserved_and_unknown_values() {
        // 0 is reserved so a zeroed register fails closed, and every value
        // past the defined set is rejected rather than guessed.
        assert_eq!(Signal::from_u32(0), Err(Errno::OutOfRange));
        assert_eq!(Signal::from_u32(6), Err(Errno::OutOfRange));
        assert_eq!(Signal::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn termination_status_follows_the_posix_familiar_convention() {
        // A terminating signal reports the `128 + n` code a POSIX shell
        // user already scripts against (130 = interrupted, 137 = killed,
        // 143 = terminated); `Continue` and `Stop` do not end the child, so
        // they have no termination status.
        assert_eq!(Signal::Continue.termination_status(), None);
        assert_eq!(Signal::Stop.termination_status(), None);
        assert_eq!(Signal::Interrupt.termination_status(), Some(130));
        assert_eq!(Signal::Kill.termination_status(), Some(137));
        assert_eq!(Signal::Terminate.termination_status(), Some(143));
        // The reported statuses sit above the small exit codes a program
        // chooses, so a reaper can tell a signalled death from a normal one.
        for signal in [Signal::Interrupt, Signal::Terminate, Signal::Kill] {
            assert!(signal.termination_status().expect("terminating") > 128);
        }
    }

    #[test]
    fn wait_status_record_round_trips_both_kinds() {
        for status in [
            WaitStatus::Exited(0),
            WaitStatus::Exited(143),
            WaitStatus::Exited(-7),
            WaitStatus::Stopped(Signal::Stop),
        ] {
            assert_eq!(WaitStatusRecord::encode(status).decode(), Ok(status));
        }
    }

    #[test]
    fn wait_status_record_byte_codec_round_trips() {
        for status in [WaitStatus::Exited(-40), WaitStatus::Stopped(Signal::Stop)] {
            let record = WaitStatusRecord::encode(status);
            let bytes = record.to_ne_bytes();
            assert_eq!(bytes.len(), WaitStatusRecord::WIRE_LEN);
            assert_eq!(WaitStatusRecord::from_ne_bytes(bytes), record);
        }
    }

    #[test]
    fn wait_status_record_wire_kinds_are_frozen() {
        assert_eq!(
            WaitStatusRecord::encode(WaitStatus::Exited(9)).kind,
            WAIT_STATUS_KIND_EXITED
        );
        let stopped = WaitStatusRecord::encode(WaitStatus::Stopped(Signal::Stop));
        assert_eq!(stopped.kind, WAIT_STATUS_KIND_STOPPED);
        assert_eq!(stopped.value, 5);
    }

    #[test]
    fn wait_status_record_rejects_reserved_and_malformed_records() {
        // The zeroed default (kind 0) and every unassigned kind fail closed.
        assert_eq!(WaitStatusRecord::default().decode(), Err(Errno::OutOfRange));
        assert_eq!(
            WaitStatusRecord { kind: 3, value: 0 }.decode(),
            Err(Errno::OutOfRange)
        );
        // A stopped record must carry a defined signal discriminant.
        for value in [0, -1, 6, i32::MAX, i32::MIN] {
            assert_eq!(
                WaitStatusRecord { kind: 2, value }.decode(),
                Err(Errno::OutOfRange)
            );
        }
    }

    #[test]
    fn standard_fd_numbers_are_frozen() {
        // The fd numbers are part of the process ABI.
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
    fn standard_table_reads_stdin_and_writes_out_and_err_only() {
        let table = DescriptorTable::standard();
        assert_eq!(table.mode(STDIN), StreamMode::Read);
        assert_eq!(table.mode(STDOUT), StreamMode::Write);
        assert_eq!(table.mode(STDERR), StreamMode::Write);
        // `stdinfo` is advisory metadata, never terminal text: a console
        // session leaves it unattached so its records cannot smear over
        // stdout (the kernel discards unattached fd 3 writes best-effort).
        assert_eq!(table.mode(STDINFO), StreamMode::Closed);
        // `standard()` is the primary console: every attached descriptor
        // backed by console 0.
        for fd in [STDIN, STDOUT, STDERR] {
            assert_eq!(table.console(fd), 0);
        }
        assert_eq!(table, DescriptorTable::standard_on(0));
    }

    #[test]
    fn standard_on_attaches_every_text_descriptor_to_the_named_console() {
        let table = DescriptorTable::standard_on(1);
        for fd in [STDIN, STDOUT, STDERR] {
            assert_eq!(table.console(fd), 1);
        }
        // The direction shape is identical to the primary table; only
        // the backing console differs, and `stdinfo` stays unattached.
        assert_eq!(table.mode(STDIN), StreamMode::Read);
        assert_eq!(table.mode(STDOUT), StreamMode::Write);
        assert_eq!(table.mode(STDINFO), StreamMode::Closed);
        assert_ne!(table, DescriptorTable::standard());
    }

    #[test]
    fn out_of_range_descriptor_console_defaults_to_zero() {
        let table = DescriptorTable::standard_on(3);
        assert_eq!(table.console(u32::MAX), 0);
    }

    #[test]
    fn session_console_answers_the_uniform_console_and_refuses_the_rest() {
        // Every attached descriptor on one console: that console.
        assert_eq!(DescriptorTable::standard_on(1).session_console(), Some(1));
        assert_eq!(DescriptorTable::standard().session_console(), Some(0));
        // No attached descriptor: no console (fail closed, never a guess).
        assert_eq!(DescriptorTable::closed().session_console(), None);
        // Attached descriptors split across consoles: no single answer.
        let mut split = DescriptorTable::standard_on(0);
        split.consoles[STDOUT as usize] = 1;
        assert_eq!(split.session_console(), None);
    }

    #[test]
    fn slot_mutators_retarget_and_close_in_range_only() {
        let mut table = DescriptorTable::standard_on(0);
        table.set_slot(STDERR, StreamMode::Write, 1);
        assert_eq!(table.console(STDERR), 1);
        assert_eq!(table.mode(STDERR), StreamMode::Write);
        table.close_slot(STDOUT);
        assert_eq!(table.mode(STDOUT), StreamMode::Closed);
        // Out of range: a no-op, never a panic or a widened slot.
        let before = table;
        table.set_slot(u32::MAX, StreamMode::Read, 2);
        table.close_slot(u32::MAX);
        assert_eq!(table, before);
    }

    #[test]
    fn console_inherit_sentinel_collides_with_no_console_index() {
        // Every representable console index is below the sentinel, so
        // the spawn argument space cannot confuse the two.
        assert!(u64::from(super::CONSOLE_INDEX_MAX) < super::CONSOLE_INHERIT);
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
        // uses (one definition), proving the writer and the
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
