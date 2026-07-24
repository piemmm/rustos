//! QEMU integration test: drive a real (emulated) virtio-input **pointer**
//! device over the aarch64 `virt` board's virtio-MMIO bus end-to-end,
//! decoding a secondary (right) mouse-button press+release the QEMU runner
//! injects through the monitor.
//!
//! This is the mouse-button sibling of `input_virtio_mmio_qemu_aarch64`
//! (which injects a keyboard key). It guards the `tools/qemu`
//! button-mask fix: a scripted right-click must reach the guest as
//! `BTN_RIGHT` (`0x111`), never the middle button (`0x112`). QEMU's HMP
//! `mouse_button` help string mislabels the state bits, so a runner that
//! trusted it delivered a right-click as a middle-button event and the
//! guest never saw a right-click — the harness now sends the bit QEMU
//! actually decodes as the right button, and this vertical proves it.
//!
//! On the host (non-`aarch64-unknown-none`) target the bin is a no-op so
//! that `cargo build --workspace` does not require the freestanding
//! toolchain at every check.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod fixture {
    //! Build-time generated signed `.rxe` fixture + trust anchor.
    include!(concat!(env!("OUT_DIR"), "/rxe_fixture.rs"));
}

#[cfg(itest_aarch64)]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
