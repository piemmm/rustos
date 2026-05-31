//! Stage 5 end-to-end QEMU integration test: mount a real (emulated)
//! FAT32 volume over a modern virtio-blk-pci device on x86_64 and
//! round-trip a read **and** a write.
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
    include!(concat!(env!("OUT_DIR"), "/rxe_fixture.rs"));
}

#[cfg(itest_x86_64)]
mod kernel;

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
