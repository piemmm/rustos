//! Shared bring-up scaffolding for the Stage 4.D Item 4 virtio QEMU
//! integration tests (no duplication).
//!
//! Every per-device QEMU vertical (virtio-blk, virtio-input) on every
//! architecture (x86_64 PCI, riscv64 `virt`-board MMIO) boots the
//! production kernel pipeline, then on observing
//! `AuditEvent::BootCompleted` performs an architecture-specific bring-up
//! (carve a per-device DMA region, provision a transport, wire the device
//! interrupt, mint a `KernelVirtioHost`, load a signed `.rxe`) and runs
//! the *shared* device-agnostic `load → reload → device round-trip →
//! unload` lifecycle. Only the per-device tail differs, and even that is
//! shared across arches: the `virtio_blk_round_trip` and filesystem/users
//! tails are generic over the `tairix_virtio::Transport` trait
//! so the PCI and MMIO verticals run identical device code.
//!
//! The crate is freestanding-only: every item is gated to a
//! `target_os = "none"` target so a host `cargo build --workspace`
//! compiles it to an empty library (it owns a `#[global_allocator]` and
//! bridges the freestanding panic handler, both of which would conflict
//! with `std`).

#![cfg_attr(freestanding, no_std)]
#![deny(missing_docs)]

// Device-agnostic scaffolding, built for every freestanding vertical.
#[cfg(freestanding)]
mod common;
#[cfg(freestanding)]
pub use common::*;

// x86_64 PCI bring-up + `define_boot_harness!`.
#[cfg(itest_x86_64)]
mod imp_pci;
#[cfg(itest_x86_64)]
pub use imp_pci::*;

// riscv64 `virt`-board MMIO bring-up + `define_mmio_boot_harness!`.
#[cfg(itest_riscv64)]
mod imp_mmio;
#[cfg(itest_riscv64)]
pub use imp_mmio::*;

// aarch64 `virt`-board MMIO bring-up + `define_mmio_boot_harness_aarch64!`.
#[cfg(itest_aarch64)]
mod imp_mmio_aarch64;
#[cfg(itest_aarch64)]
pub use imp_mmio_aarch64::*;
