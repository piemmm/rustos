//! `plans/DHCP.md` D4c QEMU integration test: boot the production x86_64
//! `tairix-kernel` pipeline against the shared `dhcp6-net-root` whole-disk
//! image — whose read-only `/System` volume carries the **kernel-signed
//! virtio-net driver bundle** (cross-compiled for x86_64) and a planted
//! `/System/Settings/Network/network.conf` — with a `virtio-net-pci` device
//! attached on the PCI bus and the harness-side **DHCPv6-server** link peer on
//! the QEMU `dgram` netdev, and prove RFC 8415 dynamic IPv6 addressing end to
//! end over the x86_64 virtio-**PCI** + MSI-X device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical (`netstack_static_qemu_x86_64`) proves the
//!   declarative `match.node` binding with a *static* IPv6 address. This
//!   vertical proves the **dynamic** IPv6 path on the same bus: the planted
//!   `network.conf` binds the NIC to the `wan` alias by its stable bus
//!   location (`<iface>.match.node` = the virtio-PCI config-window BAR base the
//!   kernel enumerator assigns) but selects `ipv6.method dhcp` and disables
//!   IPv4, so the interface forms **no** global address on its own — `netstack`
//!   must drive its DHCPv6 client to *lease* one from the host DHCPv6-server
//!   peer.
//! * `netstack_dhcp6_qemu_aarch64` proves the same dynamic path over the
//!   virtio-**MMIO** bus. This is its x86_64 virtio-PCI sibling: the only
//!   difference is the `match.node` value the planted config names (the BAR
//!   base the kernel's PCI enumerator assigns).
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
//!    `network.conf`, delivering the `wan` interface's DHCPv6 configuration.
//! 4. **`netstack`** binds the `wan` alias to the NIC, starts its DHCPv6
//!    client, Solicits, accepts the peer's Advertise, Requests it, applies the
//!    peer's Reply (leasing the interface its only global address), and — once
//!    the peer's RA has installed the on-link route — answers the host peer's
//!    echo campaign to that leased address.
//!
//! ## How the run completes — harness-driven, race-free
//!
//! The guest does **not** self-terminate. It boots the production pipeline and
//! keeps serving the host peer's leased-address echo campaign; the harness
//! ends the run the instant the peer's out-of-guest observer confirms
//! success — it received the guest's echo reply at the leased address. That
//! confirmation is the *last* link in the causal chain (driver autoloaded and
//! bound, the DHCPv6 lease acquired and applied, an inbound echo served and
//! its reply transmitted back over virtio-PCI), so a guest that instead
//! self-exited on an intermediate witness would tear the machine down before
//! the reply left it and lose the race — the defect this choreography
//! removes. The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
//! `DHCP6_LEASE_ACQUIRED` and `INBOUND_ECHO_SERVED`) still reach the serial
//! transcript for diagnosis, and the peer's own DHCPv6-server + leased-address
//! echo campaign verdict subsumes them: it cannot be met unless the lease was
//! granted and the reply arrived. A run that never earns the peer's
//! confirmation fails loud on the runner's inactivity/absolute deadline.
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
    fn tairix_netstack_dhcp6_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with [`SERIAL_SINK`] taking both the log and the
    /// audit streams, so every boot/autoload/bind/lease/echo record reaches
    /// the QEMU transcript for diagnosis. The guest does not self-exit: the
    /// harness ends the run when the host peer confirms the echo round-trip
    /// at the leased address (its success gate), so teardown can never
    /// precede that confirmation.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &ALLOCATOR,
            &SERIAL_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
