//! RustOS riscv64 architecture port.
//!
//! Stage 0 reserves this crate so the workspace builds end-to-end; the
//! bulk of the port (boot trampoline, paging, traps, SMP) is delivered by
//! **Stage 3c** of `PLAN.md`. Ahead of that, [`qemu_exit`] lands the
//! `virt`-board `SiFive` Test finisher the Stage 4.D virtio-MMIO QEMU
//! integration tests need to report their result.
#![no_std]

pub mod qemu_exit;
