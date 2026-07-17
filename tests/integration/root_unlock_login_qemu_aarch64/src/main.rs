//! `plans/PI.md` P11 Chunk B-2 QEMU integration test: drive the production
//! interactive root-unlock policy over a real (emulated) whole-disk
//! encrypted-root image on the aarch64 `virt` board's virtio-MMIO bus —
//! type the passphrase at the prompt, mount the encrypted `ARXFS` root,
//! install the loaded users database into a `LateUsersDb` cell, and prove
//! the planted account authenticates through it.
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
