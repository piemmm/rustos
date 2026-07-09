//! The `Run` entry-point binary of the USB **mass-storage class driver**,
//! installed as a signed `/System/Drivers/` bundle and autoloaded into user
//! space by `devmgr` when a bulk-only mass-storage **interface** node is
//! discovered (`plans/DEVICES.md` D2).
//!
//! This is a pure *class* driver: it touches **no** controller register,
//! owns **no** controller DMA, and holds no IRQ line. The USB
//! host-controller driver (`drivers/bus/usb/xhci`) owns the controller,
//! enumerates the device, publishes one node per interface, and serves that
//! interface's transfers over the bus-agnostic URB transport. This driver
//! binds the mass-storage interface node, reads the device's own
//! configuration descriptor to learn its interface number and bulk endpoint
//! pair, brings each logical unit up over BOT/SCSI
//! (`rustos_drv_storage_usb_msd::bot`), emits one **storage-class**
//! hardware-tree node per ready LUN carrying a block-service call endpoint
//! and a shared data window (`rustos_abi::blkio`), and serves those
//! endpoints for the life of the device. It knows neither the controller
//! type nor the bus.
//!
//! # Least privilege
//!
//! It holds only `CAP_SHM` (map the granted URB buffer; create the per-LUN
//! data windows), `CAP_IPC_ENDPOINT` (submit URBs on its one interface's
//! transport endpoint), `CAP_IPC_BIND_PRIVILEGED` (bind the per-LUN
//! block-service endpoints it serves), `CAP_HW_EMIT` (publish/retract the
//! per-LUN storage nodes), and `CAP_LOG_EMIT` (diagnostics). No MMIO, no
//! DMA, no IRQ: a compromised disk driver cannot reprogram the controller
//! or reach another device's buffers.
//!
//! # Event-driven, never a busy-poll
//!
//! The serve loop parks on a kernel wait-set over the per-LUN endpoints;
//! each block request executes as blocking URB `ipc_call`s (the HCD replies
//! when the controller completes the transfer), so the driver sleeps
//! between requests and inside each transfer rather than spinning.
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
