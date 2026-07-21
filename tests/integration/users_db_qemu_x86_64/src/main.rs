//! `plans/ARCHSUPPORT.md` A2 QEMU integration test (x86_64 sibling of the
//! aarch64 users-database vertical): read `/System/Security/Users` off a
//! real (emulated) users-root arxfs volume over the x86_64 virtio-**PCI**
//! bus, through the kernel's boot-time users-database load path.
//!
//! Walk PCI to the modern virtio-blk-pci function, provision a
//! `PciTransport` through the capability-gated `KernelMmioMapper`, route
//! its MSI-X interrupt, load the signed virtio-blk `.rxe`, then drive the
//! *shared* users-database device tail
//! (`tairix_test_virtio_qemu_support::users_db_load` — the exact one
//! definition the aarch64 vertical runs, `AGENTS.md` §2.2): mount the
//! planted plaintext users-root volume, run
//! `tairix_kernel_core::load_users_db`, and prove the parsed database
//! authenticates the planted account while a wrong password is refused.
//! Only the bus bring-up differs from the aarch64 vertical (virtio-PCI
//! here, virtio-MMIO there).
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
