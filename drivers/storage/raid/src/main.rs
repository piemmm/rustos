//! The `Run` entry-point binary of the **RAID composition driver**, installed
//! as a signed `/System/Drivers/` bundle and autoloaded into user space by
//! `devmgr` (`plans/FIX-IO.md` IO6).
//!
//! Matched to a `tairix,raid-member` node — the node the volume manager emits
//! for a device whose first block probed as array metadata — the instance is a
//! **member agent**. It delegates exactly that one device's block-service
//! endpoint and shared data window to the array composer's reserved
//! rendezvous, offers the device, and then holds the membership open for as
//! long as the array holds the device. It never reads or writes the device
//! itself: it forwards the transport it was granted and gets out of the way.
//!
//! Its lifetime is the member's presence. When the device goes, its node goes,
//! and `devmgr` unloads the instance; while the device is there, the agent is
//! what lets a restarted composer reassemble the array without a reboot.
//!
//! # Least privilege
//!
//! It holds only `CAP_IPC_ENDPOINT` (delegate its one granted block endpoint
//! and post the offer), `CAP_SHM` (delegate its one granted data window), and
//! `CAP_LOG_EMIT` (diagnostics). No MMIO, no DMA, no IRQ, no node emission, no
//! mount authority. It can delegate only what it was granted, and only to a
//! rendezvous whose id is reserved — so a compromised agent can neither reach
//! another device's transport nor hand its own to an unprivileged squatter.
//!
//! It is a **pure-Rust** program; on the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the file, and the
//! decision logic it drives is host-tested in the crate's `lib` target.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program;

// --- Host stub ----------------------------------------------------------
#[cfg(not(freestanding))]
fn main() {
    // On the host this binary is an inert stub: the freestanding `Run`
    // program (`src/program.rs`) is built only for the bare-metal driver
    // targets. Keeping a host `main` lets `cargo build --workspace`,
    // clippy, and fmt still cover the file, mirroring the other driver
    // `Run` binaries.
}
