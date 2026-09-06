//! The `Run` entry-point binary of the `telnet` tool — the program a shell
//! spawns to reach a remote host.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, the standard-stream I/O, and the threads the relay needs;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! # Why two threads, and why neither of them polls
//!
//! Telnet must carry the keyboard and the connection at the same time. The
//! stack delivers a socket's stream events to an async **port**, which joins a
//! wait-set; a *console-backed* standard input cannot join one, because the
//! wait-set's stream source admits only a pipe or pty backing and a
//! console-backed standard stream is not in the process's open-file table at
//! all. So the two sides cannot be multiplexed by one wait.
//!
//! Both sides do have a genuine wake source, though, and a blocking read is
//! one of them — it just needs its own flow of control. So a second thread does
//! nothing but block in `Stdin::read` and forward what it read to a second
//! port; the main thread parks on a wait-set holding *both* ports and sees one
//! ordered event stream. Neither thread ever spins, and no timer is armed.
//!
//! The keyboard port carries a one-byte tag ahead of the bytes, so the reader
//! can report end-of-input without an empty message. It is process-local
//! plumbing between two threads of one program, not an interface: the port's
//! sender origin is checked against this process, exactly as the socket port's
//! is checked against the network stack, so neither inbox is trusted.
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
    use alloc::vec::Vec;

    use tairix_abi::net::{
        ShutdownHow, SocketAddr, SocketId, SocketStreamEvent, StreamCloseReason,
    };
    use tairix_abi::net_ipc::NetAddrFamily;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, InputMode, Origin, Signal, TerminalSize};
    use tairix_help::BundleHelp;
    use tairix_net::IpAddr;
    use tairix_rt::io::{write_stderr_line, Read, Stderr, Stdin, Stdout, Write};
    use tairix_telnet::net::{CloseReason, Endpoint, IoEvent, TelnetIo};
    use tairix_telnet::{parse, run, Command, Output, FALLBACK_TERM, USAGE};

    /// The stack's stream-event delivery port: an app-local, unrestricted
    /// well-known endpoint id (not a reserved kernel id), so binding it needs
    /// no capability.
    const DELIVER_PORT: u64 = 0x_746E_7400;

    /// The port the keyboard-reader thread posts to. A sibling of
    /// [`DELIVER_PORT`] so the wait-set holds both.
    const KEYBOARD_PORT: u64 = 0x_746E_7401;

    /// Wait-set token for the stack's delivery port.
    const DELIVER_TOKEN: u64 = 1;
    /// Wait-set token for the keyboard port.
    const KEYBOARD_TOKEN: u64 = 2;

    /// Mailbox depth for each port. Generous headroom so a burst of server
    /// output or fast typing queues rather than back-pressuring its producer;
    /// the connection's own receive window bounds the true in-flight volume.
    const PORT_CAPACITY: usize = 64;

    /// Largest keyboard read, and so the keyboard port's message size (the tag
    /// byte plus the bytes read). A terminal delivers keystrokes a few at a
    /// time; this is ample and bounds the reader's stack buffer.
    const KEYBOARD_CHUNK: usize = 512;

    /// The keyboard message tag: bytes follow.
    const KEY_TAG_DATA: u8 = 1;
    /// The keyboard message tag: standard input reached end of file.
    const KEY_TAG_EOF: u8 = 2;

    /// How many times to retry `connect` while the interface is still coming up
    /// at boot (the NIC driver may not be bound to the stack yet, so the stack
    /// has no egress and answers `NetworkUnreachable`).
    const CONNECT_ATTEMPTS: u32 = 200;

    /// One-shot park between connect retries and between send back-pressure
    /// retries — a tickless timed wait, never a busy spin.
    const RETRY_PARK_NANOS: u64 = 25_000_000;

    /// Bytes offered per `stream_send` call, so one marshalled request stays
    /// modest however much the operator pasted.
    const SEND_CHUNK: usize = 4096;

    /// How many times the reader will wait for mailbox room before giving up on
    /// a port that is evidently never going to drain. A bound, so a wedged
    /// relay ends the reader instead of leaving it parked forever.
    const POST_ATTEMPTS: u32 = 64;

    /// One park slice while awaiting the handshake's `Connected` event.
    const HANDSHAKE_PARK_NANOS: u64 = 100_000_000;

    /// How many such slices before the handshake is declared dead. Generous for
    /// a three-way handshake on an emulated link, bounded so a wedged stack
    /// fails loud with a reason instead of hanging the terminal.
    const HANDSHAKE_PARKS: u32 = 600;

    /// The conventional fallback grid for a console whose true size the kernel
    /// cannot attest — the terminal-library policy, applied here rather than
    /// fabricated by the kernel.
    const FALLBACK_ROWS: u16 = 24;
    /// Columns of the fallback grid.
    const FALLBACK_COLS: u16 = 80;

    /// The production standard-output stream: the remote host's output and the
    /// command interpreter's replies go to fd 1. The tool names only
    /// descriptors its spawner chose, so the same binary drives a serial
    /// terminal, a framebuffer console, or a windowed terminal unchanged.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production standard-error stream: diagnostics and negotiation traces
    /// go to fd 2, keeping the session's own output on fd 1 clean for pipes.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production [`TelnetIo`]: the stream socket, the two ports, the
    /// wait-set both are registered on, and the terminal controls.
    struct RtTelnetIo {
        /// The wait-set the relay parks on.
        set: u64,
        /// The connected socket, when there is one.
        socket: Option<SocketId>,
        /// The kernel-attested origin of the network stack, captured from the
        /// first event it sent, so a later forged event is refused.
        stack: Option<Origin>,
        /// This process's own origin, for authenticating the keyboard port.
        own: Origin,
        /// A local address to bind before connecting (`-b`).
        bind: Option<SocketAddr>,
        /// Decode buffer for one delivery message.
        buf: Vec<u8>,
    }

    impl RtTelnetIo {
        /// Bind both ports, create the wait-set, and register both on it.
        fn open(bind: Option<SocketAddr>) -> Result<Self, Errno> {
            let own = tairix_rt::self_origin().map_err(Errno::from_syscall)?;
            if tairix_rt::port_bind(DELIVER_PORT, SocketStreamEvent::MAX_WIRE_LEN, PORT_CAPACITY)
                < 0
            {
                return Err(Errno::AddressInUse);
            }
            if tairix_rt::port_bind(KEYBOARD_PORT, KEYBOARD_CHUNK + 1, PORT_CAPACITY) < 0 {
                return Err(Errno::AddressInUse);
            }
            let set =
                u64::try_from(tairix_rt::waitset_create()).map_err(|_| Errno::NotImplemented)?;
            for (port, token) in [
                (DELIVER_PORT, DELIVER_TOKEN),
                (KEYBOARD_PORT, KEYBOARD_TOKEN),
            ] {
                if tairix_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::Port, port, token)
                    != 0
                {
                    return Err(Errno::NotImplemented);
                }
            }
            Ok(Self {
                set,
                socket: None,
                stack: None,
                own,
                bind,
                buf: alloc::vec![0u8; SocketStreamEvent::MAX_WIRE_LEN],
            })
        }

        /// Park until one of the two ports has a message, giving the CPU up.
        fn park(&self) {
            let mut token = 0u64;
            let _ = tairix_rt::waitset_wait(self.set, u64::MAX, &mut token);
        }

        /// Drain one keyboard message, or [`None`] if the mailbox was empty.
        fn drain_keyboard(&mut self) -> Option<Result<IoEvent, Errno>> {
            let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
            let Ok(len) = tairix_rt::ipc_recv(KEYBOARD_PORT, &mut self.buf, &mut sender) else {
                return None;
            };
            // The keyboard port is an inbox like any other: only this process's
            // own reader thread may fill it (fail closed).
            match Origin::from_bytes(&sender) {
                Ok(origin) if origin.pid() == self.own.pid() => {}
                _ => return None,
            }
            match self.buf.get(..len) {
                Some([KEY_TAG_EOF, ..]) => Some(Ok(IoEvent::KeyboardClosed)),
                Some([KEY_TAG_DATA, rest @ ..]) if !rest.is_empty() => {
                    Some(Ok(IoEvent::Keyboard(rest.to_vec())))
                }
                // A tag byte alone, or an unknown tag, carries nothing to act
                // on; dropping it is the fail-closed reading.
                _ => None,
            }
        }

        /// Drain one stream event, or [`None`] if the mailbox was empty or the
        /// message was not one this session may act on.
        fn drain_network(&mut self) -> Option<Result<IoEvent, Errno>> {
            let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
            let Ok(len) = tairix_rt::ipc_recv(DELIVER_PORT, &mut self.buf, &mut sender) else {
                return None;
            };
            let Ok(origin) = Origin::from_bytes(&sender) else {
                return None;
            };
            // The first event fixes the stack's attested identity; every later
            // one must match it, so nothing else can post a forged event.
            match self.stack {
                Some(known) if known != origin => return None,
                Some(_) => {}
                None => self.stack = Some(origin),
            }
            let bytes = self.buf.get(..len)?;
            let event = SocketStreamEvent::parse(bytes).ok()?;
            let socket = self.socket?;
            match event {
                SocketStreamEvent::Data { socket: s, payload } if s == socket => {
                    Some(Ok(IoEvent::Network(payload.to_vec())))
                }
                SocketStreamEvent::Closed { socket: s, reason } if s == socket => {
                    self.socket = None;
                    Some(Ok(IoEvent::Closed(match reason {
                        StreamCloseReason::PeerClosed => CloseReason::PeerClosed,
                        _ => CloseReason::Reset,
                    })))
                }
                // `Connected` is consumed by `connect`; an event for a socket
                // this session no longer holds, and an `Accepted` a client can
                // never own, are dropped.
                _ => None,
            }
        }

        /// Await the `Connected` event for `socket`, or the `Closed` that says
        /// the handshake failed.
        ///
        /// Bounded: a stack that answers neither is a wedged stack, and this
        /// must fail loud with a reason rather than hang the terminal.
        fn await_connected(&mut self, socket: SocketId) -> Result<(), Errno> {
            for _ in 0..HANDSHAKE_PARKS {
                let mut sender = [0u8; tairix_abi::ORIGIN_WIRE_LEN];
                let Ok(len) = tairix_rt::ipc_recv(DELIVER_PORT, &mut self.buf, &mut sender) else {
                    park_for(self.set, HANDSHAKE_PARK_NANOS);
                    continue;
                };
                let Some(bytes) = self.buf.get(..len) else {
                    continue;
                };
                let event = SocketStreamEvent::parse(bytes);
                let ours = matches!(
                    event,
                    Ok(SocketStreamEvent::Connected { socket: s } | SocketStreamEvent::Closed { socket: s, .. })
                        if s == socket
                );
                if !ours {
                    continue;
                }
                // The stack's attested identity is pinned from an event that
                // both parsed *and* named this socket, so a stray message
                // cannot install a foreign origin that later events would then
                // be measured against.
                if self.stack.is_none() {
                    if let Ok(origin) = Origin::from_bytes(&sender) {
                        self.stack = Some(origin);
                    }
                }
                return match event {
                    Ok(SocketStreamEvent::Connected { .. }) => Ok(()),
                    _ => Err(Errno::NotConnected),
                };
            }
            Err(Errno::TimedOut)
        }
    }

    impl TelnetIo for RtTelnetIo {
        fn resolve(
            &mut self,
            host: &str,
            port: u16,
            family: Option<NetAddrFamily>,
        ) -> Option<Endpoint> {
            // The literal-first, family-preference policy is shared, so
            // `telnet` and `ping` cannot disagree about what a host operand
            // means. A literal needs no query, which is what makes
            // `telnet <address>` work with no resolver configured.
            let address = tairix_resolver::host_address(host, family)?;
            Some(endpoint_for(address, port))
        }

        fn connect(&mut self, endpoint: Endpoint) -> Result<(), Errno> {
            let socket = tairix_rt::net::stream_socket(endpoint.family, DELIVER_PORT)?;
            if let Some(local) = self.bind {
                if local.family != endpoint.family {
                    let _ = tairix_rt::net::close(socket);
                    return Err(Errno::AddressUnavailable);
                }
                if let Err(errno) = tairix_rt::net::bind(socket, local) {
                    let _ = tairix_rt::net::close(socket);
                    return Err(errno);
                }
            }
            let peer = SocketAddr {
                family: endpoint.family,
                addr: endpoint.addr,
                port: endpoint.port,
            };
            let mut attempt = 0u32;
            loop {
                match tairix_rt::net::connect(socket, peer) {
                    Ok(()) => break,
                    // The stack has no egress until a NIC is bound to it, which
                    // at boot can be after this program starts. Retrying a
                    // bounded number of times on a one-shot park is what makes
                    // `telnet` usable in the first seconds of a session.
                    Err(Errno::NetworkUnreachable) if attempt < CONNECT_ATTEMPTS => {
                        attempt += 1;
                        park_for(self.set, RETRY_PARK_NANOS);
                    }
                    Err(errno) => {
                        let _ = tairix_rt::net::close(socket);
                        return Err(errno);
                    }
                }
            }
            self.socket = Some(socket);
            if let Err(errno) = self.await_connected(socket) {
                let _ = tairix_rt::net::close(socket);
                self.socket = None;
                return Err(errno);
            }
            Ok(())
        }

        fn connected(&self) -> bool {
            self.socket.is_some()
        }

        fn next_event(&mut self) -> Result<IoEvent, Errno> {
            loop {
                // Both mailboxes are drained before parking again, so an event
                // that arrived while the last one was being handled is never
                // left waiting behind a park.
                if let Some(event) = self.drain_network() {
                    return event;
                }
                if let Some(event) = self.drain_keyboard() {
                    return event;
                }
                self.park();
            }
        }

        fn send(&mut self, bytes: &[u8]) -> Result<(), Errno> {
            let socket = self.socket.ok_or(Errno::NotConnected)?;
            let mut offset = 0usize;
            while offset < bytes.len() {
                let end = (offset + SEND_CHUNK).min(bytes.len());
                match tairix_rt::net::stream_send(socket, &bytes[offset..end]) {
                    // A momentarily-full send buffer is retried on a one-shot
                    // park rather than a spin.
                    Ok(0) => park_for(self.set, RETRY_PARK_NANOS),
                    Ok(accepted) => offset += accepted as usize,
                    Err(errno) => return Err(errno),
                }
            }
            Ok(())
        }

        fn terminal_size(&mut self) -> Option<TerminalSize> {
            tairix_rt::terminal_size(tairix_abi::STDOUT)
                .ok()
                .or_else(|| {
                    // A byte-stream console's true geometry is a property of the
                    // terminal at the far end, which the kernel cannot attest; the
                    // conventional fallback is the client's policy to apply.
                    TerminalSize::new(FALLBACK_ROWS, FALLBACK_COLS).ok()
                })
        }

        fn set_input_mode(&mut self, mode: InputMode) {
            let _ = tairix_rt::set_input_mode(mode);
        }

        fn suspend(&mut self) -> Result<(), Errno> {
            let origin = tairix_rt::self_origin().map_err(Errno::from_syscall)?;
            let pid = i64::try_from(origin.pid()).map_err(|_| Errno::OutOfRange)?;
            let ret = tairix_rt::signal(pid, Signal::Stop);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }

        fn shutdown_write(&mut self) -> Result<(), Errno> {
            let socket = self.socket.ok_or(Errno::NotConnected)?;
            tairix_rt::net::shutdown(socket, ShutdownHow::Write)
        }

        fn close(&mut self) -> Result<(), Errno> {
            match self.socket.take() {
                Some(socket) => tairix_rt::net::close(socket),
                None => Ok(()),
            }
        }
    }

    /// Park for `nanos` on the process's wait-set: the task gives the CPU up
    /// until the one-shot timeout elapses or a port becomes ready.
    fn park_for(set: u64, nanos: u64) {
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(set, nanos, &mut token);
    }

    /// The socket endpoint for a resolved address.
    fn endpoint_for(address: IpAddr, port: u16) -> Endpoint {
        let (family, addr) = tairix_abi::net_ipc::address_parts(address);
        Endpoint { family, addr, port }
    }

    /// Parse a `-b` local-bind address. The port is always ephemeral: binding a
    /// *privileged* local port would need a capability a client has no business
    /// holding, and no server matches on a client's source port.
    fn bind_address(text: &str) -> Option<SocketAddr> {
        let address: IpAddr = text.parse().ok()?;
        let endpoint = endpoint_for(address, 0);
        Some(SocketAddr {
            family: endpoint.family,
            addr: endpoint.addr,
            port: 0,
        })
    }

    /// The keyboard-reader thread: block in `Stdin::read`, forward what arrived
    /// to the keyboard port, repeat.
    ///
    /// It ends on end-of-input; otherwise the process exit that follows the
    /// relay tears it down, since `exit` is a thread-group exit. Its own
    /// wait-set holds the port's *room* source, so a mailbox the relay has not
    /// drained yet is waited on rather than polled and no keystroke is dropped.
    fn read_keyboard() {
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            let _ = tairix_rt::ipc_send(KEYBOARD_PORT, &[KEY_TAG_EOF]);
            return;
        };
        let room_armed = tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::PortRoom,
            KEYBOARD_PORT,
            KEYBOARD_TOKEN,
        ) == 0;
        let mut buf = [0u8; KEYBOARD_CHUNK + 1];
        buf[0] = KEY_TAG_DATA;
        // A zero-length or refused read is end of input as far as the relay is
        // concerned: there is no further keystroke to carry.
        while let Ok(read @ 1..) = Stdin.read(&mut buf[1..]) {
            if !post(set, room_armed, &buf[..=read]) {
                break;
            }
        }
        let _ = tairix_rt::ipc_send(KEYBOARD_PORT, &[KEY_TAG_EOF]);
    }

    /// Post one keyboard message, waiting for mailbox room rather than
    /// dropping a keystroke. Returns `false` when the port is unusable, so the
    /// reader ends instead of spinning on a destination that can never drain.
    fn post(set: u64, room_armed: bool, message: &[u8]) -> bool {
        for _ in 0..POST_ATTEMPTS {
            if tairix_rt::ipc_send(KEYBOARD_PORT, message) >= 0 {
                return true;
            }
            if !room_armed {
                return false;
            }
            let mut token = 0u64;
            let _ = tairix_rt::waitset_wait(set, u64::MAX, &mut token);
        }
        false
    }

    /// Program entry point. Exit codes: `0` for a session that ran, `1` for a
    /// failure that defeated it (an unresolvable host, a refused socket, a
    /// terminal that could not be taken), and `2` for a usage error.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("telnet: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The terminal type the session exports, reported over TERMINAL-TYPE;
        // a session that exported none is told so rather than claiming to be a
        // terminal this console may not implement.
        let term = tairix_rt::env_var(b"TERM")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .unwrap_or(FALLBACK_TERM);
        let help = BundleHelp::new("telnet");

        if matches!(command, Command::Help) {
            // The help path opens no socket, binds no port and starts no
            // thread: it is a plain write to standard output.
            return match run(
                command, locale, term, &mut NoIo, &help, &RtOutput, &RtErrors,
            ) {
                Ok(()) => 0,
                Err(err) => {
                    write_stderr_line(&format!("telnet: {err}"));
                    2
                }
            };
        }

        // `-a` asks for the login name without naming it, so it comes from the
        // session's own `USER`. Nothing else is ever disclosed by an
        // invocation.
        let command = match command {
            Command::Session { mut config, target } => {
                if config.auto_login && config.user.is_none() {
                    config.user = tairix_rt::env_var(b"USER")
                        .and_then(|raw| core::str::from_utf8(raw).ok())
                        .map(alloc::string::ToString::to_string);
                }
                Command::Session { config, target }
            }
            help_command @ Command::Help => help_command,
        };

        let bind = match &command {
            Command::Session { config, .. } => match config.bind.as_deref() {
                Some(text) => {
                    let Some(address) = bind_address(text) else {
                        write_stderr_line(&format!("telnet: '{text}' is not a local address"));
                        return 2;
                    };
                    Some(address)
                }
                None => None,
            },
            Command::Help => None,
        };

        let mut io = match RtTelnetIo::open(bind) {
            Ok(io) => io,
            Err(errno) => {
                write_stderr_line(&format!("telnet: cannot set up the session: {errno}"));
                return 1;
            }
        };
        // The raw discipline is taken *before* the reader thread exists: a
        // keystroke it consumed under the cooked one would be echoed by the
        // console and held to the end of a line. `run` takes it again, which is
        // idempotent, and is what restores the cooked default on every exit
        // path.
        io.set_input_mode(InputMode::Raw);
        // The reader thread is detached: it has no value to hand back, and the
        // process exit below is what ends it.
        match tairix_rt::thread::Thread::spawn(read_keyboard) {
            Ok(handle) => handle.detach(),
            Err(errno) => {
                write_stderr_line(&format!("telnet: cannot start the input reader: {errno}"));
                return 1;
            }
        }

        match run(command, locale, term, &mut io, &help, &RtOutput, &RtErrors) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("telnet: {err}"));
                1
            }
        }
    }

    /// The seam for the help path, which reaches nothing. Every method denies
    /// rather than acting, and none is called: `run` returns after writing the
    /// help.
    struct NoIo;

    impl TelnetIo for NoIo {
        fn resolve(
            &mut self,
            _host: &str,
            _port: u16,
            _family: Option<NetAddrFamily>,
        ) -> Option<Endpoint> {
            None
        }

        fn connect(&mut self, _endpoint: Endpoint) -> Result<(), Errno> {
            Err(Errno::NotConnected)
        }

        fn connected(&self) -> bool {
            false
        }

        fn next_event(&mut self) -> Result<IoEvent, Errno> {
            Err(Errno::NotConnected)
        }

        fn send(&mut self, _bytes: &[u8]) -> Result<(), Errno> {
            Err(Errno::NotConnected)
        }

        fn terminal_size(&mut self) -> Option<TerminalSize> {
            None
        }

        fn set_input_mode(&mut self, _mode: InputMode) {}

        fn suspend(&mut self) -> Result<(), Errno> {
            Err(Errno::NotImplemented)
        }

        fn shutdown_write(&mut self) -> Result<(), Errno> {
            Err(Errno::NotConnected)
        }

        fn close(&mut self) -> Result<(), Errno> {
            Ok(())
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
