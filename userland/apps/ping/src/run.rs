//! The `Run` entry-point binary of the `ping` tool — the program a shell
//! spawns to measure reachability and round-trip time to a host.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, the standard-stream I/O, and the ICMP-echo socket wrappers;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! `main` parses the argument vector, reads the `LANG` locale preference from
//! the inherited environment (the shell exports it; the tool invents no
//! second source), and runs the parsed command against the production seams:
//! the `RtPingIo` echo-socket seam over `tairix_rt::net`, the shared
//! `tairix_help::BundleHelp` for the short-help switches, and `RtOutput`,
//! which writes the per-reply lines and statistics to the inherited standard
//! output and diagnostics to standard error. The tool binds only to its
//! inherited descriptors, never a console device, and holds no ambient
//! authority (the ICMP echo socket is capability-gated stack-side).
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
    use alloc::vec;

    use tairix_abi::net::{SocketAddr, SocketEcho, SocketId};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, Origin};
    use tairix_help::BundleHelp;
    use tairix_ping::{parse, run, Command, Config, EchoReply, Output, PingError, PingIo, USAGE};
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    /// The client's async delivery-port endpoint id: an app-local,
    /// unrestricted well-known value (not a reserved kernel id), so binding
    /// it needs no capability. The stack sends this socket's echo replies
    /// here.
    const DELIVER_PORT: u64 = 0x_7069_6e67; // "ping"

    /// Delivery-port mailbox depth. Generous headroom so a burst of replies
    /// queues rather than back-pressuring the stack.
    const DELIVER_CAPACITY: usize = 16;

    /// Wait-set token for the delivery port (one source, so any non-zero
    /// token identifies it).
    const DELIVER_TOKEN: u64 = 1;

    /// One park slice while blocking for the next reply: the tool gives the
    /// CPU up and is woken when the stack posts a reply or this one-shot
    /// timer elapses — never a busy poll.
    const RECV_PARK_NANOS: u64 = 20_000_000;

    /// Attempts to retry a send that failed `NetworkUnreachable` (the link
    /// is still coming up at boot), and the park between them.
    const SEND_RETRIES: u32 = 200;
    /// One-shot park between send retries (a tickless timed wait).
    const SEND_RETRY_PARK_NANOS: u64 = 25_000_000;

    /// The production standard-output stream.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production standard-error stream.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production [`PingIo`]: the monotonic clock, the ICMP echo socket,
    /// and the wait-set park, over the `tairix-rt` syscall wrappers.
    struct RtPingIo {
        socket: SocketId,
        set: u64,
        /// The kernel-attested origin of the stack, captured from the first
        /// reply so every later reply can be required to match it — the
        /// delivery port is otherwise an unauthenticated inbox (fail closed).
        stack: Option<Origin>,
        /// The receive scratch buffer (reused across replies).
        buf: alloc::vec::Vec<u8>,
    }

    impl RtPingIo {
        /// Bind the delivery port, open the echo socket, and connect it to
        /// the target so the stack filters replies to that peer too.
        fn open(config: &Config) -> Result<Self, PingError> {
            if tairix_rt::port_bind(DELIVER_PORT, SocketEcho::MAX_WIRE_LEN, DELIVER_CAPACITY) < 0 {
                return Err(PingError::Socket(Errno::AddressInUse));
            }
            // A negative result is the kernel's `-errno`, never a handle.
            let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
                return Err(PingError::Socket(Errno::NotImplemented));
            };
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Port,
                DELIVER_PORT,
                DELIVER_TOKEN,
            ) != 0
            {
                return Err(PingError::Socket(Errno::NotImplemented));
            }
            let socket = tairix_rt::net::icmp_echo_socket(config.target.family, DELIVER_PORT)
                .map_err(map_open_error)?;
            // An echo peer carries no port. Connecting records the default
            // peer (so the stack filters replies to it) and assigns the
            // socket's ICMP identifier; it performs no routing, so it never
            // fails on a link that is still coming up.
            let peer = SocketAddr {
                family: config.target.family,
                addr: config.target.addr,
                port: 0,
            };
            tairix_rt::net::connect(socket, peer).map_err(PingError::Socket)?;
            Ok(Self {
                socket,
                set,
                stack: None,
                buf: vec![0u8; SocketEcho::MAX_WIRE_LEN],
            })
        }

        fn park(&self, nanos: u64) {
            let mut token = 0u64;
            let _ = tairix_rt::waitset_wait(self.set, nanos, &mut token);
        }
    }

    impl PingIo for RtPingIo {
        fn now(&self) -> u64 {
            tairix_rt::clock_get()
        }

        fn send(&mut self, seq: u16, payload: &[u8]) -> Result<(), Errno> {
            let mut last = Errno::NetworkUnreachable;
            for _ in 0..SEND_RETRIES {
                match tairix_rt::net::send_echo(self.socket, None, seq, payload) {
                    Ok(()) => return Ok(()),
                    // The interface may not be bound yet at boot; park and
                    // retry rather than record a spurious loss.
                    Err(Errno::NetworkUnreachable) => {
                        last = Errno::NetworkUnreachable;
                        self.park(SEND_RETRY_PARK_NANOS);
                    }
                    Err(other) => return Err(other),
                }
            }
            Err(last)
        }

        fn recv(&mut self, deadline_ns: u64) -> Result<Option<EchoReply>, Errno> {
            loop {
                if tairix_rt::clock_get() >= deadline_ns {
                    return Ok(None);
                }
                match tairix_rt::net::recv_echo(DELIVER_PORT, &mut self.buf) {
                    Ok((echo, origin)) => {
                        // Authenticate the sender: capture the stack's origin
                        // on the first reply, then require every later reply
                        // to match it (a forged reply from any other origin
                        // is silently ignored — fail closed).
                        match self.stack {
                            Some(known) if known != origin => continue,
                            None => self.stack = Some(origin),
                            _ => {}
                        }
                        return Ok(Some(owned_reply(&echo)));
                    }
                    // The mailbox is momentarily empty: park until the stack
                    // posts a reply or the one-shot timer elapses.
                    Err(Errno::WouldBlock) => self.park(RECV_PARK_NANOS),
                    Err(other) => return Err(other),
                }
            }
        }

        fn sleep_until(&mut self, deadline_ns: u64) {
            while tairix_rt::clock_get() < deadline_ns {
                let remaining = deadline_ns - tairix_rt::clock_get();
                self.park(remaining);
            }
        }
    }

    /// Copy a borrowed [`SocketEcho`] into an owned [`EchoReply`].
    fn owned_reply(echo: &SocketEcho<'_>) -> EchoReply {
        EchoReply {
            seq: echo.sequence,
            family: echo.source.family,
            addr: echo.source.addr,
            payload: echo.payload.to_vec(),
        }
    }

    /// Map an echo-socket open refusal onto the tool's error type.
    fn map_open_error(err: Errno) -> PingError {
        match err {
            Errno::PermissionDenied => PingError::Denied,
            other => PingError::Socket(other),
        }
    }

    /// A stub [`PingIo`] for the help path, which touches no network. Its
    /// methods are never called (help returns before the ping loop), so they
    /// deny rather than act.
    struct NoNet;

    impl PingIo for NoNet {
        fn now(&self) -> u64 {
            0
        }
        fn send(&mut self, _seq: u16, _payload: &[u8]) -> Result<(), Errno> {
            Err(Errno::NotImplemented)
        }
        fn recv(&mut self, _deadline_ns: u64) -> Result<Option<EchoReply>, Errno> {
            Ok(None)
        }
        fn sleep_until(&mut self, _deadline_ns: u64) {}
    }

    /// Program entry point. Exit codes: `0` on success (at least one reply,
    /// or help), `1` when every request went unanswered, and `2` on a usage
    /// error or a setup/output failure.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("ping: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let help = BundleHelp::new("ping");
        let is_help = matches!(command, Command::Help);
        let result = match &command {
            Command::Run(config) => match RtPingIo::open(config) {
                Ok(mut io) => run(command, locale, &mut io, &help, &RtOutput, &RtErrors),
                Err(err) => Err(err),
            },
            Command::Help => run(command, locale, &mut NoNet, &help, &RtOutput, &RtErrors),
        };
        match result {
            Ok(summary) => i32::from(!is_help && !summary.any_received()),
            Err(err) => {
                write_stderr_line(&format!("ping: {err}"));
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
