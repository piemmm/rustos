//! Panic-handler bridge for the aarch64 boot binaries.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary
//! declares its own one-liner that forwards to
//! [`handle_panic_via_serial`]. The bridge emits a single best-effort
//! record through the PL011 console and parks the CPU forever
//! (`AGENTS.md` §2 — fail closed, never silently reset; §2.9 — no
//! panic recovery in production paths).
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
use crate::serial::Pl011Writer;

/// Shared `#[panic_handler]` body for the aarch64 boot binaries.
///
/// Always returns `!`: emits one record on the PL011 console, then parks
/// the CPU via [`halt_current_cpu`].
pub fn handle_panic_via_serial(info: &PanicInfo<'_>) -> ! {
    let mut w = Pl011Writer;
    let _ = writeln!(w, "[rustos-kernel] aarch64 panic: {info}");
    halt_current_cpu()
}
