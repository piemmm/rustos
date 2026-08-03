//! This port's console *devices*: the kernel-typed console list the boot path
//! installs, over the COM1 console the architecture crate owns.
//!
//! The console itself — the 16550 device, the shared output queue, and the log
//! [`tairix_log::Sink`] — lives in [`tairix_arch_x86_64::serial`], because that
//! is where the hardware is. What lives here is only what names `kernel/core`
//! types an architecture crate may not depend on: the
//! [`ConsoleWrite`] backing, the unlock-gated read half, and the console list.

use tairix_arch_x86_64::serial;
use tairix_kernel_core::ConsoleWrite;

/// A [`ConsoleWrite`] backing over the COM1 console.
///
/// The boot path lists this in the `tairix_kernel_core::BootInfo::with_consoles`
/// console list, so the `stream_write` syscall on fd 1/2/3 reaches the same
/// serial line the log sink writes. It is the bootstrap stream **backing**, not
/// a program-facing device interface — a program names only its inherited
/// descriptors and the kernel routes the bytes here.
///
/// Zero-sized and shared through a `&'static` reference: the console is the
/// shared state, not the wrapper.
#[derive(Debug)]
pub struct Com1Console;

impl Com1Console {
    /// Construct the console handle. `const` so it can live in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for Com1Console {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleWrite for Com1Console {
    fn write(&self, bytes: &[u8]) -> Result<usize, tairix_abi::Errno> {
        // A short count is legitimate and is what the caller retries on: the
        // console reports what it accepted rather than claiming bytes it could
        // not carry.
        Ok(serial::write_console_bytes(bytes))
    }
}

/// The single `'static` [`Com1Console`] the boot path lists in the console
/// list.
pub static COM1_CONSOLE: Com1Console = Com1Console::new();

/// COM1's console-0 read half, gated on the in-kernel root-unlock service's
/// ownership latch (`plans/PI.md` P11): a `stream_read` from the primary
/// console's `login` is withheld (parked) until the unlock kthread has finished
/// reading the root passphrase off the same interrupt-fed queue and opened the
/// gate, so the two never race for console-0 input. Wraps the interrupt-fed
/// [`crate::x86_64::com1_rx::COM1_CONSOLE_READ`]; the receive-drain
/// (`console_input`) half stays the raw
/// [`crate::x86_64::com1_rx::COM1_INPUT`] queue the interrupt fills.
static GATED_COM1_READ: crate::unlock_service::GatedConsoleRead =
    crate::unlock_service::GatedConsoleRead::new(
        &crate::x86_64::com1_rx::COM1_CONSOLE_READ,
        &crate::unlock_service::CONSOLE0_GATE,
    );

/// The boot console list: COM1 is the only console (index 0, PID 1's banner +
/// the login session). Its read half is the interrupt-fed, unlock-gated COM1
/// receive queue: once the gate opens a parked `login` reader is woken by a
/// keystroke (`crate::x86_64::com1_rx`), never left polling. The receive-drain
/// half is that same [`COM1_INPUT`] queue, so a pushed byte reaches the parked
/// reader.
///
/// [`COM1_INPUT`]: crate::x86_64::com1_rx::COM1_INPUT
pub static COM1_CONSOLES: [tairix_kernel_core::ConsoleDevice; 1] =
    [tairix_kernel_core::ConsoleDevice::with_input(
        &COM1_CONSOLE,
        &GATED_COM1_READ,
        &crate::x86_64::com1_rx::COM1_INPUT,
    )];

/// The **installed** COM1 console device — the primary (and only) console on
/// the QEMU PC target.
///
/// The receive drain pushes bytes through this device rather than the raw
/// [`crate::x86_64::com1_rx::COM1_INPUT`] queue, so the console's cooked-mode
/// line discipline sees every byte at arrival time (`plans/SPAWN.md` SP9): a
/// `^C`/`^Z` typed while a foreground job runs is delivered as a signal even
/// though no task is reading.
#[must_use]
pub fn com1_console_device() -> &'static tairix_kernel_core::ConsoleDevice {
    &COM1_CONSOLES[0]
}
