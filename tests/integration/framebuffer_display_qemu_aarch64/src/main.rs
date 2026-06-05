//! Stage W11-B display vertical: drive a real (emulated) linear
//! framebuffer display end-to-end on the aarch64 `virt` board — the
//! EL1/GICv2 analogue of the riscv64 framebuffer-display vertical.
//!
//! The vertical brings the `virt`-board PE up to a translated, FP-enabled
//! state (FP enable + 2 GiB identity MMU + EL1 vectors, shared from
//! `rustos_test_virtio_qemu_support`), synthesises a genuine framebuffer
//! device with QEMU's `ramfb` — a scan-out surface in guest RAM whose
//! geometry is programmed into the device over the shared `fw_cfg` MMIO
//! DMA interface (`src/kernel.rs`) — assembles the geometry as a
//! `rustos_drv_display_framebuffer::FramebufferConfig`, then loads the
//! signed framebuffer display `.rxe` through `rustos_drvhost::Host` and
//! drives it through `load -> use -> unload -> reload`: "use" maps the
//! surface through the capability-gated `KernelMmioMapper` and `present`s
//! a frame, which a second independently-mapped window reads back to
//! confirm the pixels landed in the `ramfb` scan-out memory the device
//! consumes.
//!
//! On the host (non-`aarch64-unknown-none`) target the bin is a no-op so
//! that `cargo build --workspace` does not require the freestanding
//! toolchain at every check.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod fixture {
    //! Build-time generated signed `.rxe` fixture, trust anchor, and the
    //! embedded `virt` device tree.
    include!(concat!(env!("OUT_DIR"), "/fb_fixture.rs"));
}

#[cfg(itest_aarch64)]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
