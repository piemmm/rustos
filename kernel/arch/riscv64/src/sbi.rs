//! Minimal Supervisor Binary Interface (SBI) calls.
//!
//! The boot pipeline needs exactly one SBI service — console output —
//! so this module exposes only that (`AGENTS.md` §2.3 — no bloat).
//! Every kernel print (the boot log and the `BootCompleted` audit
//! record) goes out through the SBI legacy console, which OpenSBI on
//! the QEMU `virt` board routes to the same NS16550 UART that
//! `-serial stdio` captures.
//!
//! The legacy `console_putchar` call (extension id `0x01`) is used
//! rather than the newer Debug Console (`DBCN`) extension because every
//! OpenSBI build QEMU ships implements the legacy call, and the boot
//! pipeline's output is diagnostic only — the pass/fail result is
//! reported out-of-band through the `SiFive` Test device
//! (`qemu_exit`), so console availability is not load-bearing for the
//! test outcome.

/// SBI legacy extension id for `set_timer`.
const SBI_SET_TIMER: usize = 0x00;

/// SBI legacy extension id for `console_putchar`.
const SBI_CONSOLE_PUTCHAR: usize = 0x01;

/// Program the next supervisor timer interrupt for absolute `time`
/// (in `time`-CSR ticks).
///
/// Issues the legacy `set_timer` `ecall` (extension id `0x00`). On
/// RV64 the 64-bit `stime_value` is passed whole in `a0`. The call
/// also clears any pending supervisor timer interrupt (`sip.STIP`), so
/// the timer trap handler re-arms the timer through this call to
/// acknowledge the tick. Errors are not observable through the legacy
/// ABI, so it never panics (`AGENTS.md` §2.9).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn set_timer(time: u64) {
    // SAFETY: an `ecall` with `a7 = 0x00` is the documented SBI legacy
    // `set_timer` service. It reads `a0` (the absolute deadline), writes
    // no guest memory, and clobbers only the SBI return registers
    // `a0`/`a1`, which are marked `lateout`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_SET_TIMER,
            inout("a0") time => _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Write one byte to the SBI console.
///
/// Issues the legacy `console_putchar` `ecall`. Errors are not
/// observable through the legacy ABI (it returns no status), and the
/// console is diagnostic-only, so the call is treated as infallible —
/// it never panics (`AGENTS.md` §2.9).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn console_putchar(byte: u8) {
    // SAFETY: an `ecall` with `a7 = 0x01` is the documented SBI legacy
    // `console_putchar` service. It reads `a0` (the character), writes
    // no guest memory, and clobbers only the SBI-defined return
    // registers `a0`/`a1`, which are marked `lateout`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_CONSOLE_PUTCHAR,
            inout("a0") usize::from(byte) => _,
            lateout("a1") _,
            options(nostack),
        );
    }
}
