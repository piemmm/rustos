//! The `Run` entry-point binary of the **RAID array-composer driver**,
//! installed as a signed `/System/Drivers/` bundle and autoloaded into user
//! space by `devmgr` (`plans/FIX-IO.md` `IO6d`).
//!
//! Matched to the kernel's synthetic `tairix,virtual-bus` node — one instance
//! for the whole machine — the composer owns the reserved rendezvous the
//! per-disk member agents offer their devices to. It reads each offered
//! device's superblock itself, decides through the pure `compose`/`service`
//! logic which array a member belongs to and when that array may come online,
//! assembles the surviving members into a served block device, and publishes
//! it as a `tairix,raid-array` node so `volmgr` mounts its filesystems through
//! the unchanged volume path.
//!
//! # Least privilege
//!
//! It holds `CAP_IPC_ENDPOINT` (own the rendezvous and each array's
//! block-service endpoint, and connect to each member), `CAP_SHM` (map each
//! member's data window and create each array's), `CAP_HW_EMIT` (publish the
//! array node), `CAP_LOG_EMIT` (diagnostics), and `CAP_IPC_BIND_PRIVILEGED`
//! (the rendezvous id is reserved, so a squatter cannot claim it first). No
//! MMIO, DMA, IRQ, or mount authority: it never touches hardware directly and
//! never mounts — it composes and hands the array to `volmgr`.
//!
//! It is a **pure-Rust** program; on the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the file, and the
//! composition and live-array logic it drives is host-tested in the crate's
//! `lib` target.

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
