//! `plans/NETWORK.md` N4e / `plans/ARCHSUPPORT.md` A4 QEMU integration test:
//! boot the production x86_64 `tairix-kernel` pipeline against the shared
//! whole-disk autoload-root image — whose always-readable `/System` volume
//! carries the **kernel-signed virtio-net driver bundle** in its `Drivers/`
//! store (cross-compiled for x86_64) alongside the input and display bundles
//! — with a `virtio-net-pci` device attached on the PCI bus and the
//! harness-side `netstack_peer` link peer on the QEMU `dgram` netdev, and
//! prove the full **two-process** network path end to end over the x86_64
//! virtio-**PCI** + MSI-X device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * `netstack_pci_x86_64` proves the pure `tairix-netstack` engine pumps a
//!   live virtio-net-PCI device — but in a *single* process, over the
//!   in-kernel `register` scaffold. This vertical is the two-process
//!   production-boot replacement: the driver runs in its own user process,
//!   the stack in another, and they speak the `netchan-v1` device-channel
//!   contract across the boundary.
//! * `netstack_autoload_qemu_aarch64` / `netstack_autoload_qemu_riscv64`
//!   prove the same two-process path over the virtio-**MMIO** bus. This is
//!   their x86_64 virtio-PCI sibling: discovery, match, spawn, and interrupt
//!   delivery all run over the PCI enumeration + kernel-routed MSI-X path.
//!
//! The production boot path:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-PCI enumeration). The NIC node carries its four
//!    role-tagged config windows, a coherent DMA constraint, and — the
//!    x86_64-specific step — the **routed MSI line** the kernel enumerator
//!    allocated and programmed into the function's MSI-X table entry 0, so a
//!    user-space driver never touches PCI config or the MSI-X BAR.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device up
//!    over `PciTransport` (`enable_msix(0)`), claims its reserved
//!    device-channel endpoint under `CAP_IPC_BIND_PRIVILEGED`, and publishes a
//!    `netchan` hardware-tree node. The `/System` store binds independently of
//!    the encrypted-root passphrase, so the network driver autoloads
//!    regardless of the interactive unlock.
//! 3. The long-running user-space **`devmgr`** observes the `netchan` node and
//!    calls **`netstack`** `BindDriver` under `CAP_NET_ADMIN`.
//! 4. **`netstack`** provisions the shared frame region, attaches the driver
//!    channel, and auto-configures the interface's EUI-64 IPv6 link-local
//!    address (no IPv4), then answers the host peer's link-local echo campaign.
//!
//! ## Why PASS keys on three witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a userland
//! `log_emit` record the kernel routes to the log sink): `devmgr`'s
//! `NETSTACK_BOUND` (the `netchan` node was handed to the stack), `netstack`'s
//! `DRIVER_BOUND` (the channel was provisioned and the interface brought up),
//! and `netstack`'s `INBOUND_ECHO_SERVED` (an inbound echo request crossed the
//! two-process boundary over virtio-PCI and was answered). Witness 3 can only
//! fire after 1 and 2, so the three together prove the whole chain; it gates
//! exit so the guest stays alive until a frame has actually been answered. The
//! harness additionally requires the peer thread's own v6 link-local echo
//! verdict, so neither side can pass alone.
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
    /// stack), the stack's `DRIVER_BOUND` (the channel was provisioned and the
    /// interface brought up), and the stack's `INBOUND_ECHO_SERVED` (an
    /// inbound echo request crossed the two-process boundary and was
    /// answered). The guest exits only after the last, so the host peer's
    /// verdict never races an early teardown.
    struct NetstackAutoloadSink {
        netstack_bound: AtomicBool,
        driver_bound: AtomicBool,
        echo_served: AtomicBool,
    }

    impl NetstackAutoloadSink {
        const fn new() -> Self {
            Self {
                netstack_bound: AtomicBool::new(false),
                driver_bound: AtomicBool::new(false),
                echo_served: AtomicBool::new(false),
            }
        }
    }

    impl Sink for NetstackAutoloadSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + bind + echo timeline for a failing run.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_devmgr::events::NETSTACK_BOUND.0 {
                self.netstack_bound.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::DRIVER_BOUND.0 {
                self.driver_bound.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::INBOUND_ECHO_SERVED.0 {
                self.echo_served.store(true, Ordering::Release);
            } else {
                return;
            }
            if self.netstack_bound.load(Ordering::Acquire)
                && self.driver_bound.load(Ordering::Acquire)
                && self.echo_served.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: NetstackAutoloadSink = NetstackAutoloadSink::new();

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `WITNESS_SINK`, so a panic
    /// before PASS does not trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_netstack_autoload_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
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
