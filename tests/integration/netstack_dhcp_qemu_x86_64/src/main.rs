//! `plans/DHCP.md` D3 QEMU integration test: boot the production x86_64
//! `tairix-kernel` pipeline against the shared `dhcp-net-root` whole-disk
//! image — whose read-only `/System` volume carries the **kernel-signed
//! virtio-net driver bundle** (cross-compiled for x86_64) and a planted
//! `/System/Settings/Network/network.conf` — with a `virtio-net-pci` device
//! attached on the PCI bus and the harness-side **DHCP-server** link peer on
//! the QEMU `dgram` netdev, and prove RFC 2131 dynamic IPv4 addressing end to
//! end over the x86_64 virtio-**PCI** + MSI-X device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical (`netstack_static_qemu_x86_64`) proves the
//!   declarative `match.node` binding with a *static* IPv6 address. This
//!   vertical proves the **dynamic** path on the same bus: the planted
//!   `network.conf` binds the NIC to the `wan` alias by its stable bus
//!   location (`<iface>.match.node` = the virtio-PCI config-window BAR base the
//!   kernel enumerator assigns) but selects `ipv4.method dhcp` and disables
//!   IPv6, so the interface forms **no** address on its own — `netstack` must
//!   drive its DHCP client to *lease* one from the host DHCP-server peer.
//! * `netstack_dhcp_qemu_aarch64` proves the same dynamic path over the
//!   virtio-**MMIO** bus. This is its x86_64 virtio-PCI sibling: the only
//!   difference is the `match.node` value the planted config names (the BAR
//!   base the kernel's PCI enumerator assigns).
//! * The host peer runs a minimal DHCP server (OFFER on DISCOVER, ACK on
//!   REQUEST) leasing `DHCP_LEASED_V4`, then pings the guest at that leased
//!   address. If the DHCP exchange failed the guest has no IPv4 at all and the
//!   campaign goes unanswered, so the run times out fail-loud rather than
//!   passing on an address the guest formed itself — a real discriminator,
//!   not a tautology.
//!
//! The production boot path (the `/System` store binds independently of the
//! encrypted-root passphrase, so the config is read pre-unlock — headless, no
//! console dialogue, exactly like the static/headless siblings):
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-PCI enumeration). The NIC node carries its four
//!    role-tagged config windows, a coherent DMA constraint, and the routed
//!    MSI line the kernel enumerator allocated and programmed into the
//!    function's MSI-X table entry 0.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device up
//!    over `PciTransport` (`enable_msix(0)`), claims its reserved
//!    device-channel endpoint, and publishes a `netchan` hardware-tree node.
//! 3. The long-running user-space **`devmgr`** service observes the `netchan`
//!    node, calls **`netstack`** `BindDriver`, *and* reads the planted
//!    `network.conf`, delivering the `wan` interface's DHCPv4 configuration.
//! 4. **`netstack`** binds the `wan` alias to the NIC, starts its DHCP client,
//!    broadcasts DISCOVER, accepts the peer's OFFER, REQUESTs it, applies the
//!    peer's ACK (leasing the interface its only address), and answers the
//!    host peer's echo campaign to that leased address.
//!
//! ## Why PASS keys on three witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a userland
//! `log_emit` record the kernel routes to the log sink):
//!
//! 1. `devmgr`'s `NETSTACK_BOUND` — the `netchan` node was handed to the stack
//!    over the capability-gated admin surface.
//! 2. `netstack`'s `DHCP_LEASE_ACQUIRED` — the DHCP client completed the
//!    exchange and applied the leased address to the interface.
//! 3. `netstack`'s `INBOUND_ECHO_SERVED` — an echo request addressed to the
//!    interface's *leased* address was answered, so a frame crossed the
//!    two-process boundary over virtio-PCI at the DHCP-configured address.
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
//! It reuses the entire production x86_64 boot pipeline and only swaps in a
//! log-sink observer. Splitting the observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, Sink};

    /// Static heap for the bump allocator (identical to the production bin's
    /// declaration; `#[global_allocator]` is per-binary).
    ///
    /// `static mut` because the bump allocator hands out disjoint slices via
    /// an atomic cursor; the storage is otherwise never aliased.
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

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `WITNESS_SINK`, so a panic
    /// before PASS does not trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_netstack_dhcp_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with the witness observer as the **log** sink
    /// (the three witnesses are userland `log_emit` records the kernel routes
    /// there) and the plain [`SERIAL_SINK`] taking the audit stream so kernel
    /// audit records still reach the transcript.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
