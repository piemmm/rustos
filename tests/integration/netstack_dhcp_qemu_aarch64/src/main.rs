//! `plans/DHCP.md` D3 QEMU integration test: boot the production aarch64
//! `tairix-kernel` pipeline on the `virt` board against the shared
//! `dhcp-net-root` whole-disk image — whose read-only `/System` volume
//! carries the **kernel-signed virtio-net driver bundle** and a planted
//! `/System/Settings/Network/network.conf` — with a `virtio-net-device`
//! attached and the harness-side **DHCP-server** link peer on the QEMU
//! `dgram` netdev, and prove RFC 2131 dynamic IPv4 addressing end to end.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical proves the declarative `match.node`
//!   binding with a *static* IPv6 address. This vertical proves the
//!   **dynamic** path: the planted `network.conf` binds the NIC to the `wan`
//!   alias by its stable bus location (`<iface>.match.node`) but selects
//!   `ipv4.method dhcp` and disables IPv6, so the interface forms **no**
//!   address on its own — `netstack` must drive its DHCP client to *lease*
//!   one from the host DHCP-server peer.
//! * The host peer runs a minimal DHCP server (OFFER on DISCOVER, ACK on
//!   REQUEST) leasing `DHCP_LEASED_V4`, then pings the guest at that leased
//!   address. If the DHCP exchange failed the guest has no IPv4 at all and
//!   the campaign goes unanswered, so the run times out fail-loud rather than
//!   passing on an address the guest formed itself — a real discriminator,
//!   not a tautology.
//!
//! The production boot path (all **before** any root unlock — the `/System`
//! store and its `Settings/` config are on the read-only volume mounted
//! before the passphrase, so the guest needs no console dialogue, exactly
//! like the static/headless siblings):
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-MMIO enumeration), each carrying its register
//!    window, DMA constraint, and GICv2 interrupt line as capability-grant
//!    requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device
//!    up, claims its reserved device-channel endpoint, and publishes a
//!    `netchan` hardware-tree node.
//! 3. The long-running user-space **`devmgr`** service observes the `netchan`
//!    node, calls **`netstack`** `BindDriver`, *and* reads the planted
//!    `network.conf`, delivering the `wan` interface's DHCPv4 configuration.
//! 4. **`netstack`** binds the `wan` alias to the NIC, starts its DHCP
//!    client, broadcasts DISCOVER, accepts the peer's OFFER, REQUESTs it,
//!    applies the peer's ACK (leasing the interface its only address), and
//!    answers the host peer's echo campaign to that leased address.
//!
//! ## Why PASS keys on three witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a
//! userland `log_emit` record the kernel routes to the log sink):
//!
//! 1. `devmgr`'s `NETSTACK_BOUND` — the `netchan` node was handed to the
//!    stack over the capability-gated admin surface.
//! 2. `netstack`'s `DHCP_LEASE_ACQUIRED` — the DHCP client completed the
//!    exchange and applied the leased address to the interface.
//! 3. `netstack`'s `INBOUND_ECHO_SERVED` — an echo request addressed to the
//!    interface's *leased* address was answered, so a frame crossed the
//!    two-process boundary end to end at the DHCP-configured address.
//!
//! Witness 3 can only fire after 1 and 2 (and the driver's own `netchan`
//! readiness), so the three together prove the whole chain; it gates exit so
//! the guest stays alive until a frame has actually been answered, avoiding a
//! race with the host peer's verdict. The harness additionally requires the
//! peer thread's own DHCP-server + leased-address echo campaign to have
//! completed, so neither side can pass alone.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only swaps in a
//! log-sink observer. Splitting the observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the free-list allocator hands out disjoint slices
    /// via an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// The kernel **log** sink: it replays every record through
    /// [`SERIAL_SINK`] and reports PASS to QEMU once all three witnesses have
    /// appeared. All three are *userland* `log_emit` records (from the
    /// `devmgr` and `netstack` services), which the kernel routes to the log
    /// sink — not the audit sink — so this observer is installed there:
    /// `devmgr`'s `NETSTACK_BOUND` (the `netchan` node was handed to the
    /// stack), the stack's `DHCP_LEASE_ACQUIRED` (the DHCP client leased and
    /// applied an address), and the stack's `INBOUND_ECHO_SERVED` (an inbound
    /// echo request addressed to the leased address crossed the two-process
    /// boundary and was answered). The guest exits only after the last, so the
    /// host peer's verdict never races an early teardown.
    struct NetstackDhcpSink {
        netstack_bound: AtomicBool,
        dhcp_lease_acquired: AtomicBool,
        echo_served: AtomicBool,
    }

    impl NetstackDhcpSink {
        const fn new() -> Self {
            Self {
                netstack_bound: AtomicBool::new(false),
                dhcp_lease_acquired: AtomicBool::new(false),
                echo_served: AtomicBool::new(false),
            }
        }
    }

    impl Sink for NetstackDhcpSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + bind + DHCP lease + echo timeline for a
            // failing run.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_devmgr::events::NETSTACK_BOUND.0 {
                self.netstack_bound.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::DHCP_LEASE_ACQUIRED.0 {
                self.dhcp_lease_acquired.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::INBOUND_ECHO_SERVED.0 {
                self.echo_served.store(true, Ordering::Release);
            } else {
                return;
            }
            if self.netstack_bound.load(Ordering::Acquire)
                && self.dhcp_lease_acquired.load(Ordering::Acquire)
                && self.echo_served.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: NetstackDhcpSink = NetstackDhcpSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_dhcp_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. The witness
    /// observer is installed as the **log** sink (the three witnesses are
    /// userland `log_emit` records the kernel routes there), and the plain
    /// [`SERIAL_SINK`] takes the audit stream so kernel audit records still
    /// reach the transcript. Boot at the default `Info` filter: the three
    /// witnesses are `Info` records.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
