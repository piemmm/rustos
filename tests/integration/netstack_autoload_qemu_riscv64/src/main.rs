//! `plans/NETWORK.md` N4e-riscv64 QEMU integration test: boot the production
//! riscv64 (QEMU `virt` / SiFive) `tairix-kernel` pipeline against the shared
//! whole-disk autoload-root image — whose always-readable `/System` volume
//! carries the **kernel-signed virtio-net driver bundle** in its `Drivers/`
//! store (cross-compiled for riscv64) alongside the input and display bundles
//! — with a `virtio-net-device` attached on the `virtio,mmio` transport and
//! the harness-side `netstack_peer` link peer on the QEMU `dgram` netdev, and
//! prove the full **two-process** network path end to end over the riscv64
//! PLIC device-IRQ path.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The full **two-process** network path (N4e-riscv64): the driver runs in
//!   its own user process, the stack in another, and they speak the
//!   `netchan-v1` device-channel contract across the boundary — the frame
//!   provably crosses a real process boundary, unlike a single-process
//!   in-kernel engine test.
//! * `autoload_input_qemu_riscv64` proves the driver-loading-by-discovery
//!   autoload path for the *input* class. This vertical composes the same
//!   production autoload path for the *network* class.
//!
//! The production boot path:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-MMIO enumeration), each carrying its register
//!    window, DMA constraint, and discovered PLIC interrupt line as
//!    capability-grant requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process (the pre-unlock `devmgr` autoload
//!    hook, verified against the kernel's embedded driver trust anchor); the
//!    driver brings the device up, claims its reserved device-channel endpoint
//!    under `CAP_IPC_BIND_PRIVILEGED`, and publishes a `netchan` hardware-tree
//!    node. The `/System` store binds independently of the encrypted-root
//!    passphrase (the riscv64 SBI console has no interactive input drain this
//!    slice, so the interactive unlock fails closed) — so the network driver
//!    still autoloads.
//! 3. The long-running user-space **`devmgr`** service observes the `netchan`
//!    node and calls **`netstack`** `BindDriver` under `CAP_NET_ADMIN`.
//! 4. **`netstack`** provisions the shared frame region, attaches the driver
//!    channel, and auto-configures the interface's EUI-64 IPv6 link-local
//!    address (no IPv4 — no DHCP/admin client at boot), then answers the host
//!    peer's link-local echo campaign.
//!
//! ## How the run completes — harness-driven, race-free
//!
//! The guest does **not** self-terminate. It boots the production pipeline and
//! keeps serving the host peer's link-local echo campaign; the harness ends the
//! run the instant the peer's out-of-guest observer confirms success — it
//! received the guest's echo reply. That confirmation is the *last* link in the
//! causal chain (driver autoloaded and bound, `netstack` bound and the
//! interface up, an inbound echo served and its reply transmitted back), so a
//! guest that instead self-exited on an intermediate witness would tear the
//! machine down before the reply left it and lose the race — the defect this
//! choreography removes. The three witness records (`devmgr`'s `NETSTACK_BOUND`,
//! `netstack`'s `DRIVER_BOUND` and `INBOUND_ECHO_SERVED`) still reach the serial
//! transcript for diagnosis, and the peer's echo verdict subsumes them: it
//! cannot be met unless all three occurred. A run that never earns the peer's
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
    fn tairix_netstack_autoload_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with [`SERIAL_SINK`] taking both the log and the
    /// audit streams, so every boot/autoload/bind/echo record reaches the QEMU
    /// transcript for diagnosis. The guest does not self-exit: the harness ends
    /// the run when the host peer confirms the echo round-trip (its success
    /// gate), so teardown can never precede that confirmation. Boot at the
    /// default `Info` filter: keeping the noisier `Debug` syscall trace off the
    /// wire stops the NULL-console login read-retry chatter from crowding the
    /// network timeline out of a failing run's serial tail.
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
