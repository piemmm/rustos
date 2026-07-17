//! The two seams through which a System Information API client touches the
//! outside world.
//!
//! Keeping the request transport and the terminal behind object-safe traits
//! is what lets the request/render logic that consumes this crate run against
//! in-memory fixtures with no kernel, mirroring the seam design of the other
//! userland crates (`init`'s `Spawner`/`Reaper`, `login`'s `LoginView`).

use alloc::vec::Vec;
use tairix_abi::Errno;

/// Carries an encoded `sysinfo-v1` request to `sysinfod` and returns the
/// reply.
///
/// `request` is a [`SysinfoRequestHeader`](tairix_abi::sysinfo::SysinfoRequestHeader)
/// followed by the query's typed payload, already encoded little-endian
/// (build it with [`encode_request`](crate::encode_request)). The returned
/// bytes are the service's reply exactly as `sysinfod` produced them: packed
/// [`ProcessRecord`](tairix_abi::sysinfo::ProcessRecord)s, a scalar struct's
/// wire image, or the hardware-tree bytes. The transport
/// owns the reply allocation so the caller never has to guess a response
/// buffer size.
///
/// # Errors
///
/// Any [`Errno`] the service or the IPC path raises, propagated verbatim —
/// in particular [`Errno::PermissionDenied`] when the caller lacks the
/// query's required capability.
pub trait Transport {
    /// Issue `request` and return the service's reply bytes.
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno>;
}

/// Writes one rendered line to the terminal.
///
/// A client renders each row of a response as a separate line; the seam owns
/// the trailing newline so a fixture can capture lines cleanly.
///
/// # Errors
///
/// Any [`Errno`] the console raises (e.g. a closed terminal).
pub trait Output {
    /// Emit `line` followed by a newline.
    fn write_line(&self, line: &str) -> Result<(), Errno>;

    /// Emit one framed `stdinfo` record on the advisory stream (fd 3),
    /// best-effort: advisory by contract, so a missing consumer or a short
    /// write is silently a no-op and never affects the rendered listing or
    /// the exit status. The default drops the record — the contract's
    /// "ignorable" form for a sink with no advisory channel.
    fn info(&self, record: &[u8]) {
        let _ = record;
    }
}
