//! The wasm32 boot trampoline and the host-callback export surface.
//!
//! A bare-metal port enters Rust from an assembly `_start`; the wasm32
//! port is entered by its JavaScript host *calling an export*. This
//! module defines that export surface — the three functions `web/tairix.js`
//! invokes:
//!
//! * [`tairix_arch_wasm32_main`] — the boot entry. The host instantiates
//!   the module, wires up the imports, and calls this once. It forwards
//!   to the binary-supplied `kernel_main`, mirroring the
//!   `tairix_arch_<arch>_main` seam of the other ports.
//! * [`tairix_arch_wasm32_on_frame`] — invoked by the host's
//!   `requestAnimationFrame` callback; drives one cooperative scheduler
//!   tick ([`crate::preempt::on_animation_frame`]).
//! * [`tairix_arch_wasm32_on_message`] — invoked by the host's
//!   `MessageChannel` `onmessage`; delivers an inter-context reschedule
//!   ([`crate::preempt::on_ipi_message`]).
//!
//! Unlike the bare-metal ports, `kernel_main` here returns: the wasm32
//! cooperative model hands control back to the host event loop after
//! init so the animation-frame and message callbacks above can fire.
//! Returning is the normal, expected path (the desktop
//! is a session frontend; the same applies to the browser event loop).

extern "C" {
    /// Provided by the linked module. Runs one-time boot/init, then
    /// returns so the host event loop drives the cooperative scheduler.
    fn kernel_main();
}

/// The host calls this once after instantiating the module.
///
/// Forwards to the binary-supplied `kernel_main`. This seam exists so the
/// host hands off to Rust through one named, stable export.
#[no_mangle]
pub extern "C" fn tairix_arch_wasm32_main() {
    // SAFETY: `kernel_main` is provided by the linked module; calling it
    // exactly once on the host's initial turn is the entire contract.
    unsafe { kernel_main() }
}

/// The host's `requestAnimationFrame` callback calls this each frame.
#[no_mangle]
pub extern "C" fn tairix_arch_wasm32_on_frame() {
    crate::preempt::on_animation_frame();
}

/// The host's `MessageChannel` `onmessage` calls this on each delivered
/// inter-context reschedule.
#[no_mangle]
pub extern "C" fn tairix_arch_wasm32_on_message() {
    crate::preempt::on_ipi_message();
}
