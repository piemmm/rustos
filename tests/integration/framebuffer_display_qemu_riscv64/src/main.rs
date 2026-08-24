//! Stage 4 first-driver vertical: drive a real (emulated) linear
//! framebuffer display end-to-end on the riscv64 `virt` board.
//!
//! The vertical synthesises a genuine framebuffer device with QEMU's
//! `ramfb`: a scan-out surface lives in guest RAM and the geometry is
//! programmed into the device over the `fw_cfg` MMIO DMA interface
//! (`src/kernel.rs`). The kernel publishes the resulting geometry as a
//! `tairix_display::FramebufferConfig` boot
//! hand-off, then the signed framebuffer display `.rxe` is loaded
//! through `tairix_drvhost::Host` and driven through `load -> use ->
//! unload -> reload`: "use" maps the surface through the
//! capability-gated `KernelMmioMapper` and `present`s a frame, which
//! a second independently-mapped window reads back to confirm the
//! pixels landed in the ramfb scan-out memory the device consumes.
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
    include!(concat!(env!("OUT_DIR"), "/fb_fixture.rs"));
}

#[cfg(itest_riscv64)]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
