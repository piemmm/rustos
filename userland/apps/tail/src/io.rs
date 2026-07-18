//! The seams through which `tail` touches the outside world.
//!
//! Keeping the filesystem, standard input, the two output streams, and the
//! standard information stream (fd 3) behind object-safe traits is what lets
//! the streaming logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland tools
//! (`head`'s `FileSource`/`Input`/`Output`, `ls`'s advisory `info`).

use tairix_abi::{Errno, FileId};

/// The identity and current length of a filesystem node, as the follow
/// engine observes it.
///
/// The engine compares [`Meta`]s across time to tell the three things
/// `tail -f`/`-F` must distinguish: the file *grew* (size increased, same
/// [`FileId`]), was *truncated* (size shrank below the read offset), or was
/// *rotated* (a different [`FileId`] now sits at the same name).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Meta {
    /// The node's stable system-wide identity.
    pub id: FileId,
    /// The node's current length in bytes.
    pub size: u64,
}

/// The seam through which `tail -f`/`-F` follows its sources: persistent
/// open handles, a kernel-backed change-wait, and process liveness.
///
/// Handles are opaque `u64` ids so the whole seam is object-safe and the
/// follow engine in [`crate::client`] runs against an in-memory fake with no
/// kernel. The production implementation ([`crate::run`]) backs a handle
/// with an owned descriptor and [`block`](Watcher::block) with a wait-set of
/// [`WaitSourceKind::File`](tairix_abi::WaitSourceKind::File) members, so a
/// follow parks off-CPU until a watched node changes — never a busy poll.
pub trait Watcher {
    /// Open `path` (a regular file) for reading, returning an opaque handle.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the open raises (e.g. [`Errno::NotFound`]).
    fn open(&self, path: &str) -> Result<u64, Errno>;

    /// Open `path` as a directory, returning an opaque handle. Used to watch
    /// a followed name's parent directory so a rotation there wakes the
    /// engine immediately.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the open raises.
    fn open_dir(&self, path: &str) -> Result<u64, Errno>;

    /// Release the descriptor behind `handle` (and stop watching it).
    /// Idempotent against a stale handle.
    fn close(&self, handle: u64);

    /// Read up to `buf.len()` bytes of `handle` starting at `offset`,
    /// returning the number read (`0` at end of file).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the read raises.
    fn read_at(&self, handle: u64, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>;

    /// The identity and size of the node behind `handle`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stat raises.
    fn meta(&self, handle: u64) -> Result<Meta, Errno>;

    /// The identity and size of the node currently at `path` (a fresh
    /// by-name resolution, for detecting rotation and for `--retry`).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stat raises (e.g. [`Errno::NotFound`] for a name
    /// that does not currently resolve).
    fn meta_path(&self, path: &str) -> Result<Meta, Errno>;

    /// Begin watching `handle` for changes (idempotent). A subsequent
    /// [`block`](Watcher::block) parks until this — or any other watched
    /// handle — changes.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the watch registration raises.
    fn watch(&self, handle: u64) -> Result<(), Errno>;

    /// Stop watching `handle` (idempotent).
    fn unwatch(&self, handle: u64);

    /// Park until a watched handle may have changed, or `timeout_ns`
    /// nanoseconds elapse (`u64::MAX` for no timeout). A spurious return is
    /// harmless — the caller re-reads every source and re-parks.
    fn block(&self, timeout_ns: u64);

    /// Whether process `pid` is still observably alive. A process the caller
    /// cannot observe reads as not alive (fail closed), which ends a
    /// `--pid` follow rather than waiting forever on an invisible process.
    fn pid_alive(&self, pid: u64) -> bool;
}

/// Reads a byte range of a named file.
///
/// The client streams a file by repeatedly calling [`read`](FileSource::read)
/// with an advancing `offset` until a call returns `0`, which marks
/// end-of-file. An implementation must return at most `buf.len()` bytes and
/// must report a short read (fewer than requested) only at end-of-file or by
/// returning an [`Errno`].
pub trait FileSource {
    /// Read up to `buf.len()` bytes of `path` starting at `offset`, returning
    /// the number of bytes written into `buf` (`0` at end-of-file).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// read it.
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// Reads the next bytes of standard input.
///
/// The client streams standard input by repeatedly calling
/// [`read`](Input::read) until a call returns `0` (end-of-input).
pub trait Input {
    /// Read up to `buf.len()` bytes of standard input, returning the number
    /// of bytes written into `buf` (`0` at end-of-input).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// Writes bytes to one of the tool's output streams.
///
/// The client uses two instances: standard output for the selected data and
/// headers, and standard error for the per-file diagnostics it reports
/// before moving to the next operand.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Emits framed advisory records to the standard information stream (fd 3).
///
/// Kept separate from [`Output`] because fd 3 is ignorable by contract: an
/// unattached consumer or a short write is silently a no-op and never
/// affects `tail`'s output, exit status, or pipeline semantics.
pub trait Info {
    /// Emit one framed `stdinfo` record on fd 3, best-effort.
    fn emit(&self, record: &[u8]);
}
