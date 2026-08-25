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
//! the `RtPingIo` seam — name resolution through the shared `lib/resolver`
//! stub resolver, the echo socket over `tairix_rt::net`, and payload entropy
//! from `lib/rng`'s fast generator seeded by the kernel CSPRNG — the shared
//! `tairix_help::BundleHelp` for the short-help switches, and `RtOutput`,
//! which writes the per-reply lines and statistics to the inherited standard
//! output and diagnostics to standard error. The tool binds only to its
//! inherited descriptors, never a console device, and holds no ambient
//! authority (the ICMP echo socket is capability-gated stack-side).
//!
//! The payload generator is deliberately the *fast* non-cryptographic
//! xoshiro256++, seeded once from the kernel CSPRNG: bulk uncompressible
//! bytes are not a security surface, so drawing every payload from the
//! CSPRNG would spend the reserve for nothing, and rolling a private
//! generator is forbidden — `lib/rng` owns both.
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
    use tairix_abi::net_ipc::NetAddrFamily;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, Origin, RandomFlags};
    use tairix_help::BundleHelp;
    use tairix_ping::{
        parse, run, Command, EchoReply, Output, PingError, PingIo, ResolveFailure, USAGE,
    };
    use tairix_rng::{FastRng, RandU64};
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
        /// The echo socket, opened by `connect` once the target resolved.
        socket: Option<SocketId>,
        set: u64,
        /// The kernel-attested origin of the stack, captured from the first
        /// reply so every later reply can be required to match it — the
        /// delivery port is otherwise an unauthenticated inbox (fail closed).
        stack: Option<Origin>,
        /// The receive scratch buffer (reused across replies).
        buf: alloc::vec::Vec<u8>,
        /// The payload generator, seeded once from the kernel CSPRNG.
        rng: FastRng,
    }

    impl RtPingIo {
        /// Bind the delivery port, arm the wait-set, and seed the payload
        /// generator. The socket itself is opened by
        /// [`PingIo::connect`](tairix_ping::PingIo::connect), once the target
        /// has resolved and its family is known.
        fn open() -> Result<Self, PingError> {
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
            Ok(Self {
                socket: None,
                set,
                stack: None,
                buf: vec![0u8; SocketEcho::MAX_WIRE_LEN],
                rng: seed_generator()?,
            })
        }

        fn park(&self, nanos: u64) {
            let mut token = 0u64;
            let _ = tairix_rt::waitset_wait(self.set, nanos, &mut token);
        }
    }

    /// Seed the payload generator from the kernel CSPRNG.
    ///
    /// The blocking draw (no [`RandomFlags::NON_BLOCKING`]) waits only for the
    /// kernel RNG to initialise and never blocks thereafter. A source that
    /// cannot supply the seed is reported, never worked around with a
    /// predictable stream — a compressible payload would silently invalidate
    /// the measurement the tool exists to make.
    fn seed_generator() -> Result<FastRng, PingError> {
        let mut seed = [0u8; 32];
        let drawn = tairix_rt::random_get(&mut seed, RandomFlags::empty())
            .map_err(|raw| PingError::Socket(Errno::from_syscall(raw)))?;
        if drawn != seed.len() {
            return Err(PingError::Socket(Errno::EntropyNotReady));
        }
        Ok(FastRng::from_seed_bytes(&seed))
    }

    impl PingIo for RtPingIo {
        fn resolve(
            &mut self,
            host: &str,
            family: Option<NetAddrFamily>,
        ) -> Result<(NetAddrFamily, [u8; 16]), ResolveFailure> {
            // The literal-first, family-preference policy is the shared one,
            // so `ping` and `telnet` cannot disagree about a host operand. A
            // literal needs no query, which is what makes `ping <address>`
            // work with no resolver configured.
            if let Some(address) = tairix_resolver::host_address(host, family) {
                return Ok(tairix_resolver::address_parts(address));
            }
            // An operand that *is* a literal, merely of the excluded family,
            // gets its own diagnosis: the fix is to drop the `-4`/`-6`.
            if family.is_some() && tairix_resolver::literal_address(host, None).is_some() {
                return Err(ResolveFailure::FamilyMismatch);
            }
            Err(ResolveFailure::Unknown)
        }

        fn connect(&mut self, family: NetAddrFamily, addr: [u8; 16]) -> Result<(), Errno> {
            let socket = tairix_rt::net::icmp_echo_socket(family, DELIVER_PORT)?;
            // An echo peer carries no port. Connecting records the default
            // peer (so the stack filters replies to it) and assigns the
            // socket's ICMP identifier; it performs no routing, so it never
            // fails on a link that is still coming up.
            let peer = SocketAddr {
                family,
                addr,
                port: 0,
            };
            if let Err(errno) = tairix_rt::net::connect(socket, peer) {
                let _ = tairix_rt::net::close(socket);
                return Err(errno);
            }
            self.socket = Some(socket);
            Ok(())
        }

        fn fill_payload(&mut self, out: &mut [u8]) {
            self.rng.fill_bytes(out);
        }

        fn now(&self) -> u64 {
            tairix_rt::clock_get()
        }

        fn send(&mut self, seq: u16, payload: &[u8]) -> Result<(), Errno> {
            let socket = self.socket.ok_or(Errno::NotConnected)?;
            let mut last = Errno::NetworkUnreachable;
            for _ in 0..SEND_RETRIES {
                match tairix_rt::net::send_echo(socket, None, seq, payload) {
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

    /// A stub [`PingIo`] for the help path, which touches no network. Its
    /// methods are never called (help returns before the ping loop), so they
    /// deny rather than act.
    struct NoNet;

    impl PingIo for NoNet {
        fn resolve(
            &mut self,
            _host: &str,
            _family: Option<NetAddrFamily>,
        ) -> Result<(NetAddrFamily, [u8; 16]), ResolveFailure> {
            Err(ResolveFailure::Unknown)
        }
        fn connect(&mut self, _family: NetAddrFamily, _addr: [u8; 16]) -> Result<(), Errno> {
            Err(Errno::NotImplemented)
        }
        fn fill_payload(&mut self, _out: &mut [u8]) {}
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
        let result = if is_help {
            run(command, locale, &mut NoNet, &help, &RtOutput, &RtErrors)
        } else {
            match RtPingIo::open() {
                Ok(mut io) => run(command, locale, &mut io, &help, &RtOutput, &RtErrors),
                Err(err) => Err(err),
            }
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
