//! QEMU integration test: a sandboxed SVG decode draws glyphs a live `fontd`
//! supplied over the parser-sandbox pipe (`plans/SVG.md` S23/S24).
//!
//! The production aarch64 boot pipeline runs unchanged — bootstrap-floor
//! virtio-MMIO discovery, the unlock kthread, the encrypted `ARXFS` root,
//! driver autoload, the display service and the console login — and only
//! the diagnostic sink is swapped for the gate. The host unlocks the root,
//! logs in as the seeded fixture account, and types the `svgtext` fixture's
//! command word.
//!
//! Nothing waits for the font service. It is registered on-demand, so the
//! fixture's own first glyph request activates it and the service manager
//! holds that call until the endpoint is answerable
//! (`plans/NEW-SERVICEMANAGER.md` SVC-5). A script that gated on a readiness
//! line would be proving its own ordering rather than the system's.
//!
//! # What only a running machine can show
//!
//! Every layer of the text path is host-tested — the wire form and its
//! refusals, the service's resolution and synthesis, the layout, the
//! two-round exchange, and the decoder's own `<text>` drawing — and the
//! build-time icon verification already drives the real service. What no
//! host test reaches is the **pipe between them**: a decoder holding no
//! capability recording the faces and scalars it cannot answer, a parent
//! fetching exactly those from a live service, and a second decode drawing
//! the outlines that came back, all under a real kernel with real processes
//! and real spawn/pipe/wait.
//!
//! # Why the gate listens on the diagnostic trail, not the audit trail
//!
//! The measurement is a *userland* one: only the fixture can see the pixels
//! its sandbox produced, and a freestanding test kernel cannot issue
//! userland IPC. The fixture re-emits what it measured through `log_emit`,
//! which the kernel decodes into a typed record and delivers to the
//! **diagnostic** sink. So the gate is installed there and the audit trail
//! goes to the finisher, which holds the PASS until the scripted shell's own
//! `exit` — so the record provably reached the transcript before the run
//! ended. Everything either sink sees is replayed to serial first.
//!
//! # What the guest attests
//!
//! One measured record that `tairix_test_svgtext::Report::verdict` accepts
//! (plain backticks, not a link: the fixture crate is an aarch64-only
//! dependency and the host doc build has no such item to resolve): two drawings differing in exactly one character inked
//! differently and in the direction their characters do, neither blank and
//! neither a solid fill, and the same drawing through a sandbox with no font
//! seam refused for want of glyphs. Nothing but real, character-dependent
//! outlines fetched from the service satisfies all of that — a decoder
//! drawing nothing, a placeholder, or a fixed box fails it.
//!
//! # Failing loudly
//!
//! A refused measurement exits QEMU non-zero with its own code naming which
//! expectation it missed, and the record that produced it is already in the
//! transcript. A `svgtext` run that could not render at all says so in its
//! own record and the gate fails the run on sight of it, rather than waiting
//! out the budget. A panic parks the CPU, the guest falls silent, and the
//! runner reports a timeout — loud failure, never a false pass.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink};
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, FieldValue, Sink};
    use tairix_test_svgtext::{Report, Verdict, COMMAND, REPORT_FAILED_EVENT};

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

    /// `EventId` emitted by the syscall dispatcher for an audited syscall
    /// that passed every check. Pinned by the audit-id test in
    /// `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// The fixture could not render at all, so there is no measurement to
    /// judge and none is coming.
    const FAIL_NOT_MEASURED: NonZeroU16 = fail_point!(1);
    /// The record describes a render with no pixels in it.
    const FAIL_NO_DESTINATION: NonZeroU16 = fail_point!(2);
    /// The drawing decoded with no font seam produced a picture instead of a
    /// refusal, so its lettering need not have come from the service.
    const FAIL_GLYPHS_NOT_FROM_THE_SERVICE: NonZeroU16 = fail_point!(3);
    /// A render left no ink: the drawing's only content is its text.
    const FAIL_NOTHING_DRAWN: NonZeroU16 = fail_point!(4);
    /// The render is entirely ink — a fill, not lettering.
    const FAIL_SOLID_FILL: NonZeroU16 = fail_point!(5);
    /// Both characters inked the same, so what was drawn does not depend on
    /// the character.
    const FAIL_GLYPHS_NOT_DISTINCT: NonZeroU16 = fail_point!(6);

    /// Set once an accepted measurement has been observed.
    static MEASURED: AtomicBool = AtomicBool::new(false);

    /// Set once the fixture's own audited `exit` has been observed, so the
    /// PASS fires on the *next* one — the shell's, typed by the runner only
    /// after the measurement appeared. Exiting QEMU on the fixture's own
    /// exit would tear the run down with the last scripted line still owed,
    /// which the harness fails as an incomplete script.
    static FIXTURE_EXITED: AtomicBool = AtomicBool::new(false);

    /// The string value of `event`'s field `key`, if present.
    fn field_str<'e>(event: &Event<'e>, key: &str) -> Option<&'e str> {
        event.fields.iter().find_map(|field| match field.value {
            FieldValue::Str(s) if field.key == key => Some(s),
            _ => None,
        })
    }

    /// The finisher a refused verdict exits with. Each expectation has its
    /// own code, so a failing run names which one it missed rather than
    /// leaving an indistinguishable timeout.
    const fn finisher(verdict: Verdict) -> Option<NonZeroU16> {
        match verdict {
            Verdict::Accepted => None,
            Verdict::NoDestination => Some(FAIL_NO_DESTINATION),
            Verdict::GlyphsNotFromTheService => Some(FAIL_GLYPHS_NOT_FROM_THE_SERVICE),
            Verdict::NothingDrawn => Some(FAIL_NOTHING_DRAWN),
            Verdict::SolidFill => Some(FAIL_SOLID_FILL),
            Verdict::GlyphsNotDistinct => Some(FAIL_GLYPHS_NOT_DISTINCT),
        }
    }

    /// Gate on the diagnostic trail: replay everything to serial, and judge
    /// the fixture's measurement through the fixture's own rule.
    struct Gate;

    impl Sink for Gate {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the transcript records the
            // full boot → unlock → font service → measurement timeline,
            // including the record every decision below was taken from.
            SerialSink::new().write_event(event);
            if event.id == REPORT_FAILED_EVENT {
                // Nothing later can supply the missing measurement, so fail
                // now with the reason already on serial rather than waiting
                // out the budget for a record that will never come.
                qemu_exit::exit_failure(FAIL_NOT_MEASURED);
            }
            let Some(report) = Report::from_event(event) else {
                return;
            };
            match finisher(report.verdict()) {
                None => MEASURED.store(true, Ordering::Release),
                Some(code) => qemu_exit::exit_failure(code),
            }
        }
    }

    static GATE: Gate = Gate;

    /// Report PASS on the shell's audited `exit`, once an accepted
    /// measurement has been seen.
    struct AuditObserver;

    impl Sink for AuditObserver {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(COMMAND) {
                FIXTURE_EXITED.store(true, Ordering::Release);
            } else if MEASURED.load(Ordering::Acquire) && FIXTURE_EXITED.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: AuditObserver = AuditObserver;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_svgtext_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
            &ALLOCATOR,
            // The measurement arrives on the *diagnostic* stream, because a
            // userland `log_emit` does; the audited `exit` that completes
            // the PASS chain is on the audit trail. Both replay to serial.
            &GATE,
            &AUDIT_SINK,
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
