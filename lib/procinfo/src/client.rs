//! The production client seams that back the `sysinfo`, `ps`, and `top` `Run`
//! binaries: the real IPC transport to the `sysinfod` service and the
//! standard-output line sink. (The generic argument-vector and stderr-line
//! helpers live in `rustos_rt` — the runtime owns them, not this client.)
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
use rustos_rt::io::{StdInfo, Stdout, Write};

use crate::{Output, Transport};

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
        let n = rustos_rt::ipc_call(SYSINFO_ENDPOINT, request, &mut reply)
            .map_err(Errno::from_syscall)?;
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
        // The shared `rustos_rt::io` short-write loop — no procinfo-private
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
