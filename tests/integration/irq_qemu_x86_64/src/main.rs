//! Stage 4.D Item 2-tail.2 QEMU validation: exercise the full
//! x86_64 external-IRQ trap glue end-to-end against a real
//! (emulated) IO-APIC + PIT.
//!
//! ## What this test asserts
//!
//! The production `rustos-kernel` boot pipeline runs through
//! `rustos_kernel::boot` until `AuditEvent::BootCompleted`
//! (`EventId(4004)`) fires. The audit Sink that observes the event
//! hijacks the boot CPU before `kernel_main`'s trailing
//! `arch.halt()` and drives a real hardware-interrupt round-trip:
//!
//! 1. Read the published `rustos_kernel_irq::IrqTable` via
//!    `rustos_kernel::x86_64::arch_wrapper::published_irq_table` and the
//!    typed `IoApicController` via
//!    `rustos_kernel::x86_64::ioapic_controller::published_typed`.
//! 2. Look up the IDT vector assigned to **GSI 2** (the legacy
//!    IRQ-0 line under QEMU's PC/Q35 default `InterruptSourceOverride`
//!    `source = 0 → gsi = 2`) through
//!    `rustos_arch_x86_64::irq::global_routing().vector_for_gsi(2)`.
//! 3. Bind GSI 2 in the `IrqTable` for the synthesised
//!    `TaskId(0)`; the kernel boot pipeline programmed the line
//!    `masked = true`, so no spurious delivery has reached the LAPIC.
//! 4. Mask the legacy 8259 PIC (write `0xFF` to ports `0x21` and
//!    `0xA1`) so PIT pulses do not double-deliver through the
//!    legacy chain — QEMU's PIIX/Q35 firmware leaves the PIC in
//!    its power-on state by default.
//! 5. Unmask GSI 2 through `IoApicController::unmask`.
//! 6. Program PIT channel 0 in mode-0 (interrupt-on-terminal-count,
//!    one-shot) with a small reload value so the IRQ fires within
//!    a few hundred microseconds.
//! 7. `sti` to enable interrupts; spin-poll
//!    `IrqTable::try_wait_step` (with a generous deadline) until
//!    either `WaitStep::Ready` or `WaitStep::TimedOut` fires.
//! 8. `cli` and re-read the IO-APIC redirection-entry low half
//!    through `IoApicController::read_pin_low`; assert the mask bit
//!    (bit 16) is set — the load-bearing evidence that
//!    `IrqTable::fire` honoured the mask-before-wake invariant
//!    documented in `docs/src/security/irq.md`.
//!
//! Any deviation — missing `IrqTable` / controller, no vector bound
//! to GSI 2, `WaitStep::TimedOut` instead of `Ready`, mask bit
//! clear after the wake — flips `qemu_exit::exit_failure`. Only the
//! happy path reaches `qemu_exit::exit_success`.
//!
//! ## `test-hooks` Cargo feature
//!
//! The synthesised observer only compiles under
//! `#[cfg(feature = "test-hooks")]`. The feature is on by default
//! for this crate so `cargo build -p rustos-test-irq-qemu-x86-64`
//! and `cargo xtask test --qemu` do the obvious thing; release
//! builds that enable it are rejected by the `compile_error!` guard
//! below (no hacks; — fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// `alloc` is required by the freestanding configuration so the
// synthesised observer can build an `Arc<BinArch>` — same idiom the
// syscall-dispatch test uses. On host targets the declaration is
// unused (the freestanding cfg gates the module that consumes it).
#[cfg(itest_x86_64)]
#[allow(unused_extern_crates)]
extern crate alloc;

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-irq-qemu-x86-64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use rustos_abi::IrqHandle;
    use rustos_arch_x86_64::irq as arch_irq;
    use rustos_arch_x86_64::qemu_exit;
    use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::x86_64::arch_wrapper::published_irq_table;
    use rustos_kernel::x86_64::ioapic_controller::published_typed;
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_kernel_irq::WaitStep;
    use rustos_kernel_sec::TaskId as SecTaskId;
    use rustos_log::{Event, EventId, Sink};

    // --- Bump-allocator-backed `#[global_allocator]` ---------------

    /// Static heap for the bump allocator. Mirrors the production
    /// `rustos-kernel` binary's allocator wiring.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by the module-private `HEAP` static.
    ///
    /// SAFETY: the `HEAP` static outlives the binary; the allocator
    /// is the only consumer. Identical justification to the
    /// `syscall_dispatch_qemu` template.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Stable audit identifiers --------------------------------

    /// `EventId` emitted by `kernel_core::kernel_main` when every
    /// init phase completed successfully. Pinned by the
    /// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Set once the IRQ scenario has been driven, so a stray
    /// duplicate `BootCompleted` (which the catalogue disallows but
    /// the audit pipeline cannot statically prove) never re-enters
    /// the test logic. — fail closed.
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    // --- Test parameters ----------------------------------------

    /// GSI the test drives. QEMU's PIIX/Q35 firmware ships an MADT
    /// `InterruptSourceOverride { source: 0, gsi: 2 }` mapping the
    /// legacy ISA IRQ-0 (PIT channel 0) to GSI 2. The boot pipeline
    /// programs every IO-APIC pin masked, so GSI 2 has been left
    /// `masked = true` with a vector allocated from the
    /// `0x30..=0xFE` external-IRQ range.
    const PIT_GSI: u32 = 2;

    /// PIT channel-0 reload value. The PIT input frequency is the
    /// architectural 1.193182 MHz. `2000` ticks ≈ 1.68 ms — long
    /// enough that the unmask + `sti` sequence completes before
    /// the line asserts, short enough that the timeout-deadline
    /// loop never observes `WaitStep::TimedOut` in practice.
    const PIT_RELOAD: u16 = 2000;

    /// Polling deadline for the [`WaitStep`] loop. Expressed in
    /// nanoseconds against `KernelArch::monotonic_ns`. One second
    /// is three orders of magnitude longer than the PIT reload,
    /// which gives QEMU's interrupt scheduling plenty of slack
    /// without dragging the run out on a slow CI host.
    const WAIT_DEADLINE_NS: u64 = 1_000_000_000;

    // --- Port I/O helpers ----------------------------------------

    /// Issue an `outb` to `port`.
    ///
    /// SAFETY: x86 port I/O is `unsafe` because the architecture
    /// makes no guarantees about side-effects. The kernel runs in
    /// ring 0 throughout this test, so the instruction is privileged
    /// but otherwise well-defined; the call sites only ever target
    /// the PIT (`0x40..=0x43`) and the legacy PIC (`0x21`, `0xA1`)
    /// — both standard x86 platform devices with known semantics.
    #[inline]
    unsafe fn outb(port: u16, value: u8) {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags),
            );
        }
    }

    /// Mask every line on the legacy 8259 PIC pair.
    ///
    /// SAFETY: see [`outb`]. Writing `0xFF` to the master (`0x21`)
    /// and slave (`0xA1`) data ports sets the OCW1 IMR to "all
    /// masked", which is the standard way to disable the PIC when
    /// running under IO-APIC. QEMU's PIC-emulation respects the
    /// mask immediately; subsequent IRQ-0 deliveries route only
    /// through the IO-APIC (Intel SDM Vol 3A §11.4).
    fn mask_legacy_pic() {
        unsafe {
            outb(0x21, 0xFF);
            outb(0xA1, 0xFF);
        }
    }

    /// Program PIT channel 0 in mode 0 (interrupt-on-terminal-count,
    /// one-shot) with the supplied 16-bit reload.
    ///
    /// SAFETY: see [`outb`]. The command byte `0x30` selects
    /// channel 0, lobyte/hibyte access, mode 0, binary count — the
    /// canonical one-shot sequence (Intel 8254 datasheet, Section
    /// 6).
    fn arm_pit_channel0_one_shot(reload: u16) {
        unsafe {
            outb(0x43, 0x30);
            outb(0x40, (reload & 0xFF) as u8);
            #[allow(clippy::cast_possible_truncation)]
            outb(0x40, ((reload >> 8) & 0xFF) as u8);
        }
    }

    /// Enable maskable interrupts on the current CPU.
    ///
    /// SAFETY: the kernel's IDT is fully populated by `Phase::Irq`;
    /// every external-vector slot points at the asm trampoline in
    /// `external_irq.s`. The legacy PIC has been masked, the
    /// IO-APIC pins have been programmed by the boot pipeline. A
    /// stray IRQ at this point is routed through the same Rust
    /// dispatcher every production interrupt uses.
    #[inline]
    unsafe fn sti() {
        unsafe {
            core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        }
    }

    /// Disable maskable interrupts on the current CPU.
    ///
    /// SAFETY: `cli` only clears `EFLAGS.IF`; on x86_64 this is a
    /// well-defined privileged instruction with no other side
    /// effects.
    #[inline]
    unsafe fn cli() {
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
    }

    /// Park the CPU until the next interrupt fires.
    ///
    /// SAFETY: `hlt` requires CPL=0 (kernel ring) and waits for the
    /// next unmasked interrupt. After arming the PIT one-shot and
    /// enabling interrupts, the line is guaranteed to fire within
    /// the reload window; the `hlt` returns when the IRQ ISR exits.
    #[inline]
    unsafe fn hlt() {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }

    // --- Audit observer Sink -------------------------------------

    /// Outer audit sink installed via `rustos_kernel::boot`.
    ///
    /// Forwards every event through the serial sink, and on
    /// observing [`BOOT_COMPLETED_EVENT_ID`] drives
    /// [`run_irq_scenario`] before flipping QEMU's `isa-debug-exit`
    /// device. The handler never returns: it either passes through
    /// `qemu_exit::exit_success` (on test success) or
    /// `qemu_exit::exit_failure` (on any mismatch).
    struct BootCompletedIrqSink;

    impl Sink for BootCompletedIrqSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript
            // captures the full boot + IRQ timeline.
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID
                && TEST_DRIVEN
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                run_irq_scenario();
            }
        }
    }

    static AUDIT_SINK: BootCompletedIrqSink = BootCompletedIrqSink;

    // --- IRQ scenario -------------------------------------------

    /// Drive the full IRQ round-trip and exit through QEMU's
    /// debug-exit device.
    ///
    /// The function never returns: every path tail-calls
    /// `qemu_exit::exit_success` (on success) or
    /// `qemu_exit::exit_failure` (on any mismatch).
    fn run_irq_scenario() -> ! {
        // 1. Reach the published IrqTable + typed controller.
        let Some(table) = published_irq_table() else {
            qemu_exit::exit_failure();
        };
        let Some(controller) = published_typed() else {
            qemu_exit::exit_failure();
        };

        // 2. Resolve the IDT vector the boot pipeline assigned to
        //    GSI 2. The bound is `Some(_)` for every GSI in
        //    `0..max_redirection_entry + 1`; a `None` here would mean
        //    QEMU advertised an MADT without the standard 24-pin
        //    IO-APIC, which is an environment defect rather than a
        //    test failure — surface as `exit_failure` per.
        let Some(_vector) = arch_irq::global_routing().vector_for_gsi(PIT_GSI) else {
            qemu_exit::exit_failure();
        };

        // 3. Bind GSI 2 in the IrqTable. `TaskId(0)` is the
        //    synthesised caller — no real task runs in this test;
        //    the bind only needs an opaque owner.
        let owner = SecTaskId(0);
        let Ok(bind_outcome) = table.bind(PIT_GSI, owner) else {
            qemu_exit::exit_failure();
        };
        let handle: IrqHandle = bind_outcome.handle;

        // 4. Belt-and-braces: mask the legacy 8259 PIC so the PIT
        //    pulse only delivers through the IO-APIC.
        mask_legacy_pic();

        // 5. Unmask GSI 2 — boot pipeline left it masked.
        if controller.unmask(PIT_GSI).is_err() {
            qemu_exit::exit_failure();
        }

        // 6. Arm PIT channel 0 as a one-shot. The pulse will
        //    propagate through the IO-APIC → LAPIC → IDT vector →
        //    asm trampoline → `production_external_irq_dispatch`
        //    → `IrqTable::fire(2, controller)` → mask + ready=true
        //    → return → LAPIC EOI → `iretq`.
        arm_pit_channel0_one_shot(PIT_RELOAD);

        // 7. Enable interrupts and poll the WaitStep until either
        //    `Ready` or `TimedOut` fires. The boot pipeline has
        //    already wired `rustos_kernel::production_external_irq_dispatch`
        //    so the trap path is live.
        //
        //    `published_irq_table` returns a `&'static IrqTable` —
        //    no scheduler is involved here; we drive the table
        //    directly with `monotonic_ns` snapshots taken through
        //    `arch_wrapper`'s RDTSC path (which is what
        //    `KernelArch::monotonic_ns` would have used inside the
        //    syscall handler).
        // SAFETY: see `sti`.
        unsafe { sti() };

        let start_ns = rdtsc_ns();
        let deadline_ns = start_ns.saturating_add(WAIT_DEADLINE_NS);
        loop {
            let now_ns = rdtsc_ns();
            match table.try_wait_step(handle, owner, now_ns, deadline_ns) {
                WaitStep::Ready => break,
                WaitStep::Continue => {
                    // Park until the IRQ fires. Each `hlt` returns
                    // either because the PIT IRQ-0 wake delivered
                    // (the expected path) or because some other
                    // unmasked interrupt did (the LAPIC timer is
                    // installed and active — its null-callback ISR
                    // returns promptly). Either way the loop
                    // re-evaluates `try_wait_step`; the deadline
                    // bound prevents an infinite spin.
                    // SAFETY: see `hlt`.
                    unsafe { hlt() };
                }
                WaitStep::TimedOut => {
                    // SAFETY: see `cli`.
                    unsafe { cli() };
                    qemu_exit::exit_failure();
                }
                WaitStep::NotFound => {
                    unsafe { cli() };
                    qemu_exit::exit_failure();
                }
            }
        }

        // 8. Disable interrupts and re-read the IO-APIC redirection
        //    entry. The mask bit must be set — `IrqTable::fire`
        //    masked the line *before* setting `ready = true`, and
        //    the SeqCst fence inside `IoApicController::mask`
        //    guarantees that observation order (`docs/src/security/irq.md`).
        // SAFETY: see `cli`.
        unsafe { cli() };
        let Some(low) = controller.read_pin_low(PIT_GSI) else {
            qemu_exit::exit_failure();
        };
        if low & (1 << 16) == 0 {
            qemu_exit::exit_failure();
        }

        qemu_exit::exit_success();
    }

    /// Read the TSC and convert to nanoseconds against a synthetic
    /// 1 GHz frequency.
    ///
    /// The kernel's `KernelArch::monotonic_ns` reads RDTSC through
    /// the boot-measured `Calibration`; we cannot reach that
    /// `Calibration` from outside `KernelState`, so we use the
    /// synthetic 1 GHz conversion (1 tick ≈ 1 ns) for the
    /// `WaitStep` deadline maths. The deadline is generous enough
    /// (1 s vs. a sub-millisecond IRQ latency) that the actual TSC
    /// frequency does not matter for the pass/fail decision.
    fn rdtsc_ns() -> u64 {
        // SAFETY: RDTSC is unprivileged on every x86_64 CPU RustOS
        // supports; the instruction has no architectural side
        // effects beyond producing the timestamp value.
        let value: u64;
        unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!(
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
            value = (u64::from(hi) << 32) | u64::from(lo);
        }
        value
    }

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `rustos_kernel::x86_64::panic_ctx`.
    #[panic_handler]
    fn rustos_test_irq_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// Forwards to `rustos_kernel::boot` with the production COM1
    /// log sink and our audit-observer sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            rustos_log::Level::Info,
        )
    }
}

// --- Stub when the test-hooks feature is off ----------------------

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    // No audit observer, no IRQ scenario, no QEMU exit affordance:
    // the run will time out under `tools/qemu::Runner`, which is
    // the correct fail-loud signal for "test-hooks feature was
    // disabled for a QEMU enrolment that needs it".
    // — no flaky tests; the timeout is deterministic.
    loop {
        // SAFETY: `cli; hlt` is the documented parked-CPU sequence.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn rustos_test_irq_qemu_x86_64_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
