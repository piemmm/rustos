//! The seams through which `log` touches the outside world.
//!
//! Keeping the segment store and the terminal behind object-safe traits is
//! what lets the render/verify engine in [`crate::client`] run against
//! in-memory fixtures with no kernel, mirroring the seam design of the other
//! userland crates (`cat`'s `FileSource`, `ls`'s `Listing`, `sysinfo`'s
//! `Transport`). The binary that ships as `log` wires the real syscall-backed
//! `/System/Logs` reader and console; tests wire in-memory fixtures.

use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_log::Stream;

/// Reads the persisted segments of a stream, one at a time.
///
/// A stream's segments live as immutable append-only files under
/// `/System/Logs/<stream>/`, named by segment id so a lexical listing is
/// chronological (SYSLOG §6). The engine reads a stream by calling
/// [`read`](SegmentSource::read) with `index` `0, 1, 2, …` — **oldest
/// first** — until it returns [`None`], which marks the end of the stream.
/// Only one segment is held in memory at a time, so a stream of any length
/// streams through bounded memory.
pub trait SegmentSource {
    /// Read the whole `index`-th segment image of `stream` (oldest first), or
    /// [`None`] once `index` is past the last segment (including a stream with
    /// no segments at all).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the store raises while reading — e.g.
    /// [`Errno::PermissionDenied`] when the caller may not read the stream.
    /// A *missing* stream directory is not an error: it reads as an empty
    /// stream ([`None`] at `index` 0).
    fn read(&self, stream: Stream, index: usize) -> Result<Option<Vec<u8>>, Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// The engine hands [`write_all`](Output::write_all) one fully-rendered line
/// (or table row / JSON object / Markdown fragment) at a time, including its
/// trailing newline, so a fixture can capture the exact byte stream.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
