//! `plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond QEMU integration test: boot the
//! production aarch64 `tairix-kernel` pipeline on the `virt` board against
//! the shared `bond-net-root` whole-disk image — whose read-only `/System`
//! volume carries the **kernel-signed virtio-net driver bundle** and a
//! planted `/System/Settings/Network/network.conf` composing an
//! **active-backup bond** over two NICs — with **two** `virtio-net-device`s
//! attached and the harness-side **bond** link peer serving both wires, and
//! prove live link-aggregation **failover** end to end.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical proves the declarative bind + static
//!   address path over a *single* NIC. This vertical proves **bonding and
//!   failover** on top of it: `netstack` composes an active-backup bond
//!   (`wan`) over two NICs bound by `match.mac`, assigns the *bond* a static
//!   IPv6 address, and answers the peer over the active member. Mid-flow the
//!   harness drops the **primary** member's carrier over the QEMU monitor
//!   (`set_link net0 off`); the driver's virtio config-change interrupt
//!   reports the link down, the bond fails over to the surviving member, and
//!   the guest keeps answering — now over the second wire.
//! * The failover is the whole point, so PASS requires an
//!   `INBOUND_ECHO_SERVED`
//!   that arrives **after** the
//!   `BOND_FAILOVER`: a request
//!   served only *before* the primary was dropped would not prove the flow
//!   survived, so the post-failover echo is a real discriminator, not a
//!   tautology.
//!
//! ## Why PASS keys on three ordered witnesses
//!
//! The log-sink observer reports PASS once it has seen all of (each a
//! userland `log_emit` record the kernel routes to the log sink):
//!
//! 1. `netstack`'s `BOND_CONFIG_APPLIED` — the planted bond `network.conf`
//!    was applied: two members enrolled, the active-backup bond composed,
//!    its static address assigned.
//! 2. `netstack`'s `BOND_FAILOVER` — the bond's transmit path changed after
//!    a member link report (the harness dropped the primary member's
//!    carrier and the driver's config-change interrupt reported it down).
//! 3. `netstack`'s `INBOUND_ECHO_SERVED` **observed after witness 2** — an
//!    echo request addressed to the bond's static address was answered
//!    *after* the failover, so a frame crossed the two-process boundary over
//!    the surviving member end to end.
//!
//! Witness 3 (post-failover) can only fire after 1 and 2, so the three
//! together prove the whole chain; it gates exit so the guest stays alive
//! until a frame has actually been served post-failover, avoiding a race
//! with the host peer's verdict. The harness additionally requires the peer
//! thread's own echo campaign (to the bond's static address) to have
//! completed, so neither side can pass alone.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only swaps in a
//! log-sink observer. Splitting the observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, Sink};

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

    /// The kernel **log** sink: it replays every record through
    /// [`SERIAL_SINK`] and reports PASS to QEMU once the three ordered
    /// bond-failover witnesses have appeared. All are *userland* `log_emit`
    /// records (from the `netstack` service), which the kernel routes to the
    /// log sink — not the audit sink — so this observer is installed there:
    /// `netstack`'s `BOND_CONFIG_APPLIED` (the planted bond `network.conf`
    /// was applied), `netstack`'s `BOND_FAILOVER` (the bond's transmit path
    /// changed after a member link-down report), and a `netstack`
    /// `INBOUND_ECHO_SERVED` **observed after the failover** (an echo request
    /// addressed to the bond's static address was answered over the surviving
    /// member). The guest exits only after the last, so the host peer's
    /// verdict never races an early teardown, and the *ordering* requirement
    /// makes an echo served only before the failover insufficient.
    struct NetstackBondSink {
        bond_config_applied: AtomicBool,
        failover: AtomicBool,
        echo_after_failover: AtomicBool,
    }

    impl NetstackBondSink {
        const fn new() -> Self {
            Self {
                bond_config_applied: AtomicBool::new(false),
                failover: AtomicBool::new(false),
                echo_after_failover: AtomicBool::new(false),
            }
        }
    }

    impl Sink for NetstackBondSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + bond-config + failover + echo timeline
            // for a failing run.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_netstack::events::BOND_CONFIG_APPLIED.0 {
                self.bond_config_applied.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::BOND_FAILOVER.0 {
                self.failover.store(true, Ordering::Release);
            } else if event.id.0 == tairix_netstack::events::INBOUND_ECHO_SERVED.0 {
                // Only an echo served *after* the failover proves the flow
                // survived the member drop; one served before it is expected
                // and not sufficient.
                if self.failover.load(Ordering::Acquire) {
                    self.echo_after_failover.store(true, Ordering::Release);
                }
            } else {
                return;
            }
            if self.bond_config_applied.load(Ordering::Acquire)
                && self.failover.load(Ordering::Acquire)
                && self.echo_after_failover.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: NetstackBondSink = NetstackBondSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_bond_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. The witness
    /// observer is installed as the **log** sink (the witnesses are userland
    /// `log_emit` records the kernel routes there), and the plain
    /// [`SERIAL_SINK`] takes the audit stream so kernel audit records still
    /// reach the transcript. Boot at the default `Info` filter: the
    /// witnesses are `Info` records.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
