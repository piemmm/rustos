//! The [`Fs`] seam: everything the file manager asks of the filesystem.
//!
//! The model is I/O-free; every directory read and free-space query goes
//! through this trait. The `Run` binary implements it over the
//! kernel-authorised `fs_*` syscalls (every per-inode and mount check stays
//! kernel-side, and a refusal comes back as the frozen [`Errno`] the model
//! surfaces on its message line); the tests implement it over an in-memory
//! tree, so the whole session is drivable without a kernel.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::time::Time64;
use tairix_abi::{Errno, FileKind};

/// One directory entry as the listing reports it: exactly the fields the
/// kernel's `fs_readdir` stream carries per entry, so a listing never costs
/// a per-child stat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsEntry {
    /// The entry's name within its directory (a single component).
    pub name: String,
    /// The entry's **own** kind, as the stream reports it: a symbolic link
    /// arrives as [`FileKind::Symlink`], never as what it names.
    pub kind: FileKind,
    /// Apparent length in bytes; `0` for a directory.
    pub size: u64,
    /// Last contents-modification instant as the mounted format stores it;
    /// [`Time64::UNIX_EPOCH`] means the backing keeps no stamp and renders
    /// as absent, never as a fabricated date.
    pub modified: Time64,
}

/// Free/total byte counts of the volume backing a path, for the status
/// line. `None` when the query is unavailable (no sysinfo service, or the
/// caller lacks the query's capability) — the status line then simply
/// omits the figure; absence is never an error.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeSpace {
    /// Bytes still allocatable on the volume.
    pub free_bytes: u64,
    /// Total capacity of the volume in bytes.
    pub total_bytes: u64,
}

/// One published storage root the volume list (`V`) offers: where it is
/// mounted, what backs it, and its space figures when known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeInfo {
    /// The mount target — the path the tree opens when the volume is
    /// chosen.
    pub target: String,
    /// The mounted filesystem's type name (`arxfs`, `ext4`, …), as the
    /// mount table reports it.
    pub fstype: String,
    /// Free/total bytes, or `None` when the volume cannot report them
    /// (shown as absent, never fabricated).
    pub space: Option<VolumeSpace>,
}

/// The filesystem operations the landed stages perform: listings, the
/// volume list, the mode editor's stat/set pair, and the mutating
/// operations the file commands drive (probe, streamed read/write,
/// create, mkdir, unlink, rename). Later stages extend this trait in
/// place together with the callers they introduce.
pub trait Fs {
    /// List every entry of the directory `path`, in any order (the model
    /// sorts them).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// vanished path or [`Errno::PermissionDenied`] when the caller may not
    /// list it. The model surfaces the error and keeps its previous state.
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, Errno>;

    /// Report the free/total space of the volume backing `path`, or `None`
    /// when the figure is unavailable (best-effort; never an error).
    fn volume_space(&mut self, path: &str) -> Option<VolumeSpace>;

    /// The published storage roots the session can open — the mounted
    /// volumes as the System Information API reports them. Best-effort by
    /// contract: an unreachable service yields an empty list (the volume
    /// list then says no volumes were reported), never an error.
    fn list_volumes(&mut self) -> Vec<VolumeInfo>;

    /// The current permission bits of the entry at `path` (a resolve-only
    /// stat — no read authority is requested), for the mode editor's
    /// pre-filled prompt.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; the model surfaces it and
    /// opens no prompt.
    fn stat_mode(&mut self, path: &str) -> Result<u32, Errno>;

    /// Set the permission bits of the entry at `path` to `mode` (at most
    /// [`tairix_abi::FS_MODE_MASK`]). The kernel owns the authorisation:
    /// only the entry's owner may change its mode, and a refusal comes
    /// back as the frozen [`Errno`] the model surfaces unchanged.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; the model reports it and
    /// changes nothing.
    fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Errno>;

    /// Every visible extended-attribute key of the entry at `path`, in the
    /// backing's stable order (the `lib/fsmeta` `namespace.rest` grammar;
    /// keys the caller may not read are omitted by the kernel, never
    /// shown).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; [`Errno::NotSupported`] means
    /// the mounted format stores no attributes, which the attributes view
    /// states honestly rather than treating as an empty set.
    fn attr_list(&mut self, path: &str) -> Result<Vec<String>, Errno>;

    /// The value of extended attribute `key` on the entry at `path`
    /// (opaque bytes; a value may be empty).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; [`Errno::NoData`] means the
    /// entry carries no such attribute.
    fn attr_get(&mut self, path: &str, key: &str) -> Result<Vec<u8>, Errno>;

    /// Set (insert or replace) extended attribute `key` on the entry at
    /// `path` to `value`. The kernel owns the authorisation: write
    /// permission on the node, a writable mount, the shared key grammar,
    /// and the fixed size bounds; a refusal comes back as the frozen
    /// [`Errno`] the model surfaces unchanged.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; the model reports it and
    /// changes nothing.
    fn attr_set(&mut self, path: &str, key: &str, value: &[u8]) -> Result<(), Errno>;

    /// Remove extended attribute `key` from the entry at `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; [`Errno::NoData`] means no
    /// such attribute was stored.
    fn attr_remove(&mut self, path: &str, key: &str) -> Result<(), Errno>;

    /// The [`FileKind`] of the entry `path` **names** — a resolve-only stat
    /// that does *not* follow a final symbolic link — used to probe a
    /// destination before any I/O.
    ///
    /// Keeping the final link is what makes the probe safe: a link already
    /// sitting at a destination must be seen as the link it is, because
    /// creating or truncating "through" it would act on whatever it points
    /// at, anywhere on the volume. Following it here would hand an attacker
    /// who can plant a name inside a destination tree a redirect for every
    /// later write.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; [`Errno::NotFound`] means the
    /// path is absent, which the operations treat as "free to create".
    fn stat_kind(&mut self, path: &str) -> Result<FileKind, Errno>;

    /// The target the symbolic link at `path` stores, verbatim.
    ///
    /// The final component is never followed — the call is about the link —
    /// and the target comes back exactly as stored, still unresolved, which
    /// is what recreating the link at a new name needs.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; [`Errno::OutOfRange`] when `path`
    /// names anything but a symbolic link, and [`Errno::NotSupported`] on a
    /// mount whose format stores no links.
    fn read_link(&mut self, path: &str) -> Result<String, Errno>;

    /// Create a symbolic link at `path` whose stored target is `target`.
    ///
    /// `target` is stored verbatim and never resolved, so recreating a link
    /// elsewhere reproduces exactly the spelling the original held. A new
    /// link never replaces an existing name: an occupied `path` is
    /// [`Errno::AlreadyExists`], so a caller that means to replace one
    /// removes it first.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — [`Errno::AlreadyExists`] for a
    /// taken name, [`Errno::NotSupported`] on a format that stores no links.
    fn create_link(&mut self, target: &str, path: &str) -> Result<(), Errno>;

    /// Read up to `buf.len()` bytes of the file at `path` from `offset`,
    /// returning the count read (`0` at end of file). A short read (fewer
    /// than requested) occurs only at end of file.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reading.
    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Create (or truncate to empty) the regular file at `path`, ready to
    /// be written from offset `0`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while creating.
    fn create(&mut self, path: &str) -> Result<(), Errno>;

    /// Write every byte of `bytes` to the file at `path` from `offset`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while writing; a partial write
    /// is an error.
    fn write(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno>;

    /// Create the directory at `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::AlreadyExists`].
    fn mkdir(&mut self, path: &str) -> Result<(), Errno>;

    /// Remove the non-directory entry `path` names — the name as typed, so
    /// removing a symbolic link removes the *link* and never what it names.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while removing.
    fn remove_file(&mut self, path: &str) -> Result<(), Errno>;

    /// Remove the **empty** directory at `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — a non-empty directory is the
    /// kernel's refusal to surface, never silently recursed into.
    fn remove_dir(&mut self, path: &str) -> Result<(), Errno>;

    /// Rename `src` to `dst` atomically within one volume, or report
    /// [`RenameOutcome::CrossDevice`] when the two paths live on different
    /// volumes so the caller can fall back to copy-then-remove.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises for a non-boundary failure.
    fn rename(&mut self, src: &str, dst: &str) -> Result<RenameOutcome, Errno>;
}

/// What a [`Fs::rename`] achieved: the atomic rename, or the honest
/// cross-volume report that drives the copy-then-remove fallback.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RenameOutcome {
    /// The entry was renamed atomically.
    Renamed,
    /// `src` and `dst` live on different volumes; nothing was changed.
    CrossDevice,
}
