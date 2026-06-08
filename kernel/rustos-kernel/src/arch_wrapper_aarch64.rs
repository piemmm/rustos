//! In-crate [`KernelArch`] wrapper around
//! [`rustos_arch_aarch64::Aarch64Arch`], plus the [`UartConsole`]
//! [`ConsoleWrite`] device the aarch64 boot path installs on
//! [`rustos_kernel_core::BootInfo`].
//!
//! # Why a wrapper
//!
//! `rustos_kernel_core::KernelArch` is a foreign trait and
//! `rustos_arch_aarch64::Aarch64Arch` is a foreign type, so Rust's
//! coherence rules forbid implementing the trait for the type directly.
//! [`Aarch64BinArch`] is the smallest local type that owns an
//! `Aarch64Arch`, delegates the [`SchedulerArch`] super-trait, and
//! implements [`KernelArch::halt`] / [`KernelArch::monotonic_ns`] by
//! forwarding to the arch port — the orphan-rule sibling of the x86_64
//! [`crate::BinArch`] and the riscv64 `RiscvBinArch` (`plans/PI.md`
//! P6c-2).
//!
//! The aarch64 port keeps the conservative default [`KernelArch`]
//! interrupt-routing surface (`irq_routing` returns the unsupported
//! routing, `install_irq_dispatch` is the no-op): reaching
//! `AuditEvent::BootCompleted` needs no external-IRQ delivery, exactly
//! as the riscv64 boot consumer does. Wiring the GICv2 dispatcher into
//! this surface is a later PI stage.

use rustos_arch_aarch64::context_hal::ContextSwitchHal;
use rustos_arch_aarch64::{halt_current_cpu, serial, Aarch64Arch};
use rustos_arch_api::{CpuId, SchedulerArch};
use rustos_kernel_core::{ConsoleRead, ConsoleWrite, KernelArch};

/// Local [`KernelArch`] wrapper around the arch port's
/// [`Aarch64Arch`].
///
/// Owns the concrete arch handle and delegates every trait method to
/// it. Constructed once by the boot path (`boot_aarch64`) and handed to
/// `kernel_core::kernel_main` inside an `Arc` — the single
/// `AGENTS.md` §17.1/§17.2 concrete-arch selection point for the
/// aarch64 kernel image.
#[derive(Debug)]
pub struct Aarch64BinArch {
    arch: Aarch64Arch,
}

impl Aarch64BinArch {
    /// Wrap `arch` so it can be handed to `kernel_core::kernel_main`.
    #[must_use]
    pub const fn new(arch: Aarch64Arch) -> Self {
        Self { arch }
    }

    /// Borrow the wrapped [`Aarch64Arch`].
    #[must_use]
    pub const fn arch(&self) -> &Aarch64Arch {
        &self.arch
    }
}

impl SchedulerArch for Aarch64BinArch {
    fn current_cpu(&self) -> CpuId {
        self.arch.current_cpu()
    }

    fn ticks_now(&self) -> u64 {
        self.arch.ticks_now()
    }

    fn send_ipi(&self, target: CpuId) {
        self.arch.send_ipi(target);
    }
}

impl KernelArch for Aarch64BinArch {
    type Cs = ContextSwitchHal;

    fn context_switch(&self) -> Self::Cs {
        ContextSwitchHal::new()
    }

    fn halt(&self) -> ! {
        halt_current_cpu()
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        self.arch.monotonic_ns()
    }
}

// SAFETY-INVARIANT: `Aarch64BinArch::halt` returns the bottom type. The
// coercion fails to type-check if the impl ever loses `-> !`, pinning
// the contract at compile time (`AGENTS.md` §2.10), exactly as the
// x86_64 `BinArch` and riscv64 `RiscvBinArch` wrappers do.
const _AARCH64_BIN_ARCH_HALT_RETURNS_NEVER: fn(&Aarch64BinArch) -> ! =
    <Aarch64BinArch as KernelArch>::halt;

/// The system console device the aarch64 boot path installs on
/// [`rustos_kernel_core::BootInfo`].
///
/// A zero-sized [`ConsoleWrite`] + [`ConsoleRead`] adapter over the
/// discovered console UART: every `console_write` byte is forwarded
/// verbatim through [`rustos_arch_aarch64::serial::write_console_bytes`]
/// and every `console_read` drains pending input through
/// [`rustos_arch_aarch64::serial::read_console_bytes`], both targeting
/// the board-discovered UART base (`plans/PI.md` P2 / P6). It is the
/// "first discovered UART" half of the §10 console seam; the framebuffer
/// path (`plans/PI.md` P7) replaces it with a text-console device once a
/// display driver loads.
///
/// This is the bootstrap stream **backing** the spawner attaches to fd
/// 0/1 (`AGENTS.md` §20), not a program-facing interface.
#[derive(Debug, Default, Copy, Clone)]
pub struct UartConsole;

impl ConsoleWrite for UartConsole {
    fn write(&self, bytes: &[u8]) -> Result<usize, rustos_abi::Errno> {
        // The busy-wait transmit path accepts every byte, so the write
        // is total and never short. It performs no `\n` translation:
        // the bytes reach the device exactly as the program wrote them
        // (`AGENTS.md` §16.4).
        Ok(serial::write_console_bytes(bytes))
    }
}

impl ConsoleRead for UartConsole {
    fn read(&self, buf: &mut [u8]) -> Result<usize, rustos_abi::Errno> {
        // The non-blocking receive path drains whatever input is
        // immediately available and never busy-waits (`AGENTS.md` §2.1);
        // a read with no pending input is a valid zero-length read the
        // caller loops on, never an error.
        Ok(serial::read_console_bytes(buf))
    }
}

/// The single `'static` [`UartConsole`] the boot path installs through
/// [`rustos_kernel_core::BootInfo::with_console`] (output) and
/// [`rustos_kernel_core::BootInfo::with_console_read`] (input).
/// Zero-sized, so it has no `.bss`/`.data` footprint — mirroring
/// [`rustos_arch_aarch64::SERIAL_SINK`].
pub static UART_CONSOLE: UartConsole = UartConsole;

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_aarch64::Aarch64Arch;

    #[test]
    fn bin_arch_delegates_scheduler_arch_to_inner() {
        let bin = Aarch64BinArch::new(Aarch64Arch::new(2, 1_000));
        assert_eq!(bin.current_cpu(), 2);
        // The monotonic clock is the inner handle's; two reads are
        // strictly increasing on the host substitute counter.
        let a = bin.monotonic_ns(2);
        let b = bin.monotonic_ns(2);
        assert!(b > a, "clock must be monotonically increasing");
    }

    #[test]
    fn uart_console_reports_full_write_and_is_inert_on_host() {
        // `serial::write_console_bytes` is a no-op transmit on the host
        // build but reports the full byte count, so the adapter's
        // contract (never short) holds without touching MMIO.
        assert_eq!(UART_CONSOLE.write(b"hello"), Ok(5));
        assert_eq!(UartConsole.write(&[]), Ok(0));
    }

    #[test]
    fn uart_console_read_reports_zero_and_is_inert_on_host() {
        // `serial::read_console_bytes` yields no input on the host build
        // (the device `getchar` returns `None`), so the adapter reports a
        // valid zero-length read — never an error — without touching MMIO.
        let mut buf = [0u8; 8];
        assert_eq!(UART_CONSOLE.read(&mut buf), Ok(0));
        assert_eq!(UartConsole.read(&mut []), Ok(0));
    }
}
