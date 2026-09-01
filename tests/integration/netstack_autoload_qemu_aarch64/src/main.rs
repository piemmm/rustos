//! `plans/NETWORK.md` N4e-β QEMU integration test: boot the production
//! aarch64 `tairix-kernel` pipeline on the `virt` board against the shared
//! whole-disk autoload-root image — whose read-only `/System` volume now
//! carries the **kernel-signed virtio-net driver bundle** in its `Drivers/`
//! store alongside the input and display bundles — with a `virtio-net-device`
//! attached and the harness-side `netstack_peer` link peer on the QEMU
//! `dgram` netdev, and prove the full **two-process** network path end to
//! end.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The full **two-process** network path (N4e-β): the driver runs in its
//!   own user process, the stack in another, and they speak the `netchan-v1`
//!   device-channel contract across the boundary — unlike a single-process
//!   in-kernel engine test, the frame provably crosses a real process
//!   boundary here.
//! * `autoload_input_qemu_aarch64` proves the driver-loading-by-discovery
//!   autoload path for the *input* and *display* classes. This vertical
//!   composes the same production autoload path for the *network* class.
//!
//! The production boot path:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-MMIO enumeration), each carrying its register
//!    window, DMA constraint, and GICv2 interrupt line as capability-grant
//!    requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process (the pre-unlock `devmgr` autoload
//!    hook, verified against the kernel's embedded driver trust anchor); the
//!    driver brings the device up, claims its reserved device-channel
//!    endpoint under `CAP_IPC_BIND_PRIVILEGED`, and publishes a `netchan`
//!    hardware-tree node.
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
    fn tairix_netstack_autoload_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. [`SERIAL_SINK`]
    /// takes both the log and the audit streams, so every boot/autoload/bind/
    /// echo record reaches the QEMU transcript for diagnosis. The guest does
    /// not self-exit: the harness ends the run when the host peer confirms the
    /// echo round-trip (its success gate), so teardown can never precede that
    /// confirmation. Boot at the default `Info` filter: keeping the noisier
    /// `Debug` syscall trace off the wire stops the console-login read-retry
    /// chatter from crowding the network timeline out of a failing run's serial
    /// tail.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
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
