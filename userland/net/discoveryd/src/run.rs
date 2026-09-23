//! The `Run` entry-point binary of `discoveryd`, installed at
//! `/System/Services/discoveryd.app/Run` (`plans/ZEROCONF.md`).
//!
//! One binary, two roles. Spawned in the sandbox's session-worker role it is
//! the decoder, holding two pipe ends and nothing else; started normally it is
//! the front, holding the multicast DNS sockets.
//!
//! # The reactor
//!
//! One wait-set: the delivery port the stack posts datagrams to — registered
//! only while the front will take them — the decoder's two pipe ends,
//! registered as its session wants them, and a timeout for the one instant
//! the front must next act by. The loop parks; it never polls.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::net::{SocketAddr, SocketDatagram};
    use tairix_abi::net_ipc::{address_parts, NetAddrFamily};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, FieldValue};
    use tairix_discoveryd::decoder::Decoder;
    use tairix_discoveryd::events::{SERVICE_STARTED, SERVICE_UNAVAILABLE};
    use tairix_discoveryd::front::{Entropy, Front};
    use tairix_log::{Event, Field, Level};
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

    /// Wait-set tokens.
    const TOKEN_PORT: u64 = 1;
    const TOKEN_READ: u64 = 2;
    const TOKEN_WRITE: u64 = 3;

    /// The audit sink every record is written through.
    static LOG_SINK: LogSink = LogSink;

    /// The kernel CSPRNG, which keys every decoder.
    struct RtEntropy;

    impl Entropy for RtEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno> {
            tairix_rt::random_fill(out)
        }
    }

    /// Record why the service cannot serve, and the exit code that says so.
    fn unavailable(reason: &'static str) -> i32 {
        tairix_log::log(
            &LOG_SINK,
            &Event {
                level: Level::Error,
                id: SERVICE_UNAVAILABLE,
                message: "discoveryd: cannot serve",
                fields: &[Field {
                    key: "reason",
                    value: FieldValue::Str(reason),
                }],
            },
        );
        EXIT_UNAVAILABLE
    }

    /// Open one family's socket on the multicast DNS port and join its group,
    /// delivering to `deliver`; `false` when the stack serves no such family
    /// or refused any step, which closes what was opened.
    fn listen(family: NetAddrFamily, group: IpAddr, deliver: u64) -> bool {
        let Ok(socket) = tairix_rt::net::socket(family, deliver) else {
            return false;
        };
        let local = SocketAddr {
            family,
            addr: [0u8; 16],
            port: PORT,
        };
        let (group_family, group_addr) = address_parts(group);
        let joined = tairix_rt::net::bind(socket, local).is_ok()
            && tairix_rt::net::join_multicast(
                socket,
                SocketAddr {
                    family: group_family,
                    addr: group_addr,
                    port: 0,
                },
            )
            .is_ok();
        if !joined {
            let _ = tairix_rt::net::close(socket);
        }
        joined
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
        let ipv4 = listen(NetAddrFamily::V4, IpAddr::V4(GROUP_V4), deliver);
        let ipv6 = listen(NetAddrFamily::V6, IpAddr::V6(GROUP_V6), deliver);
        if !ipv4 && !ipv6 {
            return unavailable("no multicast DNS socket could be opened");
        }
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return unavailable("the reactor wait-set could not be created");
        };
        let Ok(mut front) = Front::new(RtSessionLauncher::own_binary(), LOG_SINK, RtEntropy) else {
            return unavailable("the relay buffer could not be committed");
        };
        let Some(mut scratch) = fallible::filled(SocketDatagram::MAX_WIRE_LEN, 0u8) else {
            return unavailable("the receive buffer could not be committed");
        };
        tairix_log::log(
            &LOG_SINK,
            &Event {
                level: Level::Info,
                id: SERVICE_STARTED,
                message: "discoveryd: joined the multicast DNS groups",
                fields: &[
                    Field {
                        key: "ipv4",
                        value: FieldValue::Bool(ipv4),
                    },
                    Field {
                        key: "ipv6",
                        value: FieldValue::Bool(ipv6),
                    },
                ],
            },
        );

        let mut members = SessionMembers::new(set, TOKEN_READ, TOKEN_WRITE);
        let mut port_armed = false;
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
            let draining = front.wants_datagrams();
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
                        // decoder's pipes or its timer.
                        for _ in 0..DELIVER_CAPACITY {
                            if !front.wants_datagrams() {
                                break;
                            }
                            let Ok(datagram) = tairix_rt::net::recv(deliver, &mut scratch) else {
                                break;
                            };
                            front.on_datagram(now, &datagram);
                        }
                    }
                    TOKEN_READ => front.on_decoder_readable(now),
                    TOKEN_WRITE => front.on_decoder_writable(now),
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
