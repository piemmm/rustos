//! The `Run` entry-point binary of `discoveryd`, installed at
//! `/System/Services/discoveryd.app/Run` (`plans/ZEROCONF.md`).
//!
//! One binary, two roles. Spawned in the sandbox's session-worker role it is
//! the decoder, holding two pipe ends and nothing else; started normally it is
//! the front, holding the multicast DNS sockets and the discovery endpoint.
//!
//! # The reactor
//!
//! One wait-set: the delivery port the stack posts datagrams and link events
//! to — registered only while the front will take them — the decoder's two
//! pipe ends, registered as its session wants them, the discovery endpoint,
//! the exits of its clients, room in any client port a doorbell was refused
//! by, and a timeout for the one instant the front must next act by. The loop
//! parks; it never polls.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::vec::Vec;
    use core::fmt::Write as _;

    use tairix_abi::discovery_ipc::{
        DISCOVERY_ENDPOINT, DISCOVERY_MAX_REPLY, DISCOVERY_MAX_REQUEST,
    };
    use tairix_abi::discovery_policy::{DISCOVERY_POLICY_MAX, DISCOVERY_POLICY_PATH};
    use tairix_abi::net::{SocketAddr, SocketDatagram, SocketId};
    use tairix_abi::net_ipc::{address_parts, NetAddrFamily, IF_NAME_LEN};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, FieldValue, Origin, ProcId, ORIGIN_WIRE_LEN};
    use tairix_caps::CapabilitySet;
    use tairix_discoveryd::decoder::Decoder;
    use tairix_discoveryd::events::{GRANTS_REFUSED, SERVICE_STARTED, SERVICE_UNAVAILABLE};
    use tairix_discoveryd::front::{Front, Host, Sockets};
    use tairix_discoveryd::grants::Grants;
    use tairix_inline::ArrayString;
    use tairix_log::{Event, EventId, Field, Level};
    use tairix_net::mdns::{GROUP_V4, GROUP_V6, PORT};
    use tairix_net::IpAddr;
    use tairix_rt::LogSink;
    use tairix_sandbox::rt::{
        serve_session_stdio, session_worker_role, RtSessionLauncher, SessionMembers,
    };
    use tairix_util::fallible;

    /// Exit code when the service cannot serve; the reason is logged first.
    const EXIT_UNAVAILABLE: i32 = 70;

    /// Delivery-port mailbox depth: room for a segment's burst of
    /// announcements while the front is busy, the bound on what the stack
    /// queues for it while the front is not draining, and the most one wake
    /// drains before the loop returns to its wait-set.
    const DELIVER_CAPACITY: usize = 64;

    /// Calls the discovery endpoint holds at once — a fixed memory bound on
    /// the service's clients, each of whose calls is answered at once.
    const CALL_CAPACITY: usize = 16;

    /// Wait-set tokens.
    const TOKEN_PORT: u64 = 1;
    const TOKEN_READ: u64 = 2;
    const TOKEN_WRITE: u64 = 3;
    const TOKEN_CALL: u64 = 4;
    const TOKEN_EXIT: u64 = 5;
    const TOKEN_ROOM: u64 = 6;

    /// The audit sink every record is written through.
    static LOG_SINK: LogSink = LogSink;

    /// The kernel, as the front sees it.
    struct RtHost {
        sockets: Sockets,
    }

    impl Host for RtHost {
        fn fill_random(&mut self, out: &mut [u8]) -> Result<(), Errno> {
            tairix_rt::random_fill(out)
        }

        fn transmit(
            &mut self,
            interface: [u8; IF_NAME_LEN],
            to: SocketAddr,
            payload: &[u8],
        ) -> Result<(), Errno> {
            let socket = match to.family {
                NetAddrFamily::V4 => self.sockets.v4,
                NetAddrFamily::V6 => self.sockets.v6,
            }
            .ok_or(Errno::NotFound)?;
            tairix_rt::net::send(socket, Some(to), Some(interface), payload)
        }

        fn ring(&mut self, port: u64, doorbell: &[u8]) -> Result<(), Errno> {
            match tairix_rt::ipc_send(port, doorbell) {
                0 => Ok(()),
                ret => Err(Errno::from_syscall(ret)),
            }
        }

        fn watch(&mut self, peer: ProcId) -> Result<(), Errno> {
            tairix_rt::peer_watch(peer)
        }
    }

    fn record(id: EventId, level: Level, message: &'static str, reason: &'static str) {
        tairix_log::log(
            &LOG_SINK,
            &Event {
                level,
                id,
                message,
                fields: &[Field {
                    key: "reason",
                    value: FieldValue::Str(reason),
                }],
            },
        );
    }

    /// Record why the service cannot serve, and the exit code that says so.
    fn unavailable(reason: &'static str) -> i32 {
        record(
            SERVICE_UNAVAILABLE,
            Level::Error,
            "discoveryd: cannot serve",
            reason,
        );
        EXIT_UNAVAILABLE
    }

    /// The step of a family's socket set-up the stack refused, and why.
    struct Refused {
        step: &'static str,
        errno: Errno,
    }

    /// Open one family's socket on the multicast DNS port and join its group,
    /// delivering to `deliver`. A refused step closes what was opened.
    fn listen(family: NetAddrFamily, group: IpAddr, deliver: u64) -> Result<SocketId, Refused> {
        let socket = tairix_rt::net::socket(family, deliver).map_err(|errno| Refused {
            step: "open",
            errno,
        })?;
        let local = SocketAddr {
            family,
            addr: [0u8; 16],
            port: PORT,
        };
        let (group_family, group_addr) = address_parts(group);
        let joined = tairix_rt::net::bind(socket, local)
            .map_err(|errno| Refused {
                step: "bind",
                errno,
            })
            .and_then(|_| {
                tairix_rt::net::join_multicast(
                    socket,
                    SocketAddr {
                        family: group_family,
                        addr: group_addr,
                        port: 0,
                    },
                )
                .map_err(|errno| Refused {
                    step: "join",
                    errno,
                })
            });
        if let Err(refused) = joined {
            let _ = tairix_rt::net::close(socket);
            return Err(refused);
        }
        Ok(socket)
    }

    /// What became of one family's socket, as the start record states it.
    fn describe(opened: &Result<SocketId, Refused>) -> ArrayString<64> {
        let mut text = ArrayString::new();
        let _ = match opened {
            Ok(_) => text.write_str("joined"),
            Err(refused) => write!(text, "{} refused: {}", refused.step, refused.errno),
        };
        text
    }

    /// The grants the store records. A store that is absent grants nothing,
    /// and so does one that cannot be opened, read, or taken whole — the
    /// refusal recorded, so an administrator learns why no application can
    /// browse.
    fn load_grants() -> Grants {
        let file = match tairix_rt::open(DISCOVERY_POLICY_PATH.as_bytes()) {
            Ok(file) => file,
            Err(err) if Errno::from_syscall(err) == Errno::NotFound => return Grants::default(),
            Err(_) => {
                record(
                    GRANTS_REFUSED,
                    Level::Warn,
                    "discoveryd: no application may browse",
                    "the grant store could not be opened",
                );
                return Grants::default();
            }
        };
        // One byte past the bound, so a store that exceeds it is seen to.
        let Some(mut store) = fallible::filled(DISCOVERY_POLICY_MAX + 1, 0u8) else {
            record(
                GRANTS_REFUSED,
                Level::Warn,
                "discoveryd: no application may browse",
                "the grant store could not be read into memory",
            );
            return Grants::default();
        };
        let mut filled = 0;
        while filled < store.len() {
            match file.read_at(filled as u64, &mut store[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(_) => {
                    record(
                        GRANTS_REFUSED,
                        Level::Warn,
                        "discoveryd: no application may browse",
                        "the grant store could not be read",
                    );
                    return Grants::default();
                }
            }
        }
        Grants::load(&store[..filled]).unwrap_or_else(|_| {
            record(
                GRANTS_REFUSED,
                Level::Warn,
                "discoveryd: no application may browse",
                "the grant store is malformed or past its bound",
            );
            Grants::default()
        })
    }

    /// Bind the discovery endpoint: unrestricted-sender, since the front
    /// admits every call against the caller's attested origin.
    fn bind_endpoint(set: u64) -> bool {
        let empty = CapabilitySet::empty();
        tairix_rt::call_create(
            DISCOVERY_ENDPOINT,
            &empty,
            &empty,
            DISCOVERY_MAX_REQUEST,
            DISCOVERY_MAX_REPLY,
            CALL_CAPACITY,
        ) == 0
            && tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Endpoint,
                DISCOVERY_ENDPOINT,
                TOKEN_CALL,
            ) == 0
            && tairix_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::PeerExit, 0, TOKEN_EXIT)
                == 0
    }

    /// Serve one waiting call. A call whose origin cannot be attested is
    /// never served.
    fn serve_call<L, H>(
        front: &mut Front<L, LogSink, H>,
        now: u64,
        request: &mut [u8],
        reply: &mut [u8],
    ) where
        L: tairix_sandbox::supervise::SessionLauncher,
        H: Host,
    {
        let mut ticket = 0u64;
        let Ok(len) = tairix_rt::call_recv(DISCOVERY_ENDPOINT, request, &mut ticket) else {
            return;
        };
        let mut origin = [0u8; ORIGIN_WIRE_LEN];
        let attested = tairix_rt::call_peer_origin(DISCOVERY_ENDPOINT, ticket, &mut origin)
            .ok()
            .and_then(|read| Origin::from_bytes(&origin[..read]).ok());
        let Some(origin) = attested else {
            let status = tairix_abi::reply::encode_status_reply(Err(Errno::PermissionDenied));
            let _ = tairix_rt::call_reply(DISCOVERY_ENDPOINT, ticket, &status);
            return;
        };
        let written = front.serve(now, &origin, &request[..len], reply);
        let _ = tairix_rt::call_reply(DISCOVERY_ENDPOINT, ticket, &reply[..written]);
    }

    /// Park on room in exactly the client ports a doorbell is owed to.
    fn sync_rooms(set: u64, owed: &[u64], rooms: &mut Vec<u64>) {
        rooms.retain(|port| {
            let keep = owed.contains(port);
            if !keep {
                let _ = tairix_rt::waitset_ctl(
                    set,
                    WaitSetOp::Del,
                    WaitSourceKind::PortRoom,
                    *port,
                    TOKEN_ROOM,
                );
            }
            keep
        });
        for &port in owed {
            if rooms.contains(&port) || rooms.try_reserve(1).is_err() {
                continue;
            }
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::PortRoom,
                port,
                TOKEN_ROOM,
            ) == 0
            {
                rooms.push(port);
            }
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // The worker role first, before anything else: a decoder never
        // behaves as the service.
        if session_worker_role() {
            let _ = serve_session_stdio(&mut Decoder::new());
            return 0;
        }

        let Ok(deliver) =
            tairix_rt::bind_private_port(SocketDatagram::MAX_WIRE_LEN, DELIVER_CAPACITY)
        else {
            return unavailable("the delivery port could not be bound");
        };
        let v4 = listen(NetAddrFamily::V4, IpAddr::V4(GROUP_V4), deliver);
        let v6 = listen(NetAddrFamily::V6, IpAddr::V6(GROUP_V6), deliver);
        let (v4_state, v6_state) = (describe(&v4), describe(&v6));
        let families = [
            Field {
                key: "ipv4",
                value: FieldValue::Str(v4_state.as_str()),
            },
            Field {
                key: "ipv6",
                value: FieldValue::Str(v6_state.as_str()),
            },
        ];
        let sockets = Sockets {
            v4: v4.ok(),
            v6: v6.ok(),
        };
        if sockets.v4.is_none() && sockets.v6.is_none() {
            tairix_log::log(
                &LOG_SINK,
                &Event {
                    level: Level::Error,
                    id: SERVICE_UNAVAILABLE,
                    message: "discoveryd: cannot serve: no multicast DNS socket could be opened",
                    fields: &families,
                },
            );
            return EXIT_UNAVAILABLE;
        }
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return unavailable("the reactor wait-set could not be created");
        };
        if !bind_endpoint(set) {
            return unavailable("the discovery endpoint could not be bound");
        }
        let grants = load_grants();
        let Ok(mut front) = Front::new(
            RtSessionLauncher::own_binary(),
            LOG_SINK,
            RtHost { sockets },
            sockets,
            grants,
        ) else {
            return unavailable("the front's buffers could not be committed");
        };
        tairix_log::log(
            &LOG_SINK,
            &Event {
                level: Level::Info,
                id: SERVICE_STARTED,
                message: "discoveryd: joined the multicast DNS groups",
                fields: &families,
            },
        );
        serve(&mut front, set, deliver)
    }

    /// Serve for the life of the service, returning only the exit code of a
    /// failure it cannot serve past.
    fn serve<L>(front: &mut Front<L, LogSink, RtHost>, set: u64, deliver: u64) -> i32
    where
        L: tairix_sandbox::supervise::SessionLauncher,
    {
        let (Some(mut scratch), Some(mut request), Some(mut reply)) = (
            fallible::filled(SocketDatagram::MAX_WIRE_LEN, 0u8),
            fallible::filled(DISCOVERY_MAX_REQUEST, 0u8),
            fallible::filled(DISCOVERY_MAX_REPLY, 0u8),
        ) else {
            return unavailable("the receive buffers could not be committed");
        };
        let mut members = SessionMembers::new(set, TOKEN_READ, TOKEN_WRITE);
        let mut port_armed = false;
        let mut rooms: Vec<u64> = Vec::new();
        let mut owed: Vec<u64> = Vec::new();
        loop {
            let now = tairix_rt::clock_get();
            if front.on_wake(now).is_err() {
                return unavailable("the random source cannot key a decoder");
            }
            if members
                .sync(front.descriptors(), front.wants_read(), front.wants_write())
                .is_err()
            {
                return unavailable("the decoder's pipes could not be watched");
            }
            // A port left registered while the front will not take from it
            // would wake the loop for nothing, level-triggered.
            let draining = front.wants_deliveries();
            if draining != port_armed {
                let op = if draining {
                    WaitSetOp::Add
                } else {
                    WaitSetOp::Del
                };
                if tairix_rt::waitset_ctl(set, op, WaitSourceKind::Port, deliver, TOKEN_PORT) != 0 {
                    return unavailable("the delivery port could not be watched");
                }
                port_armed = draining;
            }
            owed.clear();
            owed.extend(front.owed_rings());
            sync_rooms(set, &owed, &mut rooms);

            let timeout = front
                .wake_at()
                .map_or(u64::MAX, |at| at.saturating_sub(now));
            let mut token = 0u64;
            let waited = tairix_rt::waitset_wait(set, timeout, &mut token);
            let now = tairix_rt::clock_get();
            if waited == 0 {
                match token {
                    TOKEN_PORT => {
                        // Bounded, so a segment that refills the mailbox as
                        // fast as it drains cannot keep the loop from the
                        // decoder's pipes, its clients, or its timer.
                        for _ in 0..DELIVER_CAPACITY {
                            if !front.wants_deliveries() {
                                break;
                            }
                            let Ok(delivery) = tairix_rt::net::recv(deliver, &mut scratch) else {
                                break;
                            };
                            front.on_delivery(now, &delivery);
                        }
                    }
                    TOKEN_READ => front.on_decoder_readable(now),
                    TOKEN_WRITE => front.on_decoder_writable(now),
                    TOKEN_CALL => serve_call(front, now, &mut request, &mut reply),
                    TOKEN_EXIT => {
                        while let Ok(peer) = tairix_rt::peer_exit_take() {
                            front.on_peer_exit(now, peer);
                        }
                    }
                    TOKEN_ROOM => front.retry_rings(),
                    _ => {}
                }
            } else if Errno::from_syscall(waited) != Errno::TimedOut {
                // A dead wait-set would degrade the loop into a busy poll.
                return unavailable("the reactor wait-set failed");
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host the program's real entry — the freestanding `tairix-rt`
// `_start` path — is not compiled, so this inert `main` keeps the crate
// building under the host tooling. The front and the decoder are host-tested
// in their own modules.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
