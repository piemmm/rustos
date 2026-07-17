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

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, NetOffloads};
use tairix_abi::time::Duration64;
use tairix_net::eth::{EthernetFrame, ETHERNET_HEADER_LEN};
use tairix_net::iface::MAX_IPV6_ADDRS;
use tairix_net::stack::{Stack, StackConfig};
use tairix_net::{IpAddr, Ipv4Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 5_000;

const OUR_MAC: MacAddress = MacAddress([0x02, 0xAA, 0, 0, 0, 0x01]);
const PEER_MAC: MacAddress = MacAddress([0x02, 0xBB, 0, 0, 0, 0x02]);

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in the sibling harnesses so failures reproduce one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, buf.len() - i);
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

fn fresh_stack() -> Stack {
    let facts = DeviceFacts {
        mac: OUR_MAC,
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, [0, 0, 0, 0, 0, 0, 0, 0xA1], 0x4242),
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
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "random_frames_never_panic_and_state_stays_bounded",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        let mut stack = fresh_stack();
        let mut buf = [0u8; 512];
        let mut secs: i64 = 0;
        for iteration in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x3FF) as usize) % (buf.len() + 1);
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
            let out = stack.on_frame(&buf[..size], now);
            for frame in &out.frames {
                assert!(frame.len() <= ETHERNET_HEADER_LEN + 1500);
                assert!(EthernetFrame::parse(frame).is_some());
            }
            // Occasionally step time and run the timers; sometimes
            // originate an echo so the pending queue is exercised.
            if iteration % 64 == 0 {
                secs += 1;
                let now = Duration64::from_secs(secs);
                let _ = stack.advance(now);
                let dest = IpAddr::V4(Ipv4Addr::new(10, 0, 2, (rng.next_u64() & 0xFF) as u8));
                let _ = stack.send_echo_request(dest, 1, 1, b"fuzz", now);
            }
            assert!(stack.counters().rx_frames > 0);
            assert!(stack.iface().ipv6_addresses().len() <= MAX_IPV6_ADDRS);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
