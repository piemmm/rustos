//! Stage 4.D Item 4 QEMU integration test: drive a real (emulated)
//! virtio-net device over the riscv64 `virt` board's virtio-MMIO bus
//! end-to-end.
//!
//! On the host (non-`riscv64gc-unknown-none-elf`) target the bin is a
//! no-op so that `cargo build --workspace` does not require the
//! freestanding toolchain at every check.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_riscv64)]
mod fixture {
    //! Build-time generated signed `.rxe` fixture + trust anchor.
    include!(concat!(env!("OUT_DIR"), "/rxe_fixture.rs"));
}

#[cfg(itest_riscv64)]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
