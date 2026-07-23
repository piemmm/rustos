//! QEMU integration test: the riscv64 software-managed Accessed bit (the
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
//! riscv64 that bit is the Accessed bit (A, PTE bit 6). RISC-V leaves A/D
//! update implementation-defined; the riscv64 QEMU runner pins the CPU to
//! `svade=true,svadu=false`, so the hardware raises a page fault when A
//! must be set — the software referenced-bit mechanism the kernel must
//! carry for conservative silicon.
//!
//! This vertical proves the riscv64 port reports it honestly, on real
//! (emulated) Svade hardware where the software `HostPageTable` double
//! cannot:
//!
//! 1. A freshly-mapped page (eager A) reads **set**; the probe clears A
//!    (and invalidates the TLB entry).
//! 2. With no access since the clear, the next probe reads **clear** — the
//!    "genuinely untouched" verdict the scanner acts on.
//! 3. A genuine read of the page now takes a load page fault; the trap
//!    path sets A back and retries, so the read succeeds **without**
//!    reaching the unexpected-fault handler.
//! 4. The next probe reads **set** again (the fault path re-set A),
//!    proving the whole clock/second-chance mechanism end to end.
//!
//! It also checks the fail-closed edges: a misaligned address is rejected
//! and an unmapped address reports "not mapped", never a fabricated
//! verdict, and the port declares `AccessTracking::Supported`.
//!
//! Any other outcome is a closed failure reported to QEMU.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_riscv64)]
mod kernel;

// Host stub. The crate is *only* meaningful on the bare-metal riscv64
// target; on the host we provide a no-op `main` so `cargo build` /
// `cargo test` against the host triple work for IDE indexing and so
// `cargo xtask ci` doesn't have to special-case this crate.
#[cfg(not(itest_riscv64))]
fn main() {}
