//! Downstream boot consumer for the riscv64 Arch HAL port.
//!
//! The arch port (`kernel/arch/riscv64`) is a pure Arch HAL
//! implementation: it implements `rustos_arch_api::SchedulerArch`, the
//! monotonic clock, the hart-park primitive, and the PLIC register
//! driver, but it names no concrete kernel subsystem (`AGENTS.md`
//! §17.2 / §17.4). This crate is where the riscv64 boot pipeline that
//! *does* name `kernel/{core,mem,sec,irq}` and `kernel/sched/api`
//! lives, mirroring how x86_64 keeps its `BinArch` wrapper and
//! `BootInfo` assembly in the downstream `rustos-kernel` crate.
//!
//! It is an integration-test (Tooling) crate, so the §17.4 layering is
//! relaxed for it; `cargo xtask deps-check` exempts `tests/`.
//!
//! # Module map
//!
//! | Module       | Role                                                                 |
//! | ------------ | -------------------------------------------------------------------- |
//! | [`plic_irq`] | [`PlicIrqController`] — the `kernel/irq` `IrqController` bridge.      |
//! | `boot`       | `RiscvBinArch` + the `boot()` pipeline + boot-state slots (freestanding). |
//!
//! [`plic_irq`] is host-buildable so its mask-before-wake regression
//! test runs under `cargo test`; `boot` is gated to the freestanding
//! `virt`-board target because it drives the arch port's
//! freestanding-only `halt_current_hart` / SBI console and is exercised
//! by the QEMU boot vertical.

#![cfg_attr(freestanding, no_std)]
#![deny(missing_docs)]

// The boot pipeline builds an `Arc<RiscvBinArch>` for `BootInfo`, so it
// needs `alloc`. Only the freestanding build links it; the consuming
// bin provides the `#[global_allocator]`.
#[cfg(freestanding)]
extern crate alloc;

// Host tests use `std` (the `plic_irq` mock register file). The crate
// stays `no_std` for the freestanding build (`AGENTS.md` §1 — no hacks).
#[cfg(test)]
extern crate std;

pub mod plic_irq;
pub use plic_irq::PlicIrqController;

#[cfg(freestanding)]
pub mod boot;
#[cfg(freestanding)]
pub mod publish;

#[cfg(freestanding)]
pub use boot::{boot, try_boot, BootError, RiscvBinArch};
#[cfg(freestanding)]
pub use publish::{published_dtb, published_memory_map};
