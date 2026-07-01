//! The production client seams that back the `sysinfo`, `ps`, and `top` `Run`
//! binaries: the real IPC transport to the `sysinfod` service and the
//! standard-output line sink.
//!
//! These are the concrete implementations of the [`Transport`](crate::Transport)
//! and [`Output`](crate::Output) seams the tools' request/render logic runs
//! against. Keeping them here, rather than in each tool's `Run` binary, is what
//! stops three identical copies of the IPC call, the reply-frame unwrap, and
//! the standard-output write loop from being pasted into sibling crates.
//!
//! The module compiles only for a freestanding userland program (the
//! `freestanding` cfg) that opts into the `program` feature, which pulls the
//! `rustos-rt` runtime. Host builds and the pure library never compile it, so
//! the shared request/render logic stays testable against in-memory fixtures
//! with no kernel.

use alloc::vec::Vec;

use rustos_abi::sysinfo::{decode_reply, SYSINFO_ENDPOINT, SYSINFO_MAX_REPLY};
use rustos_abi::Errno;

use crate::{Output, Transport};

/// Recover the [`Errno`] a syscall encoded as a negative register (`-errno`,
/// the standard `abi-v1` convention). An unrecognised code fails closed as
/// [`Errno::NotImplemented`] rather than being guessed.
fn errno_from(ret: i64) -> Errno {
    i32::try_from(-ret)
        .ok()
        .and_then(Errno::from_i32)
        .unwrap_or(Errno::NotImplemented)
}

/// The production [`Transport`]: carry a framed `sysinfo-v1` request to the
/// `sysinfod` service over the synchronous
/// [`SYSINFO_ENDPOINT`](rustos_abi::sysinfo::SYSINFO_ENDPOINT) IPC call and
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
        let n = rustos_rt::ipc_call(SYSINFO_ENDPOINT, request, &mut reply).map_err(errno_from)?;
        // Unwrap the status-word frame: a served payload, or the service's
        // per-query Errno. A truncated or corrupt frame fails closed.
        let payload = decode_reply(&reply[..n])?;
        Ok(payload.to_vec())
    }
}

/// The production [`Output`]: write each rendered line to the inherited
/// standard output (fd 1) through `rustos-rt`, followed by a newline.
///
/// A tool binds to its inherited descriptor, never a console device, so the
/// same binary works whatever the spawner backed fd 1 with. Output is
/// best-effort: a stream that will accept no more bytes ends the write rather
/// than spinning.
pub struct RtOutput;

impl Output for RtOutput {
    fn write_line(&self, line: &str) -> Result<(), Errno> {
        write_all(line.as_bytes(), rustos_rt::stdout);
        write_all(b"\n", rustos_rt::stdout);
        Ok(())
    }
}

/// Collect the calling program's arguments — argv[1..], excluding the program
/// name — as UTF-8 string slices, ready for a command parser.
///
/// Returns `None` if any argument is not valid UTF-8: a malformed argument
/// vector is a usage error the caller reports, never something to guess at.
///
/// Shared by the `ps` and `sysinfo` `Run` binaries so the argument-vector
/// walk is written once, not pasted into each.
#[must_use]
pub fn args() -> Option<Vec<&'static str>> {
    let mut out = Vec::new();
    let count = rustos_rt::arg_count();
    for index in 1..count {
        let bytes = rustos_rt::arg(index)?;
        match core::str::from_utf8(bytes) {
            Ok(text) => out.push(text),
            Err(_) => return None,
        }
    }
    Some(out)
}

/// Write `line` and a trailing newline to standard error (fd 2), best-effort.
///
/// A tool routes a diagnostic (a usage banner, a failed-query message) here so
/// it never contaminates the standard-output data stream. Shared by the `ps`
/// and `sysinfo` `Run` binaries.
pub fn write_stderr_line(line: &str) {
    write_all(line.as_bytes(), rustos_rt::stderr);
    write_all(b"\n", rustos_rt::stderr);
}

/// Write all of `bytes` to the stream `sink` writes to, looping over short
/// writes.
///
/// A write that accepts zero bytes means the stream will accept no more (a
/// closed or full backing); the loop stops rather than spinning.
fn write_all(mut bytes: &[u8], sink: fn(&[u8]) -> usize) {
    while !bytes.is_empty() {
        let written = sink(bytes);
        if written == 0 {
            break;
        }
        bytes = &bytes[written.min(bytes.len())..];
    }
}
