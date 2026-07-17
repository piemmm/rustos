//! Panic-handler bridge for the wasm32 kernel modules.
//!
//! Rust forbids library-defined `#[panic_handler]`s, so each wasm module
//! declares its own one-liner that forwards to
//! [`handle_panic_via_console`]. The bridge emits a single best-effort
//! record to the host console and then traps the WebAssembly instance
//! (fail closed, never silently reset; — no panic
//! recovery in production paths).
//!
//! Trapping (rather than looping) is the wasm32 analogue of the
//! bare-metal ports' "park the hart forever": a `wasm32` `unreachable`
//! aborts the current call into the module, surfaces to the host as a
//! `RuntimeError`, and lets the browser harness observe the failure and
//! report it loudly.

use core::fmt::Write as _;
use core::panic::PanicInfo;

/// Shared `#[panic_handler]` body for the wasm32 kernel modules.
///
/// Always returns `!`: emits one record on the host console, then traps
/// the instance.
pub fn handle_panic_via_console(info: &PanicInfo<'_>) -> ! {
    let mut w = ConsoleWriter;
    let _ = writeln!(w, "[tairix-kernel] wasm32 panic: {info}");
    // `unreachable` emits the wasm `unreachable` opcode, which traps the
    // instance immediately and surfaces to the host as a `RuntimeError`.
    // It is the documented way to abort a `wasm32-unknown-unknown`
    // module with no unwinding support, and is a safe intrinsic.
    core::arch::wasm32::unreachable()
}

/// Minimal [`core::fmt::Write`] adapter for the panic path, independent
/// of `console::ConsoleWriter` (which is private to that module) so the
/// panic bridge has no dependency beyond the host import.
struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::bindings::host_console_write(s.as_bytes());
        Ok(())
    }
}
