//! Architecture-neutral kernel-side virtio wiring (Stage 4.D Item 4).
//!
//! This crate holds the kernel virtio facilities that are independent
//! of any CPU architecture:
//!
//! * [`virtio_factory`] — the per-driver [`KernelVirtioFactory`] that
//!   mints one capability-checked
//!   [`KernelVirtioHost`] over a
//!   fresh per-process [`DmaPool`](tairix_kernel_mem::DmaPool) for every
//!   loaded virtio-class driver.
//! * [`virtio_pci_walk`] — the ring-0 walk that maps a virtio-PCI
//!   device's register windows into a
//!   [`PciTransportWindows`](tairix_virtio::PciTransportWindows) and
//!   hands them to a caller-supplied transport builder.
//! * [`virtio_mmio_walk`] — the ring-0 walk that maps a `virt`-board
//!   virtio-MMIO slot's register window and hands it to a
//!   caller-supplied transport builder.
//!
//! Both walks are generic over the transport builder, so this crate
//! names only `lib/*` types and never the concrete
//! `drivers/bus/virtio` transports — keeping ring 0 off any
//! `drivers/bus/*` crate (: `kernel/* → lib/*`, never
//! a driver). The production builders are
//! `tairix_drv_bus_virtio::{PciTransport, MmioTransport}::new`.
//!
//! # Why a separate crate
//!
//! These facilities used to live in the `tairix-kernel` binary crate,
//! which depends on the x86_64 architecture port and therefore does not
//! build for the `riscv64gc-unknown-none-elf` / `aarch64-unknown-none`
//! `virt`-board targets. The riscv64 virtio-MMIO QEMU verticals need the
//! *same* factory and MMIO walk the x86_64 PCI path uses, so the
//! arch-neutral code moved here where every Tier-1 freestanding target
//! can link it without pulling in a foreign architecture port
//! (no duplication — shared code lives in one
//! place). `tairix-kernel` re-exports every item below, so its public
//! API is unchanged.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

// Host tests need `std` for the `kernel/mem` host-test doubles. The
// crate itself stays `no_std` for the freestanding build
// (no hacks).
#[cfg(test)]
extern crate std;

pub mod kernel_host;
pub mod kernel_mmio;
pub mod virtio_factory;
pub mod virtio_mmio_walk;
pub mod virtio_pci_walk;

pub use kernel_host::KernelVirtioHost;
pub use kernel_mmio::KernelMmioMapper;
pub use virtio_factory::{KernelVirtioFactory, KernelVirtioFactoryConfig};
pub use virtio_mmio_walk::{
    provision_virtio_mmio, VirtioMmioProvision, VirtioMmioWalkError, MAX_SLOTS,
};
pub use virtio_pci_walk::{
    provision_virtio_pci, VirtioPciWalkError, VirtioProvision, MAX_FUNCTIONS,
};
