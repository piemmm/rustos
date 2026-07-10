//! The `Run` entry-point binary of the **volume-manager policy driver**,
//! installed as a signed `/System/Drivers/` bundle and autoloaded into
//! user space by `devmgr` when a block-service storage node is discovered
//! (`plans/DEVICES.md` D3c).
//!
//! One instance runs per matched storage node, holding exactly that
//! node's transport grants (the blkio call endpoint and the shared data
//! window). It probes the unit's partition table and filesystem
//! signatures read-only, derives each recognised volume's deterministic
//! catalog name, and asks the kernel to attach and publish it through the
//! `CAP_FS_MOUNT`-gated, audited `volume_attach` syscall — the kernel
//! re-validates the caller's grants, the extent, and the name, opens the
//! filesystem itself, and owns the mount from then on. The instance then
//! exits `0`: publication is a run-to-completion job, and the kernel-held
//! mount outlives it. It knows neither the bus nor the vendor behind the
//! block service.
//!
//! # Least privilege
//!
//! It holds only `CAP_SHM` (map the granted data window),
//! `CAP_IPC_ENDPOINT` (issue blkio calls on its one granted endpoint),
//! `CAP_FS_MOUNT` (request the audited attach), and `CAP_LOG_EMIT`
//! (diagnostics). No MMIO, no DMA, no IRQ, no node emission: a
//! compromised volume manager can neither reach another device's
//! transport (the per-endpoint grant gates every call) nor mount anything
//! the kernel's own validation refuses.
//!
//! It is a **pure-Rust** program; on the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the file. The
//! live device path is metal-only because QEMU models no Pi USB; the
//! host-testable logic lives in the crate's `lib` target.

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
