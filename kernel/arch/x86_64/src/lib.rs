//! RustOS architecture port: x86_64 (Stage 3a, partial).
//!
//! This crate is the **minimum** x86_64 boot path required by the two
//! Stage-2 deliverable integration tests under `tests/integration/`. The
//! full Stage 3a surface (UEFI, ACPI, APIC, SMP, context switch, syscall
//! entry) is tracked separately in `PLAN.md`'s Stage 3a sub-checklist.
//!
//! # What's here
//!
//! * Multiboot1 entry, 32→64 long-mode trampoline, identity-mapped
//!   bootstrap paging — see `boot.s`.
//! * A 16550-compatible serial console on COM1 (`serial`).
//! * The smallest IDT that can field a `#PF`/`#GP`/`#DF` and route it to
//!   a Rust callback registered by the test (`idt`).
//! * Primitives for building extra page tables and switching CR3 — the
//!   test driver for `tests/integration/memory_isolation` uses these to
//!   model two isolated address spaces (`paging`).
//! * The QEMU `isa-debug-exit` helper that translates a Pass/Fail result
//!   into the exit code `tools/qemu` recognises (`qemu_exit`).
//!
//! # Contract with test binaries
//!
//! A binary that links this crate provides a single
//! `#[no_mangle] extern "C" fn kernel_main() -> !` function. The boot
//! stub calls into `rustos_arch_x86_64_main` (this crate's `extern "C"`
//! entry), which validates the multiboot magic, brings the platform up
//! to a sane minimum, then transfers to `kernel_main`. If `kernel_main`
//! ever returns the arch port halts the CPU per `AGENTS.md` §2.9.
//!
//! # Why no Rust crate dependencies on `kernel/core` here?
//!
//! `kernel/core::KernelArch` is the eventual integration point but pulls
//! in `alloc::sync::Arc` and the full `IdentityTable` builder. The
//! Stage-2 QEMU tests deliberately do not exercise that surface (they
//! verify the *underlying* page-table isolation, not the orchestration
//! around it). Wiring `KernelArch` into the x86_64 port is on the Stage
//! 3a checklist.

#![no_std]
#![cfg_attr(not(any(test, target_arch = "x86_64")), allow(dead_code))]
#![deny(missing_docs)]

// The boot trampoline is only meaningful on the bare-metal target. When
// `cargo test` runs the host-target unit tests in this crate the
// assembly is omitted so the file builds on x86_64-linux too.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

pub mod qemu_exit;
pub mod serial;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod idt;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod paging;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod entry;

/// Multiboot2 magic the bootloader passes in `%eax` to `_start`
/// (multiboot2 spec §3.1.2).
///
/// Re-exported as a `const` so test binaries can include their own
/// regression test that the value never drifts from the multiboot2 spec.
pub const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36D7_6289;
