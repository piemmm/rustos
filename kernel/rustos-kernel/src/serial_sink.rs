//! COM1-backed [`rustos_log::Sink`] for the freestanding kernel.
//!
//! The Sink writes one record per call in the canonical format the
//! `kernel/core` audit consumers expect:
//!
//! ```text
//! [<level>] id=<id> <message> <key>=<value> <key>=<value> ...
//! ```
//!
//! The trailing newline is part of the record contract — every
//! external log scraper RustOS ships (`tools/qemu`'s serial capture,
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
//! surface (`AGENTS.md` §2 — *"No global mutable static beyond the
//! per-CPU bootstrap area"*).

use core::fmt::Write as _;

use rustos_arch_x86_64::serial::{self, Serial, COM1_BASE};
use rustos_kernel_core::ConsoleWrite;
use rustos_log::{Event, Level, Sink};

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
        // mut` for the writer.
        let mut s = Serial::init(COM1_BASE);
        // Ignore write errors: `Serial`'s `core::fmt::Write` impl is
        // infallible in practice (it busy-polls the line-status
        // register). Logging path must not panic per AGENTS.md §2.9.
        let _ = write!(
            s,
            "[{}] id={} {}",
            level_str(event.level),
            event.id.0,
            event.message
        );
        for field in event.fields {
            let _ = write!(s, " {}={}", field.key, field.value);
        }
        let _ = writeln!(s);
    }
}

const fn level_str(level: Level) -> &'static str {
    match level {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
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
/// `rustos_kernel_core::BootInfo::with_consoles` console list, so the `stream_write`
/// syscall on fd 1/2/3 reaches the same serial line the [`SerialSink`] logs
/// to (`plans/PI.md` X3a). It is the x86_64 counterpart of the aarch64
/// `UartConsole`: the bootstrap stream **backing**, not a program-facing
/// device interface — a program names only its inherited descriptors and
/// the kernel routes the bytes here (`AGENTS.md` §20).
///
/// Zero-sized and shared through a `&'static` reference: every call
/// constructs a fresh [`Serial`] handle (the UART itself is the shared
/// mutable state, not the wrapper), matching [`SerialSink`]'s discipline of
/// holding no global mutable static (`AGENTS.md` §2).
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
    fn write(&self, bytes: &[u8]) -> Result<usize, rustos_abi::Errno> {
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

/// The x86_64 boot console list: COM1 is the only console. Its read half
/// is the fail-closed [`rustos_kernel_core::NULL_CONSOLE_READ`] (no
/// non-blocking COM1 RX drain is wired on this slice), so fd 0 reads keep
/// failing closed exactly as before (`AGENTS.md` §5.4).
pub static COM1_CONSOLES: [rustos_kernel_core::ConsoleDevice; 1] =
    [rustos_kernel_core::ConsoleDevice::new(
        &COM1_CONSOLE,
        &rustos_kernel_core::NULL_CONSOLE_READ,
    )];
