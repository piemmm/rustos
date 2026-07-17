//! The editor's file seam: how `:w`, `:e`, `:r`, and the startup load reach
//! the filesystem.
//!
//! The editor core is I/O-free; every file it touches goes through this one
//! injected trait. The `Run` binary implements it over the kernel-authorised
//! `fs_*` syscalls (every per-inode and mount check stays kernel-side, and a
//! refusal comes back as the frozen `Errno` the editor spells out on the
//! status line); the tests implement it over an in-memory map.

use alloc::vec::Vec;

use tairix_abi::Errno;

/// Named-file access for the editor.
pub trait FileIo {
    /// Read the whole file at `path`. `Ok(None)` means the file does not
    /// exist — vim's "new file", an empty buffer that will create it on the
    /// first write. Any other refusal is the kernel's answer, surfaced as a
    /// status-line message.
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, Errno>;

    /// Create-or-truncate the file at `path` with `bytes`. All-or-nothing
    /// from the editor's view: an error means the buffer stays dirty.
    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Errno>;
}
