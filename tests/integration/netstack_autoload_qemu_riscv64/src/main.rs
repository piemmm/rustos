//! `plans/NETWORK.md` N4e-riscv64 QEMU integration test: boot the production
//! riscv64 (QEMU `virt` / SiFive) `rustos-kernel` pipeline against the shared
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
//! * `netstack_mmio_riscv64` proves the pure `rustos-netstack` engine pumps a
//!   live virtio-net device — but in a *single* process, over the in-kernel
//!   `register` scaffold (`plans/NETWORK.md` N3c). This vertical is the
//!   two-process production-boot replacement (N4e-riscv64): the driver runs in
//!   its own user process, the stack in another, and they speak the
//!   `netchan-v1` device-channel contract across the boundary.
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
//! ## Why PASS keys on three witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a userland
//! `log_emit` record the kernel routes to the log sink):
//!
//! 1. `devmgr`'s `NETSTACK_BOUND` — the `netchan` node was handed to the stack
//!    over the capability-gated admin surface.
//! 2. `netstack`'s `DRIVER_BOUND` — the stack provisioned the channel and
//!    brought the interface up.
//! 3. `netstack`'s `INBOUND_ECHO_SERVED` — an echo request from the host peer
//!    was answered, so a frame crossed the two-process boundary end to end.
//!
//! Witness 3 can only fire after 1 and 2 (and, before them, the driver's own
//! `netchan`-published readiness), so the three together prove the whole
//! chain; it gates exit so the guest stays alive until a frame has actually
//! been answered, avoiding a race with the host peer's verdict. The harness
//! additionally requires the peer thread's own v6 link-local echo campaign to
//! have completed, so neither side can pass alone.
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

    use rustos_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::riscv64::boot as boot_riscv64;
    use rustos_log::{Event, Sink};

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
            if event.id.0 == rustos_devmgr::events::NETSTACK_BOUND.0 {
                self.netstack_bound.store(true, Ordering::Release);
            } else if event.id.0 == rustos_netstack::events::DRIVER_BOUND.0 {
                self.driver_bound.store(true, Ordering::Release);
            } else if event.id.0 == rustos_netstack::events::INBOUND_ECHO_SERVED.0 {
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

    /// Forward to the shared riscv64 panic bridge. A panic before the PASS
    /// finisher parks the hart, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn rustos_netstack_autoload_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with the witness observer installed as the
    /// **log** sink (the three witnesses are userland `log_emit` records the
    /// kernel routes there), and the plain [`SERIAL_SINK`] taking the audit
    /// stream so kernel audit records still reach the transcript. Boot at the
    /// default `Info` filter: the three witnesses are `Info` records, and
    /// keeping the noisier `Debug` syscall trace off the wire stops the
    /// NULL-console login read-retry chatter from crowding the network
    /// timeline out of a failing run's serial tail.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot_riscv64::boot(
            hartid,
            dtb,
            &WITNESS_SINK,
            &SERIAL_SINK,
            rustos_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
