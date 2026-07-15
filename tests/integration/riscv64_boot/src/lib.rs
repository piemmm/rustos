//! Downstream boot consumer for the riscv64 Arch HAL port.
//!
//! The arch port (`kernel/arch/riscv64`) is a pure Arch HAL
//! implementation: it implements `rustos_arch_api::SchedulerArch`, the
//! monotonic clock, the hart-park primitive, and the PLIC register
//! driver, but it names no concrete kernel subsystem. The riscv64 boot pipeline that *does* name
//! `kernel/{core,mem,sec}` and `kernel/sched/api` lives in the
//! production `rustos-kernel` crate (`rustos_kernel::riscv64::boot`),
//! mirroring how x86_64 / aarch64 keep their `BinArch` wrapper and
//! `BootInfo` assembly there. This crate is the test-side wrapper that
//! consumes that one pipeline: it re-exports the
//! production types and adds the device-bring-up observers + the PLIC
//! `IrqController` bridge the riscv64 QEMU verticals need.
//!
//! It is an integration-test (Tooling) crate, so the layering is
//! relaxed for it; `cargo xtask deps-check` exempts `tests/`.
//!
//! # Module map
//!
//! | Module    | Role                                                                 |
//! | --------- | -------------------------------------------------------------------- |
//! | `publish` | Set-once firmware-map / DTB observers for the device verticals (freestanding). |
//! | `boot`    | Thin wrapper: publish, then delegate to `rustos_kernel::riscv64::boot` (freestanding). |
//!
//! The `kernel/irq` `IrqController` bridge over the arch port's PLIC register
//! driver (`PlicIrqController`) is the production
//! `rustos_kernel::riscv64_plic_irq` definition, re-exported here for the
//! freestanding `virt`-board verticals (one definition, no duplication); its
//! mask-before-wake / re-arm regression test lives with it and runs under
//! `cargo test` on the CI host. `boot` / `publish` are gated to the
//! freestanding `virt`-board target because they drive the arch port's
//! freestanding-only `halt_current_hart` / SBI console and are exercised by
//! the QEMU boot vertical.

#![cfg_attr(freestanding, no_std)]
#![deny(missing_docs)]

// Host tests use `std`. The crate stays `no_std` for the freestanding build
// (no hacks).
#[cfg(test)]
extern crate std;

#[cfg(freestanding)]
pub mod boot;
#[cfg(freestanding)]
pub mod publish;

#[cfg(freestanding)]
pub use boot::{boot, try_boot, BootError, RiscvBinArch};
#[cfg(freestanding)]
pub use publish::{published_dtb, published_memory_map};

// The single `PlicIrqController` definition lives in the production kernel
// (`rustos_kernel::riscv64_plic_irq`); re-export it so the `virt`-board
// virtio-MMIO verticals name it under the same path they always have. Only
// the freestanding target links the kernel, so the re-export is gated to it.
#[cfg(freestanding)]
pub use rustos_kernel::riscv64_plic_irq::PlicIrqController;
