//! `plans/NETWORK.md` N9b-3-2-β-2-ii-b QEMU integration test: boot the
//! production x86_64 `tairix-kernel` pipeline against the shared
//! `static-net-root` whole-disk image — whose always-readable `/System`
//! volume carries the **kernel-signed virtio-net driver bundle**
//! (cross-compiled for x86_64) and a planted
//! `/System/Settings/Network/network.conf` — with a `virtio-net-pci` device
//! attached on the PCI bus and the harness-side **static-addressing** link
//! peer on the QEMU `dgram` netdev, and prove the `<iface>.match.node`
//! binding **and** static addressing end to end over the x86_64 virtio-**PCI**
//! + MSI-X device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The two-process autoload vertical (`netstack_autoload_qemu_x86_64`)
//!   proves the NIC autoloads over virtio-PCI and the stack answers over the
//!   interface's *auto-configured EUI-64 link-local*. This vertical proves
//!   the **declarative** path on top of it: `devmgr` reads the planted
//!   `network.conf`, binds the NIC to the admin alias `wan` by its stable
//!   **bus location** (`<iface>.match.node`, resolved from the matched
//!   hardware-tree node's lowest config-window BAR base — never MAC or
//!   discovery order), and `netstack` assigns the config's **static IPv6
//!   address**.
//! * `netstack_static_qemu_aarch64` proves the same declarative path over the
//!   virtio-**MMIO** bus, where the bus location is the device's mmio register
//!   base. This is its x86_64 virtio-PCI sibling: the only difference is the
//!   `match.node` value the planted config names (the BAR base the kernel's
//!   PCI enumerator assigns).
//! * The host peer therefore addresses the guest by its *static* address
//!   (`GUEST_STATIC_V6`), never the link-local the guest also forms. A
//!   `match.node` mis-bind (the alias never applied, the static address never
//!   assigned) leaves the peer's campaign unanswered, so the run times out
//!   fail-loud rather than passing on the link-local — a real discriminator,
//!   not a tautology.
//!
//! The production boot path (the `/System` store binds independently of the
//! encrypted-root passphrase, so the config is read pre-unlock):
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
//! 3. The long-running user-space **`devmgr`** observes the `netchan` node,
//!    calls **`netstack`** `BindDriver` (recording the NIC's bus location),
//!    *and* reads the planted `network.conf`, delivering the `wan` interface's
//!    declarative configuration.
//! 4. **`netstack`** binds the `wan` alias to the NIC whose bus location
//!    matches the config's `match.node`, assigns the static IPv6 address, and
//!    answers the host peer's campaign to that static address.
//!
//! ## Why PASS keys on three witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a
//! userland `log_emit` record the kernel routes to the log sink):
//!
//! 1. `devmgr`'s `NETSTACK_BOUND` — the `netchan` node was handed to the
//!    stack over the capability-gated admin surface.
//! 2. `netstack`'s `INTERFACE_CONFIG_APPLIED` — the planted per-interface
//!    `network.conf` (the `match.node` binding + static address) was applied.
//! 3. `netstack`'s `INBOUND_ECHO_SERVED` — an echo request addressed to the
//!    interface's static address was answered, so a frame crossed the
//!    two-process boundary over virtio-PCI at the statically-configured
//!    address.
//!
//! Witness 3 can only fire after 1 and 2 (and the driver's own `netchan`
//! readiness), so the three together prove the whole chain; it gates exit so
//! the guest stays alive until a frame has actually been answered, avoiding a
//! race with the host peer's verdict. The harness additionally requires the
//! peer thread's own static-address echo campaign to have completed, so
//! neither side can pass alone.
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
    /// stack), the stack's `INTERFACE_CONFIG_APPLIED` (the planted
    /// `network.conf`'s `match.node` binding + static address was applied),
    /// and the stack's `INBOUND_ECHO_SERVED` (an inbound echo request
    /// addressed to the static address crossed the two-process boundary and
    /// was answered). The guest exits only after the last, so the host peer's
    /// verdict never races an early teardown.
    struct NetstackStaticSink {
        netstack_bound: AtomicBool,
        interface_config_applied: AtomicBool,
        echo_served: AtomicBool,
    }

    impl NetstackStaticSink {
        const fn new() -> Self {
            Self {
                netstack_bound: AtomicBool::new(false),
                interface_config_applied: AtomicBool::new(false),
                echo_served: AtomicBool::new(false),
            }
        }
    }

    impl Sink for NetstackStaticSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + bind + config + echo timeline for a
            // failing run.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_devmgr::events::NETSTACK_BOUND.0 {
                self.netstack_bound.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::INTERFACE_CONFIG_APPLIED.0 {
                self.interface_config_applied.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::INBOUND_ECHO_SERVED.0 {
                self.echo_served.store(true, Ordering::Release);
            } else {
                return;
            }
            if self.netstack_bound.load(Ordering::Acquire)
                && self.interface_config_applied.load(Ordering::Acquire)
                && self.echo_served.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: NetstackStaticSink = NetstackStaticSink::new();

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `WITNESS_SINK`, so a panic
    /// before PASS does not trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_netstack_static_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
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
