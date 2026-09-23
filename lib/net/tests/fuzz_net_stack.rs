//! Deterministic fuzz harness for the dual-stack host engine's frame
//! entry point.
//!
//! Invariants, for any frame bytes a peer crafts:
//!
//! 1. [`Stack::on_frame`] and [`Stack::advance`] never panic.
//! 2. The interface address table never exceeds its bound, no matter
//!    what RAs the peer sends (the other state bounds are enforced
//!    inside the engine and exercised by its unit suite).
//! 3. Every emitted frame is itself a parseable Ethernet frame no
//!    larger than the link MTU plus its header.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing
//! from the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses
//! under `cargo xtask fuzz`.

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, McastFilter, NetOffloads};
use tairix_abi::time::Duration64;
use tairix_fuzzseed::Prng;
use tairix_net::eth::{EthernetFrame, ETHERNET_HEADER_LEN};
use tairix_net::iface::{TempAddrSource, MAX_IPV6_ADDRS};
use tairix_net::stack::{Stack, StackConfig, StackOutput};
use tairix_net::{IpAddr, Ipv4Addr};

/// A fixed key for the stack's neighbour-cache index, so a run's table layout
/// is reproducible.
const STACK_HASH_KEY: tairix_hash::HashSeed =
    tairix_hash::HashSeed::from_words(0x5354_4143_4B00_0001, 0x5354_4143_4B00_0002);

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 5_000;

const OUR_MAC: MacAddress = MacAddress([0x02, 0xAA, 0, 0, 0, 0x01]);
const PEER_MAC: MacAddress = MacAddress([0x02, 0xBB, 0, 0, 0, 0x02]);

/// Temporary-address randomness drawn from the harness generator.
#[derive(Debug)]
struct TempRandom(Prng);

impl TempAddrSource for TempRandom {
    fn fill_random(&mut self, out: &mut [u8]) {
        self.0.fill(out);
    }
}

fn fresh_stack() -> Stack {
    let facts = DeviceFacts {
        mac: OUR_MAC,
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let mut config = StackConfig::new(facts, [0, 0, 0, 0, 0, 0, 0, 0xA1], 0x4242, STACK_HASH_KEY);
    // Exercise the RFC 8981 privacy-address path against the hostile RAs
    // this harness crafts: temporary addresses must never breach the
    // address-table bound (invariant 2) no matter what the peer sends.
    config.iface.privacy = true;
    let mut stack = Stack::new(
        &config,
        Box::new(TempRandom(Prng::new(0xF00D_C0DE))),
        Duration64::from_secs(0),
    )
    .expect("valid facts");
    stack
        .set_ipv4_config(
            Ipv4Addr::new(10, 0, 2, 15),
            24,
            Some(Ipv4Addr::new(10, 0, 2, 2)),
        )
        .expect("configure v4");
    stack
}

#[test]
fn random_frames_never_panic_and_state_stays_bounded() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "random_frames_never_panic_and_state_stays_bounded",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        let mut stack = fresh_stack();
        let mut buf = [0u8; 512];
        let mut secs: i64 = 0;
        for iteration in 0..SMOKE_ITERATIONS {
            let size = rng.at_most(buf.len());
            rng.fill(&mut buf[..size]);
            if size >= ETHERNET_HEADER_LEN {
                // Bias toward frames the engine actually accepts:
                // mostly our MAC or a group MAC, and a real EtherType.
                match rng.next_u64() % 4 {
                    0 => {}
                    1 => buf[..6].copy_from_slice(OUR_MAC.as_octets()),
                    2 => buf[..6].copy_from_slice(&[0xFF; 6]),
                    _ => {
                        buf[..6].copy_from_slice(&[0x33, 0x33, 0, 0, 0, 1]);
                    }
                }
                buf[6..12].copy_from_slice(PEER_MAC.as_octets());
                match rng.next_u64() % 4 {
                    0 => buf[12..14].copy_from_slice(&0x0806u16.to_be_bytes()),
                    1 => buf[12..14].copy_from_slice(&0x0800u16.to_be_bytes()),
                    2 => buf[12..14].copy_from_slice(&0x86DDu16.to_be_bytes()),
                    _ => {}
                }
            }
            let now = Duration64::from_secs(secs);
            let mut out = StackOutput::default();
            stack.on_frame(&buf[..size], now, &mut out);
            for frame in &out.frames {
                assert!(frame.bytes.len() <= ETHERNET_HEADER_LEN + 1500);
                assert!(EthernetFrame::parse(&frame.bytes).is_some());
            }
            // Occasionally step time and run the timers; sometimes
            // originate an echo so the pending queue is exercised.
            if iteration % 64 == 0 {
                secs += 1;
                let now = Duration64::from_secs(secs);
                stack.advance(now, &mut out);
                let dest = IpAddr::V4(Ipv4Addr::new(10, 0, 2, rng.next_u8()));
                let _ = stack.send_echo_request(dest, 1, 1, b"fuzz", now, &mut out);
            }
            assert!(stack.counters().rx_frames > 0);
            assert!(stack.iface().ipv6_addresses().len() <= MAX_IPV6_ADDRS);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
