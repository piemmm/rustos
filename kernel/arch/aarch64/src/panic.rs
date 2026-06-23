//! Panic-handler bridge for the aarch64 boot binaries.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary
//! declares its own one-liner that forwards to
//! [`handle_panic_via_serial`]. The bridge emits a single best-effort
//! record through the currently-configured console and parks the CPU
//! forever (`AGENTS.md` §2 — fail closed, never silently reset; §2.9 —
//! no panic recovery in production paths).
//!
//! Like the riscv64 bridge, this does not route through
//! `kernel_core::handle_panic`: the boot slice has no post-init arch
//! handle to publish for a richer panic context, and adding that
//! machinery now would be unused (`AGENTS.md` §2.3 — no bloat). A panic
//! before a test's success finisher therefore parks the CPU, the QEMU
//! integration test times out, and the harness reports `Outcome::Timeout`
//! — the documented fail-loud behaviour (`AGENTS.md` §7).

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
    // off the producer's hot path — `AGENTS.md` §2.16 / §20). Blocking is
    // fine here: the CPU is about to stop running the dispatch loop.
    crate::serial::flush_serial_blocking();
    let mut w = ConsoleWriter;
    let _ = writeln!(w, "[rustos-kernel] aarch64 panic: {info}");
    halt_current_cpu()
}
