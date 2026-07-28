//! The `Run` entry-point binary of the `host` tool — the program a shell
//! spawns to look a name up over DNS.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the standard-stream I/O; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` parses the argument vector, reads the `LANG` locale preference from
//! the inherited environment (the shell exports it; the tool invents no
//! second source), and runs the parsed command against the production seams:
//! one `RtDnsTransport`-backed `Resolver` reused across the A and AAAA
//! lookups (so the delivery port is bound once), the shared
//! `tairix_help::BundleHelp` for the short-help switches, and
//! `RtOutput`/`RtErrors`, which write the answers to the inherited standard
//! output and the diagnostics to standard error. The tool binds only to its
//! inherited descriptors, never a console device, and holds no ambient
//! authority (the UDP socket is capability-gated stack-side).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::format;

    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_host::{parse, run, Command, Output, Resolver, USAGE};
    use tairix_net::dns::{RecordType, Resolution};
    use tairix_resolver::{ResolveError, RtDnsTransport};
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    /// The production standard-output stream: the answers go to fd 1. The
    /// tool names only descriptors its spawner chose, so the same binary
    /// drives a serial terminal, a framebuffer console, or a future windowed
    /// terminal unchanged.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production standard-error stream: diagnostics go to fd 2, keeping
    /// the answers on fd 1 clean for pipes.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production [`Resolver`]: one socket-backed transport, reused across
    /// the lookup's record types so the delivery port is bound only once.
    struct RtResolver {
        udp: RtDnsTransport,
    }

    impl Resolver for RtResolver {
        fn resolve(
            &mut self,
            name: &str,
            record_type: RecordType,
        ) -> Result<Resolution, ResolveError> {
            self.udp.resolve(name, record_type)
        }
    }

    /// A stub [`Resolver`] for the help path, which resolves nothing (help
    /// returns before any lookup). Its method is never called, so it denies
    /// rather than acts.
    struct NoResolver;

    impl Resolver for NoResolver {
        fn resolve(
            &mut self,
            _name: &str,
            _record_type: RecordType,
        ) -> Result<Resolution, ResolveError> {
            Err(ResolveError::NoServers)
        }
    }

    /// Program entry point. Exit codes: `0` when an address was found (or the
    /// short help was written), `1` when the name resolved to no address (a
    /// negative answer, a timeout, or a resolver failure), and `2` on a usage
    /// error or an output failure.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("host: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let help = BundleHelp::new("host");
        let result = match &command {
            Command::Lookup(_) => match RtDnsTransport::open() {
                Ok(udp) => {
                    let mut resolver = RtResolver { udp };
                    run(command, locale, &mut resolver, &help, &RtOutput, &RtErrors)
                }
                Err(err) => {
                    write_stderr_line(&format!("host: cannot open a DNS socket: {err}"));
                    return 2;
                }
            },
            Command::Help => run(
                command,
                locale,
                &mut NoResolver,
                &help,
                &RtOutput,
                &RtErrors,
            ),
        };
        match result {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("host: cannot write output: {err}"));
                2
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
