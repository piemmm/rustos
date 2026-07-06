//! Userland filesystem API wire types (`PREREQUISITES.md` P-A).
//!
//! These types describe the data a program exchanges with the kernel over
//! the `fs_open` / `fs_read` / `fs_write` / `fs_readdir` / `fs_stat` /
//! `fs_truncate` / `fs_sync` / `fs_mkdir` / `fs_unlink` / `fs_close`
//! syscalls. The syscalls themselves resolve a path or an open-file handle
//! against the kernel's secured VFS, which makes every per-inode
//! owner/mode/ACL/capability decision; this module only fixes the *shapes*
//! that cross the boundary, so the kernel and `lib/rt` cannot drift
//! (no duplication).
//!
//! Like the rest of the ABI surface the encodings are little-endian and
//! allocation-free, and every decoder treats its bytes as untrusted input:
//! it bounds-checks against the structure's `WIRE_LEN` and fails closed with
//! an [`Errno`] rather than indexing out of range.

use crate::le::{put_u32, put_u64, read_u32, read_u64};
use crate::Errno;

/// Maximum length, in bytes, of a single path passed to a filesystem
/// syscall.
///
/// A fail-closed validation bound on untrusted input, not a capacity that
/// scales with hardware: it caps the kernel staging buffer a path is copied
/// into. It is the sum of the VFS's own component-count and component-length
/// limits with room for separators, far larger than any real path, and a
/// path exceeding it is refused before any resolution begins.
pub const FS_PATH_MAX: usize = 4096;

/// Maximum number of bytes a single `fs_read` / `fs_write` transfers.
///
/// A fail-closed bound on the per-call kernel staging buffer (the same role
/// [`crate::RANDOM_REQUEST_MAX_BYTES`] plays for `random_get`): a larger
/// transfer is split by the `lib/rt` wrapper into successive calls, so this
/// never caps total file size, only one syscall's copy.
pub const FS_IO_MAX: usize = 1 << 20;

/// What an inode is, as reported by [`FileStat`] and each [`DirEntry`].
///
/// Deliberately closed: the VFS distinguishes only regular files and
/// directories at this layer (the on-disk driver may know more, but the
/// userland contract does not), and an unknown discriminant on decode fails
/// closed rather than being guessed.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum FileKind {
    /// A regular file: readable/writable byte content.
    Regular = 0,
    /// A directory: listable with `fs_readdir`.
    Directory = 1,
}

impl FileKind {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a [`FileKind`] from its wire discriminant, or
    /// [`Errno::OutOfRange`] for an unknown value (fail closed — never
    /// guess an unrecognised kind).
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `raw` is not a defined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Regular),
            1 => Ok(Self::Directory),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether this is a directory.
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// Flags accepted by [`fs_open`](crate::SyscallNumber::FS_OPEN).
///
/// A `#[repr(transparent)]` newtype over the `u32` flags register, mirroring
/// [`crate::MapFlags`]: only the bits named here are defined and
/// [`OpenFlags::from_bits`] rejects any reserved bit, so a future flag is
/// never silently ignored by an older kernel (validate every input, fail
/// closed).
///
/// An open with neither [`READ`](Self::READ) nor [`WRITE`](Self::WRITE) is a
/// *resolve-only* handle: it validates the path and search permission and
/// can be `fs_stat`'d or `fs_readdir`'d, but `fs_read`/`fs_write` against it
/// fail closed. This is the handle a caller opens purely to stat a node it
/// may traverse to but not read.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct OpenFlags(u32);

impl OpenFlags {
    /// Request read access. `fs_read` requires it.
    pub const READ: Self = Self(1 << 0);
    /// Request write access. `fs_write`/`fs_truncate` require it.
    pub const WRITE: Self = Self(1 << 1);
    /// Create the file if it does not exist (regular file only).
    pub const CREATE: Self = Self(1 << 2);
    /// Truncate the file to zero length on open. Requires
    /// [`WRITE`](Self::WRITE).
    pub const TRUNCATE: Self = Self(1 << 3);
    /// Every `fs_write` appends at the current end of file, ignoring the
    /// supplied offset (the journal-append posture). Requires
    /// [`WRITE`](Self::WRITE).
    pub const APPEND: Self = Self(1 << 4);
    /// The target must be a directory; opening a regular file fails closed.
    pub const DIRECTORY: Self = Self(1 << 5);
    /// With [`CREATE`](Self::CREATE), fail closed if the file already
    /// exists (exclusive create).
    pub const EXCLUSIVE: Self = Self(1 << 6);

    /// The set of all defined flag bits.
    const DEFINED_BITS: u32 = Self::READ.0
        | Self::WRITE.0
        | Self::CREATE.0
        | Self::TRUNCATE.0
        | Self::APPEND.0
        | Self::DIRECTORY.0
        | Self::EXCLUSIVE.0;

    /// An empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw flag bits, as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// The union of `self` and `other`.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Build a flag set from raw bits, rejecting any reserved bit and any
    /// combination the contract forbids.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `bits` sets a reserved bit, or if
    /// [`TRUNCATE`](Self::TRUNCATE) / [`APPEND`](Self::APPEND) /
    /// [`EXCLUSIVE`](Self::EXCLUSIVE) appears without the access it requires
    /// (so an illegal request is rejected at the boundary, never half-applied).
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::DEFINED_BITS != 0 {
            return Err(Errno::OutOfRange);
        }
        let flags = Self(bits);
        let writes = flags.contains(Self::WRITE);
        if flags.contains(Self::TRUNCATE) && !writes {
            return Err(Errno::OutOfRange);
        }
        if flags.contains(Self::APPEND) && !writes {
            return Err(Errno::OutOfRange);
        }
        if flags.contains(Self::EXCLUSIVE) && !flags.contains(Self::CREATE) {
            return Err(Errno::OutOfRange);
        }
        if flags.contains(Self::DIRECTORY) && writes {
            // A directory is never opened for byte writes.
            return Err(Errno::OutOfRange);
        }
        Ok(flags)
    }

    /// Whether every bit set in `other` is also set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether read access was requested.
    #[must_use]
    pub const fn is_read(self) -> bool {
        self.contains(Self::READ)
    }

    /// Whether write access was requested.
    #[must_use]
    pub const fn is_write(self) -> bool {
        self.contains(Self::WRITE)
    }
}

/// Flags accepted by [`fs_unlink`](crate::SyscallNumber::FS_UNLINK).
///
/// A `#[repr(transparent)]` newtype over the `u32` flags register, mirroring
/// [`OpenFlags`]: only the bits named here are defined and
/// [`UnlinkFlags::from_bits`] rejects any reserved bit, so a future flag is
/// never silently ignored by an older kernel (validate every input, fail
/// closed).
///
/// An empty flag set is the historical `fs_unlink`: it removes the named
/// file or (empty) directory. [`DIRECTORY`](Self::DIRECTORY) is the
/// `rmdir`/`unlinkat(AT_REMOVEDIR)` posture: the removal succeeds only when
/// the name is an (empty) **directory**, decided atomically by the
/// filesystem under its own lock — never by a caller-side `fs_stat` that a
/// concurrent rename could invalidate between the check and the removal.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct UnlinkFlags(u32);

impl UnlinkFlags {
    /// Remove the name only if it is an (empty) directory; a non-directory
    /// fails closed with [`Errno::NotADirectory`].
    pub const DIRECTORY: Self = Self(1 << 0);

    /// The set of all defined flag bits.
    const DEFINED_BITS: u32 = Self::DIRECTORY.0;

    /// An empty flag set: remove the named file or (empty) directory.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw flag bits, as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build a flag set from raw bits, rejecting any reserved bit.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `bits` sets a reserved bit (an unknown
    /// request is rejected at the boundary, never silently ignored).
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::DEFINED_BITS != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }

    /// Whether the removal is restricted to an (empty) directory.
    #[must_use]
    pub const fn is_directory_only(self) -> bool {
        self.0 & Self::DIRECTORY.0 != 0
    }
}

/// The structural metadata `fs_stat` reports for an inode.
///
/// Carries only what the userland contract exposes: the node kind, its byte
/// size, its allocated on-disk bytes, the POSIX mode bits, and the owning
/// uid/gid. The kernel fills it
/// from the VFS's authorised view of the node; a program never reads it from
/// a `/proc`-style file (there is none).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FileStat {
    /// Whether the node is a regular file or a directory.
    pub kind: FileKind,
    /// File length in bytes; `0` for a directory.
    pub size: u64,
    /// Bytes of on-disk storage the node's data occupies — the real
    /// allocation the mounted format tracks, reported by the filesystem
    /// driver (never derived from `size` when the format knows better).
    /// `0` for a node whose data occupies no dedicated blocks.
    pub allocated: u64,
    /// POSIX mode bits (the low 12 bits are meaningful).
    pub mode: u32,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
}

impl FileStat {
    /// Encoded size of a [`FileStat`] on the wire.
    ///
    /// `kind(1)` + `pad(7)` + `size(8)` + `allocated(8)` + `mode(4)` +
    /// `uid(4)` + `gid(4)`, padded to a multiple of 8 for natural alignment
    /// of the `u64` fields.
    pub const WIRE_LEN: usize = 40;

    /// Encode `self` into the first [`FileStat::WIRE_LEN`] bytes of `out`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `out` is shorter than
    /// [`FileStat::WIRE_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        out[..Self::WIRE_LEN].fill(0);
        out[0] = self.kind.as_u8();
        put_u64(out, 8, self.size);
        put_u64(out, 16, self.allocated);
        put_u32(out, 24, self.mode);
        put_u32(out, 28, self.uid);
        put_u32(out, 32, self.gid);
        Ok(Self::WIRE_LEN)
    }

    /// Decode a [`FileStat`] from the first [`FileStat::WIRE_LEN`] bytes of
    /// `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than
    ///   [`FileStat::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] if the `kind` byte is not a defined
    ///   [`FileKind`].
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            kind: FileKind::from_u8(bytes[0])?,
            size: read_u64(bytes, 8),
            allocated: read_u64(bytes, 16),
            mode: read_u32(bytes, 24),
            uid: read_u32(bytes, 28),
            gid: read_u32(bytes, 32),
        })
    }
}

/// Maximum length, in bytes, of a single directory-entry name in the
/// `fs_readdir` stream.
///
/// A fail-closed bound matching the VFS's own component-length limit; a
/// driver reporting a longer name is a structural fault, not a capacity.
pub const FS_NAME_MAX: usize = 255;

/// One entry in the packed `fs_readdir` stream.
///
/// `fs_readdir` fills the caller's buffer with consecutive records, each a
/// fixed [`DirEntry::HEADER_LEN`]-byte header (`kind` + name length) followed
/// by exactly `name_len` UTF-8 name bytes (no NUL). The reader walks the
/// buffer with [`DirEntry::decode`]; the kernel writes it with
/// [`DirEntry::encode_into`]. The packing lives here, once, so producer and
/// consumer cannot disagree (no duplication).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DirEntry<'a> {
    /// Whether the entry names a regular file or a directory.
    pub kind: FileKind,
    /// The entry's name (UTF-8, no terminator, never empty, never `.`/`..`).
    pub name: &'a [u8],
}

impl<'a> DirEntry<'a> {
    /// Size of the fixed per-entry header: `kind(1)` + `pad(1)` +
    /// `name_len(2)`.
    pub const HEADER_LEN: usize = 4;

    /// The total encoded length of this entry (header plus name).
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        Self::HEADER_LEN + self.name.len()
    }

    /// Encode this entry into the front of `out`, returning the number of
    /// bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if the name is empty or longer than
    ///   [`FS_NAME_MAX`].
    /// * [`Errno::BufferTooSmall`] if `out` cannot hold the whole record.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let name_len = self.name.len();
        if name_len == 0 || name_len > FS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let total = self.encoded_len();
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        out[0] = self.kind.as_u8();
        out[1] = 0;
        // `name_len <= FS_NAME_MAX` (255) fits a u16 with room to spare; the
        // checked conversion makes the bound explicit rather than truncating.
        let name_len_u16 = u16::try_from(name_len).map_err(|_| Errno::LengthOutOfRange)?;
        let [lo, hi] = name_len_u16.to_le_bytes();
        out[2] = lo;
        out[3] = hi;
        out[Self::HEADER_LEN..total].copy_from_slice(self.name);
        Ok(total)
    }

    /// Decode the first entry from `bytes`, returning it and the number of
    /// bytes it consumed (so the caller can advance to the next record).
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than the header or
    ///   than the declared name.
    /// * [`Errno::OutOfRange`] if the `kind` byte is not a defined
    ///   [`FileKind`].
    /// * [`Errno::LengthOutOfRange`] if the declared name length is zero or
    ///   exceeds [`FS_NAME_MAX`].
    pub fn decode(bytes: &'a [u8]) -> Result<(Self, usize), Errno> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let kind = FileKind::from_u8(bytes[0])?;
        let name_len = usize::from(bytes[2]) | (usize::from(bytes[3]) << 8);
        if name_len == 0 || name_len > FS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let total = Self::HEADER_LEN + name_len;
        if bytes.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        Ok((
            Self {
                kind,
                name: &bytes[Self::HEADER_LEN..total],
            },
            total,
        ))
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::{DirEntry, FileKind, FileStat, OpenFlags, UnlinkFlags, FS_NAME_MAX};
    use crate::Errno;
    use alloc::vec;

    #[test]
    fn file_kind_round_trips_and_rejects_unknown() {
        for k in [FileKind::Regular, FileKind::Directory] {
            assert_eq!(FileKind::from_u8(k.as_u8()), Ok(k));
        }
        assert_eq!(FileKind::from_u8(2), Err(Errno::OutOfRange));
        assert_eq!(FileKind::from_u8(0xFF), Err(Errno::OutOfRange));
        assert!(FileKind::Directory.is_dir());
        assert!(!FileKind::Regular.is_dir());
    }

    #[test]
    fn open_flags_reject_reserved_bits() {
        assert_eq!(OpenFlags::from_bits(1 << 7), Err(Errno::OutOfRange));
        assert_eq!(OpenFlags::from_bits(u32::MAX), Err(Errno::OutOfRange));
        assert_eq!(OpenFlags::from_bits(0).map(OpenFlags::bits), Ok(0));
    }

    #[test]
    fn unlink_flags_reject_reserved_bits_and_decode_directory() {
        // Only bit 0 (DIRECTORY) is defined; anything else fails closed.
        assert_eq!(UnlinkFlags::from_bits(1 << 1), Err(Errno::OutOfRange));
        assert_eq!(UnlinkFlags::from_bits(u32::MAX), Err(Errno::OutOfRange));
        let plain = UnlinkFlags::from_bits(0).unwrap();
        assert!(!plain.is_directory_only());
        assert_eq!(plain, UnlinkFlags::empty());
        let dir_only = UnlinkFlags::from_bits(UnlinkFlags::DIRECTORY.bits()).unwrap();
        assert!(dir_only.is_directory_only());
        assert_eq!(dir_only, UnlinkFlags::DIRECTORY);
    }

    #[test]
    fn open_flags_enforce_dependent_combinations() {
        // TRUNCATE/APPEND require WRITE; EXCLUSIVE requires CREATE.
        assert_eq!(
            OpenFlags::from_bits(OpenFlags::TRUNCATE.bits()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            OpenFlags::from_bits(OpenFlags::APPEND.bits()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            OpenFlags::from_bits(OpenFlags::EXCLUSIVE.bits()),
            Err(Errno::OutOfRange)
        );
        // A directory open never carries WRITE.
        assert_eq!(
            OpenFlags::from_bits(OpenFlags::DIRECTORY.union(OpenFlags::WRITE).bits()),
            Err(Errno::OutOfRange)
        );
        // Valid combinations decode.
        let rw = OpenFlags::READ
            .union(OpenFlags::WRITE)
            .union(OpenFlags::CREATE)
            .union(OpenFlags::TRUNCATE);
        let decoded = OpenFlags::from_bits(rw.bits()).expect("valid");
        assert!(decoded.is_read() && decoded.is_write());
        assert!(decoded.contains(OpenFlags::CREATE));
        let excl = OpenFlags::from_bits(
            OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::EXCLUSIVE)
                .bits(),
        )
        .expect("create+excl valid");
        assert!(excl.contains(OpenFlags::EXCLUSIVE));
    }

    #[test]
    fn file_stat_round_trips() {
        let stat = FileStat {
            kind: FileKind::Regular,
            size: 0x0123_4567_89AB_CDEF,
            allocated: 0x0FED_CBA9_8765_4321,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
        };
        let mut buf = [0u8; FileStat::WIRE_LEN];
        assert_eq!(stat.encode(&mut buf), Ok(FileStat::WIRE_LEN));
        assert_eq!(FileStat::decode(&buf), Ok(stat));
    }

    #[test]
    fn file_stat_rejects_short_buffers() {
        let stat = FileStat {
            kind: FileKind::Directory,
            size: 0,
            allocated: 0,
            mode: 0o755,
            uid: 0,
            gid: 0,
        };
        let mut tiny = [0u8; FileStat::WIRE_LEN - 1];
        assert_eq!(stat.encode(&mut tiny), Err(Errno::BufferTooSmall));
        assert_eq!(
            FileStat::decode(&[0u8; FileStat::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn file_stat_decode_rejects_unknown_kind() {
        let mut buf = [0u8; FileStat::WIRE_LEN];
        buf[0] = 9;
        assert_eq!(FileStat::decode(&buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn dir_entry_stream_round_trips() {
        let entries = [
            DirEntry {
                kind: FileKind::Directory,
                name: b"Logs",
            },
            DirEntry {
                kind: FileKind::Regular,
                name: b"motd.txt",
            },
        ];
        let mut buf = vec![0u8; 256];
        let mut off = 0;
        for e in &entries {
            off += e.encode_into(&mut buf[off..]).expect("fits");
        }
        let total = off;

        let mut cursor = 0;
        let mut decoded = vec![];
        while cursor < total {
            let (entry, used) = DirEntry::decode(&buf[cursor..total]).expect("valid");
            decoded.push((entry.kind, entry.name.to_vec()));
            cursor += used;
        }
        assert_eq!(cursor, total);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], (FileKind::Directory, b"Logs".to_vec()));
        assert_eq!(decoded[1], (FileKind::Regular, b"motd.txt".to_vec()));
    }

    #[test]
    fn dir_entry_rejects_empty_and_oversize_names() {
        let mut buf = [0u8; 8];
        let empty = DirEntry {
            kind: FileKind::Regular,
            name: b"",
        };
        assert_eq!(empty.encode_into(&mut buf), Err(Errno::LengthOutOfRange));
        let big = vec![b'a'; FS_NAME_MAX + 1];
        let oversize = DirEntry {
            kind: FileKind::Regular,
            name: &big,
        };
        let mut wide = vec![0u8; FS_NAME_MAX + 8];
        assert_eq!(
            oversize.encode_into(&mut wide),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn dir_entry_encode_into_rejects_short_buffer() {
        let e = DirEntry {
            kind: FileKind::Regular,
            name: b"abcd",
        };
        let mut buf = [0u8; DirEntry::HEADER_LEN + 2];
        assert_eq!(e.encode_into(&mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn dir_entry_decode_rejects_truncated_record() {
        // Header claims a 4-byte name but only 2 follow.
        let buf = [FileKind::Regular.as_u8(), 0, 4, 0, b'a', b'b'];
        assert_eq!(DirEntry::decode(&buf), Err(Errno::BufferTooSmall));
        assert_eq!(DirEntry::decode(&[0u8; 2]), Err(Errno::BufferTooSmall));
    }
}
