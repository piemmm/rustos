//! QEMU integration test: the aarch64 software-managed Access Flag (the
//! cold-page referenced bit), read and cleared through the Arch HAL.
//!
//! ## What this test asserts
//!
//! The compressed-memory tier (`plans/SWAPSWAPSWAP.md`) reclaims a task's
//! anonymous page only once the page-replacement clock scan
//! (`kernel/mem::coldscan`) has shown it *genuinely cold* — untouched
//! across a full scan pass. That decision rests on a per-page referenced
//! bit exposed through
//! `tairix_arch_api::mmu::AddressSpace::test_and_clear_accessed`. On
//! aarch64 that bit is the Access Flag (AF, descriptor bit 10). The QEMU
//! CPU is `cortex-a72`, which lacks ARMv8.1 HAFDBS hardware AF management,
//! so the architecture raises an **Access-Flag fault** when a valid leaf
//! whose AF is clear is accessed — the software referenced-bit mechanism.
//!
//! This vertical proves the aarch64 port reports it honestly, on real
//! (emulated) hardware where the software `HostPageTable` double cannot:
//!
//! 1. A freshly-mapped page (eager AF) reads **set**; the probe clears AF
//!    (and invalidates the TLB entry).
//! 2. With no access since the clear, the next probe reads **clear** — the
//!    "genuinely untouched" verdict the scanner acts on.
//! 3. A genuine read of the page now takes an Access-Flag fault; the
//!    synchronous-exception path sets AF back and retries, so the read
//!    succeeds **without** reaching the unexpected-fault handler.
//! 4. The next probe reads **set** again (the fault handler re-set AF),
//!    proving the whole clock/second-chance mechanism end to end.
//!
//! It also checks the fail-closed edges: a misaligned address is rejected
//! and an unmapped address reports "not mapped", never a fabricated
//! verdict, and the port declares `AccessTracking::Supported`.
//!
//! Any other outcome is a closed failure reported to QEMU.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel;

// Host stub. The crate is *only* meaningful on the bare-metal aarch64
// target; on the host we provide a no-op `main` so `cargo build` /
// `cargo test` against the host triple work for IDE indexing and so
// `cargo xtask ci` doesn't have to special-case this crate.
#[cfg(not(itest_aarch64))]
fn main() {}
