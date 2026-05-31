//! Stage 4 first-driver vertical: drive a real (emulated) VESA linear
//! framebuffer display end-to-end on x86_64.
//!
//! The vertical synthesises a genuine framebuffer device with QEMU's
//! `ramfb`: a scan-out surface lives in guest RAM and the geometry is
//! programmed into the device over the `fw_cfg` I/O-port DMA interface
//! (`src/kernel/ioport.rs`). It then publishes a bootloader-captured VBE
//! `ModeInfoBlock` describing that surface as the boot hand-off — the
//! shape a real VBE BIOS mode query (`0x4F01`) would have produced — and
//! loads the signed vesa display `.rxe` through `rustos_drvhost::Host`,
//! driving it through `load -> use -> unload -> reload`: "use" maps the
//! surface through the capability-gated `KernelMmioMapper` and `present`s
//! a frame, which a second independently-mapped window reads back to
//! confirm the pixels landed in the ramfb scan-out memory the device
//! consumes.
//!
//! On the host (non-`x86_64-unknown-none`) target the bin is a no-op so
//! that `cargo build --workspace` does not require the freestanding
//! toolchain at every check.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod fixture {
    //! Build-time generated signed `.rxe` fixture + trust anchor.
    include!(concat!(env!("OUT_DIR"), "/vesa_fixture.rs"));
}

#[cfg(itest_x86_64)]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
