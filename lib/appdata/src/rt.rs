//! The production, `tairix-rt`-backed [`AppDataHost`] (feature `rt`).
//!
//! The client engine itself is I/O-free so it stays host-testable, so this is
//! where its three syscalls actually land: the `ipc_call` to the app-data
//! endpoint, the bounded whole-file read of a bundle's shipped defaults, and
//! the session-dependent half of command-word resolution (`HOME` and `PATH`,
//! which only the running process can see).
//!
//! Feature-gated so a host test injects its own host instead of linking the
//! userland runtime.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::APPDATA_ENDPOINT;
use tairix_abi::fs::OpenFlags;
use tairix_abi::Errno;
use tairix_cmdres::{bundle_candidates, CommandEnv};

use crate::AppDataHost;

/// Bytes read per `fs_read` while loading a bundle's shipped defaults.
///
/// One comfortable transfer per call over a document bounded at 64 KiB, so a
/// realistic defaults file is read in a single syscall.
const READ_CHUNK: usize = 4096;

/// The app-data client's syscall host: `ipc_call`, `fs_*`, and the session's
/// own `HOME`/`PATH`.
pub struct RtHost;

impl AppDataHost for RtHost {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        tairix_rt::ipc_call(APPDATA_ENDPOINT, request, reply).map_err(Errno::from_syscall)
    }

    fn read_file(&mut self, path: &str, cap: usize) -> Result<Vec<u8>, Errno> {
        let file =
            tairix_rt::File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            // Read one chunk past the ceiling so a document *at* it is still
            // read whole, and refuse anything beyond rather than truncating a
            // store into one that means something else.
            let read = file
                .read_at(bytes.len() as u64, &mut chunk)
                .map_err(Errno::from_syscall)?;
            if read == 0 {
                return Ok(bytes);
            }
            let slice = chunk.get(..read).ok_or(Errno::OutOfRange)?;
            bytes
                .try_reserve(slice.len())
                .map_err(|_| Errno::OutOfMemory)?;
            bytes.extend_from_slice(slice);
            if bytes.len() > cap {
                return Err(Errno::LengthOutOfRange);
            }
        }
    }

    fn bundle_candidates(&mut self, word: &str) -> Vec<String> {
        let home = tairix_rt::env_var(b"HOME").and_then(|value| core::str::from_utf8(value).ok());
        let path_var =
            tairix_rt::env_var(b"PATH").and_then(|value| core::str::from_utf8(value).ok());
        bundle_candidates(word, CommandEnv { home, path_var })
    }
}
