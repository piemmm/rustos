//! The `Run` entry-point binary of the time service, installed at
//! `/System/Services/timed.app/Run` (`plans/TIMESYNC.md` TS-2).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI. `tairix-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `#[global_allocator]`, the wall-clock and monotonic syscall wrappers, the
//! datagram socket and wait-set reactor, and the file reads and writes the
//! persisted record goes through; `tairix_rt::entry!` names this program's
//! `main`.
//!
//! # The reactor
//!
//! One wait-set over the delivery port the network stack posts this service's
//! datagrams to, armed with the timeout the engine's single folded deadline
//! implies. The loop parks; it never polls. A wake is either a datagram
//! (evaluate it in the sandbox worker, apply the verdict) or the deadline
//! lapsing (send the next request). With no deadline left — every server in
//! use retired and the re-selection ladder spent — the service exits rather
//! than holding a task and a bound delivery port doing nothing.
//!
//! # Where the servers come from
//!
//! The three tiers of `tairix_timesync::select_servers`: the operator's
//! `time.servers`, else what DHCP offered, else the built-in fallback. A
//! boot-floor service starts before either of the first two is knowable, so it
//! starts on the fallback and re-selects on a bounded ladder, replacing its
//! servers only when a *strictly better* tier appears.
//!
//! # Two roles, one binary
//!
//! Started with the sandbox worker argument, this binary *is* the
//! capability-less NTP evaluation worker (`tairix_sandbox::rt`); started
//! normally it is the service. That is the seam's canonical shape: the parent
//! spawns its own binary through the kernel's sandbox spawn mode, so the
//! worker holds two pipe ends and nothing else.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the optional
// `tairix-rt` runtime through the default `program` feature. Host tooling
// builds only this crate's *library*, so this module (and `tairix-rt`) never
// enter that build.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::vec;
    use alloc::vec::Vec;

    use tairix_abi::net::{SocketAddr, SocketDatagram, SocketId};
    use tairix_abi::net_ipc::{ip_from_parts, NetAddrFamily};
    use tairix_abi::rtc_ipc::{self, RtcOp, RtcReading, RTC_ENDPOINT};
    use tairix_abi::time::{Duration64, Time64};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{
        Errno, Origin, RandomFlags, WallClockReading, WallTimeState, MAX_TIME_SERVERS,
    };
    use tairix_log::{Event, EventId, Level};
    use tairix_net::addr::IpAddr;
    use tairix_net::ntp::PORT;
    use tairix_procinfo::{IpcTransport, WalkStep};
    use tairix_resolver::RtDnsTransport;
    use tairix_rt::LogSink;
    use tairix_sandbox::host::ParserSandbox;
    use tairix_sandbox::rt::{worker_role, RtLauncher};
    use tairix_sandbox::timesync::TimeSyncService;
    use tairix_sysconfig::SystemConfig;
    use tairix_timed::{
        Clock, RecordStore, RetryLadder, RtcSource, Timed, TimedConfig, Transport,
        CONFIG_RETRY_ATTEMPTS, CONFIG_RETRY_BASE_NANOS,
    };
    use tairix_timesync::events::{SERVERS_FROM_FALLBACK, SERVICE_UNAVAILABLE, TIMED_RANGE_START};
    use tairix_timesync::{
        select_servers, ServerSelection, ServerSource, TimeServer, RECORD_DIR, RECORD_LEN,
        RECORD_PATH,
    };

    /// Exit code when the reactor could not be armed: the delivery port could
    /// not be bound, or the wait-set could not be created or armed. A
    /// reserved, fail-closed value — the service exits rather than degrading
    /// into a poll, and PID 1 relaunches it.
    const EXIT_REACTOR_UNAVAILABLE: i32 = 70;

    /// The service's delivery-port endpoint id — an app-local, unrestricted
    /// well-known value (not a reserved kernel id), so binding it needs no
    /// capability. The stack posts this socket's inbound datagrams here.
    /// (`0x_6e_74_70_71` spells "ntpq".)
    const DELIVER_PORT: u64 = 0x_6e74_7071;

    /// Delivery-port mailbox depth. One request is in flight at a time, but a
    /// late reply from a previous transaction can still be arriving; this
    /// headroom lets a few queue rather than back-pressure the stack.
    const DELIVER_CAPACITY: usize = 8;

    /// Wait-set token for the delivery port (one source, so any token
    /// identifies it).
    const DELIVER_TOKEN: u64 = 1;

    /// The audit sink every record is written through.
    static LOG_SINK: LogSink = LogSink;

    /// The production [`Clock`]: the kernel's monotonic and wall clocks.
    struct RtClock;

    impl Clock for RtClock {
        fn monotonic(&self) -> Duration64 {
            Duration64::from_nanos(tairix_rt::clock_get())
        }

        fn wall(&self) -> Result<WallClockReading, Errno> {
            tairix_rt::wall_time().map_err(Errno::from_syscall)
        }

        fn set_wall(&self, time: Time64, state: WallTimeState) -> Result<(), Errno> {
            let ret = tairix_rt::wall_time_set(time, state);
            if ret < 0 {
                Err(Errno::from_syscall(ret))
            } else {
                Ok(())
            }
        }
    }

    /// The production [`RtcSource`]: the board's clock chip, reached through
    /// the autoloaded RTC driver's well-known call endpoint.
    ///
    /// A board with no clock chip has no driver serving the endpoint, so the
    /// call fails `NotFound` and the engine's bounded ladder decides how long
    /// to keep looking. Every refusal is typed and in-band; nothing here
    /// retries on the spot.
    struct RtRtc;

    impl RtRtc {
        /// Issue one request and return the framed reply bytes.
        fn call(op: RtcOp, time: Time64, reply: &mut [u8]) -> Result<usize, Errno> {
            let mut request = [0u8; rtc_ipc::REQUEST_LEN];
            rtc_ipc::encode_request(&mut request, op, time)?;
            tairix_rt::ipc_call(RTC_ENDPOINT, &request, reply).map_err(Errno::from_syscall)
        }
    }

    impl RtcSource for RtRtc {
        fn read(&mut self) -> Result<RtcReading, Errno> {
            let mut reply = [0u8; rtc_ipc::REPLY_LEN];
            let n = Self::call(RtcOp::Read, Time64::UNIX_EPOCH, &mut reply)?;
            rtc_ipc::decode_reading(&reply[..n])
        }

        fn set(&mut self, time: Time64) -> Result<(), Errno> {
            let mut reply = [0u8; rtc_ipc::REPLY_LEN];
            let n = Self::call(RtcOp::Set, time, &mut reply)?;
            rtc_ipc::decode_ack(&reply[..n])
        }
    }

    /// The production [`RecordStore`]: the fixed-length document under
    /// `/System/Settings/Time`, one of the two writable paths beneath the
    /// read-only `/System`.
    struct RtRecordStore;

    impl RecordStore for RtRecordStore {
        fn read(&self) -> Result<Option<Vec<u8>>, Errno> {
            let file = match tairix_rt::open(RECORD_PATH.as_bytes()) {
                Ok(file) => file,
                // A machine that has never synchronised has no record; that
                // is the normal first-boot case, not a failure.
                Err(ret) if Errno::from_syscall(ret) == Errno::NotFound => return Ok(None),
                Err(ret) => return Err(Errno::from_syscall(ret)),
            };
            let mut buf = vec![0u8; RECORD_LEN];
            let read = file.read_at(0, &mut buf).map_err(Errno::from_syscall)?;
            buf.truncate(read);
            Ok(Some(buf))
        }

        fn write(&self, bytes: &[u8]) -> Result<(), Errno> {
            let file = match tairix_rt::create(RECORD_PATH.as_bytes()) {
                Ok(file) => file,
                // The directory is absent only on a volume laid out before
                // this service existed. Creating it is attempted only then:
                // `/System/Settings` is system-owned, so an unconditional
                // attempt is refused on every provisioned machine and would
                // file a denied-mutation record on each successful sync,
                // burying a real denial in routine noise.
                Err(ret) if Errno::from_syscall(ret) == Errno::NotFound => {
                    let made = tairix_rt::fs_mkdir(RECORD_DIR.as_bytes());
                    if made != 0 && Errno::from_syscall(made) != Errno::AlreadyExists {
                        return Err(Errno::from_syscall(made));
                    }
                    tairix_rt::create(RECORD_PATH.as_bytes()).map_err(Errno::from_syscall)?
                }
                Err(ret) => return Err(Errno::from_syscall(ret)),
            };
            let written = file.write_at(0, bytes).map_err(Errno::from_syscall)?;
            if written == bytes.len() {
                Ok(())
            } else {
                Err(Errno::BufferTooSmall)
            }
        }
    }

    /// The production [`Transport`]: a UDP datagram socket per address family,
    /// with each configured server's name resolved once and cached.
    ///
    /// Resolution is lazy and per-server rather than a start-up sweep: a
    /// machine whose network comes up after this service does would otherwise
    /// have resolved nothing and never retried. A name that does not resolve
    /// yet simply fails the send, and the engine's own timeout and bounded
    /// backoff pace the next attempt.
    ///
    /// The stub-resolver transport is opened lazily too, and only for a server
    /// that is *not* an address literal: a machine configured with literals
    /// never binds a resolver delivery port or a wait-set it would not use.
    struct RtTransport {
        servers: Vec<ServerEntry>,
        dns: Option<RtDnsTransport>,
        v4: Option<SocketId>,
        v6: Option<SocketId>,
    }

    /// One selected server: its spelling and, once known, its address.
    struct ServerEntry {
        name: Vec<u8>,
        address: Option<IpAddr>,
    }

    impl RtTransport {
        /// Build the transport over the selected servers.
        ///
        /// A network-supplied server arrives as an address and is entered
        /// already resolved, so a machine whose only DNS advice would have
        /// come from the same lease never needs a resolver to keep time.
        fn new(servers: &[TimeServer]) -> Self {
            let servers = servers
                .iter()
                .take(MAX_TIME_SERVERS)
                .map(|server| ServerEntry {
                    name: server.name.as_bytes().to_vec(),
                    address: server.address,
                })
                .collect();
            Self {
                servers,
                dns: None,
                v4: None,
                v6: None,
            }
        }

        /// The address of the server at `index`, resolving and caching it on
        /// first use.
        fn address_of(&mut self, index: u8) -> Option<IpAddr> {
            let entry = self.servers.get(usize::from(index))?;
            if let Some(address) = entry.address {
                return Some(address);
            }
            let name = core::str::from_utf8(&entry.name).ok()?;
            // An address literal is answered by the one shared spelling
            // policy without a resolver at all, which is what lets a machine
            // with no DNS configured still keep time.
            let resolved = if let Some(address) = tairix_resolver::literal_address(name, None) {
                address
            } else {
                if self.dns.is_none() {
                    self.dns = RtDnsTransport::open().ok();
                }
                self.dns.as_mut()?.host_address(name, None)?
            };
            self.servers.get_mut(usize::from(index))?.address = Some(resolved);
            Some(resolved)
        }

        /// The datagram socket for `family`, opened on first use with a
        /// CSPRNG-drawn ephemeral source port and cached thereafter.
        fn socket_for(&mut self, family: NetAddrFamily) -> Result<SocketId, Errno> {
            let cached = match family {
                NetAddrFamily::V4 => &mut self.v4,
                NetAddrFamily::V6 => &mut self.v6,
            };
            if let Some(socket) = *cached {
                return Ok(socket);
            }
            let socket = tairix_rt::net::socket(family, DELIVER_PORT)?;
            // A local port of 0 asks the stack for a CSPRNG-drawn ephemeral
            // port, widening an off-path spoofer's search space beyond the
            // nonce alone.
            let local = SocketAddr {
                family,
                addr: [0u8; 16],
                port: 0,
            };
            tairix_rt::net::bind(socket, local)?;
            *cached = Some(socket);
            Ok(socket)
        }
    }

    impl Transport for RtTransport {
        fn send(&mut self, index: u8, packet: &[u8]) -> Result<(), Errno> {
            let address = self.address_of(index).ok_or(Errno::NetworkUnreachable)?;
            let (family, addr) = tairix_abi::net_ipc::address_parts(address);
            let socket = self.socket_for(family)?;
            let dest = SocketAddr {
                family,
                addr,
                port: PORT,
            };
            tairix_rt::net::send(socket, Some(dest), packet)
        }
    }

    impl Drop for RtTransport {
        fn drop(&mut self) {
            // Best-effort teardown so the ephemeral ports do not linger.
            if let Some(socket) = self.v4.take() {
                let _ = tairix_rt::net::close(socket);
            }
            if let Some(socket) = self.v6.take() {
                let _ = tairix_rt::net::close(socket);
            }
        }
    }

    /// One fresh CSPRNG word. Every nonce and every jitter draw comes from
    /// the kernel random subsystem — never a counter or a clock reading, which
    /// would hand an off-path attacker a predictable target.
    fn entropy() -> u64 {
        let mut bytes = [0u8; 8];
        let _ = tairix_rt::random_get(&mut bytes, RandomFlags::empty());
        u64::from_le_bytes(bytes)
    }

    /// Record a start-up or reactor outcome.
    fn record(id: EventId, level: Level, message: &str) {
        tairix_log::log(
            &LOG_SINK,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Read the network time servers this host's DHCP client(s) learned, over
    /// the ungated system-information query `sysinfod` fronts.
    ///
    /// Advice, not authority: the answer is a set of addresses to *ask*, and
    /// every sample one returns is still nonce-gated and plausibility-checked
    /// before the clock moves. An unavailable service (the usual case on a
    /// boot-floor start-up, before `sysinfod` and `netstack` are up) is an
    /// empty set, which simply leaves a lower tier in place; the caller's
    /// bounded ladder is what looks again.
    fn learned_time_servers() -> Vec<IpAddr> {
        let mut servers = Vec::new();
        let _ = tairix_procinfo::for_each_time_server(&IpcTransport, |record| {
            servers.push(ip_from_parts(record.family, record.addr));
            Ok(WalkStep::Continue)
        });
        servers
    }

    /// Read the boot-time configuration store, falling back to the documented
    /// defaults when it is absent or cannot be understood.
    ///
    /// A store this service cannot fully parse yields the defaults — which
    /// name no server, so the service simply never queries — rather than a
    /// guess at a partial intent. Before the encrypted root is mounted the
    /// path has no backing at all, which is indistinguishable from a
    /// volume-less boot and equally yields the defaults; the caller's bounded
    /// re-read ladder is what separates the two, by outlasting the unlock.
    fn load_config() -> SystemConfig {
        match tairix_rt::open(tairix_sysconfig::CONFIG_PATH.as_bytes()) {
            Ok(file) => load_from(&file),
            Err(_) => SystemConfig::default(),
        }
    }

    /// Parse the store document `file` holds, or the documented defaults when
    /// it cannot be read or fully understood — never a guess at a partial
    /// intent.
    fn load_from(file: &tairix_rt::File) -> SystemConfig {
        let mut buf = vec![0u8; tairix_sysconfig::MAX_CONFIG_LEN];
        let Ok(read) = file.read_at(0, &mut buf) else {
            return SystemConfig::default();
        };
        buf.truncate(read);
        core::str::from_utf8(&buf)
            .ok()
            .and_then(|text| SystemConfig::parse(text).ok())
            .unwrap_or_default()
    }

    /// Widen a non-negative monotonic [`Duration64`] to the `u64` nanosecond
    /// count the wait-set and clock syscalls speak.
    fn nanos(span: Duration64) -> u64 {
        span.saturating_total_nanos()
    }

    /// The reactor's remaining wait until the earlier of two absolute
    /// nanosecond deadlines, either of which may be absent.
    ///
    /// [`None`] means there is nothing left to wait for at all. Zero means a
    /// deadline has already lapsed, so the next turn of the loop runs at once.
    fn timeout_until(now: u64, engine: Option<u64>, config: Option<u64>) -> Option<u64> {
        let soonest = match (engine, config) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some(soonest.saturating_sub(now))
    }

    /// Bind the delivery port and arm the reactor's wait-set over it.
    ///
    /// [`None`] once the failure has been recorded: the service exits rather
    /// than degrading into a poll, and PID 1 relaunches it.
    fn arm_reactor() -> Option<u64> {
        if tairix_rt::port_bind(DELIVER_PORT, SocketDatagram::MAX_WIRE_LEN, DELIVER_CAPACITY) < 0 {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "timed: the delivery port could not be bound",
            );
            return None;
        }
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "timed: the reactor wait-set could not be created",
            );
            return None;
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            DELIVER_PORT,
            DELIVER_TOKEN,
        ) != 0
        {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "timed: the reactor wait-set could not be armed",
            );
            return None;
        }
        Some(set)
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // The worker role first, before any other argument handling: a worker
        // never behaves as the service.
        if worker_role() {
            let mut service = TimeSyncService;
            let _ = tairix_sandbox::rt::serve_stdio(&mut service);
            return 0;
        }

        let Some(set) = arm_reactor() else {
            return EXIT_REACTOR_UNAVAILABLE;
        };

        // Built immediately, from whatever is knowable right now — which on a
        // boot-floor service is usually neither the store (the encrypted root
        // is not mounted yet) nor a lease (no NIC is up), so it is usually the
        // built-in fallback. Waiting here instead would hold the boot up
        // behind a service nothing else is waiting for, and the fallback tier
        // is exactly what makes waiting unnecessary.
        let mut config = load_config();
        let mut selection = select_servers(&config.time_servers, &learned_time_servers());
        let mut retry = RetryLadder::arm(
            tairix_rt::clock_get(),
            CONFIG_RETRY_BASE_NANOS,
            CONFIG_RETRY_ATTEMPTS,
            selection.source == ServerSource::Configured,
        );
        let build = |selection: &ServerSelection, refresh: Duration64| {
            Timed::new(TimedConfig {
                clock: RtClock,
                rtc: RtRtc,
                store: RtRecordStore,
                transport: RtTransport::new(&selection.servers),
                sandbox: ParserSandbox::new(RtLauncher::own_binary(), LOG_SINK),
                sink: LOG_SINK,
                selection: selection.clone(),
                refresh,
                entropy: entropy(),
            })
        };
        let mut service = build(&selection, config.time_refresh.interval());

        // The stack's kernel-attested origin, captured from the first
        // datagram so every later one can be required to match it. The
        // delivery port is otherwise an unauthenticated inbox: a datagram
        // from any other sender is dropped (fail closed) before the engine
        // ever sees it.
        let mut stack: Option<Origin> = None;
        let mut scratch = vec![0u8; SocketDatagram::MAX_WIRE_LEN];
        let mut token = 0u64;
        loop {
            let now = tairix_rt::clock_get();
            let engine_at = service.next_deadline().map(nanos);
            let Some(timeout) = timeout_until(now, engine_at, retry.as_ref().map(|r| r.at)) else {
                // Nothing left to wait for: every server in use has retired
                // and the re-read ladder is spent. Exit rather than hold a
                // task and a bound delivery port for the rest of the boot
                // doing nothing; restarting the service is what picks a later
                // configuration up either way, and PID 1 audits the exit.
                return 0;
            };
            let waited = tairix_rt::waitset_wait(set, timeout, &mut token);
            let woken = Duration64::from_nanos(tairix_rt::clock_get());
            if waited == 0 {
                // A datagram is waiting. Drain the mailbox, authenticating
                // each sender, then fall through to the deadline check. Each
                // payload is copied out of the shared receive buffer before
                // the engine is handed it, so the next drain can reuse it.
                while let Ok((datagram, origin)) = tairix_rt::net::recv(DELIVER_PORT, &mut scratch)
                {
                    let bytes = match stack {
                        // A datagram from any sender but the network stack is
                        // dropped: the delivery port is otherwise an
                        // unauthenticated inbox (fail closed).
                        Some(known) if known != origin => continue,
                        _ => {
                            stack = Some(origin);
                            datagram.payload.to_vec()
                        }
                    };
                    service.on_datagram(woken, &bytes);
                }
            } else if Errno::from_syscall(waited) != Errno::TimedOut {
                // A dead wait-set would degrade the loop into a busy poll;
                // exit fail-loud instead and let PID 1 relaunch the service.
                record(
                    SERVICE_UNAVAILABLE,
                    Level::Warn,
                    "timed: the reactor wait-set failed",
                );
                return EXIT_REACTOR_UNAVAILABLE;
            }

            // The re-selection rung, if one is due: the encrypted root may
            // have been mounted, or a DHCP lease acquired, since the last
            // attempt. Only a *strictly better* tier replaces the servers in
            // use — an equal-tier change (a renewed lease naming a different
            // server) would reset the engine's rotation and backoff and
            // forget which servers had refused, which costs more than it
            // buys.
            if let Some(rung) = retry.as_mut() {
                if nanos(woken) >= rung.at {
                    config = load_config();
                    let next = select_servers(&config.time_servers, &learned_time_servers());
                    let upgraded = next.source > selection.source;
                    if upgraded {
                        selection = next;
                        service = build(&selection, config.time_refresh.interval());
                    }
                    if selection.source == ServerSource::Configured {
                        // The best tier there is: nothing further to look for.
                        retry = None;
                    } else if !rung.advance(nanos(woken)) {
                        if selection.source == ServerSource::Fallback {
                            record(
                                SERVERS_FROM_FALLBACK,
                                Level::Info,
                                "timed: no configured or network-supplied time server was found within the start-up window; the built-in fallback stands",
                            );
                        }
                        retry = None;
                    }
                    if upgraded {
                        continue;
                    }
                }
            }

            service.poll(woken, entropy());
        }
    }

    // Touch the reserved-range constant so the audit range this service owns
    // is linked into the program that emits it.
    const _: u32 = TIMED_RANGE_START;

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O; the engine and the sandboxed evaluation are host-tested in
// their own modules and the whole path is exercised by the QEMU vertical.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
