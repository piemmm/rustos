//! RustOS microkernel binary support library (Stage 3a (c7-bin)).
//!
//! This is the library half of the `rustos-kernel` crate. It carries
//! every piece of the x86_64 boot pipeline that is reusable across the
//! production binary (`src/main.rs`) and the QEMU integration test
//! (`tests/integration/kernel_arch_boot`). Pulling the pipeline into a
//! library is the only way to satisfy `AGENTS.md` §2.2 (no
//! duplication) without leaking the test-only audit-observer sink into
//! the production binary through Cargo feature unification.
//!
//! # Module map
//!
//! | Module          | Role                                                                              |
//! | --------------- | --------------------------------------------------------------------------------- |
//! | [`bumpalloc`]   | Forward-only bump allocator + the `GlobalAlloc` impl shared by every bin.         |
//! | [`arch_wrapper`]| [`BinArch`] — the in-crate `KernelArch` wrapper around `X86_64Arch`.              |
//! | [`dispatch`]    | Fail-closed syscall-dispatch callback (Stage 2.7 will replace with a real wire-up).|
//! | `boot`          | The `boot(multiboot_info, log_sink, audit_sink)` entry point (bare-metal only).    |
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
//! freestanding `x86_64-unknown-none` binary. `extern crate alloc` is
//! pulled in because the architecture-neutral
//! [`rustos_kernel_core::BootInfo`] hand-off type holds an
//! `Arc<KernelArch>`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// `alloc` is consumed by the bare-metal-only [`boot`] module which
// constructs an `Arc<BinArch>` for `kernel_core::BootInfo`. The
// `extern crate` declaration must live here at the crate root for the
// `boot` module to resolve `alloc::sync::Arc`; on host builds the
// declaration shows up as unused (`boot` is `cfg`-gated to
// `target_os = "none"`) but stripping it would break the bare-metal
// build. `AGENTS.md` §15.10 — every `#[allow]` carries a justifying
// comment.
#[allow(unused_extern_crates)]
extern crate alloc;

// Host tests need `std` for `Box::leak` (`TestSink`) and friends. The
// crate itself remains `no_std` for production builds
// (`AGENTS.md` §1 — no hacks).
#[cfg(test)]
extern crate std;

pub mod arch_wrapper;
pub mod bumpalloc;
pub mod dispatch;
pub mod ioapic_controller;
pub mod virtio_boot;
pub mod virtio_factory;
pub mod virtio_pci_walk;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod boot;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod panic_ctx;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub mod serial_sink;

pub use arch_wrapper::BinArch;
pub use bumpalloc::BumpAllocator;
pub use dispatch::{production_dispatch, DISPATCH_SLOT};
pub use virtio_boot::{provision_and_run, VirtioBootConfig};
pub use virtio_factory::{KernelVirtioFactory, KernelVirtioFactoryConfig};
pub use virtio_pci_walk::{provision_virtio_pci, VirtioPciWalkError, MAX_FUNCTIONS};

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use boot::{boot, BootError};
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use panic_ctx::handle_panic_via_kernel_core;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use serial_sink::{SerialSink, SERIAL_SINK};
