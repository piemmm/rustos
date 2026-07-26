//! Regression guard for the engine's allocation-free data-plane hot
//! path (`plans/NETWORK.md` N7c-3).
//!
//! A network stack that heap-allocates per packet on its receive and
//! transmit fast paths is a performance defect the charter forbids
//! (§2.16). The [`tairix_net::stack::Stack`] engine therefore reuses a
//! caller-owned [`tairix_net::stack::StackOutput`] scratch across every
//! call: it recycles the previous call's frame and payload byte buffers
//! into an internal pool before it fills the output again, so once the
//! pool and the output vectors are warm the steady-state data path
//! allocates **nothing**.
//!
//! This test proves that invariant with a counting global allocator: it
//! warms two back-to-back stacks (resolving ARP so transmits no longer
//! park), then drives a fixed number of UDP transmit + receive rounds
//! through reused outputs and asserts the allocation count over the
//! measured window is exactly zero. A regression — any per-packet
//! allocation reintroduced on the send or receive path — turns the count
//! non-zero and fails the build.
//!
//! Scope: the *data plane* (`send_datagram` / `send_tcp` transmit and
//! `on_frame` receive). Infrequent control-plane emissions (ARP/ND,
//! ICMP errors, IGMP/MLD reports) and the timer sweep [`Stack::advance`]
//! (which collects transient action vectors from sub-components) are not
//! part of this steady-state budget and are deliberately excluded from
//! the measured window; they run only during warm-up here.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::alloc::System;

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, NetOffloads};
use tairix_abi::time::Duration64;
use tairix_net::addr::{IpAddr, Ipv4Addr};
use tairix_net::iface::TempAddrSource;
use tairix_net::stack::{Stack, StackConfig, StackEvent, StackOutput};

/// A fixed [`TempAddrSource`]: this test keeps privacy addresses off, so
/// the engine never consults it; it exists only to satisfy the
/// constructor without allocating on the measured data path.
#[derive(Debug)]
struct FixedTempSource;

impl TempAddrSource for FixedTempSource {
    fn fill_random(&mut self, out: &mut [u8]) {
        out.fill(0xA5);
    }
}

/// A pass-through allocator that counts every allocation, reallocation,
/// and zeroed allocation while counting is armed, so the test can assert
/// that a measured window performs none.
struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

#[inline]
fn note_alloc() {
    if ARMED.load(Ordering::Relaxed) {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the
// only added behaviour is a relaxed atomic counter bump, which cannot
// affect memory safety.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_alloc();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

const MAC_A: MacAddress = MacAddress([0x02, 0xAA, 0, 0, 0, 0x01]);
const MAC_B: MacAddress = MacAddress([0x02, 0xBB, 0, 0, 0, 0x02]);
const V4_A: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const V4_B: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// Transmit + receive rounds in the measured (armed) window.
const ROUNDS: usize = 512;

fn facts(mac: MacAddress) -> DeviceFacts {
    DeviceFacts {
        mac,
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
    }
}

fn stack(mac: MacAddress, iid: u8, addr: Ipv4Addr) -> Stack {
    let config = StackConfig::new(facts(mac), [0, 0, 0, 0, 0, 0, 0, iid], 1);
    let mut s = Stack::new(&config, Box::new(FixedTempSource), Duration64::ZERO).expect("stack");
    s.set_ipv4_config(addr, 24, None).expect("addr");
    s
}

/// Copy one emitted frame into a fixed stack buffer (no heap) so it can
/// be fed to the peer without allocating.
fn ferry(from: &StackOutput, wire: &mut [u8; 2048]) -> Option<usize> {
    let frame = from.frames.first()?;
    let len = frame.bytes.len();
    wire[..len].copy_from_slice(&frame.bytes);
    Some(len)
}

#[test]
fn udp_data_plane_is_allocation_free_in_steady_state() {
    let mut a = stack(MAC_A, 0xA1, V4_A);
    let mut b = stack(MAC_B, 0xB2, V4_B);
    let mut out_a = StackOutput::default();
    let mut out_b = StackOutput::default();
    let mut wire = [0u8; 2048];
    let now = Duration64::from_secs(1);
    let payload = b"tairix-hotpath-probe";

    // Warm-up: resolve ARP (A parks the first datagram, ARP-resolves via
    // B, then drains it) and let every reused vector and the buffer pool
    // reach steady capacity. Several rounds guarantee the pool is stocked.
    for _ in 0..64 {
        a.send_datagram(IpAddr::V4(V4_B), 4000, 4001, payload, now, &mut out_a)
            .expect("send");
        // Ferry whatever A emitted (ARP request first, then the datagram)
        // to B, and B's replies (ARP reply) back to A, until quiescent.
        let mut carry = ferry(&out_a, &mut wire).map(|len| (len, true));
        for _ in 0..8 {
            let Some((len, to_b)) = carry.take() else {
                break;
            };
            if to_b {
                b.on_frame(&wire[..len], now, &mut out_b);
                carry = ferry(&out_b, &mut wire).map(|l| (l, false));
            } else {
                a.on_frame(&wire[..len], now, &mut out_a);
                carry = ferry(&out_a, &mut wire).map(|l| (l, true));
            }
        }
    }

    // A must now reach B without parking (steady state), and B must
    // deliver the datagram — otherwise the measurement below would be
    // measuring the wrong path.
    a.send_datagram(IpAddr::V4(V4_B), 4000, 4001, payload, now, &mut out_a)
        .expect("resolved send");
    assert_eq!(out_a.frames.len(), 1, "steady-state send emits one frame");
    let len = ferry(&out_a, &mut wire).expect("frame");
    b.on_frame(&wire[..len], now, &mut out_b);
    assert!(
        out_b
            .events
            .iter()
            .any(|e| matches!(e, StackEvent::UdpDatagram { .. })),
        "B delivers the datagram in steady state",
    );

    // Measured window: a fixed number of transmit + receive rounds with
    // the warm, reused outputs. Every buffer is recycled, so the count
    // must be exactly zero.
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    for _ in 0..ROUNDS {
        a.send_datagram(IpAddr::V4(V4_B), 4000, 4001, payload, now, &mut out_a)
            .expect("send");
        let len = out_a.frames[0].bytes.len();
        wire[..len].copy_from_slice(&out_a.frames[0].bytes);
        b.on_frame(&wire[..len], now, &mut out_b);
    }
    ARMED.store(false, Ordering::Relaxed);

    let allocations = ALLOC_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        allocations, 0,
        "the UDP data-plane hot path must allocate nothing in steady state; \
         saw {allocations} allocations over {ROUNDS} transmit+receive rounds",
    );
}
