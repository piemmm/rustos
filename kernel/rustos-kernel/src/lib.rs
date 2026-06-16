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
//! | `dispatch_core` | Arch-neutral syscall-dispatch helpers shared by every port (host-tested).         |
//! | `arch_wrapper`  | `BinArch` — the in-crate `KernelArch` wrapper around `X86_64Arch` (x86_64).        |
//! | `dispatch`      | Fail-closed syscall-dispatch callback (x86_64).                                   |
//! | `arch_wrapper_aarch64` | `Aarch64BinArch` `KernelArch` wrapper + `UartConsole` (aarch64).            |
//! | `dispatch_aarch64` | Fail-closed syscall-dispatch callback (aarch64).                              |
//! | `boot`          | x86_64 `boot(multiboot_info, log_sink, audit_sink)` entry point (bare-metal only).|
//! | `boot_aarch64`  | aarch64 `boot(dtb, log_sink, audit_sink)` entry point (bare-metal only; `plans/PI.md` P1).|
//! | `mem_map`       | aarch64 `/memory` → `BootMemoryMap` builder (host-tested; `plans/PI.md` P6c-1).    |
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

// The architecture-neutral syscall-dispatch helpers (frame read, errno
// encoding, slot forwarding) shared by every port's `production_dispatch`
// callback (`AGENTS.md` §2.2). Un-gated: it names only unconditional
// `kernel/*` + `lib/abi` deps, so it compiles on every target and the CI
// host, where its unit tests run.
pub mod dispatch_core;

// The VL805/xHCI USB-keyboard composition (`plans/PI.md` P10): assembles
// the BCM2711 PCIe root-complex bring-up, the windowed PCI config
// mechanism, the xHCI controller engine, and the HID boot-keyboard →
// console-byte producer into one chain feeding a console's input queue.
// `rustos-kernel` (`Layer::Tooling`) is the one crate permitted to name
// those driver crates across strata (`AGENTS.md` §17.4 / §8). The engine
// is architecture-neutral (it consumes only the `lib/abi` driver seams
// and the discovered `HwNode`), so it is un-gated — it compiles on every
// target and its host unit tests run on the CI host; the aarch64 boot
// path supplies the concrete `DriverHost` and generic-timer `Delay` that
// drive it on metal (`plans/PI.md` P10 "Remaining").
pub mod keyboard_service;
pub mod usb_keyboard;

// The in-kernel driver registry (`plans/PI.md` P10 5c / PLAN Stage 4.HW
// item 5): the single source of truth pairing each in-kernel driver's
// canonical `BIND_KEYS`, `/System/Drivers/` image path, build-signed
// manifest image, and `register()` entry, and resolving a discovered
// hardware node against them through the shared `lib/devmatch` policy — the
// data-driven replacement for hand-sequenced bring-up. It carries each
// driver's `register()` entry (a `rustos-drvhost` type) and `include!`s the
// signed manifest images `build.rs` bakes, so it is gated on the two
// instruction sets where `rustos-drvhost` is a dependency of this crate —
// `x86_64` (the CI host, where its unit tests run) and `aarch64` (the
// Raspberry Pi 4 boot path that consumes it to gate the live VL805 bring-up
// on a match).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod driver_catalog;

// The in-kernel signed-driver-load gate (`plans/PI.md` P10 5c-ii): admits
// any driver in the `driver_catalog` registry through `drvhost::Host::load`
// (Ed25519 signature against the build's embedded driver-signing key + the
// `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` gates), generic over hardware. Gated on
// the two instruction sets where `rustos-drvhost` is a dependency of this
// crate — `x86_64` (the CI host, where its unit tests run) and `aarch64`
// (the Raspberry Pi 4 boot path that consumes it); riscv64 does not link
// `drvhost` and never reaches this path.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod driver_loader;

#[cfg(kernel_isa = "x86_64")]
pub mod arch_wrapper;
#[cfg(kernel_isa = "x86_64")]
pub mod dispatch;

// The aarch64 (Raspberry Pi 4) `KernelArch` wrapper + console device and
// the fail-closed `svc` dispatch callback (`plans/PI.md` P6c-2). Gated on
// the aarch64 instruction set so they link the aarch64 port; their unit
// tests run under an aarch64-host `cargo test`, exactly as the x86_64
// `arch_wrapper`/`dispatch` modules' tests run on the x86_64 CI host.
#[cfg(kernel_isa = "aarch64")]
pub mod arch_wrapper_aarch64;
#[cfg(kernel_isa = "aarch64")]
pub mod dispatch_aarch64;

// The riscv64 (QEMU `virt` / SiFive) fail-closed `ecall` dispatch callback
// (`plans/PI.md` RV-P2): the riscv64 sibling of `dispatch`/`dispatch_aarch64`,
// installed before user space can be entered so the production paged boot
// routes each syscall through the resident `DispatchHook`. Gated on the
// riscv64 instruction set so it links the riscv64 port; the arch-neutral
// dispatch logic it wraps is unit-tested once in `dispatch_core`.
#[cfg(kernel_isa = "riscv64")]
pub mod dispatch_riscv64;
#[cfg(kernel_isa = "x86_64")]
pub mod ioapic_controller;
#[cfg(kernel_isa = "x86_64")]
pub mod virtio_boot;

// The in-kernel `DriverHost` composition (`plans/PI.md` P10): assembles a
// `drvhost::Host` that serves a loaded driver both an MMIO mapper and a
// per-driver DMA host, so a non-virtio, DMA-driving bus driver (the VL805
// xHCI behind the BCM2711 PCIe root complex) can map its own register
// windows and carve its DMA region through the capability-gated kernel
// facilities. The helper itself is generic over the page-table backend;
// it is gated on `kernel_isa = "x86_64"` (as `virtio_boot` is) because
// `rustos-drvhost` / `rustos-kernel-virtio` are target-conditional
// dependencies of this crate today — the host-only `cargo test`/clippy run
// (x86_64) exercises and proves it, and the aarch64 metal boot invocation
// that consumes it lands with its own dependency additions and gate
// extension (`plans/PI.md` P10 "Remaining").
#[cfg(kernel_isa = "x86_64")]
pub mod driver_host;

#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod boot;

// The x86_64 PID 1 (`init`) spawn seam (`plans/PI.md` X3a): the
// `rustos_kernel_core::InitSpawn` implementation `boot` installs into the
// `BootInfo` hand-off so `kernel_main` drops into ring 3 after boot
// completes — the cross-port sibling of the aarch64 `init_spawn`.
// Freestanding+x86_64-only: it links the x86_64 port's page-table /
// `EnterUser` / `set_kernel_rsp0` primitives and `include!`s the build-time
// `init` `rxe` blob.
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod init_spawn_x86_64;

// The x86_64 runtime `spawn` producer + embedded-program registry
// (`plans/PI.md` X3b): the `rustos_kernel_core::ProcessSpawn` implementation
// `boot` installs into the `BootInfo` hand-off so the `spawn` syscall can
// build a fresh, hardware-isolated child PML4 and admit it Ready, so PID 1
// `init` can launch the user's session — the cross-port sibling of the
// aarch64 `spawn_producer`. Freestanding+x86_64-only: it links the x86_64
// port's page-table / `EnterUser` / `set_kernel_rsp0` primitives and
// `include!`s the build-time `Shell` `rxe` blob.
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod panic_ctx;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod serial_sink;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub mod spawn_producer_x86_64;

// The aarch64 (Raspberry Pi 4) production boot path (`plans/PI.md` P1).
// Freestanding-only: it links the aarch64 port's console / FP-enable /
// park primitives, which exist only on the bare-metal aarch64 target.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub mod boot_aarch64;

// The riscv64 (QEMU `virt` / SiFive) production boot path (`plans/PI.md`
// RV-P1): boot the BSP to `kernel_core::kernel_main` →
// `AuditEvent::BootCompleted` over the device-tree-discovered RAM window
// and timer rate. Freestanding-only: it links the riscv64 port's
// `halt_current_hart` / SBI console primitives, which exist only on the
// bare-metal riscv64 target. The riscv64 QEMU verticals
// (`tests/integration/riscv64_boot` and the bins it backs) consume this
// very pipeline, so there is exactly one riscv64 boot orchestration
// (`AGENTS.md` §2.2).
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
pub mod boot_riscv64;

// The riscv64 PID 1 (`init`) spawn seam (`plans/PI.md` RV-P3): the
// `rustos_kernel_core::InitSpawn` implementation `boot_riscv64` installs into
// the `BootInfo` hand-off so `kernel_main` drops into U-mode after boot
// completes — the cross-port sibling of the aarch64 `init_spawn` / x86_64
// `init_spawn_x86_64`. Freestanding+riscv64-only: it links the riscv64 port's
// Sv39 page-table / `EnterUser` primitives and `include!`s the build-time
// `init` `rxe` blob.
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
pub mod init_spawn_riscv64;

// The riscv64 runtime `spawn` producer + embedded-program registry
// (`plans/PI.md` RV-P3 / `plans/SPAWN.md` SP3b): the
// `rustos_kernel_core::ProcessSpawn` implementation `boot_riscv64` installs into
// the `BootInfo` hand-off so the `spawn` syscall can build a fresh,
// hardware-isolated child Sv39 space and admit it Ready, so PID 1 `init` can
// launch the user's session. Freestanding+riscv64-only: it links the riscv64
// port's page-table / `EnterUser` primitives and `include!`s the build-time
// `Shell` `rxe` blob.
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
pub mod spawn_producer_riscv64;

// The aarch64 PID 1 (`init`) spawn seam (`plans/PI.md` P6c-3): the
// `rustos_kernel_core::InitSpawn` implementation `boot_aarch64` installs
// into the `BootInfo` hand-off so `kernel_main` drops into user mode after
// boot completes. Freestanding+aarch64-only: it links the aarch64 port's
// page-table / `EnterUser` primitives and `include!`s the build-time `init`
// `rxe` blob.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub mod init_spawn;

// The aarch64 runtime `spawn` producer + embedded-program registry
// (`plans/SPAWN.md` SP3b): the `rustos_kernel_core::ProcessSpawn`
// implementation `boot_aarch64` installs into the `BootInfo` hand-off so the
// `spawn` syscall can build a fresh, hardware-isolated child address space
// and admit it Ready. Freestanding+aarch64-only: it links the aarch64 port's
// page-table / `EnterUser` primitives and `include!`s the build-time `Shell`
// `rxe` blob.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub mod spawn_producer;

// The boot memory-map arithmetic (`plans/PI.md` P6c-1, G3b-2): the aarch64
// `/memory` → `BootMemoryMap` window translation and the shared
// guard-arena sizing / carving every port uses to reserve the kthread-stack
// guard arena. The arithmetic is free of the bare-metal-only ports, so it
// compiles — and its bounds-check unit tests run — on the CI host under
// `cargo test` as well as on each production build that consumes it
// (`boot_aarch64`, `boot`, `boot_riscv64`). Gated to exactly those
// configurations so it is never dead code (`AGENTS.md` §2.3); the per-port
// carve helpers are further gated to the port(s) that use them.
#[cfg(any(
    all(
        freestanding,
        any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
    ),
    test
))]
mod mem_map;

// The guarded kthread kernel-stack arena (`plans/PI.md` G3b-2): the
// forward-only bump allocator that hands kthread kernel stacks out of the
// boot-reserved guard arena (`mem_map`) so a stack's guard page can be
// unmapped in the owning task's root and an overrun faults in hardware
// (`init_spawn` on aarch64, `init_spawn_x86_64` on x86_64,
// `init_spawn_riscv64` on riscv64). Its bump arithmetic is free of the
// bare-metal ports, so it compiles — and its unit tests run — on the CI
// host as well as on the bare-metal production builds that consume it, and
// on no other configuration, so it is never dead code (`AGENTS.md` §2.3).
#[cfg(any(
    all(
        freestanding,
        any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
    ),
    test
))]
mod stack_arena;

// The build script's pure target-selection logic, compiled into the
// host test build so its rules are unit tested (`AGENTS.md` §7).
#[cfg(test)]
#[path = "build_support.rs"]
mod build_support;

pub use bumpalloc::BumpAllocator;

#[cfg(kernel_isa = "x86_64")]
pub use arch_wrapper::BinArch;
#[cfg(kernel_isa = "aarch64")]
pub use arch_wrapper_aarch64::{Aarch64BinArch, UartConsole, UART_CONSOLE};
#[cfg(kernel_isa = "x86_64")]
pub use dispatch::{production_dispatch, DISPATCH_SLOT};
#[cfg(kernel_isa = "x86_64")]
pub use driver_host::{run_with_driver_host, DriverHostConfig};
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
#[cfg(all(freestanding, kernel_isa = "riscv64"))]
pub use boot_riscv64::{
    boot, build_boot_memory_map, try_boot, BootError, RiscvBinArch, RiscvUartConsole,
    RISCV_UART_CONSOLE,
};
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use panic_ctx::handle_panic_via_kernel_core;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use serial_sink::{Com1Console, SerialSink, COM1_CONSOLE, SERIAL_SINK};
