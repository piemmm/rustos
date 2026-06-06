//! RustOS microkernel binary support library (Stage 3a (c7-bin)).
//!
//! This is the library half of the `rustos-kernel` crate. It carries the
//! per-instruction-set boot pipelines that are reusable across the
//! production binary (`src/main.rs`) and the QEMU integration tests.
//! Pulling a pipeline into a library is the only way to satisfy
//! `AGENTS.md` §2.2 (no duplication) without leaking the test-only
//! audit-observer sink into the production binary through Cargo feature
//! unification.
//!
//! The build script (`build.rs`) selects the pipeline per instruction set
//! via the `kernel_isa` conditional-compilation name, so the production
//! kernel image is built for exactly one architecture at a time (the
//! single `AGENTS.md` §17.1/§17.2 selection point): the x86_64
//! Multiboot2/ACPI pipeline or the aarch64 (Raspberry Pi 4) boot path.
//!
//! # Module map
//!
//! | Module          | Role                                                                              |
//! | --------------- | --------------------------------------------------------------------------------- |
//! | [`bumpalloc`]   | Forward-only bump allocator + the `GlobalAlloc` impl shared by every bin.         |
//! | `arch_wrapper`  | `BinArch` — the in-crate `KernelArch` wrapper around `X86_64Arch` (x86_64).        |
//! | `dispatch`      | Fail-closed syscall-dispatch callback (x86_64).                                   |
//! | `boot`          | x86_64 `boot(multiboot_info, log_sink, audit_sink)` entry point (bare-metal only).|
//! | `boot_aarch64`  | aarch64 `boot(dtb, log_sink)` entry point (bare-metal only; `plans/PI.md` P1).     |
//!
//! # Why this is a library, not a `[[bin]]`
//!
//! Two consumers exist:
//!
//! 1. The production binary in `src/main.rs`.
//! 2. The QEMU integration test in
//!    `tests/integration/kernel_arch_boot/src/main.rs`, which supplies
//!    its own audit sink (one that flips the QEMU debug-exit device on
//!    `AuditEvent::BootCompleted`).
//!
//! Both consumers share the same boot pipeline — but they must *not*
//! share the audit sink, because exiting QEMU on `BootCompleted` is
//! a test-harness affordance that has no place in a production kernel
//! (`AGENTS.md` §5.4.5 — fail closed; the harness never decides what
//! the kernel does next).
//!
//! # `no_std`
//!
//! `no_std` is mandatory: every consumer of this library is a
//! freestanding bare-metal binary (`x86_64-unknown-none` or
//! `aarch64-unknown-none`). `extern crate alloc` is pulled in because
//! the architecture-neutral [`rustos_kernel_core::BootInfo`] hand-off
//! type holds an `Arc<KernelArch>`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// `alloc` is consumed by the bare-metal-only [`boot`] module which
// constructs an `Arc<BinArch>` for `kernel_core::BootInfo`. The
// `extern crate` declaration must live here at the crate root for the
// `boot` module to resolve `alloc::sync::Arc`; on host builds the
// declaration shows up as unused (`boot` is gated to the
// `freestanding` build) but stripping it would break the bare-metal
// build. `AGENTS.md` §15.10 — every `#[allow]` carries a justifying
// comment.
#[allow(unused_extern_crates)]
extern crate alloc;

// Host tests need `std` for `Box::leak` (`TestSink`) and friends. The
// crate itself remains `no_std` for production builds
// (`AGENTS.md` §1 — no hacks).
#[cfg(test)]
extern crate std;

// The boot pipeline is selected per instruction set by the build script
// (`build.rs` emits the `kernel_isa` name from `CARGO_CFG_TARGET_ARCH`),
// so the crate body never names `target_arch` inline — that decision
// lives in the build glue (`AGENTS.md` §17.2; `cargo xtask cfg-check`).
//
// The x86_64 pipeline (the Multiboot2/ACPI boot path, the `BinArch`
// `KernelArch` wrapper over `X86_64Arch`, the IO-APIC controller, the
// virtio bring-up, the fail-closed syscall-dispatch callback) compiles
// whenever the target instruction set is x86_64 — the CI host included,
// so its host unit tests run under `cargo test`.
pub mod bumpalloc;

#[cfg(kernel_isa = "x86_64")]
pub mod arch_wrapper;
#[cfg(kernel_isa = "x86_64")]
pub mod dispatch;
#[cfg(kernel_isa = "x86_64")]
pub mod ioapic_controller;
#[cfg(kernel_isa = "x86_64")]
pub mod virtio_boot;

#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod boot;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod panic_ctx;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod serial_sink;

// The aarch64 (Raspberry Pi 4) production boot path (`plans/PI.md` P1).
// Freestanding-only: it links the aarch64 port's console / FP-enable /
// park primitives, which exist only on the bare-metal aarch64 target.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub mod boot_aarch64;

// The build script's pure target-selection logic, compiled into the
// host test build so its rules are unit tested (`AGENTS.md` §7).
#[cfg(test)]
#[path = "build_support.rs"]
mod build_support;

pub use bumpalloc::BumpAllocator;

#[cfg(kernel_isa = "x86_64")]
pub use arch_wrapper::BinArch;
#[cfg(kernel_isa = "x86_64")]
pub use dispatch::{production_dispatch, DISPATCH_SLOT};
#[cfg(kernel_isa = "x86_64")]
pub use virtio_boot::{provision_and_run, VirtioBootConfig};
// The architecture-neutral virtio factory and provisioning walks now
// live in `rustos-kernel-virtio` so every architecture port can reuse
// them (`AGENTS.md` §2.2); re-exported here to keep this crate's public
// API unchanged.
pub use rustos_kernel_virtio::{
    provision_virtio_mmio, provision_virtio_pci, KernelVirtioFactory, KernelVirtioFactoryConfig,
    VirtioMmioProvision, VirtioMmioWalkError, VirtioPciWalkError, VirtioProvision, MAX_FUNCTIONS,
    MAX_SLOTS,
};

#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use boot::{boot, BootError};
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use panic_ctx::handle_panic_via_kernel_core;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use serial_sink::{SerialSink, SERIAL_SINK};
