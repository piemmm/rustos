//! `plans/DHCP.md` D4c QEMU integration test: boot the production aarch64
//! `tairix-kernel` pipeline on the `virt` board against the shared
//! `dhcp6-net-root` whole-disk image — whose read-only `/System` volume
//! carries the **kernel-signed virtio-net driver bundle** and a planted
//! `/System/Settings/Network/network.conf` — with a `virtio-net-device`
//! attached and the harness-side **DHCPv6-server** link peer on the QEMU
//! `dgram` netdev, and prove RFC 8415 dynamic IPv6 addressing end to end.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical proves the declarative `match.node`
//!   binding with a *static* IPv6 address. The DHCPv4 vertical proves the
//!   **dynamic** IPv4 path. This vertical proves the **dynamic IPv6** path:
//!   the planted `network.conf` binds the NIC to the `wan` alias by its stable
//!   bus location (`<iface>.match.node`) but selects `ipv6.method dhcp` and
//!   disables IPv4, so the interface forms **no** global address on its own —
//!   `netstack` must drive its DHCPv6 client to *lease* one from the host
//!   DHCPv6-server peer.
//! * The host peer runs a minimal DHCPv6 server (Advertise on Solicit, Reply
//!   on Request) leasing `DHCP6_LEASED_V6`. Because DHCPv6 conveys no on-link
//!   prefix (RFC 8415 leaves that to Router Advertisements), the peer also
//!   acts as the on-link router: it emits RAs naming the shared `/64` on-link
//!   (non-autonomous, so the guest forms no SLAAC address) and itself a
//!   default router, then pings the guest at the leased address. If the DHCPv6
//!   exchange failed the guest has no global address at all and the campaign
//!   goes unanswered, so the run times out fail-loud rather than passing on an
//!   address the guest formed itself — a real discriminator, not a tautology.
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
//! its reply transmitted back), so a guest that instead self-exited on an
//! intermediate witness would tear the machine down before the reply left it
//! and lose the race — the defect this choreography removes. The witness
//! records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s `DHCP6_LEASE_ACQUIRED`
//! and `INBOUND_ECHO_SERVED`) still reach the serial transcript for
//! diagnosis, and the peer's own DHCPv6-server + leased-address echo campaign
//! verdict subsumes them: it cannot be met unless the lease was granted and
//! the reply arrived. A run that never earns the peer's confirmation fails
//! loud on the runner's inactivity/absolute deadline.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin the harness drives to
//! completion through the peer's success gate — there is no in-kernel QEMU-exit
//! shortcut to leak into a production build (fail closed; the harness never
//! decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::{handle_panic_via_serial, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;

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

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU; the
    /// guest never self-exits, so the run times out and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_dhcp6_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. [`SERIAL_SINK`]
    /// takes both the log and the audit streams, so every boot/autoload/bind/
    /// lease/echo record reaches the QEMU transcript for diagnosis. The guest
    /// does not self-exit: the harness ends the run when the host peer
    /// confirms the echo round-trip at the leased address (its success gate),
    /// so teardown can never precede that confirmation. Boot at the default
    /// `Info` filter: the witness records are `Info` records.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
