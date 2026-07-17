//! Architecture-neutral kernel panic policy.
//!
//! The architecture port owns the `#[panic_handler]` attribute itself
//! (Stage 3) — `kernel/core` cannot, because in a host-test build
//! `std` already supplies a handler and registering a second one is a
//! link error. The arch port's `#[panic_handler]` is therefore a
//! one-liner that delegates here:
//!
//! ```ignore
//! #[panic_handler]
//! fn tairix_panic(info: &core::panic::PanicInfo<'_>) -> ! {
//!     tairix_kernel_core::handle_panic(info, &PANIC_CTX)
//! }
//! ```
//!
//! `PANIC_CTX` is the [`PanicContext`] the arch port builds at boot
//! and stores in a once-initialised `static` (the per-CPU bootstrap
//! exception called out by — *"No global mutable static
//! beyond the per-CPU bootstrap area"*).
//!
//! [`handle_panic`] does the rest: it logs a structured
//! [`AuditEvent::Panic`] record with the failing file, line, column,
//! and the current CPU id, then calls [`KernelArch::halt`]. It
//! **never** silently resets — the `!` return type and the `halt`
//! contract together encode that.
//!
//! # Testability
//!
//! `core::panic::PanicInfo` has no public constructor on stable Rust,
//! so [`handle_panic`] forwards the location into [`panic_dump`], an
//! inner function that takes `Option<&core::panic::Location<'_>>`
//! directly. Host-side tests drive [`panic_dump`] with a
//! [`core::panic::Location::caller`]; integration tests cover the
//! `panic!`-to-`handle_panic` round-trip end-to-end.

use core::panic::PanicInfo;

use tairix_log::{log, Event, Field, Level, Sink};

use crate::audit::AuditEvent;
use crate::bootinfo::KernelArch;

/// Context the architecture port hands to [`handle_panic`].
///
/// The struct is borrowed read-only by the panic path, so the arch
/// port can store it inside a `kernel/sync::Once`-protected `static`
/// without taking any locks at panic time (the panic path never
/// blocks).
pub struct PanicContext<'a, A: KernelArch + 'static> {
    /// Architecture port instance.
    pub arch: &'a A,
    /// Sink that receives the [`AuditEvent::Panic`] record.
    ///
    /// In production this is the same `audit_sink` passed to
    /// [`crate::kernel_main`]; host tests reuse `TestSink`.
    pub audit_sink: &'a (dyn Sink + Sync),
}

impl<'a, A: KernelArch> PanicContext<'a, A> {
    /// Construct a panic context.
    #[must_use]
    pub fn new(arch: &'a A, audit_sink: &'a (dyn Sink + Sync)) -> Self {
        Self { arch, audit_sink }
    }
}

/// Dump the panic context to the audit sink and halt the boot CPU.
///
/// The function never returns: the `!` return type and the
/// [`KernelArch::halt`] contract together guarantee that the kernel
/// does not silently reset (Stage 2 deliverables).
///
/// # Emitted fields
///
/// | Key      | Value                                                    |
/// | -------- | -------------------------------------------------------- |
/// | `cpu`    | Decimal CPU id returned by `arch.current_cpu()`.         |
/// | `file`   | `info.location().file()` or `"<unknown>"`.               |
/// | `line`   | Decimal `info.location().line()` or `"0"`.               |
/// | `column` | Decimal `info.location().column()` or `"0"`.             |
///
/// The format is part of the audit contract and
/// is asserted by the integration tests.
pub fn handle_panic<A: KernelArch>(info: &PanicInfo<'_>, ctx: &PanicContext<'_, A>) -> ! {
    panic_dump(info.location(), ctx)
}

/// Audit-and-halt path shared by [`handle_panic`] and the host-side
/// tests.
///
/// Split out so tests can drive the full code path on stable Rust
/// without needing to construct a [`core::panic::PanicInfo`] (which
/// has no public constructor).
pub fn panic_dump<A: KernelArch>(
    location: Option<&core::panic::Location<'_>>,
    ctx: &PanicContext<'_, A>,
) -> ! {
    let cpu = ctx.arch.current_cpu();

    // Stack-resident formatting buffers. No allocation per
    // — the panic path must not depend on the heap,
    // which may itself be the source of the panic.
    let mut cpu_buf = [0u8; 11];
    let mut line_buf = [0u8; 11];
    let mut col_buf = [0u8; 11];

    let cpu_str = format_u32(cpu, &mut cpu_buf);
    let (file_str, line_str, col_str) = match location {
        Some(loc) => (
            loc.file(),
            format_u32(loc.line(), &mut line_buf),
            format_u32(loc.column(), &mut col_buf),
        ),
        None => ("<unknown>", "0", "0"),
    };

    let fields = [
        Field {
            key: "cpu",
            value: tairix_log::FieldValue::Str(cpu_str),
        },
        Field {
            key: "file",
            value: tairix_log::FieldValue::Str(file_str),
        },
        Field {
            key: "line",
            value: tairix_log::FieldValue::Str(line_str),
        },
        Field {
            key: "column",
            value: tairix_log::FieldValue::Str(col_str),
        },
    ];

    log(
        ctx.audit_sink,
        &Event {
            level: Level::Error,
            id: AuditEvent::Panic.id(),
            message: AuditEvent::Panic.message(),
            fields: &fields,
        },
    );

    ctx.arch.halt();
}

/// Format a `u32` into `buf` as decimal ASCII and return a borrowed
/// `&str` over the populated suffix.
///
/// Allocation-free; the buffer must be at least 11 bytes (the longest
/// `u32` decimal is `"4294967295"`). The function is total: every
/// `u32` input yields a valid ASCII string. Used only by the panic
/// path so the panic handler cannot itself panic on an allocator that
/// is already wedged.
fn format_u32(value: u32, buf: &mut [u8; 11]) -> &str {
    if value == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("0");
    }
    let mut n = value;
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_arch::{TestArch, HALT_SENTINEL};
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::string::String;
    use core::panic::Location;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn format_u32_examples() {
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(0, &mut buf), "0");
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(1, &mut buf), "1");
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(4_294_967_295, &mut buf), "4294967295");
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(42, &mut buf), "42");
    }

    fn drive_panic_dump<F>(make_location: F) -> (TestArch, &'static TestSink)
    where
        F: FnOnce() -> &'static Location<'static>,
    {
        let arch = TestArch::with_cpus(2);
        arch.set_current_cpu(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let loc = make_location();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            panic_dump(Some(loc), &ctx);
        }));
        let err = result.expect_err("halt path must panic via TestArch");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&'static str>().copied())
            .unwrap_or("");
        assert!(msg.contains(HALT_SENTINEL), "halt sentinel missing: {msg}");
        (arch, sink)
    }

    #[track_caller]
    fn caller_location() -> &'static Location<'static> {
        Location::caller()
    }

    #[test]
    fn panic_dump_emits_one_record_with_documented_fields() {
        let (arch, sink) = drive_panic_dump(caller_location);

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "expected exactly one panic record");
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
        // `track_caller` propagates through `drive_panic_dump`'s
        // `make_location()` call, so the file is whichever source
        // contains the `make_location` invocation. Asserting on a
        // specific file path is fragile across host targets; assert
        // the field exists and is non-empty instead.
        assert!(field("file").is_some_and(|s| !s.is_empty()));
        assert!(field("line").is_some_and(|s| s.parse::<u32>().is_ok()));
        assert!(field("column").is_some_and(|s| s.parse::<u32>().is_ok()));

        assert_eq!(arch.halt_count(), 1);
    }

    #[test]
    fn panic_dump_handles_missing_location() {
        let arch = TestArch::with_cpus(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("file"), Some("<unknown>"));
        assert_eq!(field("line"), Some("0"));
        assert_eq!(field("column"), Some("0"));
        assert_eq!(arch.halt_count(), 1);
    }
}
