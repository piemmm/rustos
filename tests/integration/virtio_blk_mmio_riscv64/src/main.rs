//! Stage 4.D Item 4 QEMU integration test: drive a real (emulated)
//! virtio-blk device over the riscv64 `virt` board's virtio-MMIO bus
//! end-to-end.
//!
//! On the host (non-`riscv64gc-unknown-none-elf`) target the bin is a
//! no-op so that `cargo build --workspace` does not require the
//! freestanding toolchain at every check.

#![cfg_attr(all(target_arch = "riscv64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "riscv64", target_os = "none"), no_main)]
#![deny(missing_docs)]

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
mod fixture {
    //! Build-time generated signed `.rxe` fixture + trust anchor.
    include!(concat!(env!("OUT_DIR"), "/rxe_fixture.rs"));
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
fn main() {}
