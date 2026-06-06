//! The kernel-side system console sink the `console_write` syscall
//! (`abi-v1` number 11, `AGENTS.md` §10 / §16.4) emits to.
//!
//! `console_write` lets the privileged early bring-up principals (PID 1
//! `init`, login, getty) write a byte buffer to the *hardware* console.
//! Which device that is — the detected framebuffer when one is present,
//! else the first discovered UART (`plans/PI.md` P6) — is a boot-time
//! decision the architecture port makes from the normalised hardware
//! tree (`AGENTS.md` §18). `kernel/core` does not know how to talk to a
//! PL011, a 16550, or a framebuffer; it only knows it needs *a* byte
//! sink. [`ConsoleWrite`] is that seam: the boot path installs the
//! concrete device, and the syscall handler writes the copied-in bytes
//! through it.
//!
//! Until a console is installed the handler holds [`NULL_CONSOLE`],
//! which fails closed with [`Errno::NotImplemented`] rather than
//! silently swallowing the bytes (`AGENTS.md` §2.9). A build with no
//! console device wired (a headless target with no UART, an early-boot
//! state before discovery) therefore announces an intentionally inert
//! interface instead of pretending the write succeeded.

use rustos_abi::Errno;

/// A byte sink for the privileged system console.
///
/// Implemented by the architecture-port-installed console device (a
/// UART or a framebuffer text console). The trait is deliberately
/// minimal — one method that takes already-copied-in kernel bytes —
/// so `kernel/core` stays free of any device knowledge (`AGENTS.md`
/// §17.4) and the syscall handler owns the user-memory copy and the
/// capability check, never the device implementation.
///
/// Implementations must be [`Sync`]: the single installed console is
/// shared by the per-CPU syscall handlers, exactly like the audit
/// [`Sink`](rustos_log::Sink).
pub trait ConsoleWrite: Sync {
    /// Write `bytes` to the console, returning the number actually
    /// written.
    ///
    /// The caller has already copied `bytes` out of user memory through
    /// the validated `copy_from_user` boundary (`AGENTS.md` §5.4) and
    /// checked the caller's [`CapabilityId::CONSOLE_WRITE`](rustos_abi::CapabilityId::CONSOLE_WRITE);
    /// the implementation only moves bytes to the device. A short write
    /// (fewer than `bytes.len()`) is permitted and reported through the
    /// return value, exactly as POSIX `write` allows; the caller loops.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] when the device cannot accept the
    /// bytes. The default sink ([`NullConsole`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn write(&self, bytes: &[u8]) -> Result<usize, Errno>;
}

/// The console sink installed before any real device exists.
///
/// Every write fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default `AGENTS.md` §2.9 / §5.4 require, so a
/// `console_write` issued before the boot path installs a device (or on
/// a target that genuinely has no console) announces an inert interface
/// rather than silently discarding the bytes.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullConsole;

impl ConsoleWrite for NullConsole {
    fn write(&self, _bytes: &[u8]) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullConsole`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `console` borrow here so that
/// the field is always valid without an `Option` branch on the hot
/// path; the boot path replaces it with the real device through
/// `KernelSyscallHandlers::with_console`.
pub static NULL_CONSOLE: NullConsole = NullConsole;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_console_fails_closed() {
        assert_eq!(NULL_CONSOLE.write(b"hello"), Err(Errno::NotImplemented));
        // Even an empty write announces the inert interface rather than
        // pretending success.
        assert_eq!(NullConsole.write(&[]), Err(Errno::NotImplemented));
    }
}
