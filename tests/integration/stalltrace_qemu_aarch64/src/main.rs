//! `plans/FIX-STALLTRACE.md` QEMU integration test: boot the *production*
//! aarch64 `tairix-kernel` pipeline with the debug image's diagnostics on,
//! provoke one frame-budget overrun from a planted fixture, and prove the
//! kernel's report names the blocking call and carries a live user backtrace.
//!
//! ## What this test asserts
//!
//! The vertical's disk is the shared encrypted-root fixture whose read-only
//! `/System` volume carries the standard signed store bundles **plus** the
//! test-only `stalltrace` fixture bundle
//! (`tests/integration/stalltrace_program`), composed and signed by the same
//! `tools/xtask` composer as every store bundle and planted only on this
//! vertical's disk — no production image ever ships it. The runner unlocks
//! the root at the passphrase prompt, authenticates `root`/`root` at the
//! console login, and types the bare command word `stalltrace` at the shell.
//!
//! The fixture declares the default frame budget, opens a span through a real
//! event wait on a pipe it feeds itself, and then parks well past the budget
//! inside one syscall. The kernel must notice at that syscall's *exit* and
//! emit exactly the record [`assess`] accepts:
//!
//! - it is a `TaskLatencyOverrun` (`4150`),
//! - `blocked_in` names the syscall that spent the budget rather than
//!   leaving it to be guessed,
//! - `blocked_in_ms` is at least the budget, so the report is about the
//!   stall the fixture caused and not an incidental one,
//! - `sampled=blocking`, so the frame is the blocking call's own and not a
//!   stale one, and
//! - `pc` and `bt` are present and load-relative (the `+0x` marker), which
//!   is what proves the port published a usable frame *and* that the
//!   `copy_in`-backed walk read a real chain off the stalling user stack.
//!
//! That last point is the whole reason this vertical exists: the state
//! machine is host-tested, but nothing on the host can prove the aarch64
//! trampoline hands over the right registers or that the walk crosses the
//! kernel/user boundary safely.
//!
//! ## Why the gate listens on the diagnostic trail, not the audit trail
//!
//! The overrun report is an address-bearing developer aid, so it is emitted
//! through the diagnostic (log/UART) stream and deliberately never reaches
//! the hash-chained audit trail — the same split the lockup detail uses. So
//! this vertical installs its gate there and hands the audit trail straight
//! to serial, the reverse of the usual wiring. Everything the gate sees is
//! replayed to serial first, so the transcript still carries the whole
//! boot → unlock → login → stall timeline and the host can gate its
//! injection on it.
//!
//! ## Why the PASS keys on the record *then* the shell's exit
//!
//! Exiting QEMU the instant the record arrives — or on the fixture's own
//! `exit` — would tear the run down with the last scripted line still owed,
//! which the harness fails as an incomplete script. Two things must
//! therefore have happened before the PASS fires: an accepted record on the
//! diagnostic trail, and the fixture's own audited `exit` on the audit
//! trail. It then reports PASS on the *next* audited `exit` — the shell's,
//! which the runner types only after `STALLTRACE PROVOKED` appeared. A
//! missing record, a malformed one, or a fixture that failed its own setup
//! never reaches that point: the run times out with the diagnosis in the
//! serial transcript — the documented fail-loud behaviour.
//!
//! A refused record is not merely ignored, either: the gate fails the run at
//! once through the shared finisher, so a report that arrives *wrong* is a
//! distinct, immediate failure rather than an indistinguishable timeout.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so
//! the canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline, which discovers
//! the board from it exactly as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and replaces only
//! the diagnostic sink; it additionally enables `watchdog-diagnostics`,
//! because the facility under test exists in no other configuration — that
//! is the same feature `tools/xtask` turns on for the non-shippable `debug`
//! image. Splitting the observer behaviour into a separate bin (instead of a
//! Cargo feature on a production crate) prevents feature unification from
//! leaking the QEMU-exit shortcut into any production build.

#![cfg_attr(itest_aarch64, no_std)]
#![deny(missing_docs)]
#![cfg_attr(itest_aarch64, no_main)]

/// The audit event id the kernel emits for an interactive-surface frame-budget
/// overrun. Pinned by the audit-id tests in `kernel/core/src/audit.rs`.
pub const LATENCY_OVERRUN_EVENT_ID: u32 = 4150;

/// Why a `TaskLatencyOverrun` record was refused, or that it was accepted.
///
/// A separate type rather than a bare `bool` so the gate can say *which*
/// expectation the record missed: an overrun reported without a blocking
/// call, without a live frame, or without a backtrace are three different
/// defects, and a timeout that says only "no PASS" would name none of them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The record carries everything the facility promises.
    Accepted,
    /// No `blocked_in` field: the report did not name the syscall that spent
    /// the budget, so the boundary that fired was the wrong one.
    NoBlockingCall,
    /// `blocked_in_ms` is below the budget: the record describes some other,
    /// incidental overrun rather than the fixture's deliberate stall.
    StallTooShort,
    /// `sampled` is not `blocking`: the frame is not the blocking call's own.
    WrongProvenance,
    /// No `pc`, or one that is not load-relative: the port published no
    /// usable frame, or an address escaped absolute.
    NoRelativePc,
    /// No `bt`: the user-stack walk produced no frame, so the chain above
    /// the blocking call was never read.
    NoBacktrace,
}

/// Judge one overrun record's fields.
///
/// `blocked_in` / `sampled` / `pc` / `bt` are the field values as rendered
/// ([`None`] when the field is absent); `blocked_in_ms` is the parsed
/// duration and `budget_ms` the budget the same record reported.
///
/// Pure, so the acceptance rule is host-tested rather than only exercised
/// inside a guest.
#[must_use]
pub fn assess(
    blocked_in: Option<&str>,
    blocked_in_ms: Option<u64>,
    budget_ms: u64,
    sampled: Option<&str>,
    pc: Option<&str>,
    bt: Option<&str>,
) -> Verdict {
    let (Some(_), Some(stall_ms)) = (blocked_in, blocked_in_ms) else {
        return Verdict::NoBlockingCall;
    };
    if stall_ms < budget_ms {
        return Verdict::StallTooShort;
    }
    if sampled != Some("blocking") {
        return Verdict::WrongProvenance;
    }
    // The `+0x` marker is what distinguishes a load-relative offset from an
    // absolute address, so its absence is a leak, not a formatting nit.
    if !pc.is_some_and(|pc| pc.starts_with("+0x")) {
        return Verdict::NoRelativePc;
    }
    if !bt.is_some_and(|bt| bt.starts_with("+0x")) {
        return Verdict::NoBacktrace;
    }
    Verdict::Accepted
}

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

    use super::{assess, Verdict, LATENCY_OVERRUN_EVENT_ID};

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

    /// A record arrived with no `budget_ms`, so there is nothing to judge the
    /// stall against.
    const FAIL_NO_BUDGET: NonZeroU16 = fail_point!(1);
    /// The report did not name the syscall that spent the budget, so the
    /// boundary that fired was the wrong one.
    const FAIL_NO_BLOCKING_CALL: NonZeroU16 = fail_point!(2);
    /// The report describes some incidental overrun rather than the
    /// fixture's deliberate stall.
    const FAIL_STALL_TOO_SHORT: NonZeroU16 = fail_point!(3);
    /// The frame is not the blocking call's own.
    const FAIL_WRONG_PROVENANCE: NonZeroU16 = fail_point!(4);
    /// No program counter, or one that escaped absolute.
    const FAIL_NO_RELATIVE_PC: NonZeroU16 = fail_point!(5);
    /// The user-stack walk produced no frame at all.
    const FAIL_NO_BACKTRACE: NonZeroU16 = fail_point!(6);

    /// Set once an accepted overrun record has been observed on the
    /// diagnostic trail.
    static OVERRUN_SEEN: AtomicBool = AtomicBool::new(false);

    /// Set once the fixture's own audited `exit` has been observed, so the
    /// PASS fires on the *next* one — the shell's, typed by the runner only
    /// after the fixture's marker appeared. Exiting QEMU on the fixture's
    /// own exit would tear the run down with the last scripted line still
    /// owed, which the harness fails as an incomplete script.
    static FIXTURE_EXITED: AtomicBool = AtomicBool::new(false);

    /// The string value of `event`'s field `key`, if present.
    fn field_str<'e>(event: &Event<'e>, key: &str) -> Option<&'e str> {
        event.fields.iter().find_map(|field| {
            if field.key == key {
                match field.value {
                    FieldValue::Str(s) => Some(s),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// The unsigned value of `event`'s field `key`, if present.
    fn field_u64(event: &Event<'_>, key: &str) -> Option<u64> {
        event.fields.iter().find_map(|field| {
            if field.key == key {
                match field.value {
                    FieldValue::UnsignedInt(v) => Some(v),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// Gate on the diagnostic trail: replay everything to serial, judge each
    /// overrun record, and arm the PASS on an accepted one.
    struct Gate;

    impl Sink for Gate {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot → unlock → login → stall timeline.
            SerialSink::new().write_event(event);
            if event.id.0 != LATENCY_OVERRUN_EVENT_ID {
                return;
            }
            // The budget the record itself reports, so the assertion is
            // against what the kernel armed rather than a constant restated
            // here.
            let Some(budget_ms) = field_u64(event, "budget_ms") else {
                qemu_exit::exit_failure(FAIL_NO_BUDGET)
            };
            match assess(
                field_str(event, "blocked_in"),
                field_u64(event, "blocked_in_ms"),
                budget_ms,
                field_str(event, "sampled"),
                field_str(event, "pc"),
                field_str(event, "bt"),
            ) {
                Verdict::Accepted => OVERRUN_SEEN.store(true, Ordering::Release),
                // A report that arrived *wrong* is a distinct defect, so it
                // fails the run at once with its own code rather than leaving
                // an indistinguishable timeout.
                Verdict::NoBlockingCall => qemu_exit::exit_failure(FAIL_NO_BLOCKING_CALL),
                Verdict::StallTooShort => qemu_exit::exit_failure(FAIL_STALL_TOO_SHORT),
                Verdict::WrongProvenance => qemu_exit::exit_failure(FAIL_WRONG_PROVENANCE),
                Verdict::NoRelativePc => qemu_exit::exit_failure(FAIL_NO_RELATIVE_PC),
                Verdict::NoBacktrace => qemu_exit::exit_failure(FAIL_NO_BACKTRACE),
            }
        }
    }

    static GATE: Gate = Gate;

    /// Report PASS on the shell's audited `exit`, once an accepted overrun
    /// record has been seen.
    struct AuditObserver;

    impl Sink for AuditObserver {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(tairix_test_stalltrace::COMMAND) {
                FIXTURE_EXITED.store(true, Ordering::Release);
            } else if OVERRUN_SEEN.load(Ordering::Acquire) && FIXTURE_EXITED.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: AuditObserver = AuditObserver;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_stalltrace_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
            // The overrun record is emitted on the *diagnostic* stream, so
            // the record judge is the log sink; the audited `exit` that
            // completes the PASS chain is on the audit trail, so the
            // finisher is the audit sink. Both replay to serial.
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

#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(test)]
mod tests {
    use super::{assess, Verdict};

    /// The budget every case below is judged against, in milliseconds.
    const BUDGET_MS: u64 = 250;

    /// A complete record — the one the facility promises — is accepted.
    #[test]
    fn a_complete_overrun_record_is_accepted() {
        assert_eq!(
            assess(
                Some("waitset_wait"),
                Some(500),
                BUDGET_MS,
                Some("blocking"),
                Some("+0x0000000000004a1c"),
                Some("+0x0000000000001220,+0x00000000000008f0"),
            ),
            Verdict::Accepted
        );
    }

    /// Each missing or wrong piece is named distinctly, so a failing run
    /// says which part of the facility broke rather than only that it did.
    #[test]
    fn every_missing_piece_is_named_distinctly() {
        let complete = (
            Some("waitset_wait"),
            Some(500u64),
            Some("blocking"),
            Some("+0x0000000000004a1c"),
            Some("+0x0000000000001220"),
        );
        assert_eq!(
            assess(None, complete.1, BUDGET_MS, complete.2, complete.3, complete.4),
            Verdict::NoBlockingCall
        );
        assert_eq!(
            assess(complete.0, None, BUDGET_MS, complete.2, complete.3, complete.4),
            Verdict::NoBlockingCall,
            "a named call with no duration is as unusable as no name"
        );
        assert_eq!(
            assess(
                complete.0,
                Some(BUDGET_MS - 1),
                BUDGET_MS,
                complete.2,
                complete.3,
                complete.4
            ),
            Verdict::StallTooShort
        );
        assert_eq!(
            assess(
                complete.0,
                complete.1,
                BUDGET_MS,
                Some("running"),
                complete.3,
                complete.4
            ),
            Verdict::WrongProvenance
        );
        assert_eq!(
            assess(complete.0, complete.1, BUDGET_MS, complete.2, None, complete.4),
            Verdict::NoRelativePc
        );
        assert_eq!(
            assess(complete.0, complete.1, BUDGET_MS, complete.2, complete.3, None),
            Verdict::NoBacktrace
        );
    }

    /// An address without the `+0x` marker is an absolute one, which the
    /// record must never carry — so it is refused rather than accepted as a
    /// formatting variation.
    #[test]
    fn an_absolute_address_is_refused_not_tolerated() {
        assert_eq!(
            assess(
                Some("waitset_wait"),
                Some(500),
                BUDGET_MS,
                Some("blocking"),
                Some("0xffff000000004a1c"),
                Some("+0x0000000000001220"),
            ),
            Verdict::NoRelativePc
        );
        assert_eq!(
            assess(
                Some("waitset_wait"),
                Some(500),
                BUDGET_MS,
                Some("blocking"),
                Some("+0x0000000000004a1c"),
                Some("0xffff000000001220"),
            ),
            Verdict::NoBacktrace
        );
    }

    /// A stall exactly at the budget is the boundary the kernel reports on,
    /// so it must be accepted rather than treated as short.
    #[test]
    fn a_stall_exactly_at_the_budget_is_the_provoked_one() {
        assert_eq!(
            assess(
                Some("waitset_wait"),
                Some(BUDGET_MS),
                BUDGET_MS,
                Some("blocking"),
                Some("+0x0"),
                Some("+0x0"),
            ),
            Verdict::Accepted
        );
    }
}
