//! Panic-handler bridge for the aarch64 boot binaries.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary
//! declares its own one-liner that forwards to
//! [`handle_panic_via_serial`]. The bridge emits a single best-effort
//! record through the currently-configured console and parks the CPU
//! forever (fail closed, never silently reset; —
//! no panic recovery in production paths).
//!
//! This is the minimal park-on-panic helper the freestanding QEMU
//! integration-test kernels use: a panic before a test's success finisher
//! parks the CPU, the QEMU test times out, and the harness reports
//! `Outcome::Timeout` — the documented fail-loud behaviour. The
//! *production* kernel does route its panic through
//! `tairix_kernel_core::handle_panic` (a register snapshot + a bounded
//! backtrace) via the bin-crate `panic_ctx` bridge; this helper is the
//! test-harness path, not that one.

use core::fmt::Write as _;
use core::panic::PanicInfo;

use crate::kernel_arch::halt_current_cpu;
use crate::serial::ConsoleWriter;

/// Shared `#[panic_handler]` body for the aarch64 boot binaries.
///
/// Always returns `!`: emits one record on the configured console, then
/// parks the CPU via [`halt_current_cpu`].
pub fn handle_panic_via_serial(info: &PanicInfo<'_>) -> ! {
    // Flush the buffered serial ring first so the diagnostic context
    // that led up to the panic reaches a serial capture before the panic
    // record and the park (`crate::serial::SerialSink` buffers UART output
    // off the producer's hot path). Blocking is
    // fine here: the CPU is about to stop running the dispatch loop.
    crate::serial::flush_serial_blocking();
    // The running CPU's dense id, so a multi-core post-mortem knows which
    // core faulted. Reading `MPIDR_EL1` has no side effects and is safe
    // even mid-panic.
    let cpu = crate::smp::current_cpu_index();
    let mut w = ConsoleWriter;
    // A loud, unmistakable multi-line banner. A kernel panic halts the
    // offending CPU with no recovery (fail closed), so the record must
    // carry everything a post-mortem needs: which CPU, and the panic
    // message plus source location that `PanicInfo`'s `Display` already
    // formats (`file:line:col` and the message — for an allocation failure
    // that message is the requested byte count). Terse and factual.
    let _ = writeln!(
        w,
        "\n==================== TAIRiX KERNEL PANIC ===================="
    );
    let _ = writeln!(w, "[tairix-kernel] aarch64 panic on CPU {cpu}: {info}");
    let _ = writeln!(
        w,
        "CPU {cpu} halted; the kernel is non-recoverable in production."
    );
    let _ = writeln!(
        w,
        "============================================================="
    );
    halt_current_cpu()
}
