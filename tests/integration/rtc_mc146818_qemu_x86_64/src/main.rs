//! `plans/TIMESYNC.md` TS-3 QEMU integration test: boot the production
//! x86_64 `tairix-kernel` pipeline against the shared `rtc-root` whole-disk
//! image — whose read-only `/System` volume carries the **kernel-signed CMOS
//! clock-chip driver bundle** and no other driver — and prove the wall clock
//! is established from the machine's own clock chip.
//!
//! ## What this vertical asserts
//!
//! * The chain end to end: discovery synthesises the `motorola,mc146818`
//!   node with its `0x70`/`0x71` port pair on the legacy-fallback path,
//!   `devmgr` matches its bind table against the signed bundle in the store
//!   and autoloads it into its own user-space process, the driver reads its
//!   registers through the granted-range `PORT_READ` trap and binds the
//!   well-known RTC service endpoint, and `timed` — the sole holder of
//!   `CAP_TIME_SET` — reads it and tags the reading `Firmware`.
//! * That it happens **with no network and before the operator unlocks
//!   anything**: the store carries no NIC driver and the run types nothing at
//!   the passphrase prompt, so a clock established here can only have come
//!   from the chip.
//! * That the reading is *right*, not merely present. QEMU starts the CMOS
//!   clock at the pinned instant the harness passes on its command line, so
//!   the witness requires the applied seconds to land in the shared
//!   clock-chip fixture's window. A BCD/binary confusion, a 12-hour field
//!   read as 24, a wrong register, or a fabricated epoch is still a plausible
//!   wall time and would pass a bare "a clock was set" gate; none of them
//!   lands in this one.
//!
//! ## How it differs from its two siblings
//!
//! `rtc_pl031_qemu_aarch64` and `rtc_goldfish_qemu_riscv64` read a
//! memory-mapped counter off a device-tree node. This port has neither: the
//! CMOS clock is an index/data **port pair** no ACPI table enumerates, so it
//! is the only clock in the class reached through the user-space port-I/O
//! trap, and the only one whose node is synthesised rather than parsed. The
//! disk it boots is also reached over virtio-**PCI** rather than the
//! single-aperture MMIO transport.
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
//! It reuses the entire production x86_64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin carrying the QEMU-exit
//! witness — there is no in-kernel exit shortcut to leak into a production
//! build (fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, FieldValue, Sink};

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

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through [`SERIAL_SINK`], not the witness, so a panic
    /// before PASS cannot trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_rtc_mc146818_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`].
    ///
    /// The witness sits on the **diagnostic** stream because a service's own
    /// records reach only that one; the audit stream goes straight to the
    /// transcript.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &ALLOCATOR,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
