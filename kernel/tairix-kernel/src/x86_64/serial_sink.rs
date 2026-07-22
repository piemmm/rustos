//! COM1-backed [`tairix_log::Sink`] for the freestanding kernel.
//!
//! The Sink writes one record per call in the one shared diagnostic
//! format ([`tairix_log::write_diag_line`]) the `kernel/core` audit
//! consumers expect:
//!
//! ```text
//! [<LEVEL>] id=<id> <message> <key>=<value> <key>=<value> ...
//! ```
//!
//! The trailing newline is part of the record contract — every
//! external log scraper TAIRiX ships (`tools/qemu`'s serial capture,
//! the audit-on-`BootCompleted` observer used by the QEMU integration
//! test) splits on `'\n'`.
//!
//! # Lifetime
//!
//! The Sink is a zero-sized type and is exposed through the
//! [`SERIAL_SINK`] `'static` so each bin can hand the same reference
//! to `BootInfo`'s `log_sink` / `audit_sink` slots without taking a
//! mutable reference. The UART is brought up **once** through
//! [`com1_writer`]'s one-shot guard; every later write reaches it through
//! the non-reinitialising [`Serial::at`], which touches only the
//! transmitter. A per-write re-`init` was wrong: it clears `IER` and flushes
//! the receive FIFO, so it disarmed the login console's interrupt-driven
//! receive and dropped typed-ahead input.

use tairix_arch_x86_64::serial::{self, Serial, COM1_BASE};
use tairix_kernel_core::ConsoleWrite;
use tairix_log::{Event, Sink};
use tairix_sync::once::Once;

/// One-shot COM1 bring-up guard (see [`com1_writer`]).
static COM1_INIT: Once<()> = Once::new();

/// A COM1 writer that runs the 16550 bring-up **exactly once** and returns a
/// non-reinitialising handle on every subsequent call.
///
/// [`Serial::init`] is *not* idempotent for a console with interrupt-driven
/// receive armed: it writes `IER = 0` (disarming the receive interrupt the
/// login console enabled) and the FIFO-control `clear` bits (flushing any
/// typed-ahead bytes still in the receive FIFO). Re-running it on every log
/// line or console write therefore raced the interactive `login` read and
/// silently dropped keystrokes while disabling receive delivery. The UART is
/// configured once here (the line settings are sticky) and every later write
/// reaches it through [`Serial::at`], which touches only the transmitter — so
/// diagnostic output and prompt writes never clobber the receive path.
fn com1_writer() -> Serial {
    let _ = COM1_INIT.call_once_infallible(|| {
        let _ = Serial::init(COM1_BASE);
    });
    Serial::at(COM1_BASE)
}

/// `Sink` implementation that emits one formatted line per event to
/// the 16550 UART at [`COM1_BASE`].
///
/// Zero-sized; safe to share through a `&'static` reference. The UART is
/// the shared mutable state (brought up once by [`com1_writer`]); this
/// wrapper holds nothing.
#[derive(Debug)]
pub struct SerialSink;

impl SerialSink {
    /// Construct a Sink. `const` so a binary can place the handle in
    /// a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for SerialSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for SerialSink {
    fn write_event(&self, event: &Event<'_>) {
        // The UART is brought up once (`com1_writer`) and written without
        // re-init, so logging never disarms the console's receive path. A
        // serial capture renders ANSI SGR, so the level tag is coloured; no
        // monotonic-uptime seam is wired on this port yet, so the stamp is
        // omitted.
        let mut s = com1_writer();
        tairix_log::write_diag_line(&mut s, None, true, event);
    }
}

/// Single `'static` [`SerialSink`] handle the bins install in
/// `BootInfo`'s `log_sink` / `audit_sink` slots. Zero-sized, so no
/// `.bss` or `.data` footprint.
pub static SERIAL_SINK: SerialSink = SerialSink::new();

// Re-export `serial` so the bin crates do not need to depend on the
// arch crate by name for the few items they touch (panic-handler
// fallback, COM1 base).
pub use serial::COM1_BASE as REEXPORT_COM1_BASE;

/// A [`ConsoleWrite`] backing over the COM1 16550 UART.
///
/// The x86_64 production boot path lists this in the
/// `tairix_kernel_core::BootInfo::with_consoles` console list, so the `stream_write`
/// syscall on fd 1/2/3 reaches the same serial line the [`SerialSink`] logs
/// to (`plans/PI.md` X3a). It is the x86_64 counterpart of the aarch64
/// `UartConsole`: the bootstrap stream **backing**, not a program-facing
/// device interface — a program names only its inherited descriptors and
/// the kernel routes the bytes here.
///
/// Zero-sized and shared through a `&'static` reference: the UART is the
/// shared mutable state (brought up once by [`com1_writer`], then written
/// through the non-reinitialising [`Serial::at`]), not the wrapper.
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
        // The UART is brought up once (`com1_writer`) and written without
        // re-init, so a prompt write never flushes the receive FIFO the
        // interactive `login` reader is draining. `write_byte` busy-polls the
        // transmitter-holding register, so every byte lands; the count
        // returned is therefore the full slice length, and the gated banner
        // write in PID 1 `init` (`run.rs`) sees a full write.
        let mut serial = com1_writer();
        for &b in bytes {
            serial.write_byte(b);
        }
        Ok(bytes.len())
    }
}

/// The single `'static` [`Com1Console`] the x86_64 boot path lists in the
/// `BootInfo::with_consoles` console list.
pub static COM1_CONSOLE: Com1Console = Com1Console::new();

/// COM1's console-0 read half, gated on the in-kernel root-unlock service's
/// ownership latch (`plans/PI.md` P11): a `stream_read` from the primary
/// console's `login` is withheld (parked) until the unlock kthread has
/// finished reading the root passphrase off the same interrupt-fed queue and
/// opened the gate, so the two never race for console-0 input. Wraps the
/// interrupt-fed [`crate::x86_64::com1_rx::COM1_CONSOLE_READ`]; the
/// receive-drain (`console_input`) half stays the raw
/// [`crate::x86_64::com1_rx::COM1_INPUT`] queue the interrupt fills.
static GATED_COM1_READ: crate::unlock_service::GatedConsoleRead =
    crate::unlock_service::GatedConsoleRead::new(
        &crate::x86_64::com1_rx::COM1_CONSOLE_READ,
        &crate::unlock_service::CONSOLE0_GATE,
    );

/// The x86_64 boot console list: COM1 is the only console (index 0, PID 1's
/// banner + the login session). Its read half is the interrupt-fed,
/// unlock-gated COM1 receive queue: once the gate opens a parked `login`
/// reader is woken by a keystroke (`crate::x86_64::com1_rx`), never left
/// polling. The receive-drain half is that same [`COM1_INPUT`] queue, so a
/// pushed byte reaches the parked reader.
///
/// [`COM1_INPUT`]: crate::x86_64::com1_rx::COM1_INPUT
pub static COM1_CONSOLES: [tairix_kernel_core::ConsoleDevice; 1] =
    [tairix_kernel_core::ConsoleDevice::with_input(
        &COM1_CONSOLE,
        &GATED_COM1_READ,
        &crate::x86_64::com1_rx::COM1_INPUT,
    )];

/// The **installed** COM1 console device — the primary (and only) console
/// on the QEMU PC target.
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
