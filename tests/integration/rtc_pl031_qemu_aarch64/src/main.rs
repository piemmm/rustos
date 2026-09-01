//! `plans/TIMESYNC.md` TS-3 QEMU integration test: boot the production
//! aarch64 `tairix-kernel` pipeline on the `virt` board against the shared
//! `rtc-root` whole-disk image — whose read-only `/System` volume carries the
//! **kernel-signed PL031 clock-chip driver bundle** and no other driver — and
//! prove the wall clock is established from the board's own clock chip.
//!
//! ## What this vertical asserts
//!
//! * The chain end to end: the boot pipeline discovers the `arm,pl031` node
//!   off the device tree, `devmgr` matches its bind table against the signed
//!   bundle in the store and autoloads it into its own user-space process,
//!   the driver maps its granted counter window and binds the well-known RTC
//!   service endpoint, and `timed` — the sole holder of `CAP_TIME_SET` —
//!   reads it and tags the reading `Firmware`.
//! * That it happens **with no network and before the operator unlocks
//!   anything**: the store carries no NIC driver and the run types nothing at
//!   the passphrase prompt, so a clock established here can only have come
//!   from the chip.
//! * That the reading is *right*, not merely present. QEMU starts the PL031
//!   at the pinned instant the harness passes on its command line, so the
//!   witness requires the applied seconds to land in
//!   the shared clock-chip fixture's window. A byte-swapped counter, a wrong
//!   register, or a fabricated epoch is still a plausible wall time and would
//!   pass a bare "a clock was set" gate; none of them lands in this one.
//!
//! ## How the run completes
//!
//! The guest exits on `timed`'s `RTC_CLOCK_SET` audit record carrying an
//! in-window `wall_secs=`. There is deliberately **no serial script**: the
//! witness fires before the unlock prompt is answered, so a script gated on
//! the unlock or login dialogue would still be pending when the guest exits
//! and the runner would report it unfinished. A run that never earns the
//! witness fails loud on the runner's inactivity/absolute deadline.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin carrying the QEMU-exit
//! witness — there is no in-kernel exit shortcut to leak into a production
//! build (fail closed).

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
    /// once `timed` applied a reading of the fixture's clock chip.
    ///
    /// It gates on the *value*: the mis-decodes worth catching all produce a
    /// plausible wall time, and only a correct register read lands in the
    /// pinned window.
    struct RtcClockSetExitSink;

    impl Sink for RtcClockSetExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != tairix_timesync::events::RTC_CLOCK_SET {
                return;
            }
            let applied = event.fields.iter().find(|f| f.key == "wall_secs");
            if let Some(FieldValue::SignedInt(secs)) = applied.map(|f| f.value) {
                if tairix_test_rtc_fixture::reading_is_from_fixture(secs) {
                    qemu_exit::exit_success();
                }
            }
        }
    }

    static WITNESS_SINK: RtcClockSetExitSink = RtcClockSetExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_rtc_pl031_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. The witness sits
    /// on the **diagnostic** stream because a service's own records reach
    /// only that one; the audit stream goes straight to the transcript.
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
