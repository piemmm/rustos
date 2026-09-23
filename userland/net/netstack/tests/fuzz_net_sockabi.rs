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
//!
//! Runs a fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, McastFilter, NetOffloads};
use tairix_abi::net_ipc::{NetAddrFamily, NetIfKind, IF_NAME_LEN};
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
