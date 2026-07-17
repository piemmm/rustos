//! TAIRiX user-space driver runtime host (`lib/drvrt`).
//!
//! A first-party driver that runs in **user space** (the microkernel goal)
//! gets its `register(host: &dyn DriverHost)` entry the same
//! [`DriverHost`](tairix_abi::DriverHost) surface an in-kernel driver gets —
//! but the concrete host can no longer reach the kernel's frame allocator or
//! identity map directly. Instead it maps a **granted** device resource over
//! the `abi-v1` syscall surface (`plans/PI.md` P10 chunk 5d-0): the kernel
//! mints the driver one unforgeable handle per [`HwResource`](tairix_abi::hwtree::HwResource) its matched
//! hardware-tree node requested (and *no more*), and
//! the driver maps a register window with the `mmio_map` syscall and carves a
//! coherent DMA buffer with the `dma_alloc` syscall, passing those handles.
//!
//! [`RtDriverHost`] is that host. It is the user-space analogue of the
//! in-kernel keyboard service's `IdentityMmioMapper` + frame-allocator DMA
//! host: it implements [`DriverHost`](tairix_abi::DriverHost),
//! [`MmioMapper`](tairix_abi::MmioMapper),
//! and [`VirtioHost`](tairix_abi::driver::virtio::VirtioHost) over a small
//! table of kernel-issued grants, resolving a driver's requested
//! `(phys_base, len)` window to the grant that covers it and mapping it once.
//!
//! # Not a privileged path
//!
//! The host adds **no** authority. It only translates a driver's request into
//! the grant handle the kernel already minted and issues the syscall; every
//! capability check and bounds validation happens kernel-side, on the far side
//! of the trap. A forged or another task's handle resolves
//! to nothing kernel-side and the syscall is refused. The host re-checks the
//! load-time capability bitmap up front purely so a missing grant fails fast
//! without a round trip (the kernel re-checks regardless).
//!
//! # Allocation-free and fail-closed
//!
//! The crate is `no_std` and allocation-free: the grant table is a
//! fixed-capacity array sized by [`MAX_GRANTS`], so a driver process works
//! before the userland heap is available (`plans/SPAWN.md` `SP5b`). Every error
//! path denies — a missing capability, an unmappable request, a window no
//! grant covers, or a kernel refusal returns an error, never a fabricated
//! pointer or a panic.
//!
//! # Testing seam
//!
//! The two syscalls the host issues live behind the [`GrantSyscalls`] trait so
//! the resolution and translation logic is exercised on the host without a
//! kernel. Production driver processes use [`RtGrantSyscalls`], which forwards
//! to `tairix_rt`'s wrappers — the one syscall trap.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod host;
mod syscalls;

pub use host::{GrantedResource, RtDriverHost, MAX_GRANTS};
pub use syscalls::{GrantSyscalls, RtGrantSyscalls};

#[cfg(test)]
mod tests;
