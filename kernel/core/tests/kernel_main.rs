//! Integration test for `rustos_kernel_core::kernel_main` and
//! `rustos_kernel_core::handle_panic`.
//!
//! Drives the architecture-neutral kernel entry on the host with a
//! [`TestArch`] and a [`TestSink`], then asserts:
//!
//! 1. The init order documented in `docs/src/architecture/kernel.md`
//!    (`log → mem → sec → sched → ipc`) is observed verbatim through
//!    the audit-log event stream.
//! 2. A successful boot ends in [`AuditEvent::BootCompleted`] followed
//!    by exactly one call to [`KernelArch::halt`].
//! 3. A failure in any phase logs [`AuditEvent::PhaseFailed`] with the
//!    documented `phase` and `cause` fields and halts — never silently
//!    resets (`AGENTS.md` §2 / Stage 2 deliverables).
//! 4. The panic helper (`handle_panic`) emits exactly one
//!    [`AuditEvent::Panic`] record carrying the failing CPU id and
//!    source location, then halts.
//!
//! These checks are the executable spec the issue brief asks for.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use rustos_kernel_core::test_arch::{TestArch, HALT_SENTINEL};
use rustos_kernel_core::test_sink::TestSink;
use rustos_kernel_core::{kernel_main, panic_dump, AuditEvent, BootInfo, PanicContext, Phase};
use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};
use rustos_kernel_sched::SchedulerConfig;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::Level;

fn make_usable_map() -> BootMemoryMap {
    let mut map = BootMemoryMap::new();
    map.push(MemoryRegion {
        start: PhysAddr::new(0),
        length: (PAGE_SIZE as u64) * 64,
        kind: RegionKind::Usable,
    });
    map
}

fn leak_sink() -> &'static TestSink {
    Box::leak(Box::new(TestSink::new()))
}

fn drive_kernel_main<F>(setup: F) -> (Arc<TestArch>, &'static TestSink, &'static TestSink)
where
    F: FnOnce(&mut BootInfo<'static, TestArch>),
{
    let arch = Arc::new(TestArch::with_cpus(1));
    let log_sink = leak_sink();
    let audit_sink = leak_sink();

    let mut boot = BootInfo::new(
        0,
        1,
        "",
        make_usable_map(),
        IdentityTableBuilder::new(),
        SchedulerConfig::defaults_for(1),
        Arc::clone(&arch),
        log_sink,
        audit_sink,
        Level::Info,
    );
    setup(&mut boot);

    let arch_for_halt = Arc::clone(&arch);
    let result = catch_unwind(AssertUnwindSafe(move || {
        kernel_main(boot);
    }));
    let err = result.expect_err("kernel_main always halts via TestArch");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&'static str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains(HALT_SENTINEL),
        "kernel_main must reach halt, got panic: {msg}"
    );

    (arch_for_halt, log_sink, audit_sink)
}

#[test]
fn happy_path_runs_documented_init_order_and_halts() {
    let (arch, log_sink, audit_sink) = drive_kernel_main(|_| {});

    // The boot CPU halts exactly once (success path → trailing halt
    // documented in init.rs; Stage 2.7 will replace it).
    assert_eq!(
        arch.halt_count(),
        1,
        "kernel_main must call halt exactly once"
    );

    // Audit-channel events: BootStarted then BootCompleted, with no
    // intervening PhaseFailed. Sink routing is documented in
    // `kernel/core/src/audit.rs`.
    let audit_ids: Vec<u32> = audit_sink.event_ids();
    assert_eq!(
        audit_ids.first().copied(),
        Some(AuditEvent::BootStarted.id().0),
    );
    assert_eq!(
        audit_ids.last().copied(),
        Some(AuditEvent::BootCompleted.id().0),
    );
    assert!(
        !audit_ids.contains(&AuditEvent::PhaseFailed.id().0),
        "happy path must not emit PhaseFailed on the audit sink",
    );

    // Log-channel events: every PhaseStarted in the documented order,
    // each followed by exactly one PhaseReady, and no audit-class
    // lifecycle events leaking onto the diagnostic channel.
    let log_ids: Vec<u32> = log_sink.event_ids();
    assert!(
        !log_ids.contains(&AuditEvent::BootStarted.id().0)
            && !log_ids.contains(&AuditEvent::BootCompleted.id().0)
            && !log_ids.contains(&AuditEvent::PhaseFailed.id().0),
        "audit-class events must not appear on the log sink",
    );

    let phases: Vec<String> = log_sink
        .snapshot()
        .into_iter()
        .filter(|e| e.id == AuditEvent::PhaseStarted.id())
        .map(|e| e.fields[0].1.clone())
        .collect();
    let expected: Vec<&str> = Phase::ORDER.iter().map(|p| p.as_str()).collect();
    assert_eq!(phases, expected);

    let ready_count = log_sink
        .snapshot()
        .into_iter()
        .filter(|e| e.id == AuditEvent::PhaseReady.id())
        .count();
    assert_eq!(ready_count, Phase::ORDER.len());
}

#[test]
fn mem_phase_failure_logs_phase_failed_and_halts() {
    let (arch, _log, audit_sink) = drive_kernel_main(|boot| {
        // Empty memory map → FrameAllocator::new returns OutOfMemory.
        boot.memory_map = BootMemoryMap::new();
    });

    assert_eq!(arch.halt_count(), 1, "failure path must halt exactly once");

    let snapshot = audit_sink.snapshot();
    let failed = snapshot
        .iter()
        .find(|e| e.id == AuditEvent::PhaseFailed.id())
        .expect("PhaseFailed must be emitted on the audit sink");
    let field = |key: &str| {
        failed
            .fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(field("phase"), Some("mem"));
    assert_eq!(field("cause"), Some("mem_out_of_memory"));

    // BootCompleted must NOT be present — failure halts before it.
    assert!(snapshot
        .iter()
        .all(|e| e.id != AuditEvent::BootCompleted.id()));
}

#[test]
fn bad_bootinfo_fails_under_log_phase() {
    let (arch, _log, audit_sink) = drive_kernel_main(|boot| {
        boot.boot_cpu = 99; // out of range vs cpu_count = 1.
    });

    assert_eq!(arch.halt_count(), 1);
    let failed = audit_sink
        .snapshot()
        .into_iter()
        .find(|e| e.id == AuditEvent::PhaseFailed.id())
        .expect("PhaseFailed must be emitted on the audit sink");
    let field = |key: &str| {
        failed
            .fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(field("phase").as_deref(), Some("log"));
    assert_eq!(field("cause").as_deref(), Some("boot_cpu_out_of_range"));
}

#[track_caller]
fn caller_location() -> &'static core::panic::Location<'static> {
    core::panic::Location::caller()
}

#[test]
fn panic_helper_logs_documented_fields_and_halts() {
    let arch = TestArch::with_cpus(2);
    arch.set_current_cpu(1);
    let sink: &'static TestSink = leak_sink();
    let loc = caller_location();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let ctx = PanicContext::new(&arch, sink);
        panic_dump(Some(loc), &ctx);
    }));
    let err = result.expect_err("panic_dump must reach halt");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&'static str>().copied())
        .unwrap_or("");
    assert!(msg.contains(HALT_SENTINEL));

    let events = sink.snapshot();
    assert_eq!(events.len(), 1, "exactly one panic record");
    let ev = &events[0];
    assert_eq!(ev.id, AuditEvent::Panic.id());
    assert_eq!(ev.level, Level::Error);

    let field = |key: &str| {
        ev.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(field("cpu"), Some("1"));
    assert!(field("file").is_some_and(|s| !s.is_empty()));
    assert!(field("line").is_some_and(|s| s.parse::<u32>().is_ok()));
    assert!(field("column").is_some_and(|s| s.parse::<u32>().is_ok()));

    assert_eq!(arch.halt_count(), 1);
}
