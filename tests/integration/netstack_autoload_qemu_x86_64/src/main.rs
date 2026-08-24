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
//! * The full **two-process** network path: the driver runs in its own user
//!   process, the stack in another, and they speak the `netchan-v1`
//!   device-channel contract across the boundary — the frame provably crosses
//!   a real process boundary, unlike a single-process in-kernel engine test.
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
//! ## How the run completes — harness-driven, race-free
//!
//! The guest does **not** self-terminate. It boots the production pipeline and
//! keeps serving the host peer's link-local echo campaign; the harness ends the
//! run the instant the peer's out-of-guest observer confirms success — it
//! received the guest's echo reply. That confirmation is the *last* link in the
//! causal chain (driver autoloaded and bound, `netstack` bound and the
//! interface up, an inbound echo served and its reply transmitted back over
//! virtio-PCI), so a guest that instead self-exited on an intermediate witness
//! would tear the machine down before the reply left it and lose the race — the
//! defect this choreography removes. The three witness records (`devmgr`'s
//! `NETSTACK_BOUND`, `netstack`'s `DRIVER_BOUND` and `INBOUND_ECHO_SERVED`)
//! still reach the serial transcript for diagnosis, and the peer's echo verdict
//! subsumes them: it cannot be met unless all three occurred. A run that never
//! earns the peer's confirmation fails loud on the runner's inactivity/absolute
//! deadline.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production x86_64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin the harness drives to
//! completion through the peer's success gate — there is no in-kernel QEMU-exit
//! shortcut to leak into a production build (fail closed; the harness never
//! decides what the kernel does next).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{boot, handle_panic_via_kernel_core, FreeListAllocator, SERIAL_SINK};

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

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// A panic halts the guest; it never self-exits, so the run times out and
    /// the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_netstack_autoload_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with [`SERIAL_SINK`] taking both the log and the
    /// audit streams, so every boot/autoload/bind/echo record reaches the QEMU
    /// transcript for diagnosis. The guest does not self-exit: the harness ends
    /// the run when the host peer confirms the echo round-trip (its success
    /// gate), so teardown can never precede that confirmation.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
