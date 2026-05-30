//! Shared bring-up scaffolding for the Stage 4.D Item 4 virtio QEMU
//! integration tests (`AGENTS.md` §2.2 — no duplication).
//!
//! Every per-device QEMU vertical (virtio-blk, virtio-net, ...) boots
//! the production `rustos-kernel` pipeline, then on observing
//! `AuditEvent::BootCompleted` performs the *identical* device-agnostic
//! bring-up: carve a high, identity-mapped per-device DMA region from
//! the published firmware memory map, walk PCI through the real-hardware
//! bus, map the requested modern virtio function's four register windows
//! through the `CAP_MMIO_MAP`-gated `KernelMmioMapper`, build a
//! `PciTransport`, bind + route the device's MSI-X interrupt, mint a
//! `KernelVirtioHost` over the carved pool, and load a signed `.rxe`
//! through `rustos_drvhost::Host`. Only the final step — opening the
//! concrete driver and exercising the device — differs per vertical, so
//! that tail is supplied as a closure to `run_virtio_scenario`.
//!
//! The crate is freestanding-only: all items are gated behind
//! `cfg(all(target_arch = "x86_64", target_os = "none"))` so a host
//! `cargo build --workspace` compiles it to an empty library (it owns a
//! `#[global_allocator]` and bridges the freestanding panic handler,
//! both of which would conflict with `std`).

#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_std)]
#![deny(missing_docs)]

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod imp;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use imp::*;
