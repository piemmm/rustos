//! `plans/TIMESYNC.md` TS-2 QEMU integration test: boot the production aarch64
//! `tairix-kernel` pipeline on the `virt` board against the shared
//! `time-net-root` whole-disk image — whose read-only `/System` volume carries
//! the **kernel-signed virtio-net driver bundle** and a planted
//! `/System/Settings/Network/network.conf`, and whose **encrypted root**
//! carries a planted `/System/Settings/Configuration/system.conf` naming the
//! host peer as the one time server — with a `virtio-net-device` attached and
//! the harness-side **NTP-server** link peer on the QEMU `dgram` netdev, and
//! prove the wall clock is established from the network end to end.
//!
//! ## What this vertical asserts — and how it differs from its siblings
//!
//! * The static-addressing vertical proves the declarative `match.node`
//!   binding with a static IPv6 address. This vertical reuses exactly that
//!   addressing and proves what sits *on top* of it: the guest boots with the
//!   wall clock `Unset` (no RTC is modelled), so `timed` finds the clock
//!   urgent and queries the configured server.
//! * The peer answers **twice per request, spoof first** — a well-formed reply
//!   whose origin timestamp does not echo the request's CSPRNG nonce and which
//!   reports a plainly different instant, then the truthful reply echoing the
//!   nonce. That ordering is the discriminator, and it is why the run's gate
//!   requires the applied second to be the fixture's rather than merely "a
//!   clock was set": a guest that accepted the spoof would land a million
//!   seconds away, and a guest that let the spoof cancel its outstanding
//!   transaction would ignore the truthful reply and never set the clock at
//!   all. Only the nonce-gated-but-not-cancelled path reaches the gating
//!   window.
//! * The NTP decode itself runs in a capability-empty sandbox worker, so this
//!   run also exercises the live spawn of that worker by a service holding
//!   `CAP_TIME_SET` — the containment the host tests cover in-process.
//!
//! The production boot path:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-MMIO enumeration), each carrying its register
//!    window, DMA constraint, and GICv2 interrupt line as capability-grant
//!    requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device up
//!    and publishes a `netchan` hardware-tree node.
//! 3. The **`devmgr`** service observes the node, calls **`netstack`**
//!    `BindDriver`, and reads the planted `network.conf`, delivering the `wan`
//!    interface's static IPv6 configuration.
//! 4. The operator's passphrase unlocks the encrypted root, which is what
//!    makes the planted `system.conf` reachable through the ordinary VFS.
//! 5. **`timed`** reads that store, resolves its one configured server (an
//!    address literal, so no resolver enters the path), queries it, evaluates
//!    the replies in a sandbox worker, and applies the validated sample.
//!
//! ## How the run completes — harness-driven, race-free
//!
//! The serial script drives the unlock and login dialogue and ends there; the
//! guest then exits itself on `timed`'s `CLOCK_SET` audit record whose applied
//! `wall_secs=` is the fixture's sample
//! (`tairix_test_netstack_wire::applied_is_fixture_sample`, the base instant
//! plus the delay compensation a validated sample can carry — never the base
//! alone, which would assert the host's speed rather than the guest's clock).
//! Gating the exit on that *value* is what rejects the spoof the peer sends
//! first — a guest that believed it records seconds a million away and never
//! exits, and one that let it cancel the transaction records nothing at all. The harness additionally requires the
//! peer to report a served request, so neither side passes alone: the peer
//! cannot know which reply the guest believed, and the guest's witness cannot
//! appear unless the peer answered. A run that never earns the witness fails
//! loud on the runner's inactivity/absolute deadline.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin the harness drives to
//! completion through the serial script — there is no in-kernel QEMU-exit
//! shortcut to leak into a production build (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, FieldValue, Sink};

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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// once `timed` applied the fixture instant.
    ///
    /// It gates on the *value* rather than the event: the peer answers each
    /// request spoof-first, so a guest that believed the spoof records a
    /// different `wall_secs` and never exits, and one that let the spoof
    /// cancel its transaction records nothing at all. Either way the run
    /// fails loud on the deadline.
    struct ClockSetExitSink;

    impl Sink for ClockSetExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != tairix_timesync::events::CLOCK_SET {
                return;
            }
            let applied = event.fields.iter().find(|f| f.key == "wall_secs");
            if let Some(FieldValue::SignedInt(secs)) = applied.map(|f| f.value) {
                if tairix_test_netstack_wire::applied_is_fixture_sample(secs) {
                    qemu_exit::exit_success();
                }
            }
        }
    }

    static WITNESS_SINK: ClockSetExitSink = ClockSetExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_timed_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. Both streams
    /// reach the transcript — the audit one directly, the diagnostic one
    /// replayed by the witness, which has to sit there because a service's
    /// own records are emitted to the diagnostic sink and never the audit
    /// sink. So every boot/autoload/bind/unlock/clock record is captured, and
    /// `timed`'s
    /// `CLOCK_SET` record, carrying the applied `wall_secs=`, is the run's
    /// gating witness, on which [`WITNESS_SINK`] exits. Boot at the default
    /// `Info` filter: the witness records are `Info` records.
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
