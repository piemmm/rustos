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
//! The aarch64 port wires the [`KernelArch`] interrupt-routing surface to
//! the GICv2 through [`crate::aarch64::gic_irq`]: `irq_routing` returns the
//! GICv2-backed [`rustos_kernel_core::IrqRouting`] (freestanding) and
//! `install_irq_dispatch` publishes the kernel `IrqTable` into the arch
//! crate's EL1 IRQ-vector seam, so a discovered device SPI can be bound and
//! a parked task is woken when the line fires (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (1)). On a non-freestanding host build the routing stays the
//! conservative fail-closed [`rustos_kernel_core::IrqRouting::unsupported`]
//! default (no `VolatileGicMmio` exists off the bare-metal target).

use rustos_arch_aarch64::context_hal::ContextSwitchHal;
use rustos_arch_aarch64::{halt_current_cpu, serial, Aarch64Arch};
use rustos_arch_api::{CpuId, SchedulerArch};
use rustos_kernel_core::{ConsoleRead, ConsoleWrite, InputFocus, IrqRouting, KernelArch};
use rustos_kernel_irq::IrqTable;

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

    fn irq_routing(&self) -> IrqRouting {
        // The GICv2-backed routing the kernel core builds the `IrqTable`
        // against. On the bare-metal target this names the `'static`
        // `GIC_IRQ_CONTROLLER` over the discovered GICv2 windows; on a host
        // build there is no `VolatileGicMmio`, so the routing stays the
        // conservative fail-closed default (`AGENTS.md` §5.4.5).
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            crate::aarch64::gic_irq::gic_irq_routing()
        }
        #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
        {
            IrqRouting::unsupported()
        }
    }

    fn install_irq_dispatch(&self, table: &'static IrqTable) {
        // Publish the freshly built `IrqTable` and register the production
        // device-IRQ dispatcher with the arch crate's EL1 IRQ-vector seam,
        // so an acknowledged non-timer GIC INTID is translated into an
        // `IrqTable::fire` (mask-before-wake, `docs/src/security/irq.md`).
        crate::aarch64::gic_irq::install_device_irq_dispatch(table);
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
/// discovered console UART: every `stream_write` byte is forwarded
/// verbatim through [`rustos_arch_aarch64::serial::write_console_bytes`]
/// and every `stream_read` drains pending input through
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
        // a read with no pending input is a valid zero-length read.
        // Kernel-core wraps this device in its `BlockingConsoleRead`
        // adapter, which turns that empty poll into a scheduler park so
        // a `stream_read` caller waits for input rather than seeing a
        // spurious end-of-input (`AGENTS.md` §20).
        Ok(serial::read_console_bytes(buf))
    }
}

/// The single `'static` [`UartConsole`] the boot path lists in the
/// [`rustos_kernel_core::BootInfo::with_consoles`] console list.
/// Zero-sized, so it has no `.bss`/`.data` footprint — mirroring
/// [`rustos_arch_aarch64::SERIAL_SINK`].
pub static UART_CONSOLE: UartConsole = UartConsole;

/// The video (framebuffer) console device — the **primary** console the
/// boot path lists when the P7b framebuffer boot console is configured
/// (`plans/PI.md` P11).
///
/// A zero-sized [`ConsoleWrite`] adapter over the framebuffer text
/// renderer: every `stream_write` byte is rendered on screen through
/// [`rustos_arch_aarch64::video::write_bytes`]. The video console's
/// **input** half is not this type but the shared [`VIDEO_KEYBOARD`]
/// queue: the display's session reads a directly attached keyboard
/// (USB HID / PS-2 — the P10 input wiring), whose decoded key edges the
/// keyboard-input driver hands to the [`INPUT_FOCUS`] arbiter, which (in
/// the default text focus) encodes a press and enqueues it here, never on
/// the UART, which is its own console with its own login (`plans/PI.md`
/// P11). Until a keyboard driver injects anything the queue is empty, so
/// kernel-core's `BlockingConsoleRead` parks a reader until a keystroke
/// arrives — the prompt waits instead of exiting or borrowing the serial
/// line (`AGENTS.md` §2.9 / §5.4).
#[derive(Debug, Default, Copy, Clone)]
pub struct VideoConsole;

impl ConsoleWrite for VideoConsole {
    fn write(&self, bytes: &[u8]) -> Result<usize, rustos_abi::Errno> {
        // The boot path lists this device only when the framebuffer
        // console came up, but fail closed rather than silently dropping
        // bytes if that invariant is ever violated (`AGENTS.md` §2.9).
        if !rustos_arch_aarch64::video::is_active() {
            return Err(rustos_abi::Errno::NotImplemented);
        }
        rustos_arch_aarch64::video::write_bytes(bytes);
        Ok(bytes.len())
    }
}

/// The single `'static` [`VideoConsole`] the boot path lists first when
/// the framebuffer boot console is active. Zero-sized, like
/// [`UART_CONSOLE`].
pub static VIDEO_CONSOLE: VideoConsole = VideoConsole;

/// The video console's keyboard type-ahead queue (`plans/PI.md` P11 —
/// keyboard input for the video console).
///
/// It is both the video console's [`rustos_kernel_core::ConsoleRead`]
/// half (drained by a `stream_read` from the video login) and its
/// [`rustos_kernel_core::ConsoleInput`] half — the [`INPUT_FOCUS`]
/// arbiter's *text sink*, into which an injected key press is encoded
/// while the desktop does not hold focus. The same `'static` backs both
/// halves, so the arbiter's push wakes the reader parked in kernel-core's
/// `BlockingConsoleRead`. The UART console reads its own hardware FIFO
/// and so carries no such queue — the video login takes input only from
/// its own keyboard, never the serial line (`plans/PI.md` P11).
pub static VIDEO_KEYBOARD: rustos_kernel_core::ConsoleInputQueue =
    rustos_kernel_core::ConsoleInputQueue::new();

/// The kernel input-focus arbiter the boot path installs through
/// [`rustos_kernel_core::KernelSyscallHandlers::with_input_focus`]
/// (`AGENTS.md` §10 / §17.3 / §20, `plans/PI.md` P11 — input follows the
/// surface owner).
///
/// Its *text sink* is [`VIDEO_KEYBOARD`]: while the desktop does not hold
/// focus (the default for a freshly booted text login) the arbiter encodes
/// each injected key *press* to the video console's tty bytes and enqueues
/// them there, where the video login's `stream_read` drains them. When the
/// window manager acquires the display (`display_acquire`) the arbiter
/// routes whole [`rustos_abi::input::KeyInput`] records to its desktop
/// keyboard channel instead, drained by `keyboard_read`. The same `'static`
/// is shared by the in-kernel keyboard driver's `ArbiterConsoleSink` (which
/// injects key edges) and the `key_inject` / `display_acquire` /
/// `display_release` / `keyboard_read` syscall handlers.
pub static INPUT_FOCUS: InputFocus = InputFocus::new(&VIDEO_KEYBOARD);

/// The console-0 read half of the video console, gated on the in-kernel
/// root-unlock service's ownership latch (`plans/PI.md` P11 Chunk B-2 item
/// 5): a `stream_read` from the primary console's `login` is withheld
/// (parked) until the unlock kthread has finished reading the root
/// passphrase off the same queue and opened the gate, so the two never
/// race for console-0 input. Wraps [`VIDEO_KEYBOARD`]; the injected-input
/// (`console_input`) half stays the raw queue.
static GATED_VIDEO_READ: crate::unlock_service::GatedConsoleRead =
    crate::unlock_service::GatedConsoleRead::new(
        &VIDEO_KEYBOARD,
        &crate::unlock_service::CONSOLE0_GATE,
    );

/// The console-0 read half of the UART console, gated on the same unlock
/// ownership latch as [`GATED_VIDEO_READ`] for the UART-only (QEMU `virt`,
/// headless Pi) layout where the UART *is* the primary console.
static GATED_UART_READ: crate::unlock_service::GatedConsoleRead =
    crate::unlock_service::GatedConsoleRead::new(
        &UART_CONSOLE,
        &crate::unlock_service::CONSOLE0_GATE,
    );

/// The console list installed when the framebuffer boot console is
/// active: the video console is the primary (index 0, PID 1's banner +
/// the first login) and the UART is an independent second console with
/// its own login session (`plans/PI.md` P11).
pub static VIDEO_AND_UART_CONSOLES: [rustos_kernel_core::ConsoleDevice; 2] = [
    // The video console: written to the framebuffer, read (through the
    // unlock ownership gate) from the shared keyboard type-ahead queue,
    // and fed by the input-focus arbiter's text sink into that same queue.
    rustos_kernel_core::ConsoleDevice::with_input(
        &VIDEO_CONSOLE,
        &GATED_VIDEO_READ,
        &VIDEO_KEYBOARD,
    ),
    rustos_kernel_core::ConsoleDevice::new(&UART_CONSOLE, &UART_CONSOLE),
];

/// The console list installed when no display came up (QEMU `virt`, a
/// headless Pi): the discovered UART is the only console, so it is the
/// primary console and its read half is gated on the unlock latch.
pub static UART_ONLY_CONSOLES: [rustos_kernel_core::ConsoleDevice; 1] =
    [rustos_kernel_core::ConsoleDevice::new(
        &UART_CONSOLE,
        &GATED_UART_READ,
    )];

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_aarch64::{Aarch64Arch, Aarch64ArchStorage};

    #[test]
    fn bin_arch_delegates_scheduler_arch_to_inner() {
        static S: Aarch64ArchStorage<4> = Aarch64ArchStorage::new();
        let bin = Aarch64BinArch::new(Aarch64Arch::new(&S, 2, 1_000));
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
