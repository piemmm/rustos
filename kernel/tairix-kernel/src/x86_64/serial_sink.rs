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
//! mutable reference. Re-initialisation of the UART per call is
//! idempotent on the 16550 — the divisor and line-control bits are
//! sticky — so the cost is a handful of `outb`s and the overhead is
//! deliberate: it removes one shared mutable static from the bin's
//! surface (*"No global mutable static beyond the
//! per-CPU bootstrap area"*).

use tairix_arch_x86_64::serial::{self, Serial, COM1_BASE};
use tairix_kernel_core::ConsoleWrite;
use tairix_log::{Event, Sink};

/// `Sink` implementation that emits one formatted line per event to
/// the 16550 UART at [`COM1_BASE`].
///
/// Zero-sized; safe to share through a `&'static` reference because
/// every call constructs a fresh [`Serial`] handle (the UART itself is
/// the shared mutable state, not the wrapper).
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
        // Re-`init` per call is idempotent on the 16550 (the divisor
        // and line-control bits are sticky). Avoids needing a `static
        // mut` for the writer. A serial capture renders ANSI SGR, so the
        // level tag is coloured; no monotonic-uptime seam is wired on
        // this port yet, so the stamp is omitted.
        let mut s = Serial::init(COM1_BASE);
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
/// Zero-sized and shared through a `&'static` reference: every call
/// constructs a fresh [`Serial`] handle (the UART itself is the shared
/// mutable state, not the wrapper), matching [`SerialSink`]'s discipline of
/// holding no global mutable static.
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
        // The 16550 init is idempotent (sticky divisor + line-control), so a
        // fresh handle per call avoids a shared mutable static. `write_byte`
        // busy-polls the transmitter-holding register, so every byte lands;
        // the count returned is therefore the full slice length, and the
        // gated banner write in PID 1 `init` (`run.rs`) sees a full write.
        let mut serial = Serial::init(COM1_BASE);
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
