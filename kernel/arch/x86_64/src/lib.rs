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
//! ever returns the arch port halts the CPU.
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

// AP startup trampoline (Stage 3a (b)). Embedded as a dedicated
// `.ap_trampoline` section of the kernel image; the BSP-side installer
// in `smp::install_trampoline` copies the bytes to the chosen low
// physical frame at runtime.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(include_str!("ap_trampoline.s"), options(att_syntax));

// Stage 3a (c1) context-switch primitive. The Rust side
// (`context::switch`) is host-test-compilable; the bare-metal half is
// only meaningful on the freestanding target.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(include_str!("context.s"), options(att_syntax));

// Stage 3a (c2) common ISR prologue. Same gating as `context.s`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(include_str!("interrupts.s"), options(att_syntax));

// Stage 4.D Item 2-tail.2 external-IRQ thunks. Reserves the
// architectural vector range 0x30..=0xFE for external IRQs and
// publishes the per-vector stub-address table consumed by
// `kernel/arch/x86_64::irq`. Same freestanding gate as `interrupts.s`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(include_str!("external_irq.s"), options(att_syntax));

pub mod acpi;
pub mod apic;
pub mod apic_timer;
pub mod bootinfo;
pub mod bootmemory;
pub mod context;
/// x86_64 implementation of the Arch HAL context-switch surface
/// ([`rustos_arch_api::ContextSwitch`]): the
/// architecture-neutral first-frame seeding + task switch over the
/// bare-metal primitive in [`context`].
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in, like the sibling `timer_hal` / `percpu_hal` HAL slices. The
/// underlying `context` primitive itself carries no such gate.
#[cfg(feature = "sched-arch")]
pub mod context_hal;
/// x86_64 implementation of the Arch HAL platform-entropy surface
/// ([`rustos_arch_api::PlatformEntropy`]): the `RDSEED`/`RDRAND` on-die
/// random source the kernel seeds its CSPRNG reserve from.
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in.
#[cfg(feature = "sched-arch")]
pub mod entropy;
/// x86_64 page-fault (`#PF`, vector 14) entry + settable fault hook: the
/// dedicated, error-code-aware page-fault ISR the production IDT installs
/// on vector 14 and the set-once fault observer the kernel reaches it
/// through. The x86_64 analogue of the riscv64/aarch64 `fault` modules
/// (fail closed). The error-code decode + the
/// observer slot build on the host (so their unit tests run under `cargo
/// test`); the naked ISR stub + `CR2` read are freestanding-only.
pub mod fault;
pub mod gdt;
/// Stage 3a (c7-arch): Arch HAL [`rustos_arch_api::SchedulerArch`]
/// implementation for x86_64.
///
/// Feature-gated on `sched-arch` so a freestanding consumer that does
/// not need the arch handle compiles neither this module nor the
/// `rustos-arch-api` dependency it pulls in — e.g. the Stage-2 QEMU
/// bin `tests/integration/memory_isolation`, which exercises
/// page-table isolation rather than scheduling. The `rustos-kernel`
/// bin enables the feature; see the `kernel/arch/x86_64/Cargo.toml`
/// comment for the rationale (pay for what you use).
#[cfg(feature = "sched-arch")]
pub mod hybrid;
pub mod interrupts;
pub mod irq;
#[cfg(feature = "sched-arch")]
pub mod kernel_arch;
/// x86_64 implementation of the Arch HAL memory-tagging surface
/// ([`rustos_arch_api::MemoryTagging`]). Mainstream
/// x86_64 has no per-granule memory tagging, so the port declares it an
/// honest `Unsupported` (see the module docs).
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in.
#[cfg(feature = "sched-arch")]
pub mod memtag;
pub mod multiboot2;
pub mod percpu;
/// x86_64 implementation of the Arch HAL per-CPU storage surface
/// ([`rustos_arch_api::PerCpu`]): the GS-base MSR
/// (`IA32_GS_BASE`) read/write the per-CPU anchor is reached through.
/// Distinct from [`percpu`], which owns the per-CPU GDT/IDT/IST-stack
/// bring-up the GS base points at.
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in.
#[cfg(feature = "sched-arch")]
pub mod percpu_hal;
pub mod pic;
/// x86_64 implementation of the Arch HAL port-I/O seams: the 32-bit
/// [`rustos_abi::PortIo`] backend the `lib/pci` PCI mechanism
/// consumes for PCI configuration access, and the 8-bit
/// [`rustos_abi::PortIo8`](rustos_abi::driver::port_io::PortIo8) backend
/// the `drivers/input/ps2` i8042 driver consumes — both reached only
/// through `&dyn`.
pub mod pio;
/// x86_64 implementation of the Arch HAL early-boot platform-discovery
/// surface ([`rustos_arch_api::PlatformDiscovery`]): the ACPI MADT → [`rustos_abi::hwtree`] normalisation built on
/// the [`acpi`] parser.
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in.
#[cfg(feature = "sched-arch")]
pub mod platform;
pub mod preempt;
pub mod pvh;
pub mod qemu_exit;
pub mod serial;
/// x86_64 implementation of the Arch HAL side-channel mitigation
/// surface ([`rustos_arch_api::SideChannelMitigation`]).
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in — so a
/// freestanding consumer that does not need the Arch HAL compiles
/// neither this module nor the dependency.
#[cfg(feature = "sched-arch")]
pub mod sidechannel;
pub mod smp;
pub mod syscall_entry;
/// x86_64 implementation of the Arch HAL timer-programming surface
/// ([`rustos_arch_api::Timer`]): the architecture-
/// neutral scheduler-tick callback install + dispatch over the LAPIC
/// timer wired in [`preempt`].
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in.
#[cfg(feature = "sched-arch")]
pub mod timer_hal;
pub mod tlb_shootdown;
/// x86_64 Time-Stamp Counter suitability validation: the Invariant TSC
/// CPUID probe the boot path consults before trusting `RDTSC` as a
/// cross-CPU monotonic clock source.
pub mod tsc;
/// x86_64 implementation of the Arch HAL "enter user mode" surface
/// ([`rustos_arch_api::EnterUser`]): the one `iretq`
/// sequence that drops a built process image into ring 3.
///
/// Gated on `sched-arch` — the feature that pulls in the
/// `rustos-arch-api` dependency this module's trait lives in — so a
/// freestanding consumer that does not need the Arch HAL compiles
/// neither this module nor the dependency.
#[cfg(feature = "sched-arch")]
pub mod userentry;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod idt;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod paging;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod entry;

/// Linear address of the default ISR thunk exported by `interrupts.s`.
///
/// Returned as a `u64` so callers can populate
/// [`interrupts::IdtEntry::interrupt_gate`] without an additional cast.
/// Only meaningful on the freestanding target — the symbol is provided
/// by the bundled assembly.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub(crate) fn interrupts_default_isr_addr() -> u64 {
    extern "C" {
        fn rustos_arch_x86_64_isr_default();
    }
    rustos_arch_x86_64_isr_default as *const () as usize as u64
}

/// Multiboot2 magic the bootloader passes in `%eax` to `_start`
/// (multiboot2 spec §3.1.2).
///
/// Re-exported as a `const` so test binaries can include their own
/// regression test that the value never drifts from the multiboot2 spec.
pub const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36D7_6289;
