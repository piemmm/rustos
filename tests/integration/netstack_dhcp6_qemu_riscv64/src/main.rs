//! `plans/DHCP.md` D4c QEMU integration test: boot the production riscv64
//! (QEMU `virt` / SiFive) `tairix-kernel` pipeline against the shared
//! `dhcp6-net-root` whole-disk image — whose always-readable `/System` volume
//! carries the **kernel-signed virtio-net driver bundle** (cross-compiled for
//! riscv64) and a planted `/System/Settings/Network/network.conf` — with a
//! `virtio-net-device` attached on the `virtio,mmio` transport and the
//! harness-side **DHCPv6-server** link peer on the QEMU `dgram` netdev, and
//! prove RFC 8415 dynamic IPv6 addressing end to end over the riscv64
//! virtio-**MMIO** + PLIC device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical (`netstack_static_qemu_riscv64`) proves
//!   the declarative `match.node` binding with a *static* IPv6 address. This
//!   vertical proves the **dynamic IPv6** path on the same bus: the planted
//!   `network.conf` binds the NIC to the `wan` alias by its stable bus
//!   location (`<iface>.match.node` = the NIC's virtio-mmio transport slot
//!   base) but selects `ipv6.method dhcp` and disables IPv4, so the interface
//!   forms **no** global address on its own — `netstack` must drive its
//!   DHCPv6 client to *lease* one from the host DHCPv6-server peer.
//! * `netstack_dhcp6_qemu_aarch64` proves the same dynamic path over the
//!   aarch64 `virt` board's virtio-MMIO bus, and `netstack_dhcp6_qemu_x86_64`
//!   over the virtio-PCI bus. This is the riscv64 virtio-MMIO sibling: the
//!   only difference is the `match.node` value the planted config names (this
//!   board's virtio-mmio transport slot base).
//! * The host peer runs a minimal DHCPv6 server (Advertise on Solicit, Reply
//!   on Request) leasing `DHCP6_LEASED_V6`, and — because DHCPv6 conveys no
//!   on-link prefix — also emits Router Advertisements naming the shared `/64`
//!   on-link so the guest can route back, then pings the guest at that leased
//!   address. If the DHCPv6 exchange failed the guest has no global IPv6
//!   address at all and the campaign goes unanswered, so the run times out
//!   fail-loud rather than passing on an address the guest formed itself — a
//!   real discriminator, not a tautology.
//!
//! The production boot path (the `/System` store binds independently of the
//! encrypted-root passphrase, so the config is read pre-unlock — headless, no
//! console dialogue, exactly like the static/headless siblings):
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-MMIO enumeration). The NIC node carries its
//!    register window, DMA constraint, and discovered PLIC interrupt line as
//!    capability-grant requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device up
//!    over `MmioTransport`, claims its reserved device-channel endpoint, and
//!    publishes a `netchan` hardware-tree node.
//! 3. The long-running user-space **`devmgr`** service observes the `netchan`
//!    node, calls **`netstack`** `BindDriver`, *and* reads the planted
//!    `network.conf`, delivering the `wan` interface's DHCPv6 configuration.
//! 4. **`netstack`** binds the `wan` alias to the NIC, starts its DHCPv6
//!    client, Solicits, accepts the peer's Advertise, Requests it, applies the
//!    peer's Reply (leasing the interface its only global address), and — once
//!    the peer's RA has installed the on-link route — answers the host peer's
//!    echo campaign to that leased address.
//!
//! ## Why PASS keys on three witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a userland
//! `log_emit` record the kernel routes to the log sink):
//!
//! 1. `devmgr`'s `NETSTACK_BOUND` — the `netchan` node was handed to the stack
//!    over the capability-gated admin surface.
//! 2. `netstack`'s `DHCP6_LEASE_ACQUIRED` — the DHCPv6 client completed the
//!    exchange and applied the leased address to the interface.
//! 3. `netstack`'s `INBOUND_ECHO_SERVED` — an echo request addressed to the
//!    interface's *leased* address was answered, so a frame crossed the
//!    two-process boundary over virtio-MMIO at the DHCP-configured address.
//!
//! Witness 3 can only fire after 1 and 2 (and the driver's own `netchan`
//! readiness), so the three together prove the whole chain; it gates exit so
//! the guest stays alive until a frame has actually been answered, avoiding a
//! race with the host peer's verdict. The harness additionally requires the
//! peer thread's own DHCPv6-server + leased-address echo campaign to have
//! completed, so neither side can pass alone.
//!
//! ## Real firmware device tree
//!
//! QEMU's riscv64 `virt` OpenSBI firmware hands the boot hart a valid
//! device-tree pointer in `a1`, so — unlike the aarch64 `-kernel` path — this
//! vertical forwards the verbatim pointer to the boot pipeline, which
//! discovers the board (including the `virtio,mmio` transport slots the disk
//! and the NIC populate) from it exactly as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production riscv64 boot pipeline and only swaps in a
//! log-sink observer. Splitting the observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::riscv64::boot as boot_riscv64;
    use tairix_log::{Event, Sink};

    /// Static boot heap.
    ///
    /// Placed in the linker's dedicated `.heap` (NOLOAD) section so the boot
    /// trampoline does not zero its bytes (the bump allocator does not require
    /// zeroed backing) and the boot pipeline excludes it from the usable
    /// physical-memory map, exactly as the production riscv64 kernel binary's
    /// heap does. `static mut` because the bump allocator hands out disjoint
    /// slices via an atomic cursor; the storage is otherwise never aliased.
    #[link_section = ".heap"]
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
    /// stack), the stack's `DHCP6_LEASE_ACQUIRED` (the DHCPv6 client leased and
    /// applied an address), and the stack's `INBOUND_ECHO_SERVED` (an inbound
    /// echo request addressed to the leased address crossed the two-process
    /// boundary and was answered). The guest exits only after the last, so the
    /// host peer's verdict never races an early teardown.
    struct NetstackDhcp6Sink {
        netstack_bound: AtomicBool,
        dhcp6_lease_acquired: AtomicBool,
        echo_served: AtomicBool,
    }

    impl NetstackDhcp6Sink {
        const fn new() -> Self {
            Self {
                netstack_bound: AtomicBool::new(false),
                dhcp6_lease_acquired: AtomicBool::new(false),
                echo_served: AtomicBool::new(false),
            }
        }
    }

    impl Sink for NetstackDhcp6Sink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + bind + DHCPv6 lease + echo timeline for a
            // failing run.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_devmgr::events::NETSTACK_BOUND.0 {
                self.netstack_bound.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::DHCP6_LEASE_ACQUIRED.0 {
                self.dhcp6_lease_acquired.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::INBOUND_ECHO_SERVED.0 {
                self.echo_served.store(true, Ordering::Release);
            } else {
                return;
            }
            if self.netstack_bound.load(Ordering::Acquire)
                && self.dhcp6_lease_acquired.load(Ordering::Acquire)
                && self.echo_served.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: NetstackDhcp6Sink = NetstackDhcp6Sink::new();

    /// Forward to the shared riscv64 panic bridge. A panic before the PASS
    /// finisher parks the hart, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_dhcp6_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with the witness observer installed as the
    /// **log** sink (the three witnesses are userland `log_emit` records the
    /// kernel routes there), and the plain [`SERIAL_SINK`] taking the audit
    /// stream so kernel audit records still reach the transcript.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot_riscv64::boot(
            hartid,
            dtb,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
