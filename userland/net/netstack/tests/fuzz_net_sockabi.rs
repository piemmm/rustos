//! Deterministic fuzz harness for the socket-service serve path
//! (`plans/NETWORK.md` N4b): the decode + capability-check + dispatch
//! pipeline of [`tairix_netstack::SocketService::serve`].
//!
//! Invariants, for any request bits a local caller crafts and any
//! capability set it presents:
//!
//! 1. Serving never panics, on any bytes, with or without `CAP_NET`.
//! 2. A caller without `CAP_NET` is always refused `PermissionDenied`
//!    before any state is created — no socket is ever opened for it.
//! 3. The socket table never exceeds its global bound.
//! 4. Whatever the links do and however full each socket's port is, a member
//!    socket is told each link's edges alternately, starting with the link
//!    coming up, holds exactly the published links once its port drains, and
//!    nothing a principal held survives its reclaim.
//!
//! Runs a fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, McastFilter, NetOffloads};
use tairix_abi::net::{
    decode_socket_reply, SocketAddr, SocketLinkEvent, SocketRequest, SocketType,
};
use tairix_abi::net_ipc::{NetAddrFamily, NetIfKind, IF_NAME_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::Errno;
use tairix_abi::{
    CapabilityId, CapabilitySummary, Duration64, Origin, ProcId, TrustDomain, ORIGIN_CONSOLE_NONE,
};
use tairix_fuzzseed::Prng;
use tairix_log::{Event, Sink};
use tairix_net::iface::TempAddrSource;
use tairix_netstack::{Caller, Netstack, SocketService};

/// A fixed temporary-address source: this harness exercises the socket
/// serve path, not privacy addresses, so the engine never consults it.
#[derive(Debug)]
struct FixedTempSource;

impl TempAddrSource for FixedTempSource {
    fn fill_random(&mut self, out: &mut [u8]) {
        out.fill(0xA5);
    }
}

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// A sink that drops every record — the harness asserts on behaviour, not
/// on audit output.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

fn facts() -> DeviceFacts {
    DeviceFacts {
        mac: MacAddress([0x02, 0xAA, 0, 0, 0, 1]),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    }
}

fn if_name() -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..3].copy_from_slice(b"wan");
    out
}

/// A caller whose attested origin holds `CAP_NET` iff `net`, keyed to the
/// process instance `proc_byte`.
fn caller(net: bool, proc_byte: u8) -> Caller {
    let mut summary = CapabilitySummary::EMPTY;
    if net {
        summary.insert(CapabilityId::NET);
    }
    Caller::new(Origin::new(
        TrustDomain::User,
        1000,
        100,
        u64::from(proc_byte),
        ProcId::from_raw([proc_byte; 16]),
        summary,
        ORIGIN_CONSOLE_NONE,
    ))
}

fn routed_stack() -> Netstack {
    let mut stack = Netstack::new(
        Box::new(|| Box::new(FixedTempSource) as Box<dyn TempAddrSource>),
        Box::new(|| Box::new(|| 0u32) as Box<dyn FnMut() -> u32>),
        tairix_hash::HashSeed::from_words(0xF10E_5EED_0000_0001, 0xF10E_5EED_0000_0002),
    );
    let now = Duration64::from_secs(0);
    stack
        .add_interface(if_name(), NetIfKind::Ethernet, facts(), [0; 8], 7, 0, now)
        .expect("add interface");
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[10, 0, 2, 15]);
    stack
        .addr_add(
            if_name(),
            NetAddrFamily::V4,
            24,
            addr,
            Duration64::from_secs(1),
        )
        .expect("addr add");
    stack
}

#[test]
fn serve_never_panics_and_gates_on_cap_net() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "serve_never_panics_and_gates_on_cap_net",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut svc = SocketService::new(tairix_hash::HashSeed::from_words(
        0xF00D_5EED_0000_0001,
        0xF00D_5EED_0000_0002,
    ));
    let mut stack = routed_stack();
    let sink = NullSink;
    let mut entropy_stream = Prng::new(0x1234_5678);
    let mut entropy = || entropy_stream.next_u32();
    let mut request = [0u8; 256];
    let mut reply = [0u8; 64];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let now = Duration64::from_secs(2);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let span = request.len() as u64 + 1;
            let size = usize::try_from(rng.next_u64() % span).unwrap_or(0);
            rng.fill(&mut request[..size]);
            // A caller with CAP_NET half the time; distinct principals so
            // the table exercises per-principal accounting.
            let net = rng.next_u64() & 1 == 0;
            let proc_byte = rng.next_u8();
            let who = caller(net, proc_byte);
            let before = svc.len();
            let result = svc.serve(
                &mut stack,
                &who,
                &sink,
                &mut entropy,
                &request[..size],
                &mut reply,
                now,
            );
            // Without CAP_NET nothing is ever created: a decodable
            // request is refused `PermissionDenied` (the capability check
            // runs after the frame decodes), and an undecodable one is
            // refused with its decode error — never `Ok`, never a socket.
            if !net {
                assert!(result.is_err(), "a capless caller is always refused");
                assert_eq!(svc.len(), before, "no socket created without CAP_NET");
            }
            // The delivered budget is never exceeded, whatever the request
            // stream asked for. Bytes, not sockets: the same table is a few
            // kilobytes idle and megabytes fully buffered, and it is the
            // memory the stack must not overrun.
            assert!(
                svc.committed_bytes() <= stack.settings().socket_budget_bytes,
                "committed {} against a budget of {}",
                svc.committed_bytes(),
                stack.settings().socket_budget_bytes
            );
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

/// Serve one encoded request, returning its reply frame.
fn serve(
    svc: &mut SocketService,
    stack: &mut Netstack,
    who: &Caller,
    request: &SocketRequest<'_>,
) -> Vec<u8> {
    let mut bytes = [0u8; 256];
    let Ok(len) = request.encode(&mut bytes) else {
        return Vec::new();
    };
    let mut reply = [0u8; 64];
    let mut entropy = || 7u32;
    let served = svc.serve(
        stack,
        who,
        &NullSink,
        &mut entropy,
        &bytes[..len],
        &mut reply,
        Duration64::from_secs(2),
    );
    served.map_or_else(|_| Vec::new(), |out| reply[..out.len].to_vec())
}

/// What each member socket was last told about each link.
type Beliefs = Vec<((u32, [u8; IF_NAME_LEN]), bool)>;

/// Set the link, publish it, and tell the members with room for `room`
/// events, checking every event against what its socket last heard.
fn flip(
    stack: &mut Netstack,
    svc: &mut SocketService,
    believed: &mut Beliefs,
    link: LinkState,
    room: usize,
    now: Duration64,
) {
    stack.on_member_link_change(if_name(), link, now);
    stack.publish_links();
    let links = stack.links().to_vec();
    let mut landed = 0;
    svc.tell_links(&links, stack.link_epoch(), &mut |_, frame| {
        if landed >= room {
            return Err(Errno::WouldBlock);
        }
        landed += 1;
        let event = SocketLinkEvent::parse(frame).expect("a link event");
        let key = (event.socket, event.interface);
        if let Some((_, up)) = believed.iter_mut().find(|(held, _)| *held == key) {
            assert_ne!(*up, event.up, "a link's edges alternate");
            *up = event.up;
        } else {
            assert!(event.up, "a socket first hears a link come up");
            believed.push((key, true));
        }
        Ok(())
    });
}

#[test]
fn link_events_and_reclaims_never_lose_or_leak_state() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "link_events_and_reclaims_never_lose_or_leak_state",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS / 200 {
            let mut svc = SocketService::new(tairix_hash::HashSeed::from_words(1, 2));
            let mut stack = routed_stack();
            let mut believed = Beliefs::new();
            let mut members = 0;
            let owners: Vec<u8> = (1..=4).collect();
            for &owner in &owners {
                let who = caller(true, owner);
                let Ok(socket) = decode_socket_reply(&serve(
                    &mut svc,
                    &mut stack,
                    &who,
                    &SocketRequest::Socket {
                        family: NetAddrFamily::V4,
                        sock_type: SocketType::Datagram,
                        deliver_port: u64::from(owner),
                    },
                )) else {
                    continue;
                };
                let mut group = [0u8; 16];
                group[..4].copy_from_slice(&[239, 1, 1, rng.next_u8()]);
                let joined = decode_status_reply(&serve(
                    &mut svc,
                    &mut stack,
                    &who,
                    &SocketRequest::JoinMulticast {
                        socket,
                        group: SocketAddr {
                            family: NetAddrFamily::V4,
                            addr: group,
                            port: 0,
                        },
                    },
                ));
                members += usize::from(joined.is_ok());
            }
            for step in 0..32 {
                let link = if rng.below(2) == 0 {
                    LinkState::Up
                } else {
                    LinkState::Down
                };
                let now = Duration64::from_secs(3 + step);
                flip(&mut stack, &mut svc, &mut believed, link, rng.below(3), now);
            }
            for (link, up) in [(LinkState::Up, true), (LinkState::Down, false)] {
                let now = Duration64::from_secs(36);
                flip(&mut stack, &mut svc, &mut believed, link, usize::MAX, now);
                assert_eq!(believed.len(), members, "every member heard its link");
                assert!(
                    believed.iter().all(|(_, held)| *held == up),
                    "a drained member holds the published link"
                );
            }
            for &owner in &owners {
                let _ = svc.reclaim_owner(
                    &mut stack,
                    ProcId::from_raw([owner; 16]),
                    Duration64::from_secs(40),
                );
            }
            assert_eq!(svc.len(), 0, "nothing survives its owner's reclaim");
            let mut macs = Vec::new();
            stack
                .interface(if_name())
                .expect("the interface")
                .stack()
                .multicast_macs(&mut macs);
            assert!(
                macs.iter()
                    .all(|mac| mac.as_octets()[..3] != [0x01, 0x00, 0x5E]
                        || mac.as_octets() == &[0x01, 0x00, 0x5E, 0, 0, 1]),
                "no group a reclaimed socket held is still joined: {macs:?}"
            );
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
