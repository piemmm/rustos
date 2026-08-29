//! QEMU integration test: hold a hovering desktop to per-frame work bounds
//! (`plans/FIX-DESKTOP-SPEEDUP.md` A.4).
//!
//! The production aarch64 boot pipeline runs unchanged — bootstrap-floor
//! virtio-MMIO discovery, the unlock kthread, the encrypted `ARXFS` root,
//! driver autoload, the display service and the desktop session — and only the
//! diagnostic sink is swapped for the gate. The host logs in as the seeded
//! fixture account, starts the desktop, launches the `framestats` fixture from
//! the program library, sweeps the pointer across the icon bar's controls, and
//! launches `framestats` a second time.
//!
//! # Why the gate listens on the diagnostic trail, not the audit trail
//!
//! The reading it needs is a *userland* one: only a process holding
//! `CAP_SYSINFO_GLOBAL` can ask `sysinfod` what the desktop's frames cost, and
//! a freestanding test kernel cannot issue userland IPC. The `framestats`
//! fixture does the asking and re-emits the counters through `log_emit`, which
//! the kernel decodes into a typed record and delivers to the **diagnostic**
//! sink. So this vertical installs its gate there and hands the audit trail
//! straight to serial — the reverse of the usual wiring, because the witness
//! is a userland record rather than a kernel decision. Everything the gate
//! sees is replayed to serial first, so the transcript still carries the whole
//! boot → unlock → desktop → sweep timeline and the host can gate its
//! injection on it.
//!
//! Nothing in the audit trail could stand in. A present is not recognisable
//! there: on the display endpoint both `Present` and `Configure`, and every
//! error path on either, answer with the same four-byte status word, so
//! counting frames by reply length is exactly the guess that has already cost
//! this project one defect.
//!
//! # What the guest attests
//!
//! Two `framestats` samples arrive, and the work the desktop did **between**
//! them meets every bound in [`assess`]. The bracketing is what makes the
//! assertion meaningful: the published accounting is cumulative from the
//! session's first frame, and bring-up legitimately composes full-screen
//! frames, so neither the epoch's mean nor its peak says anything about the
//! gesture that followed. Every bound is over counted work rather than elapsed
//! time, so the verdict is the same under any machine load.
//!
//! A run whose sweep composed too few frames **fails**: an empty difference
//! would otherwise satisfy every bound by measuring nothing.
//!
//! # Failing loudly
//!
//! A broken bound exits QEMU non-zero with the failing check named on serial,
//! and the sample records that produced it are already in the transcript. A
//! `framestats` run that could not sample at all says so in its own record and
//! the gate fails the run on sight of it, rather than waiting out the budget
//! for a second sample that will never come. A panic before either sample
//! parks the CPU, the guest falls silent, and the runner reports a timeout —
//! loud failure, never a false pass.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, Field, FieldValue, Level, Sink};
    use tairix_test_desktop_hover_qemu_aarch64::{assess, Verdict};
    use tairix_test_framestats::{Sample, SAMPLE_FAILED_EVENT};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the allocator hands out disjoint slices under
    /// its own lock; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Record the gate emits with its verdict, so the transcript states the
    /// judgement and not merely the exit code.
    const VERDICT_EVENT: EventId = EventId(4512);

    /// Message of the verdict record.
    const VERDICT_MESSAGE: &str = "desktop hover verdict";

    /// Finisher code: `framestats` reported that it could not sample.
    const FAIL_NO_SAMPLE: NonZeroU16 = fail_point!(1);

    /// Finisher code: the bracketed window broke a bound. Which one is named
    /// in the verdict record on serial, where a diagnosis belongs.
    const FAIL_BOUND_BROKEN: NonZeroU16 = fail_point!(2);

    /// The first [`Sample`] the gate saw, held across the sweep.
    ///
    /// Field-wise atomics rather than a lock: a sink is called from whatever
    /// context emitted the record, and this needs no exclusion beyond
    /// publishing the eight counters before the flag that says they are
    /// there.
    struct SampleCell {
        screen_px: AtomicU64,
        frames: AtomicU64,
        damaged_px: AtomicU64,
        blended_px: AtomicU64,
        blur_px: AtomicU64,
        dirty_rects: AtomicU64,
        present_calls: AtomicU64,
        chrome_misses: AtomicU64,
        /// Set last on store and read first on load, so a reader either sees
        /// every counter or none.
        held: AtomicBool,
    }

    impl SampleCell {
        /// An empty cell.
        const fn new() -> Self {
            Self {
                screen_px: AtomicU64::new(0),
                frames: AtomicU64::new(0),
                damaged_px: AtomicU64::new(0),
                blended_px: AtomicU64::new(0),
                blur_px: AtomicU64::new(0),
                dirty_rects: AtomicU64::new(0),
                present_calls: AtomicU64::new(0),
                chrome_misses: AtomicU64::new(0),
                held: AtomicBool::new(false),
            }
        }

        /// The sample already held, or `None` after taking `sample` as the
        /// first one.
        ///
        /// One operation rather than a test and a store, so the caller has no
        /// branch in which the cell is neither empty nor readable.
        fn held_or_take(&self, sample: &Sample) -> Option<Sample> {
            if self.held.load(Ordering::Acquire) {
                return Some(Sample {
                    screen_px: self.screen_px.load(Ordering::Relaxed),
                    frames: self.frames.load(Ordering::Relaxed),
                    damaged_px: self.damaged_px.load(Ordering::Relaxed),
                    blended_px: self.blended_px.load(Ordering::Relaxed),
                    blur_px: self.blur_px.load(Ordering::Relaxed),
                    dirty_rects: self.dirty_rects.load(Ordering::Relaxed),
                    present_calls: self.present_calls.load(Ordering::Relaxed),
                    chrome_misses: self.chrome_misses.load(Ordering::Relaxed),
                });
            }
            self.screen_px.store(sample.screen_px, Ordering::Relaxed);
            self.frames.store(sample.frames, Ordering::Relaxed);
            self.damaged_px.store(sample.damaged_px, Ordering::Relaxed);
            self.blended_px.store(sample.blended_px, Ordering::Relaxed);
            self.blur_px.store(sample.blur_px, Ordering::Relaxed);
            self.dirty_rects
                .store(sample.dirty_rects, Ordering::Relaxed);
            self.present_calls
                .store(sample.present_calls, Ordering::Relaxed);
            self.chrome_misses
                .store(sample.chrome_misses, Ordering::Relaxed);
            self.held.store(true, Ordering::Release);
            None
        }
    }

    /// Diagnostic observer that replays the whole trail to serial and judges
    /// the work between the two `framestats` samples.
    struct HoverGate {
        before: SampleCell,
    }

    impl HoverGate {
        /// A gate that has seen no sample.
        const fn new() -> Self {
            Self {
                before: SampleCell::new(),
            }
        }

        /// State `verdict` on serial.
        fn report(verdict: Verdict) {
            SerialSink::new().write_event(&Event {
                level: if verdict.held() {
                    Level::Info
                } else {
                    Level::Error
                },
                id: VERDICT_EVENT,
                message: VERDICT_MESSAGE,
                fields: &[Field {
                    key: "verdict",
                    value: FieldValue::Str(verdict.as_str()),
                }],
            });
        }
    }

    /// The sink installed as the kernel's diagnostic sink.
    static GATE: HoverGate = HoverGate::new();

    impl Sink for HoverGate {
        fn write_event(&self, event: &Event<'_>) {
            // Replay to serial first, so the transcript records the record
            // that drove every decision below — including the counters a
            // failed verdict is diagnosed from.
            SerialSink::new().write_event(event);
            if event.id == SAMPLE_FAILED_EVENT {
                // Nothing later can supply the missing reading, so fail now
                // with the reason already on serial rather than waiting out
                // the budget for a second sample that will never come.
                qemu_exit::exit_failure(FAIL_NO_SAMPLE);
            }
            let Some(sample) = Sample::from_event(event) else {
                return;
            };
            // The first sample opens the bracketed window and the sweep runs
            // inside it; the second closes it and decides the run.
            let Some(before) = self.before.held_or_take(&sample) else {
                return;
            };
            let verdict = assess(&before, &sample);
            Self::report(verdict);
            if verdict.held() {
                qemu_exit::exit_success();
            }
            qemu_exit::exit_failure(FAIL_BOUND_BROKEN);
        }
    }

    /// Forward to the shared aarch64 panic bridge. A panic before either
    /// sample parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_desktop_hover_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the gate
    /// installed as the diagnostic sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &GATE,
            &SERIAL_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; the host waits for that record's
            // `sc=irq_bind` marker before typing the unlock passphrase, so
            // boot with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
