//! QEMU integration test: drive a real (emulated) virtio-crypto accelerator
//! over the aarch64 `virt` board's virtio-MMIO bus end-to-end, and require
//! the bytes it produces to be the NIST SP 800-38A AES-128-CBC known-answer
//! cipher text.
//!
//! On the host (non-`aarch64-unknown-none`) target the bin is a no-op so that
//! `cargo build --workspace` does not require the freestanding toolchain at
//! every check.

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
