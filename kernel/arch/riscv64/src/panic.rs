//! Panic-handler bridge for the riscv64 boot binaries.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each binary
//! declares its own one-liner that forwards to
//! [`handle_panic_via_serial`]. The bridge emits a single best-effort
//! record through the SBI console and parks the hart forever
//! (fail closed, never silently reset; — no
//! panic recovery in production paths).
//!
//! Unlike the x86_64 bridge, this does not route through
//! `kernel_core::handle_panic`: the boot-to-`BootCompleted` slice has
//! no post-init arch handle to publish for a richer panic context, and
//! adding the `AtomicPtr<RiscvArch>` dance now would be unused
//! machinery (no bloat). A panic before
//! `BootCompleted` therefore parks the hart, the QEMU integration test
//! times out, and the harness reports `Outcome::Timeout` — the
//! documented fail-loud behaviour.

use core::fmt::Write as _;
use core::panic::PanicInfo;

use crate::kernel_arch::halt_current_hart;
use crate::serial::SbiWriter;

/// Shared `#[panic_handler]` body for the riscv64 boot binaries.
///
/// Always returns `!`: emits one record on the SBI console, then parks
/// the hart via [`halt_current_hart`].
pub fn handle_panic_via_serial(info: &PanicInfo<'_>) -> ! {
    // The running hart's id, so a multi-hart post-mortem knows which hart
    // faulted. Reading it has no side effects and is safe even mid-panic.
    let hart = crate::smp::current_hartid();
    let mut w = SbiWriter;
    // A loud, unmistakable multi-line banner. A kernel panic halts the
    // offending hart with no recovery (fail closed), so the record must
    // carry everything a post-mortem needs: which hart, and the panic
    // message plus source location that `PanicInfo`'s `Display` already
    // formats (`file:line:col` and the message — for an allocation failure
    // that message is the requested byte count). Terse and factual.
    let _ = writeln!(
        w,
        "\n==================== RustOS KERNEL PANIC ===================="
    );
    let _ = writeln!(w, "[rustos-kernel] riscv64 panic on hart {hart}: {info}");
    let _ = writeln!(
        w,
        "hart {hart} halted; the kernel is non-recoverable in production."
    );
    let _ = writeln!(
        w,
        "============================================================="
    );
    halt_current_hart()
}
