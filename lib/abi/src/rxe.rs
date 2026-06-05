//! `rxe` load-image program table and load-time hardening policy.
//!
//! An `rxe` binary (`AGENTS.md` §9) carries the signed [`crate::ManifestHeader`]
//! *and* a load image: a fixed [`LoadHeader`] followed by a table of
//! [`Segment`] records describing the memory map the kernel must materialise.
//! This module owns the load image's wire format and the §19.2 load-time
//! invariants the kernel enforces before a single page is mapped:
//!
//! * **W^X** — every segment is exactly one of read-only, read-execute, or
//!   read-write. A writable-and-executable segment is refused
//!   ([`RxeError::WriteExecSegment`]); so is a non-readable one.
//! * **Position-independence** — the load header must declare
//!   [`LOAD_FLAG_PIE`]; a fixed-address image is refused
//!   ([`RxeError::NotPositionIndependent`]) so the kernel is free to apply
//!   KASLR via [`kaslr_bias`].
//! * **CFI type-tag** — the load header carries the hash of the syscall
//!   interface the binary was linked against; a mismatch against the
//!   kernel's compiled-in hash is a load-time refusal
//!   ([`RxeError::InterfaceHashMismatch`]), never a runtime crash.
//!
//! The module is `no_std` and allocation-free: [`LoadImage::parse`] validates
//! every segment up front and stores the result in a fixed-capacity array
//! ([`LOAD_MAX_SEGMENTS`]).

use crate::le::{read_u16, read_u32, read_u64};
use crate::syscall::SYSCALL_TABLE_HASH_LEN;
use crate::ABI_VERSION_CURRENT;

/// Magic word identifying an `abi-v1` load header (`"RXEL"` little-endian).
pub const LOAD_MAGIC: u32 = u32::from_le_bytes(*b"RXEL");

/// Page size the load image is expressed in (4 KiB — the smallest unit
/// every Tier-1 MMU supports natively; matches `kernel/mem`).
pub const RXE_PAGE_SIZE: u64 = 4096;

/// Maximum number of [`Segment`] records a single load image may carry.
///
/// Bounded so a malformed or hostile image cannot force unbounded parsing
/// work or an unbounded stack footprint.
pub const LOAD_MAX_SEGMENTS: usize = 64;

/// Maximum number of shared-library references a load image may declare.
///
/// Bounded so a malformed or hostile image cannot force unbounded parsing
/// work or an unbounded fixed-array footprint in [`LoadImage`]. The
/// references are resolved by the user-space dynamic loader under the §16.4
/// policy (`AGENTS.md`); the kernel only validates and carries them.
pub const LOAD_MAX_NEEDED: usize = 8;

/// Maximum length, in bytes, of a single shared-library reference path.
///
/// Chosen to fit any absolute path under a bundle's `Libraries/` or
/// [`crate::SYSTEM_LIBRARIES_DIR`] while still fitting a record length in a
/// single byte.
pub const LIBREF_MAX: usize = 255;

/// Load-header flag: the image is position-independent (PIE).
///
/// Required by §19.2; an image without this bit is refused so the kernel
/// can relocate it under KASLR.
pub const LOAD_FLAG_PIE: u32 = 1 << 0;

/// The set of load-header flag bits defined in `abi-v1`.
const LOAD_FLAG_KNOWN: u32 = LOAD_FLAG_PIE;

/// Segment flag: the segment is readable.
pub const SEG_FLAG_READ: u32 = 1 << 0;
/// Segment flag: the segment is writable.
pub const SEG_FLAG_WRITE: u32 = 1 << 1;
/// Segment flag: the segment is executable.
pub const SEG_FLAG_EXEC: u32 = 1 << 2;

/// The set of segment flag bits defined in `abi-v1`.
const SEG_FLAG_KNOWN: u32 = SEG_FLAG_READ | SEG_FLAG_WRITE | SEG_FLAG_EXEC;

/// Why an `rxe` load image was rejected.
///
/// The loader fails closed (`AGENTS.md` §5.4): any deviation from the §19.2
/// invariants yields one of these variants and no page is mapped.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RxeError {
    /// The byte slice is shorter than the structure it must contain.
    BufferTooSmall,
    /// The load-header magic word does not match [`LOAD_MAGIC`].
    BadMagic,
    /// The load header targets an ABI version this kernel does not support.
    BadAbiVersion,
    /// A reserved header field, or an unknown header flag bit, is non-zero.
    ReservedNonZero,
    /// The image declares no segments.
    NoSegments,
    /// The image declares more than [`LOAD_MAX_SEGMENTS`] segments.
    TooManySegments,
    /// A segment sets a flag bit outside [`SEG_FLAG_READ`] /
    /// [`SEG_FLAG_WRITE`] / [`SEG_FLAG_EXEC`].
    UnknownSegmentFlags,
    /// A segment is neither readable: every segment must carry
    /// [`SEG_FLAG_READ`].
    SegmentNotReadable,
    /// A segment is both writable and executable — the W^X violation §19.2
    /// refuses at load time.
    WriteExecSegment,
    /// A segment's virtual address is not [`RXE_PAGE_SIZE`]-aligned.
    MisalignedSegment,
    /// A segment's sizes are inconsistent (`mem_size == 0`, or
    /// `file_size > mem_size`).
    BadSegmentSize,
    /// Two segments cover overlapping pages, or the table is not sorted by
    /// ascending virtual address.
    SegmentOverlap,
    /// An address computation overflowed `u64` (segment extent, or a KASLR
    /// relocation).
    AddressOverflow,
    /// The load header lacks [`LOAD_FLAG_PIE`]; §19.2 requires PIE so the
    /// kernel can apply KASLR.
    NotPositionIndependent,
    /// The header's CFI type-tag does not match the kernel's syscall
    /// interface hash (§9 / §19.2).
    InterfaceHashMismatch,
    /// The entry point does not fall inside an executable segment.
    BadEntryPoint,
    /// The image declares more than [`LOAD_MAX_NEEDED`] needed libraries.
    TooManyNeeded,
    /// A needed-library record is malformed: empty, longer than
    /// [`LIBREF_MAX`], not NUL-free UTF-8, or its padding tail is non-zero.
    BadNeededLibrary,
}

impl core::fmt::Display for RxeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::BufferTooSmall => "rxe image truncated",
            Self::BadMagic => "rxe load magic mismatch",
            Self::BadAbiVersion => "rxe abi version unsupported",
            Self::ReservedNonZero => "rxe reserved field or unknown flag set",
            Self::NoSegments => "rxe image declares no segments",
            Self::TooManySegments => "rxe image declares too many segments",
            Self::UnknownSegmentFlags => "rxe segment sets unknown flag bit",
            Self::SegmentNotReadable => "rxe segment is not readable",
            Self::WriteExecSegment => "rxe segment is writable and executable",
            Self::MisalignedSegment => "rxe segment is not page aligned",
            Self::BadSegmentSize => "rxe segment has inconsistent sizes",
            Self::SegmentOverlap => "rxe segments overlap or are unsorted",
            Self::AddressOverflow => "rxe address computation overflowed",
            Self::NotPositionIndependent => "rxe image is not position independent",
            Self::InterfaceHashMismatch => "rxe syscall interface hash mismatch",
            Self::BadEntryPoint => "rxe entry point outside an executable segment",
            Self::TooManyNeeded => "rxe image declares too many needed libraries",
            Self::BadNeededLibrary => "rxe needed-library reference is malformed",
        };
        f.write_str(message)
    }
}

/// The permission a segment is mapped with.
///
/// Construction goes through [`RxePermission::from_segment_flags`], which is
/// the single point that enforces W^X: the writable-and-executable and
/// non-readable combinations are unrepresentable here by construction.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RxePermission {
    /// Read-only data.
    ReadOnly = 0,
    /// Read + execute (code).
    ReadExecute = 1,
    /// Read + write (mutable data).
    ReadWrite = 2,
}

impl RxePermission {
    /// Classify a segment's raw flag word, enforcing W^X.
    ///
    /// # Errors
    ///
    /// * [`RxeError::UnknownSegmentFlags`] for any bit outside
    ///   [`SEG_FLAG_READ`] / [`SEG_FLAG_WRITE`] / [`SEG_FLAG_EXEC`].
    /// * [`RxeError::WriteExecSegment`] if both write and execute are set.
    /// * [`RxeError::SegmentNotReadable`] if read is not set.
    pub const fn from_segment_flags(flags: u32) -> Result<Self, RxeError> {
        if flags & !SEG_FLAG_KNOWN != 0 {
            return Err(RxeError::UnknownSegmentFlags);
        }
        let writable = flags & SEG_FLAG_WRITE != 0;
        let executable = flags & SEG_FLAG_EXEC != 0;
        if writable && executable {
            return Err(RxeError::WriteExecSegment);
        }
        if flags & SEG_FLAG_READ == 0 {
            return Err(RxeError::SegmentNotReadable);
        }
        Ok(if executable {
            Self::ReadExecute
        } else if writable {
            Self::ReadWrite
        } else {
            Self::ReadOnly
        })
    }

    /// True if the segment is executable.
    #[must_use]
    pub const fn is_executable(self) -> bool {
        matches!(self, Self::ReadExecute)
    }

    /// True if the segment is writable.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// One validated segment of a load image.
///
/// Only [`Segment::decode`] (and therefore [`LoadImage::parse`]) constructs
/// a `Segment`, so every instance already satisfies the §19.2 invariants:
/// page-aligned virtual address, `file_size <= mem_size`, a non-empty
/// extent, and a W^X-clean [`RxePermission`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    /// Image-relative virtual address; page-aligned.
    pub vaddr: u64,
    /// Offset of the segment's initialised bytes within the image file.
    pub file_offset: u64,
    /// Number of initialised bytes copied from the file.
    pub file_size: u64,
    /// Total in-memory size; bytes beyond `file_size` are zero-filled.
    pub mem_size: u64,
    /// The W^X-clean permission this segment is mapped with.
    pub permission: RxePermission,
}

impl Segment {
    /// Encoded size of a [`Segment`] on the wire.
    pub const WIRE_LEN: usize = 8 + 8 + 8 + 8 + 4 + 4;

    /// Encode `self` into its little-endian wire representation.
    ///
    /// The encoded `flags` word is reconstructed from [`Self::permission`];
    /// `decode(self.to_le_bytes())` round-trips.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut flags = SEG_FLAG_READ;
        if self.permission.is_writable() {
            flags |= SEG_FLAG_WRITE;
        }
        if self.permission.is_executable() {
            flags |= SEG_FLAG_EXEC;
        }
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..8].copy_from_slice(&self.vaddr.to_le_bytes());
        out[8..16].copy_from_slice(&self.file_offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.file_size.to_le_bytes());
        out[24..32].copy_from_slice(&self.mem_size.to_le_bytes());
        out[32..36].copy_from_slice(&flags.to_le_bytes());
        out
    }

    /// Decode and validate a single segment record.
    ///
    /// # Errors
    ///
    /// * [`RxeError::BufferTooSmall`] if `bytes` is shorter than
    ///   [`Self::WIRE_LEN`].
    /// * [`RxeError::ReservedNonZero`] if the reserved word is non-zero.
    /// * any [`RxePermission::from_segment_flags`] error.
    /// * [`RxeError::MisalignedSegment`] if `vaddr` is not page-aligned.
    /// * [`RxeError::BadSegmentSize`] if `mem_size == 0` or
    ///   `file_size > mem_size`.
    /// * [`RxeError::AddressOverflow`] if `vaddr + mem_size` overflows.
    pub fn decode(bytes: &[u8]) -> Result<Self, RxeError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(RxeError::BufferTooSmall);
        }
        let vaddr = read_u64(bytes, 0);
        let file_offset = read_u64(bytes, 8);
        let file_size = read_u64(bytes, 16);
        let mem_size = read_u64(bytes, 24);
        let flags = read_u32(bytes, 32);
        if read_u32(bytes, 36) != 0 {
            return Err(RxeError::ReservedNonZero);
        }
        let permission = RxePermission::from_segment_flags(flags)?;
        if vaddr % RXE_PAGE_SIZE != 0 {
            return Err(RxeError::MisalignedSegment);
        }
        if mem_size == 0 || file_size > mem_size {
            return Err(RxeError::BadSegmentSize);
        }
        if vaddr.checked_add(mem_size).is_none() {
            return Err(RxeError::AddressOverflow);
        }
        Ok(Self {
            vaddr,
            file_offset,
            file_size,
            mem_size,
            permission,
        })
    }

    /// Number of pages this segment occupies (`mem_size` rounded up).
    #[must_use]
    pub fn page_count(&self) -> u64 {
        self.mem_size.div_ceil(RXE_PAGE_SIZE)
    }

    /// First address past the segment, rounded up to a page boundary.
    ///
    /// # Errors
    ///
    /// [`RxeError::AddressOverflow`] if the page-rounded extent overflows.
    pub fn end(&self) -> Result<u64, RxeError> {
        match self.page_count().checked_mul(RXE_PAGE_SIZE) {
            Some(span) => match self.vaddr.checked_add(span) {
                Some(end) => Ok(end),
                None => Err(RxeError::AddressOverflow),
            },
            None => Err(RxeError::AddressOverflow),
        }
    }

    /// This segment's virtual address after applying a KASLR `bias`.
    ///
    /// # Errors
    ///
    /// [`RxeError::AddressOverflow`] if `vaddr + bias + extent` overflows.
    pub fn relocated_vaddr(&self, bias: u64) -> Result<u64, RxeError> {
        match self.vaddr.checked_add(bias) {
            Some(base) => match self.page_count().checked_mul(RXE_PAGE_SIZE) {
                Some(span) if base.checked_add(span).is_some() => Ok(base),
                _ => Err(RxeError::AddressOverflow),
            },
            None => Err(RxeError::AddressOverflow),
        }
    }
}

/// Fixed-size prefix of an `rxe` load image.
///
/// Field order is part of the frozen `abi-v1` surface; reserved fields and
/// unknown flag bits must be zero.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LoadHeader {
    /// Must equal [`LOAD_MAGIC`].
    pub magic: u32,
    /// ABI version the image targets; rejected unless it equals
    /// [`crate::ABI_VERSION_CURRENT`].
    pub abi_version: u32,
    /// Flag bits; only [`LOAD_FLAG_PIE`] is defined in `abi-v1`.
    pub flags: u32,
    /// Number of [`Segment`] records that follow the header.
    pub segment_count: u16,
    /// Number of [`NeededLibrary`] records that follow the segment table —
    /// the shared libraries the image dynamically links (`AGENTS.md` §16.4).
    /// Must not exceed [`LOAD_MAX_NEEDED`].
    pub needed_count: u16,
    /// Image-relative entry-point virtual address.
    pub entry: u64,
    /// CFI type-tag: the SHA-256 of the syscall interface this image was
    /// linked against (`AGENTS.md` §9 / §19.2).
    pub cfi_tag: [u8; SYSCALL_TABLE_HASH_LEN],
}

impl LoadHeader {
    /// Encoded size of a [`LoadHeader`] on the wire.
    pub const WIRE_LEN: usize = 4 + 4 + 4 + 2 + 2 + 8 + SYSCALL_TABLE_HASH_LEN;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.abi_version.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..14].copy_from_slice(&self.segment_count.to_le_bytes());
        out[14..16].copy_from_slice(&self.needed_count.to_le_bytes());
        out[16..24].copy_from_slice(&self.entry.to_le_bytes());
        out[24..24 + SYSCALL_TABLE_HASH_LEN].copy_from_slice(&self.cfi_tag);
        out
    }

    /// Decode the header prefix without enforcing the §19.2 policy.
    ///
    /// [`LoadImage::parse`] is the policy-enforcing entry point; this only
    /// recovers the fields.
    ///
    /// # Errors
    ///
    /// [`RxeError::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RxeError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(RxeError::BufferTooSmall);
        }
        let mut cfi_tag = [0u8; SYSCALL_TABLE_HASH_LEN];
        cfi_tag.copy_from_slice(&bytes[24..24 + SYSCALL_TABLE_HASH_LEN]);
        Ok(Self {
            magic: read_u32(bytes, 0),
            abi_version: read_u32(bytes, 4),
            flags: read_u32(bytes, 8),
            segment_count: read_u16(bytes, 12),
            needed_count: read_u16(bytes, 14),
            entry: read_u64(bytes, 16),
            cfi_tag,
        })
    }

    /// True if the image declares itself position-independent.
    #[must_use]
    pub const fn is_pie(&self) -> bool {
        self.flags & LOAD_FLAG_PIE != 0
    }
}

/// One shared-library reference an [`LoadImage`] declares it needs at load
/// time — the `rxe` analogue of an ELF `DT_NEEDED` entry.
///
/// The reference is an absolute path the user-space dynamic loader resolves
/// under the §16.4 policy (the requesting bundle's own `Libraries/` or
/// [`crate::SYSTEM_LIBRARIES_DIR`]); this type only carries and validates the
/// bytes. Like [`Segment`], it is hand-serialised, so the C header exports its
/// wire size rather than a struct mirror.
///
/// Construction goes through [`NeededLibrary::decode`] or
/// [`NeededLibrary::from_reference`], so every instance holds a non-empty,
/// NUL-free, UTF-8 path no longer than [`LIBREF_MAX`] bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NeededLibrary {
    len: u8,
    bytes: [u8; LIBREF_MAX],
}

impl NeededLibrary {
    /// Encoded size of a [`NeededLibrary`] record on the wire.
    pub const WIRE_LEN: usize = 1 + LIBREF_MAX;

    /// Build a record from a shared-library reference path.
    ///
    /// # Errors
    ///
    /// [`RxeError::BadNeededLibrary`] if `reference` is empty, longer than
    /// [`LIBREF_MAX`] bytes, or contains an embedded NUL.
    pub fn from_reference(reference: &str) -> Result<Self, RxeError> {
        let raw = reference.as_bytes();
        if raw.is_empty() || raw.len() > LIBREF_MAX || raw.contains(&0) {
            return Err(RxeError::BadNeededLibrary);
        }
        let len = u8::try_from(raw.len()).map_err(|_| RxeError::BadNeededLibrary)?;
        let mut bytes = [0u8; LIBREF_MAX];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self { len, bytes })
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.len;
        out[1..].copy_from_slice(&self.bytes);
        out
    }

    /// Decode and validate a single needed-library record.
    ///
    /// # Errors
    ///
    /// * [`RxeError::BufferTooSmall`] if `bytes` is shorter than
    ///   [`Self::WIRE_LEN`].
    /// * [`RxeError::BadNeededLibrary`] if the length is zero or exceeds
    ///   [`LIBREF_MAX`], the padding tail is non-zero, or the reference is
    ///   not NUL-free UTF-8.
    pub fn decode(bytes: &[u8]) -> Result<Self, RxeError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(RxeError::BufferTooSmall);
        }
        let len = bytes[0];
        let n = usize::from(len);
        if n == 0 || n > LIBREF_MAX {
            return Err(RxeError::BadNeededLibrary);
        }
        let mut data = [0u8; LIBREF_MAX];
        data.copy_from_slice(&bytes[1..=LIBREF_MAX]);
        if data[n..].iter().any(|&b| b != 0) {
            return Err(RxeError::BadNeededLibrary);
        }
        let name = &data[..n];
        if name.contains(&0) || core::str::from_utf8(name).is_err() {
            return Err(RxeError::BadNeededLibrary);
        }
        Ok(Self { len, bytes: data })
    }

    /// The shared-library reference path this record carries.
    #[must_use]
    pub fn reference(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

/// A fully validated `rxe` load image: entry point plus an ordered,
/// non-overlapping, W^X-clean, page-aligned segment table.
///
/// The only constructor is [`LoadImage::parse`]; holding a `LoadImage` is
/// proof that the §19.2 load-time invariants hold.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LoadImage {
    entry: u64,
    segment_count: usize,
    segments: [Segment; LOAD_MAX_SEGMENTS],
    needed_count: usize,
    needed: [NeededLibrary; LOAD_MAX_NEEDED],
}

/// Zero-extent placeholder used to initialise the fixed-capacity segment
/// array before [`LoadImage::parse`] overwrites the live entries.
const SEGMENT_PLACEHOLDER: Segment = Segment {
    vaddr: 0,
    file_offset: 0,
    file_size: 0,
    mem_size: 0,
    permission: RxePermission::ReadOnly,
};

/// Zero-length placeholder used to initialise the fixed-capacity needed-library
/// array before [`LoadImage::parse`] overwrites the live entries.
const NEEDED_PLACEHOLDER: NeededLibrary = NeededLibrary {
    len: 0,
    bytes: [0u8; LIBREF_MAX],
};

impl LoadImage {
    /// Parse and validate a load image, enforcing every §19.2 invariant.
    ///
    /// `expected_cfi_tag` is the kernel's compiled-in syscall-interface
    /// hash; the header's [`LoadHeader::cfi_tag`] must equal it.
    ///
    /// # Errors
    ///
    /// Returns the first [`RxeError`] encountered. In particular:
    /// [`RxeError::NotPositionIndependent`] if [`LOAD_FLAG_PIE`] is unset,
    /// [`RxeError::WriteExecSegment`] for an RWX segment,
    /// [`RxeError::InterfaceHashMismatch`] for a CFI-tag mismatch,
    /// [`RxeError::SegmentOverlap`] for unsorted/overlapping segments, and
    /// [`RxeError::BadEntryPoint`] if [`LoadHeader::entry`] is not inside an
    /// executable segment.
    pub fn parse(
        bytes: &[u8],
        expected_cfi_tag: &[u8; SYSCALL_TABLE_HASH_LEN],
    ) -> Result<Self, RxeError> {
        let header = LoadHeader::from_bytes(bytes)?;
        if header.magic != LOAD_MAGIC {
            return Err(RxeError::BadMagic);
        }
        if header.abi_version != ABI_VERSION_CURRENT {
            return Err(RxeError::BadAbiVersion);
        }
        if header.flags & !LOAD_FLAG_KNOWN != 0 {
            return Err(RxeError::ReservedNonZero);
        }
        if !header.is_pie() {
            return Err(RxeError::NotPositionIndependent);
        }
        if ct_ne(&header.cfi_tag, expected_cfi_tag) {
            return Err(RxeError::InterfaceHashMismatch);
        }

        let count = usize::from(header.segment_count);
        if count == 0 {
            return Err(RxeError::NoSegments);
        }
        if count > LOAD_MAX_SEGMENTS {
            return Err(RxeError::TooManySegments);
        }
        let table = LoadHeader::WIRE_LEN
            .checked_add(
                count
                    .checked_mul(Segment::WIRE_LEN)
                    .ok_or(RxeError::TooManySegments)?,
            )
            .ok_or(RxeError::TooManySegments)?;
        if bytes.len() < table {
            return Err(RxeError::BufferTooSmall);
        }

        let mut segments = [SEGMENT_PLACEHOLDER; LOAD_MAX_SEGMENTS];
        let mut prev_end = 0u64;
        for (i, slot) in segments[..count].iter_mut().enumerate() {
            let offset = LoadHeader::WIRE_LEN + i * Segment::WIRE_LEN;
            let segment = Segment::decode(&bytes[offset..offset + Segment::WIRE_LEN])?;
            if segment.vaddr < prev_end {
                return Err(RxeError::SegmentOverlap);
            }
            prev_end = segment.end()?;
            *slot = segment;
        }

        let needed_count = usize::from(header.needed_count);
        if needed_count > LOAD_MAX_NEEDED {
            return Err(RxeError::TooManyNeeded);
        }
        let needed_table_end = table
            .checked_add(
                needed_count
                    .checked_mul(NeededLibrary::WIRE_LEN)
                    .ok_or(RxeError::TooManyNeeded)?,
            )
            .ok_or(RxeError::TooManyNeeded)?;
        if bytes.len() < needed_table_end {
            return Err(RxeError::BufferTooSmall);
        }
        let mut needed = [NEEDED_PLACEHOLDER; LOAD_MAX_NEEDED];
        for (i, slot) in needed[..needed_count].iter_mut().enumerate() {
            let offset = table + i * NeededLibrary::WIRE_LEN;
            *slot = NeededLibrary::decode(&bytes[offset..offset + NeededLibrary::WIRE_LEN])?;
        }

        let image = Self {
            entry: header.entry,
            segment_count: count,
            segments,
            needed_count,
            needed,
        };
        if !image.entry_is_executable() {
            return Err(RxeError::BadEntryPoint);
        }
        Ok(image)
    }

    /// Image-relative entry-point virtual address.
    #[must_use]
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// The validated segment table.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.segment_count]
    }

    /// The shared-library references this image declares it needs, in
    /// declaration order.
    ///
    /// Each reference is resolved by the user-space dynamic loader under the
    /// §16.4 policy (the requesting bundle's own `Libraries/` or
    /// [`crate::SYSTEM_LIBRARIES_DIR`]); the kernel only validates and carries
    /// them here.
    pub fn needed_libraries(&self) -> impl Iterator<Item = &str> {
        self.needed[..self.needed_count]
            .iter()
            .map(NeededLibrary::reference)
    }

    /// Entry point after applying a KASLR `bias`.
    ///
    /// # Errors
    ///
    /// [`RxeError::AddressOverflow`] if `entry + bias` overflows.
    pub const fn relocated_entry(&self, bias: u64) -> Result<u64, RxeError> {
        match self.entry.checked_add(bias) {
            Some(addr) => Ok(addr),
            None => Err(RxeError::AddressOverflow),
        }
    }

    fn entry_is_executable(&self) -> bool {
        self.segments().iter().any(|s| {
            s.permission.is_executable()
                && self.entry >= s.vaddr
                && self.entry < s.vaddr.saturating_add(s.mem_size)
        })
    }
}

/// Derive a page-aligned KASLR load bias from a per-boot entropy seed.
///
/// `window_pages` is the number of page slots the kernel reserves for the
/// random base; the returned bias is `slot * RXE_PAGE_SIZE` for some
/// `slot` in `0..window_pages`, so it is always page-aligned and bounded.
/// `window_pages == 0` yields a zero bias (no entropy available); the
/// caller is responsible for ensuring a non-degenerate window when KASLR
/// is required.
///
/// The mixing is `splitmix64`, deterministic in `seed` so a boot can
/// reproduce the layout from its recorded seed (`AGENTS.md` §19.2 —
/// "per-boot entropy seed").
#[must_use]
pub fn kaslr_bias(seed: u64, window_pages: u64) -> u64 {
    let max_slot = u64::MAX / RXE_PAGE_SIZE;
    let window = window_pages.min(max_slot);
    if window == 0 {
        return 0;
    }
    let slot = splitmix64(seed) % window;
    slot * RXE_PAGE_SIZE
}

/// `splitmix64` finaliser — a well-known, fast, full-period mixing of a
/// 64-bit seed. Not cryptographic; the cryptographic entropy is the caller's
/// `seed` (drawn from the platform RNG, §19.2).
const fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Constant-time inequality over two equal-length byte arrays.
///
/// The CFI-tag comparison does not branch on the contents so a hostile
/// image cannot probe the kernel's hash byte-by-byte via timing.
fn ct_ne(a: &[u8; SYSCALL_TABLE_HASH_LEN], b: &[u8; SYSCALL_TABLE_HASH_LEN]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        != 0
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::syscall::SYSCALL_TABLE_HASH_LEN;
    use alloc::string::String;
    use alloc::vec::Vec;

    const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x5A; SYSCALL_TABLE_HASH_LEN];

    /// A raw segment description used by the test encoder. Unlike
    /// [`Segment`], it carries the raw `flags`/`reserved` words so a test can
    /// deliberately encode an illegal image.
    struct RawSeg {
        vaddr: u64,
        file_offset: u64,
        file_size: u64,
        mem_size: u64,
        flags: u32,
        reserved: u32,
    }

    impl RawSeg {
        fn code(vaddr: u64, mem_size: u64) -> Self {
            Self {
                vaddr,
                file_offset: 0,
                file_size: mem_size,
                mem_size,
                flags: SEG_FLAG_READ | SEG_FLAG_EXEC,
                reserved: 0,
            }
        }
        fn data(vaddr: u64, mem_size: u64) -> Self {
            Self {
                vaddr,
                file_offset: 0,
                file_size: mem_size,
                mem_size,
                flags: SEG_FLAG_READ | SEG_FLAG_WRITE,
                reserved: 0,
            }
        }
        fn encode(&self) -> [u8; Segment::WIRE_LEN] {
            let mut out = [0u8; Segment::WIRE_LEN];
            out[0..8].copy_from_slice(&self.vaddr.to_le_bytes());
            out[8..16].copy_from_slice(&self.file_offset.to_le_bytes());
            out[16..24].copy_from_slice(&self.file_size.to_le_bytes());
            out[24..32].copy_from_slice(&self.mem_size.to_le_bytes());
            out[32..36].copy_from_slice(&self.flags.to_le_bytes());
            out[36..40].copy_from_slice(&self.reserved.to_le_bytes());
            out
        }
    }

    struct Builder {
        flags: u32,
        needed_count_override: Option<u16>,
        entry: u64,
        cfi_tag: [u8; SYSCALL_TABLE_HASH_LEN],
        magic: u32,
        abi_version: u32,
        segments: Vec<RawSeg>,
        needed: Vec<String>,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                flags: LOAD_FLAG_PIE,
                needed_count_override: None,
                entry: 0,
                cfi_tag: TAG,
                magic: LOAD_MAGIC,
                abi_version: ABI_VERSION_CURRENT,
                segments: Vec::new(),
                needed: Vec::new(),
            }
        }
        fn seg(mut self, s: RawSeg) -> Self {
            self.segments.push(s);
            self
        }
        fn needed(mut self, reference: &str) -> Self {
            self.needed.push(reference.into());
            self
        }
        fn build(&self) -> Vec<u8> {
            let count = u16::try_from(self.segments.len()).expect("segment count fits u16");
            let needed_count = self.needed_count_override.unwrap_or_else(|| {
                u16::try_from(self.needed.len()).expect("needed count fits u16")
            });
            let header = LoadHeader {
                magic: self.magic,
                abi_version: self.abi_version,
                flags: self.flags,
                segment_count: count,
                needed_count,
                entry: self.entry,
                cfi_tag: self.cfi_tag,
            };
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&header.to_le_bytes());
            for s in &self.segments {
                bytes.extend_from_slice(&s.encode());
            }
            for n in &self.needed {
                bytes.extend_from_slice(
                    &NeededLibrary::from_reference(n)
                        .expect("needed reference fits")
                        .to_le_bytes(),
                );
            }
            bytes
        }
    }

    fn valid_image() -> Vec<u8> {
        Builder {
            entry: 0x1000,
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .seg(RawSeg::data(0x2000, 0x1000))
        .build()
    }

    #[test]
    fn wire_sizes_are_frozen() {
        assert_eq!(LoadHeader::WIRE_LEN, 56);
        assert_eq!(Segment::WIRE_LEN, 40);
        assert_eq!(NeededLibrary::WIRE_LEN, 1 + LIBREF_MAX);
    }

    #[test]
    fn parses_a_valid_image() {
        let image = LoadImage::parse(&valid_image(), &TAG).expect("valid");
        assert_eq!(image.entry(), 0x1000);
        let segs = image.segments();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].permission, RxePermission::ReadExecute);
        assert_eq!(segs[1].permission, RxePermission::ReadWrite);
    }

    #[test]
    fn header_round_trips() {
        let bytes = valid_image();
        let header = LoadHeader::from_bytes(&bytes).expect("header");
        assert_eq!(LoadHeader::from_bytes(&header.to_le_bytes()), Ok(header));
    }

    #[test]
    fn segment_round_trips() {
        let s = Segment::decode(&RawSeg::code(0x4000, 0x2000).encode()).expect("seg");
        assert_eq!(Segment::decode(&s.to_le_bytes()), Ok(s));
    }

    #[test]
    fn refuses_write_execute_segment() {
        let bytes = Builder::new()
            .seg(RawSeg {
                vaddr: 0x1000,
                file_offset: 0,
                file_size: 0x1000,
                mem_size: 0x1000,
                flags: SEG_FLAG_READ | SEG_FLAG_WRITE | SEG_FLAG_EXEC,
                reserved: 0,
            })
            .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::WriteExecSegment)
        );
    }

    #[test]
    fn refuses_non_readable_segment() {
        let bytes = Builder::new()
            .seg(RawSeg {
                vaddr: 0x1000,
                file_offset: 0,
                file_size: 0x1000,
                mem_size: 0x1000,
                flags: SEG_FLAG_EXEC,
                reserved: 0,
            })
            .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::SegmentNotReadable)
        );
    }

    #[test]
    fn refuses_unknown_segment_flags() {
        let bytes = Builder::new()
            .seg(RawSeg {
                vaddr: 0x1000,
                file_offset: 0,
                file_size: 0x1000,
                mem_size: 0x1000,
                flags: SEG_FLAG_READ | (1 << 5),
                reserved: 0,
            })
            .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::UnknownSegmentFlags)
        );
    }

    #[test]
    fn refuses_non_pie_image() {
        let bytes = Builder {
            flags: 0,
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::NotPositionIndependent)
        );
    }

    #[test]
    fn refuses_unknown_header_flag() {
        let bytes = Builder {
            flags: LOAD_FLAG_PIE | (1 << 7),
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::ReservedNonZero)
        );
    }

    #[test]
    fn refuses_cfi_tag_mismatch() {
        let mut wrong = TAG;
        wrong[0] ^= 0xFF;
        assert_eq!(
            LoadImage::parse(&valid_image(), &wrong),
            Err(RxeError::InterfaceHashMismatch)
        );
    }

    #[test]
    fn refuses_bad_magic_and_version() {
        let bad_magic = Builder {
            magic: LOAD_MAGIC ^ 0xFFFF,
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        assert_eq!(LoadImage::parse(&bad_magic, &TAG), Err(RxeError::BadMagic));

        let bad_ver = Builder {
            abi_version: ABI_VERSION_CURRENT + 1,
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        assert_eq!(
            LoadImage::parse(&bad_ver, &TAG),
            Err(RxeError::BadAbiVersion)
        );
    }

    #[test]
    fn refuses_misaligned_segment() {
        let bytes = Builder::new().seg(RawSeg::code(0x1001, 0x1000)).build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::MisalignedSegment)
        );
    }

    #[test]
    fn refuses_bad_sizes() {
        let zero = Builder::new().seg(RawSeg::code(0x1000, 0)).build();
        assert_eq!(LoadImage::parse(&zero, &TAG), Err(RxeError::BadSegmentSize));

        let bytes = Builder::new()
            .seg(RawSeg {
                vaddr: 0x1000,
                file_offset: 0,
                file_size: 0x2000,
                mem_size: 0x1000,
                flags: SEG_FLAG_READ | SEG_FLAG_EXEC,
                reserved: 0,
            })
            .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::BadSegmentSize)
        );
    }

    #[test]
    fn refuses_overlapping_or_unsorted_segments() {
        let overlap = Builder::new()
            .seg(RawSeg::code(0x1000, 0x2000))
            .seg(RawSeg::data(0x2000, 0x1000))
            .build();
        assert_eq!(
            LoadImage::parse(&overlap, &TAG),
            Err(RxeError::SegmentOverlap)
        );

        let unsorted = Builder::new()
            .seg(RawSeg::code(0x3000, 0x1000))
            .seg(RawSeg::data(0x1000, 0x1000))
            .build();
        assert_eq!(
            LoadImage::parse(&unsorted, &TAG),
            Err(RxeError::SegmentOverlap)
        );
    }

    #[test]
    fn refuses_reserved_nonzero_in_segment() {
        let segment = Builder::new()
            .seg(RawSeg {
                vaddr: 0x1000,
                file_offset: 0,
                file_size: 0x1000,
                mem_size: 0x1000,
                flags: SEG_FLAG_READ | SEG_FLAG_EXEC,
                reserved: 7,
            })
            .build();
        assert_eq!(
            LoadImage::parse(&segment, &TAG),
            Err(RxeError::ReservedNonZero)
        );
    }

    #[test]
    fn parses_and_round_trips_needed_libraries() {
        let bytes = Builder {
            entry: 0x1000,
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .needed("/System/Libraries/libros-sys.so")
        .needed("/Apps/Example.app/Libraries/private.so")
        .build();
        let image = LoadImage::parse(&bytes, &TAG).expect("valid");
        let names: Vec<&str> = image.needed_libraries().collect();
        assert_eq!(
            names,
            [
                "/System/Libraries/libros-sys.so",
                "/Apps/Example.app/Libraries/private.so",
            ]
        );
    }

    #[test]
    fn parses_image_without_needed_libraries() {
        let image = LoadImage::parse(&valid_image(), &TAG).expect("valid");
        assert_eq!(image.needed_libraries().count(), 0);
    }

    #[test]
    fn refuses_too_many_needed_libraries() {
        let count = u16::try_from(LOAD_MAX_NEEDED + 1).expect("fits u16");
        let bytes = Builder {
            entry: 0x1000,
            needed_count_override: Some(count),
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        assert_eq!(LoadImage::parse(&bytes, &TAG), Err(RxeError::TooManyNeeded));
    }

    #[test]
    fn refuses_truncated_needed_table() {
        let bytes = Builder {
            entry: 0x1000,
            needed_count_override: Some(1),
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::BufferTooSmall)
        );
    }

    #[test]
    fn refuses_malformed_needed_record() {
        let mut bytes = Builder {
            entry: 0x1000,
            needed_count_override: Some(1),
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .build();
        // Append a zero-length (empty) needed record: rejected fail-closed.
        bytes.extend_from_slice(&[0u8; NeededLibrary::WIRE_LEN]);
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::BadNeededLibrary)
        );
    }

    #[test]
    fn needed_library_record_round_trips() {
        let lib = NeededLibrary::from_reference("/System/Libraries/libros-sys.so").expect("valid");
        assert_eq!(NeededLibrary::decode(&lib.to_le_bytes()), Ok(lib));
        assert_eq!(lib.reference(), "/System/Libraries/libros-sys.so");
        assert_eq!(
            NeededLibrary::from_reference(""),
            Err(RxeError::BadNeededLibrary)
        );
        assert_eq!(
            NeededLibrary::from_reference("has\0nul"),
            Err(RxeError::BadNeededLibrary)
        );
    }

    #[test]
    fn refuses_segment_counts_out_of_range() {
        let none = Builder::new().build();
        assert_eq!(LoadImage::parse(&none, &TAG), Err(RxeError::NoSegments));
    }

    #[test]
    fn refuses_too_many_segments() {
        let mut b = Builder::new();
        for i in 0..=u64::try_from(LOAD_MAX_SEGMENTS).unwrap() {
            b = b.seg(RawSeg::code(0x1000 + i * 0x1000, 0x1000));
        }
        let bytes = b.build();
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::TooManySegments)
        );
    }

    #[test]
    fn refuses_truncated_table() {
        let mut bytes = valid_image();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(
            LoadImage::parse(&bytes, &TAG),
            Err(RxeError::BufferTooSmall)
        );
    }

    #[test]
    fn refuses_entry_outside_executable_segment() {
        let outside = Builder {
            entry: 0x2000,
            ..Builder::new()
        }
        .seg(RawSeg::code(0x1000, 0x1000))
        .seg(RawSeg::data(0x2000, 0x1000))
        .build();
        assert_eq!(
            LoadImage::parse(&outside, &TAG),
            Err(RxeError::BadEntryPoint)
        );
    }

    #[test]
    fn kaslr_bias_is_aligned_bounded_and_deterministic() {
        for seed in 0..256u64 {
            let bias = kaslr_bias(seed, 1024);
            assert_eq!(bias % RXE_PAGE_SIZE, 0);
            assert!(bias < 1024 * RXE_PAGE_SIZE);
            assert_eq!(bias, kaslr_bias(seed, 1024));
        }
        assert_eq!(kaslr_bias(12345, 0), 0);
    }

    #[test]
    fn kaslr_bias_does_not_overflow_on_huge_window() {
        let bias = kaslr_bias(u64::MAX, u64::MAX);
        assert_eq!(bias % RXE_PAGE_SIZE, 0);
    }

    #[test]
    fn relocation_offsets_every_segment_and_entry() {
        let image = LoadImage::parse(&valid_image(), &TAG).expect("valid");
        let bias = 0x10_0000;
        assert_eq!(image.relocated_entry(bias), Ok(0x1000 + bias));
        assert_eq!(image.segments()[0].relocated_vaddr(bias), Ok(0x1000 + bias));
        assert_eq!(image.segments()[1].relocated_vaddr(bias), Ok(0x2000 + bias));
    }

    #[test]
    fn relocation_detects_overflow() {
        let image = LoadImage::parse(&valid_image(), &TAG).expect("valid");
        assert_eq!(
            image.segments()[0].relocated_vaddr(u64::MAX),
            Err(RxeError::AddressOverflow)
        );
        assert_eq!(
            image.relocated_entry(u64::MAX),
            Err(RxeError::AddressOverflow)
        );
    }

    #[test]
    fn permission_classification_matrix() {
        assert_eq!(
            RxePermission::from_segment_flags(SEG_FLAG_READ),
            Ok(RxePermission::ReadOnly)
        );
        assert_eq!(
            RxePermission::from_segment_flags(SEG_FLAG_READ | SEG_FLAG_EXEC),
            Ok(RxePermission::ReadExecute)
        );
        assert_eq!(
            RxePermission::from_segment_flags(SEG_FLAG_READ | SEG_FLAG_WRITE),
            Ok(RxePermission::ReadWrite)
        );
        assert!(RxePermission::ReadExecute.is_executable());
        assert!(RxePermission::ReadWrite.is_writable());
        assert!(!RxePermission::ReadOnly.is_executable());
    }
}
