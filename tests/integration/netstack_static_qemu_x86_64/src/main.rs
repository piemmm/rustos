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
//! ## How the run completes — harness-driven, race-free
//!
//! The guest does **not** self-terminate. It boots the production pipeline and
//! keeps serving the host peer's static-address echo campaign; the harness
//! ends the run the instant the peer's out-of-guest observer confirms
//! success — it received the guest's echo reply at the static address. That
//! confirmation is the *last* link in the causal chain (driver autoloaded and
//! bound, the declarative config applied, an inbound echo served and its
//! reply transmitted back over virtio-PCI), so a guest that instead
//! self-exited on an intermediate witness would tear the machine down before
//! the reply left it and lose the race — the defect this choreography
//! removes. The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
//! `INTERFACE_CONFIG_APPLIED` and `INBOUND_ECHO_SERVED`) still reach the
//! serial transcript for diagnosis, and the peer's own static-address echo
//! campaign verdict subsumes them: it cannot be met unless the config was
//! applied and the reply arrived at the static address, never the link-local.
//! A run that never earns the peer's confirmation fails loud on the runner's
//! inactivity/absolute deadline.
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
    fn tairix_netstack_static_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with [`SERIAL_SINK`] taking both the log and the
    /// audit streams, so every boot/autoload/bind/config/echo record reaches
    /// the QEMU transcript for diagnosis. The guest does not self-exit: the
    /// harness ends the run when the host peer confirms the echo round-trip
    /// at the static address (its success gate), so teardown can never
    /// precede that confirmation.
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

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
