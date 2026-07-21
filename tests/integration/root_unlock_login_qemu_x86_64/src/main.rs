//! `plans/ARCHSUPPORT.md` A2 QEMU integration test: drive the production
//! interactive root-unlock policy over a real (emulated) whole-disk
//! encrypted-root image on x86_64's virtio-PCI bus — walk PCI to the modern
//! virtio-blk-pci function, provision a `PciTransport`, route its MSI-X
//! interrupt, load the signed `.rxe`, type the passphrase at the prompt,
//! mount the encrypted `ARXFS` root, install the loaded users database into
//! a `LateUsersDb` cell, and prove the planted account authenticates
//! through it. The unlock tail is the *shared* `root_unlock_login` the
//! aarch64 vertical runs (one definition, `AGENTS.md` §2.2); only the bus
//! bring-up differs (virtio-PCI here, virtio-MMIO there).
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
