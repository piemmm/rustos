//! The production client seams that back the `sysinfo`, `ps`, and `top` `Run`
//! binaries: the real IPC transport to the `sysinfod` service and the
//! standard-output line sink. (The generic argument-vector and stderr-line
//! helpers live in `tairix_rt` — the runtime owns them, not this client.)
//!
//! These are the concrete implementations of the [`Transport`](crate::Transport)
//! and [`Output`](crate::Output) seams the tools' request/render logic runs
//! against. Keeping them here, rather than in each tool's `Run` binary, is what
//! stops three identical copies of the IPC call, the reply-frame unwrap, and
//! the standard-output write loop from being pasted into sibling crates.
//!
//! The module compiles only for a freestanding userland program (the
//! `freestanding` cfg) that opts into the `program` feature, which pulls the
//! `tairix-rt` runtime. Host builds and the pure library never compile it, so
//! the shared request/render logic stays testable against in-memory fixtures
//! with no kernel.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::fs::OpenFlags;
use tairix_abi::sysinfo::{decode_reply, SYSINFO_ENDPOINT, SYSINFO_MAX_REPLY};
use tairix_abi::time::Time64;
use tairix_abi::Errno;
use tairix_resref::{KnownNamespace, NamespaceBacking, ResourceRef};
use tairix_rt::io::{StdInfo, Stdout, Write};

use crate::resolve::ResolveInfoError;
use crate::valueread::read_value;
use crate::{Output, Transport};

/// The production [`Transport`]: carry a framed `sysinfo-v1` request to the
/// `sysinfod` service over the synchronous
/// [`SYSINFO_ENDPOINT`](tairix_abi::sysinfo::SYSINFO_ENDPOINT) IPC call and
/// return the served payload.
///
/// The transport adds no authority and enforces no policy: the kernel checks
/// the endpoint's (empty) send capability, and `sysinfod` gates every query
/// against the caller's kernel-attested origin. A per-query refusal (for
/// example a missing `CAP_SYSINFO_GLOBAL`) travels back as the framed status
/// word and surfaces here as that exact [`Errno`], not as a transport error.
pub struct IpcTransport;

impl Transport for IpcTransport {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        // A reply buffer sized to the endpoint's contract, so a served answer
        // always fits; the service pages a longer list across requests.
        let mut reply = alloc::vec![0u8; SYSINFO_MAX_REPLY];
        let n = tairix_rt::ipc_call(SYSINFO_ENDPOINT, request, &mut reply)
            .map_err(Errno::from_syscall)?;
        // Unwrap the status-word frame: a served payload, or the service's
        // per-query Errno. A truncated or corrupt frame fails closed.
        let payload = decode_reply(&reply[..n])?;
        Ok(payload.to_vec())
    }
}

/// The production [`Output`]: write each rendered line to the inherited
/// standard output (fd 1) through `tairix-rt`, followed by a newline.
///
/// A tool binds to its inherited descriptor, never a console device, so the
/// same binary works whatever the spawner backed fd 1 with. Output is
/// best-effort: a stream that will accept no more bytes ends the write rather
/// than spinning.
pub struct RtOutput;

impl Output for RtOutput {
    fn write_line(&self, line: &str) -> Result<(), Errno> {
        // The shared `tairix_rt::io` short-write loop — no procinfo-private
        // copy (the charter forbids that duplication). Output is best-effort
        // (a stream that accepts no more ends the write rather than spinning),
        // so the fail-closed result is discarded.
        let mut out = Stdout;
        let _ = out.write_all(line.as_bytes());
        let _ = out.write_all(b"\n");
        Ok(())
    }

    fn info(&self, record: &[u8]) {
        // fd 3 is ignorable by contract: unattached is a no-op and a short
        // write is never an error a listing depends on.
        let _ = StdInfo.write_all(record);
    }
}

/// Why [`NamedSource::open`] refused, keeping the two refusals distinct.
///
/// A value refusal carries the resolver's typed reason, which names the
/// capability a denial wanted — detail an `Errno` cannot hold and a caller
/// needs to tell the user which grant to ask for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OpenError {
    /// A path or stream reference the kernel refused.
    Stream(Errno),
    /// A value-backed reference the broker or the resolver refused.
    Value(ResolveInfoError),
}

impl OpenError {
    /// The stable [`Errno`] to report or return.
    #[must_use]
    pub fn errno(self) -> Errno {
        match self {
            Self::Stream(errno) => errno,
            Self::Value(err) => err.to_errno(),
        }
    }
}

/// A readable source named on a command line: a stream the kernel opens, or a
/// value-backed reference read through the broker.
///
/// The one open-by-name path for a tool that accepts either. `tairix_rt::File`
/// routes a path and a stream reference; the kernel cannot serve a typed
/// broker value, so this adds that third case where every reader shares it
/// rather than each carrying the routing. It confers no authority: `sysinfod`
/// gates the query on the reading process's own attested set.
pub enum NamedSource {
    /// A path or stream reference, as the kernel descriptor it opened.
    Stream(tairix_rt::File),
    /// A value-backed reference, as the bytes its value rendered. Read once at
    /// open, so the source is one snapshot rather than a value that could
    /// change mid-read.
    Value(String),
}

impl NamedSource {
    /// Open `target` for reading.
    ///
    /// # Errors
    ///
    /// An [`OpenError`] carrying the refusal: the kernel's `Errno` for a path
    /// or stream reference, the resolver's typed reason for a value.
    pub fn open(target: &[u8]) -> Result<Self, OpenError> {
        match value_reference(target) {
            Some(reference) => {
                let now =
                    tairix_rt::wall_time().map_or(Time64::UNIX_EPOCH, |reading| reading.time());
                read_value(&reference, now, &IpcTransport)
                    .map(Self::Value)
                    .map_err(OpenError::Value)
            }
            None => tairix_rt::File::open(target, OpenFlags::READ)
                .map(Self::Stream)
                .map_err(|ret| OpenError::Stream(Errno::from_syscall(ret))),
        }
    }

    /// Read up to `buf.len()` bytes from `offset`, returning the count written
    /// (`0` at end of source).
    ///
    /// A sequential stream ignores the offset; a value is a fixed byte range,
    /// so the offset indexes it.
    ///
    /// # Errors
    ///
    /// The [`Errno`] a descriptor read failed with. A value read cannot fail.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        match self {
            Self::Stream(file) => file.read_at(offset, buf).map_err(Errno::from_syscall),
            Self::Value(value) => {
                let bytes = value.as_bytes();
                let start = usize::try_from(offset)
                    .unwrap_or(bytes.len())
                    .min(bytes.len());
                let tail = &bytes[start..];
                let n = tail.len().min(buf.len());
                buf[..n].copy_from_slice(&tail[..n]);
                Ok(n)
            }
        }
    }
}

/// The parsed reference `target` names, if it names a value-backed one.
///
/// Both decisions come from the shared registry, never a list here: the
/// spelling rule that separates a reference from a path, and the backing that
/// decides which resolver serves it.
///
/// A malformed reference yields [`None`] and falls through to
/// [`tairix_rt::File::open`], which re-applies the same spelling rule and
/// routes it to the kernel resolver's refusal — so a typo in a registered
/// namespace is still an error and never a filename fallback. Re-testing the
/// spelling there is the cost of not copying that routing here.
fn value_reference(target: &[u8]) -> Option<ResourceRef> {
    let text = core::str::from_utf8(target).ok()?;
    if !tairix_resref::names_resource_reference(text) {
        return None;
    }
    let reference = tairix_resref::parse(text).ok()?;
    (reference.namespace().known().map(KnownNamespace::backing) == Some(NamespaceBacking::Value))
        .then_some(reference)
}
