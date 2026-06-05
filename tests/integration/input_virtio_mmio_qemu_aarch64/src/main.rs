//! Stage W11-B QEMU integration test: drive a real (emulated)
//! virtio-input device over the aarch64 `virt` board's virtio-MMIO bus
//! end-to-end, decoding a key the QEMU runner injects through the
//! monitor.
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
