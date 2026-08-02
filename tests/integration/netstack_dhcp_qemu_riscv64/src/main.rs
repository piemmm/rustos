//! `plans/DHCP.md` D3 QEMU integration test: boot the production riscv64
//! (QEMU `virt` / SiFive) `tairix-kernel` pipeline against the shared
//! `dhcp-net-root` whole-disk image — whose always-readable `/System` volume
//! carries the **kernel-signed virtio-net driver bundle** (cross-compiled for
//! riscv64) and a planted `/System/Settings/Network/network.conf` — with a
//! `virtio-net-device` attached on the `virtio,mmio` transport and the
//! harness-side **DHCP-server** link peer on the QEMU `dgram` netdev, and
//! prove RFC 2131 dynamic IPv4 addressing end to end over the riscv64
//! virtio-**MMIO** + PLIC device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical (`netstack_static_qemu_riscv64`) proves
//!   the declarative `match.node` binding with a *static* IPv6 address. This
//!   vertical proves the **dynamic** path on the same bus: the planted
//!   `network.conf` binds the NIC to the `wan` alias by its stable bus
//!   location (`<iface>.match.node` = the NIC's virtio-mmio transport slot
//!   base) but selects `ipv4.method dhcp` and disables IPv6, so the interface
//!   forms **no** address on its own — `netstack` must drive its DHCP client
//!   to *lease* one from the host DHCP-server peer.
//! * `netstack_dhcp_qemu_aarch64` proves the same dynamic path over the
//!   aarch64 `virt` board's virtio-MMIO bus, and `netstack_dhcp_qemu_x86_64`
//!   over the virtio-PCI bus. This is the riscv64 virtio-MMIO sibling: the
//!   only difference is the `match.node` value the planted config names (this
//!   board's virtio-mmio transport slot base).
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
//!    (bootstrap-floor virtio-MMIO enumeration). The NIC node carries its
//!    register window, DMA constraint, and discovered PLIC interrupt line as
//!    capability-grant requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device up
//!    over `MmioTransport`, claims its reserved device-channel endpoint, and
//!    publishes a `netchan` hardware-tree node.
//! 3. The long-running user-space **`devmgr`** service observes the `netchan`
//!    node, calls **`netstack`** `BindDriver`, *and* reads the planted
//!    `network.conf`, delivering the `wan` interface's DHCPv4 configuration.
//! 4. **`netstack`** binds the `wan` alias to the NIC, starts its DHCP client,
//!    broadcasts DISCOVER, accepts the peer's OFFER, REQUESTs it, applies the
//!    peer's ACK (leasing the interface its only address), and answers the
//!    host peer's echo campaign to that leased address.
//!
//! ## How the run completes — harness-driven, race-free
//!
//! The guest does **not** self-terminate. It boots the production pipeline and
//! keeps serving the host peer's leased-address echo campaign; the harness
//! ends the run the instant the peer's out-of-guest observer confirms
//! success — it received the guest's echo reply at the leased address. That
//! confirmation is the *last* link in the causal chain (driver autoloaded and
//! bound, the DHCP lease acquired and applied, an inbound echo served and its
//! reply transmitted back over virtio-MMIO), so a guest that instead
//! self-exited on an intermediate witness would tear the machine down before
//! the reply left it and lose the race — the defect this choreography
//! removes. The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
//! `DHCP_LEASE_ACQUIRED` and `INBOUND_ECHO_SERVED`) still reach the serial
//! transcript for diagnosis, and the peer's own DHCP-server + leased-address
//! echo campaign verdict subsumes them: it cannot be met unless the lease was
//! granted and the reply arrived. A run that never earns the peer's
//! confirmation fails loud on the runner's inactivity/absolute deadline.
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
//! It reuses the entire production riscv64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin the harness drives to
//! completion through the peer's success gate — there is no in-kernel QEMU-exit
//! shortcut to leak into a production build (fail closed; the harness never
//! decides what the kernel does next).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::{handle_panic_via_serial, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::riscv64::boot as boot_riscv64;

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

    /// Forward to the shared riscv64 panic bridge. A panic parks the hart; the
    /// guest never self-exits, so the run times out and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_dhcp_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with [`SERIAL_SINK`] taking both the log and
    /// the audit streams, so every boot/autoload/bind/lease/echo record
    /// reaches the QEMU transcript for diagnosis. The guest does not
    /// self-exit: the harness ends the run when the host peer confirms the
    /// echo round-trip at the leased address (its success gate), so teardown
    /// can never precede that confirmation.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot_riscv64::boot(
            hartid,
            dtb,
            &SERIAL_SINK,
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
