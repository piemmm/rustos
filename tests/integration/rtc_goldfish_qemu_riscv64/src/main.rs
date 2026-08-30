//! `plans/TIMESYNC.md` TS-3 QEMU integration test: boot the production
//! riscv64 `tairix-kernel` pipeline on the `virt` board against the shared
//! `rtc-root` whole-disk image — whose read-only `/System` volume carries the
//! **kernel-signed Goldfish clock-chip driver bundle** and no other driver —
//! and prove the wall clock is established from the board's own clock chip.
//!
//! ## What this vertical asserts
//!
//! * That this port's discovery reaches a device at all. Before TS-3 the
//!   riscv64 hardware tree carried only a root, a memory window, and a bare
//!   timer: there was no generic `compatible` walk, so no `google,goldfish-rtc`
//!   node existed to match and nothing could ever autoload here. The port now
//!   shares the aarch64 walk, and this run is the witness that it works end to
//!   end — discover the node, match its bind table against the signed bundle,
//!   autoload the driver into user space, bind the RTC service endpoint, and
//!   have `timed` (the sole `CAP_TIME_SET` holder) read it and tag the reading
//!   `Firmware`.
//! * That it happens **with no network and before the operator unlocks
//!   anything**: the store carries no NIC driver and the run types nothing at
//!   the passphrase prompt.
//! * That the reading is *right*, not merely present. QEMU starts the chip at
//!   the pinned instant the harness passes on its command line, so the witness
//!   requires the applied seconds to land in the shared clock-chip fixture's
//!   window. The Goldfish RTC is a 64-bit nanosecond counter split across two
//!   32-bit registers; reading the halves in the wrong order, swapping them,
//!   or missing the nanosecond scale all still produce a plausible wall time,
//!   and none of them lands in this window.
//!
//! ## How the run completes
//!
//! The guest exits on `timed`'s `RTC_CLOCK_SET` audit record carrying an
//! in-window `wall_secs=`. There is deliberately **no serial script**: the
//! witness fires before the unlock prompt is answered, so a script gated on
//! the unlock or login dialogue would still be pending when the guest exits.
//!
//! ## Real firmware device tree
//!
//! QEMU's riscv64 `virt` OpenSBI firmware hands the boot hart a valid
//! device-tree pointer in `a1`, so this vertical forwards the verbatim
//! pointer to the boot pipeline, which discovers the board — the clock chip
//! included — from it exactly as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production riscv64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin carrying the QEMU-exit
//! witness — there is no in-kernel exit shortcut to leak into a production
//! build (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::riscv64::boot as boot_riscv64;
    use tairix_log::{Event, FieldValue, Sink};

    /// Static boot heap.
    ///
    /// Placed in the linker's dedicated `.heap` (NOLOAD) section so the boot
    /// trampoline does not zero its bytes and the boot pipeline excludes it
    /// from the usable physical-memory map, exactly as the production riscv64
    /// kernel binary's heap does. `static mut` because the bump allocator
    /// hands out disjoint slices via an atomic cursor; the storage is
    /// otherwise never aliased.
    #[link_section = ".heap"]
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
    /// It gates on the *value*: every mis-decode worth catching produces a
    /// plausible wall time, and only a correct read of the nanosecond
    /// counter's two halves lands in the pinned window.
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

    /// Forward to the shared riscv64 panic bridge. A panic parks the hart
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_rtc_goldfish_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline. The witness sits on the **diagnostic**
    /// stream because a service's own records reach only that one; the audit
    /// stream goes straight to the transcript.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot_riscv64::boot(
            hartid,
            dtb,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
