//! The pure execution model behind the management verbs
//! (`plans/NEW-FILEMANAGER.md` FM7b): how a planned paste is *carried out*.
//!
//! Where [`clipboard`](crate::clipboard) decides *what* moves where (a
//! [`PastePlan`](crate::clipboard::PastePlan) of source→dest items), this
//! module decides *how* each item is executed and *how a byte-for-byte copy is
//! streamed* — both as pure, host-provable models that touch no filesystem.
//! The `files.app` `Run` binary performs the actual `fs_rename` /
//! `fs_read`→`fs_write` / `fs_unlink` under the user's own identity in its
//! own capability-checked tail (§4, §5.4); the engine only names the plan, so
//! composing it grants nothing and the read-only picker never runs it.
//!
//! # Move vs. copy
//!
//! A move within one volume is a single [`fs_rename`]; a move *across* volumes
//! cannot rename and must copy-then-delete, exactly as `mv` decides from
//! `st_dev`. [`paste_strategy`] makes that decision from the clipboard
//! operation and the two items' [`VolumeId`]s, so the file manager and any
//! future caller share one definition of it (§2.2).
//!
//! # Bounded, interruptible copy
//!
//! A copy never holds an unbounded buffer and never spins (§2.23): the
//! [`CopyCursor`] walks a known-length source in fixed [`COPY_CHUNK_LEN`]
//! steps, yielding the next [`CopyChunk`] to transfer. The app reads and
//! writes that chunk, then reports the bytes actually carried with
//! [`CopyCursor::advance`]; between chunks it may cancel or be preempted, and
//! the cursor resumes from exactly where it stopped (see
//! [`CopyCursor::resume`]). It is fail closed: advancing past the source's
//! length — a source that shrank or a miscounted transfer — is
//! [`CopyError::Overrun`], never a silent wrap.
//!
//! [`fs_rename`]: tairix_abi::SyscallNumber::FS_RENAME

use crate::clipboard::ClipboardOp;

/// The stable system-wide identity of the mounted volume a path lives on — the
/// 16-byte [`volume`](tairix_abi::fs::FileId::volume) field the app reads from
/// `fs_stat`. Two paths can be moved with a single rename only when their
/// volume ids are equal; otherwise the move must copy then delete.
///
/// The engine only *compares* two ids — it never mints one, and the bytes are
/// opaque to it (the filesystem driver assigns them).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeId([u8; 16]);

impl VolumeId {
    /// The volume id read from a node's `fs_stat` result.
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw 16-byte identifier.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 16] {
        self.0
    }
}

/// How a single planned paste item must be carried out on the filesystem.
///
/// Exhaustive over the two clipboard operations crossed with the same-/
/// different-volume distinction: a `Copy` always streams; a `Cut` renames
/// within a volume and copies-then-deletes across volumes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PasteStrategy {
    /// A same-volume move: one `fs_rename` from the source to the destination.
    /// The source is gone once it succeeds; nothing is streamed.
    Rename,
    /// A cross-volume move: stream-copy the source to the destination, then
    /// remove the source once the copy has fully succeeded (never before, so a
    /// failed copy loses no data, §5.4).
    CopyThenDelete,
    /// A copy: stream-copy the source to the destination and leave the source
    /// in place.
    Copy,
}

/// Decide how to carry out a paste item, given the clipboard `op` and the
/// [`VolumeId`]s of the source and its destination directory.
///
/// * [`ClipboardOp::Copy`] → [`PasteStrategy::Copy`] (the source stays).
/// * [`ClipboardOp::Cut`] on the same volume → [`PasteStrategy::Rename`].
/// * [`ClipboardOp::Cut`] across volumes → [`PasteStrategy::CopyThenDelete`].
#[must_use]
pub fn paste_strategy(op: ClipboardOp, source: VolumeId, dest: VolumeId) -> PasteStrategy {
    match op {
        ClipboardOp::Copy => PasteStrategy::Copy,
        ClipboardOp::Cut if source == dest => PasteStrategy::Rename,
        ClipboardOp::Cut => PasteStrategy::CopyThenDelete,
    }
}

/// The maximum number of bytes one [`CopyChunk`] transfers.
///
/// A fixed I/O transfer bound, not a hardware-scaled capacity (§24.4): it caps
/// the buffer a single copy step reads and writes so a large copy stays
/// interruptible and its memory fixed, exactly as the kernel bounds one
/// `fs_read`/`fs_write` with [`FS_IO_MAX`](tairix_abi::fs::FS_IO_MAX). A copy
/// of any size is handled by iterating chunks, so this never caps a file's
/// length (§26.6). Chosen to match the per-call transfer bound so one chunk is
/// one syscall's worth of work.
pub const COPY_CHUNK_LEN: u64 = tairix_abi::fs::FS_IO_MAX as u64;

/// One bounded step of a streamed copy: the byte range to read from the source
/// and write to the destination.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CopyChunk {
    offset: u64,
    len: u64,
}

impl CopyChunk {
    /// The byte offset into the source (and destination) this chunk begins at.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The number of bytes this chunk transfers — never more than
    /// [`COPY_CHUNK_LEN`].
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// `true` when the chunk transfers no bytes. A [`CopyCursor`] never yields
    /// an empty chunk (it returns `None` when complete); this exists for
    /// callers that hold a [`CopyChunk`] directly.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Why a copy cursor cannot advance — a fail-closed refusal (§5.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CopyError {
    /// An advance (or a resume) would carry the copied count past the source's
    /// total length: the source shrank under the copy, or a transfer reported
    /// more bytes than it moved. Refused rather than wrapping or over-reading.
    Overrun,
}

impl core::fmt::Display for CopyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Overrun => f.write_str("copy advanced past the source length"),
        }
    }
}

/// A resumable, cancelable cursor over a byte-for-byte file copy.
///
/// Models the bounded, interruptible streaming copy without doing any I/O: it
/// tracks how many bytes of a known-length source have been carried and yields
/// the next [`CopyChunk`]. The app performs the read/write for that chunk with
/// its own capability-checked syscalls, then calls [`advance`](Self::advance)
/// with the number of bytes actually transferred; between chunks it may stop
/// (a Cancel) or the task may be preempted, and the cursor picks up exactly
/// where it left off. There is no unbounded buffer and no spin (§2.23).
///
/// A short transfer (fewer bytes than the chunk asked for) simply advances the
/// cursor by that amount and the next chunk covers the remainder; a transfer
/// of zero bytes with data still outstanding is a truncated or failing source
/// the *app* reports and stops on (§26.5) — the cursor makes no progress and
/// never loops on its own.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CopyCursor {
    total: u64,
    copied: u64,
}

impl CopyCursor {
    /// A cursor for copying a source of `total` bytes, starting from the
    /// beginning.
    #[must_use]
    pub const fn new(total: u64) -> Self {
        Self { total, copied: 0 }
    }

    /// A cursor resumed after `copied` bytes of a `total`-byte source have
    /// already been carried — the state persisted across a cancel or a
    /// preemption.
    ///
    /// # Errors
    ///
    /// [`CopyError::Overrun`] when `copied` exceeds `total`.
    pub const fn resume(total: u64, copied: u64) -> Result<Self, CopyError> {
        if copied > total {
            return Err(CopyError::Overrun);
        }
        Ok(Self { total, copied })
    }

    /// The source's total length in bytes.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// The number of bytes copied so far.
    #[must_use]
    pub const fn copied(&self) -> u64 {
        self.copied
    }

    /// The number of bytes still to copy.
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.total - self.copied
    }

    /// `true` once every byte has been copied.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.copied == self.total
    }

    /// The next chunk to transfer, or `None` when the copy is complete.
    ///
    /// The chunk begins at [`copied`](Self::copied) and spans the lesser of
    /// [`COPY_CHUNK_LEN`] and [`remaining`](Self::remaining), so it is always
    /// non-empty until the copy finishes.
    #[must_use]
    pub const fn next_chunk(&self) -> Option<CopyChunk> {
        let remaining = self.remaining();
        if remaining == 0 {
            return None;
        }
        let len = if remaining < COPY_CHUNK_LEN {
            remaining
        } else {
            COPY_CHUNK_LEN
        };
        Some(CopyChunk {
            offset: self.copied,
            len,
        })
    }

    /// Record that `bytes` more bytes were transferred, advancing the cursor.
    ///
    /// # Errors
    ///
    /// [`CopyError::Overrun`] when `bytes` exceeds [`remaining`](Self::remaining)
    /// (a source that shrank, or a miscounted transfer); the cursor is left
    /// unchanged.
    pub fn advance(&mut self, bytes: u64) -> Result<(), CopyError> {
        if bytes > self.remaining() {
            return Err(CopyError::Overrun);
        }
        self.copied += bytes;
        Ok(())
    }
}
