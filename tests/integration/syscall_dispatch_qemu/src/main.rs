//! Stage 2.7 follow-up (f6) QEMU integration test: drive
//! `Dispatcher::dispatch` under the freestanding `x86_64-unknown-none`
//! target and observe the resulting `AuditEvent::SyscallInvoked`
//! record before flipping `qemu_exit::exit_success`.
//!
//! ## What this test asserts
//!
//! The production `rustos-kernel` boot pipeline runs through
//! `rustos_kernel::boot` until `AuditEvent::BootCompleted`
//! (`EventId(4004)`) fires. The audit Sink that observes that event
//! then **synthesises** a Scheduler/CapTable/KernelSyscallHandlers/
//! Dispatcher quartet — independent of the production `KernelState`,
//! per `.junie/next-session-prompt.md`'s design hint — and drives the
//! dispatcher twice:
//!
//! 1. `(cap_query, CAP_TIME_SET)` — the synthesised calling task is
//!    granted `CAP_TIME_SET`, so the call must return `Ok(1)`. The
//!    `cap_query` syscall is declared `audit: false` in the abi-v1
//!    table (`lib/abi/src/syscalls.rs`) — capability probes are a
//!    "pure observer" that must not drown the audit log. The synthesised
//!    entry point therefore inspects the `SyscallResult` return value
//!    directly: that **is** the §5.4.4 evidence path for an audit-false
//!    syscall.
//!
//! 2. `(exit, 0)` — declared `audit: true`, so a successful dispatch
//!    emits one `AuditEvent::SyscallInvoked` (`EventId(5000)`) record
//!    through the synthesised inner audit sink. The sink counts those
//!    records; the entry point asserts the count reaches one before
//!    calling `qemu_exit::exit_success`.
//!
//! Any failure on either leg — wrong return code, missing audit
//! record, or a dispatcher error — drops out through
//! `qemu_exit::exit_failure`. The host-side `tools/qemu::Runner`
//! then registers the run as `Outcome::Fail` with the serial log
//! attached.
//!
//! ## How it differs from the production `rustos-kernel` binary
//!
//! The boot pipeline is unchanged: this bin reuses
//! `rustos_kernel::boot` verbatim. The audit Sink the bin installs
//! is the only divergence, and the dispatcher invocations happen
//! **after** `BootCompleted` — they never collide with the production
//! syscall path, which fail-closes when no caller context is available
//! (Stage 2.7 follow-up (f5), see
//! `kernel/rustos-kernel/src/dispatch.rs`'s `production_dispatch`).
//!
//! ## `test-hooks` Cargo feature
//!
//! The synthesised quartet only compiles under
//! `#[cfg(feature = "test-hooks")]`. The feature is on by default for
//! this crate so `cargo build -p rustos-test-syscall-dispatch-qemu`
//! and `cargo xtask test --qemu` do the obvious thing; release builds
//! that enable it are rejected by the `compile_error!` guard below
//! (AGENTS.md §1 — no hacks; §5.4.5 — fail closed). `cargo deny check`
//! additionally forbids the production `rustos-kernel` crate from
//! ever growing a `test-hooks` feature (see `deny.toml`).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// The bin crate's freestanding configuration needs `alloc::sync::Arc`
// to construct the synthesised `BinArch` handle (mirrors the
// production bin's allocator-backed `Arc<BinArch>` in
// `kernel/rustos-kernel/src/boot.rs`). Pulling `extern crate alloc`
// in at the crate root is the documented way to expose it under
// `#![no_std]` (`AGENTS.md` §15.10 — the `unused_extern_crates`
// warning on the host build is justified: the synthesised quartet is
// `cfg`-gated to `target_os = "none"`, so on host this declaration
// is unused but mandatory under the bare-metal cfg).
#[cfg(itest_x86_64)]
#[allow(unused_extern_crates)]
extern crate alloc;

// AGENTS.md §1 — test affordances must never reach a release binary.
// `test-hooks` is on by default for this crate (see `Cargo.toml`);
// release builds re-running with the feature on are a configuration
// error rather than a soundness failure, but we belt-and-brace by
// failing the build outright. `cargo build --release -p rustos-test-
// syscall-dispatch-qemu --features test-hooks` therefore fails at
// compile time with the message below; the `cargo deny check` rule
// in `deny.toml` enforces the same posture for the production
// `rustos-kernel` crate, which is forbidden from ever growing a
// `test-hooks` feature.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-syscall-dispatch-qemu: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use rustos_abi::{CapabilityId, SyscallNumber};
    use rustos_arch_x86_64::qemu_exit;
    use rustos_caps::CapabilitySet;
    use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, BinArch, BumpAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_kernel_core::{IrqRouting, KernelSyscallHandlers};
    use rustos_kernel_irq::{IrqTable, UnsupportedController};
    use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig, TaskAction};
    use rustos_kernel_sec::{CapTable, TaskCapabilities, TaskId as SecTaskId, UserId};
    use rustos_kernel_syscall::{CallerContext, Dispatcher, RawArgs};
    use rustos_log::{Event, EventId, Sink};
    use rustos_sync::RwLock;

    // --- Bump-allocator-backed `#[global_allocator]` ---------------
    //
    // Mirrors the production `rustos-kernel` bin's allocator
    // declaration (`#[global_allocator]` is a per-binary attribute,
    // see `kernel/rustos-kernel/Cargo.toml`'s top-level rationale).

    /// Static heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint
    /// slices via an `AtomicUsize` cursor; the storage itself is
    /// otherwise immutable from any other call site.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: as for the production bin's `ALLOCATOR` — the
    /// page-aligned `HEAP` static outlives the binary, the allocator
    /// is the only consumer.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Stable audit identifiers --------------------------------

    /// `EventId` emitted by `kernel_core::kernel_main` when every init
    /// phase completed successfully. Pinned by the
    /// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// `EventId` emitted by `kernel/syscall::Dispatcher::dispatch`
    /// when a security-relevant syscall passes every check and is
    /// forwarded to its handler. Pinned by the
    /// `ids_are_frozen_and_in_range` test in
    /// `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `SyscallInvoked` records the inner audit sink has
    /// observed during the synthesised-quartet test. The outer audit
    /// Sink starts the test on `BootCompleted` and inspects this
    /// counter immediately afterwards; no other call site touches it.
    static SYSCALL_INVOKED_COUNT: AtomicU32 = AtomicU32::new(0);

    /// Set once the synthesised-quartet entry point has been driven,
    /// so a stray duplicate `BootCompleted` (which the catalogue
    /// disallows but the audit pipeline cannot statically prove)
    /// never re-enters the test logic. AGENTS.md §5.4.5 — fail closed.
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    // --- Audit observer Sinks ------------------------------------

    /// Inner audit sink consumed by the synthesised Dispatcher.
    ///
    /// Forwards every event to [`SERIAL_SINK`] so the QEMU serial
    /// transcript records the full dispatcher timeline, and increments
    /// [`SYSCALL_INVOKED_COUNT`] for each `SyscallInvoked` record.
    struct InnerAuditSink;

    impl Sink for InnerAuditSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == SYSCALL_INVOKED_EVENT_ID {
                SYSCALL_INVOKED_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Outer audit sink installed via [`rustos_kernel::boot`].
    ///
    /// Forwards every event through the serial sink, and on observing
    /// [`BOOT_COMPLETED_EVENT_ID`] drives [`run_synthesised_test`]
    /// before flipping QEMU's `isa-debug-exit` device. The handler
    /// never returns: it either passes through [`qemu_exit::exit_success`]
    /// (on test success) or [`qemu_exit::exit_failure`] (on any
    /// dispatcher mismatch).
    struct BootCompletedDispatchSink;

    impl Sink for BootCompletedDispatchSink {
        fn write_event(&self, event: &Event<'_>) {
            // Always replay through the serial sink so the QEMU
            // serial transcript captures the full boot + dispatch
            // timeline.
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID
                && TEST_DRIVEN
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                run_synthesised_test();
            }
        }
    }

    static AUDIT_SINK: BootCompletedDispatchSink = BootCompletedDispatchSink;
    static INNER_SINK: InnerAuditSink = InnerAuditSink;

    // --- Synthesised quartet -------------------------------------

    /// Build a fresh Scheduler/CapTable/KernelSyscallHandlers/
    /// Dispatcher quartet on the stack and drive two syscalls through
    /// it.
    ///
    /// The function never returns: it tail-calls
    /// [`qemu_exit::exit_success`] on success or
    /// [`qemu_exit::exit_failure`] on any mismatch.
    fn run_synthesised_test() -> ! {
        // 1. Synthesised arch: shares the production `BinArch` so the
        //    `KernelArch::monotonic_ns` contract is honoured (the
        //    `clock_get` handler is not exercised by this test, but
        //    using `BinArch` keeps the type bounds on `Scheduler<A>`
        //    and `KernelSyscallHandlers<A>` aligned with the production
        //    wiring path — AGENTS.md §2.4, no parallel arch types).
        //
        //    The arch instance constructed here is independent of the
        //    production `KernelState`'s arch: `boot()` has already
        //    finished its BSP bring-up by the time `BootCompleted`
        //    fires, so re-running `X86_64Arch::new` (a pure constructor
        //    that records the BSP triple without touching MSRs) is
        //    sound and is the cleanest way to keep the test isolated
        //    from `KernelState`'s borrows.
        use rustos_arch_x86_64::apic_timer::Calibration;
        use rustos_arch_x86_64::kernel_arch::X86_64Arch;
        use rustos_arch_x86_64::percpu::MAX_CPUS;

        let mut cpu_to_lapic: [Option<u8>; MAX_CPUS] = [None; MAX_CPUS];
        // The BSP LAPIC id is recorded during `boot::try_boot` step 5;
        // we don't need its exact value here — `X86_64Arch::new`
        // accepts any `(boot_cpu, bsp_lapic_id, map)` triple where the
        // map contains the BSP. Using `0` keeps the test deterministic
        // and avoids reading the LAPIC ID register a second time.
        cpu_to_lapic[0] = Some(0);
        let Ok(arch_inner) = X86_64Arch::new(0, 0, cpu_to_lapic) else {
            qemu_exit::exit_failure();
        };
        // A synthetic 1 GHz TSC calibration. `monotonic_ns` is not
        // exercised by either dispatched syscall (`cap_query` and
        // `exit`), so the exact frequency is irrelevant to the
        // assertion — but using a well-formed `Calibration` keeps the
        // arch wrapper in the same shape the production code uses
        // (AGENTS.md §2.2 — no parallel construction paths).
        let calibration = Calibration {
            ticks_per_second: 100_000,
            initial_count: 100,
            period_micros: 1_000,
            tsc_per_second: 1_000_000_000,
        };
        // `IrqRouting::unsupported()` keeps the synthesised arch
        // free of any programmable interrupt controller — none of the
        // dispatched syscalls (cap_query / exit) touch the IRQ path,
        // and a real routing would require a live `IoApicController`
        // we deliberately do not stand up in this test. The host
        // tests in `kernel/rustos-kernel::arch_wrapper` cover the
        // `IrqRouting::unsupported` shape.
        let arch = alloc::sync::Arc::new(BinArch::new(
            arch_inner,
            calibration,
            IrqRouting::unsupported(),
        ));

        // 2. Scheduler with a single CPU (matches `boot::try_boot`).
        let cfg = SchedulerConfig::defaults_for(1);
        let Ok(sched) = Scheduler::new(cfg, arch.clone()) else {
            qemu_exit::exit_failure();
        };

        // 3. Spawn a no-op task on CPU 0 so `Scheduler::exit(task_id)`
        //    has a real entry to remove. The body is never executed —
        //    the scheduler hot loop is not run — but the closure
        //    still has to type-check (`AGENTS.md` §15.1 — no stubs:
        //    the body must be a real `TaskAction`-returning closure).
        let Ok(task_id) = sched.spawn(0, Priority::Normal, |_| TaskAction::Exit) else {
            qemu_exit::exit_failure();
        };

        // 4. CapTable registry with the calling task's record. The
        //    grant carries CAP_TIME_SET so `(cap_query, CAP_TIME_SET)`
        //    returns `Ok(1)`.
        let mut grant = CapabilitySet::empty();
        grant.insert(CapabilityId::TIME_SET);
        let task_caps =
            TaskCapabilities::derive(SecTaskId(task_id), UserId(1000), grant, grant, &INNER_SINK);
        let cap_table = RwLock::new(CapTable::new());
        // Insert a record using the same grant so `KernelSyscallHandlers::exit`
        // — which calls `CapTable::remove(task_id)` before
        // `Scheduler::exit` — sees a real entry to evict.
        let table_record =
            TaskCapabilities::derive(SecTaskId(task_id), UserId(1000), grant, grant, &INNER_SINK);
        let _ = cap_table.write().insert(table_record);

        // 5. Synthesised handlers + dispatcher. The dispatcher and the
        //    handlers share the same inner audit sink so the
        //    `SyscallInvoked` record this test relies on actually
        //    reaches `SYSCALL_INVOKED_COUNT`.
        // 4.5. IRQ subsystem state. The synthesised dispatcher will
        //      not exercise `irq_bind` / `irq_wait` in this test, but
        //      `KernelSyscallHandlers::new` requires both borrows so
        //      a successful `exit` syscall observes the same struct
        //      shape it does in production. `IrqTable::new(0)` is the
        //      conservative default; `UnsupportedController` rejects
        //      every `fire` with `MaskError::Unsupported` — exactly
        //      the behaviour `kernel/core::init` installs.
        let irq_table = IrqTable::new(0);
        let irq_controller = UnsupportedController;

        let handlers: KernelSyscallHandlers<'_, BinArch> = KernelSyscallHandlers::new(
            &sched,
            &cap_table,
            &*arch,
            &INNER_SINK,
            &irq_table,
            &irq_controller,
        );
        let dispatcher = Dispatcher::new(&handlers, &INNER_SINK);
        let caller = CallerContext {
            task_id: SecTaskId(task_id),
            caps: &task_caps,
        };

        // 6. (cap_query, CAP_TIME_SET) — observed via the return
        //    value. `cap_query` is `audit: false`, so no
        //    `SyscallInvoked` record is emitted; the dispatcher's
        //    `Ok(1)` *is* the §5.4.4 evidence path for an unaudited
        //    syscall (AGENTS.md §5.4.4 — "every security-relevant
        //    decision"; capability probes are explicitly carved out
        //    in `lib/abi/src/syscalls.rs`).
        let mut args = RawArgs::ZERO;
        args.0[0] = u64::from(CapabilityId::TIME_SET.as_u16());
        match dispatcher.dispatch(&caller, SyscallNumber::CAP_QUERY.as_u16(), args) {
            Ok(1) => {}
            // Any other outcome (wrong return value, or an `Errno`)
            // means the production dispatch path has regressed; fail
            // the test loud, AGENTS.md §7 (no flaky tests).
            _ => qemu_exit::exit_failure(),
        }
        // The unaudited path must leave `SYSCALL_INVOKED_COUNT`
        // unchanged. Inspecting it here catches a future regression
        // that accidentally flips `cap_query`'s `audit` flag —
        // AGENTS.md §9 (`abi-v1` immutable once shipped).
        if SYSCALL_INVOKED_COUNT.load(Ordering::Acquire) != 0 {
            qemu_exit::exit_failure();
        }

        // 7. (exit, 0) — observed via `SyscallInvoked`. `exit` is
        //    `audit: true`, so the dispatcher emits exactly one
        //    `SyscallInvoked` record on a successful dispatch.
        let mut exit_args = RawArgs::ZERO;
        // `i32` argument validator requires sign-extension; `0`
        // sign-extends to all-zero, which `RawArgs::ZERO` already
        // satisfies, but we leave the assignment explicit so a future
        // re-targeting of the test to a non-zero exit code does not
        // accidentally trip `Errno::OutOfRange`.
        exit_args.0[0] = 0;
        match dispatcher.dispatch(&caller, SyscallNumber::EXIT.as_u16(), exit_args) {
            Ok(_) => {}
            // Any error — whether a typed `Errno::NotFound` /
            // `OutOfRange` or anything else — fails the test.
            Err(_) => qemu_exit::exit_failure(),
        }

        // 8. Audit-record assertion. Exactly one `SyscallInvoked` —
        //    no more (no spurious duplicates), no less (the audit
        //    pipeline actually delivered the record).
        if SYSCALL_INVOKED_COUNT.load(Ordering::Acquire) != 1 {
            qemu_exit::exit_failure();
        }

        qemu_exit::exit_success();
    }

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `rustos_kernel::panic_ctx`.
    ///
    /// A panic anywhere in the boot path (including inside the
    /// synthesised quartet) routes through `SERIAL_SINK` for the
    /// transcript and then halts via `kernel_arch::halt`; the
    /// integration harness reports `Outcome::Timeout`. This is the
    /// documented fail-loud behaviour for AGENTS.md §7 (no flaky
    /// tests).
    #[panic_handler]
    fn rustos_test_syscall_dispatch_qemu_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// Forwards to [`rustos_kernel::boot`] with the production COM1
    /// log sink and our audit-observer sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Stub when the test-hooks feature is off ----------------------
//
// The synthesised quartet only compiles when `feature = "test-hooks"`
// is on. Disabling it leaves the bin as a no-op host stub so a layout
// sanity check (`cargo build --no-default-features -p
// rustos-test-syscall-dispatch-qemu`) still builds — AGENTS.md §1
// (no hacks: a disabled test must compile cleanly).
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    // No audit observer, no dispatch, no QEMU exit affordance: the
    // run will time out under `tools/qemu::Runner`, which is the
    // correct fail-loud signal for "test-hooks feature was disabled
    // for a QEMU enrolment that needs it". AGENTS.md §7 — no flaky
    // tests; the timeout is deterministic.
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on
        // x86_64 (`AGENTS.md` §2.9). Looping defends against spurious
        // wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn rustos_test_syscall_dispatch_qemu_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
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
// AGENTS.md §15.10: this `#[allow]` is justified — the freestanding
// configuration declares `#![no_main]`, but the host configuration
// needs an unused stub so `cargo build` for the host target does not
// complain about the absent `fn main`. The host stub `fn main` above
// already covers that; this helper exists only to keep the host
// build's symbol table layout in line with `kernel_arch_boot`'s.
fn _suppress_no_main() {}
