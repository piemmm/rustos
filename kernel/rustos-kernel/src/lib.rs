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
//! | `spawn_layout`  | Shared user-space layout constants for every port's spawn seam/producer (§2.2).   |
//! | `x86_64`        | The x86_64 port: `arch_wrapper`, `dispatch`, `boot`, `init_spawn`, `spawn_producer`, `ioapic_controller`, `virtio_boot`, `driver_host`, `panic_ctx`, `serial_sink`. |
//! | `aarch64`       | The aarch64 (Raspberry Pi 4) port: `arch_wrapper`, `dispatch`, `boot`, `init_spawn`, `spawn_producer` (`plans/PI.md` P1). |
//! | `riscv64`       | The riscv64 (QEMU `virt` / SiFive) port: `dispatch`, `boot`, `init_spawn`, `spawn_producer` (`plans/PI.md` RV-P1). |
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

// The production root-volume unlock + users-database load composition
// (`plans/PI.md` §3 P11 root-mount increment, Chunk A): turns the on-FAT
// `root.unlock` descriptor, the typed passphrase, and the encrypted root
// block device into the validated `users-v1` database
// `kernel/core::load_users_db_source` serves. `rustos-kernel`
// (`Layer::Tooling`) is the one layer permitted to name both the `rustfs`
// driver and `kernel/core` (`AGENTS.md` §17.4). It is architecture-neutral
// (it consumes only the `lib/abi` `Block` seam and the `rustfs`/`kernel/core`
// APIs), so it is un-gated — it compiles on every target and its host unit
// tests run on the CI host; the aarch64 boot path supplies the discovered
// root block device, the FAT-read descriptor, and the console passphrase
// in the following increment (`plans/PI.md` P11 Chunk B).
pub mod root_mount;

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

// The root-storage bind gate (`plans/PI.md` §3 P11 root-mount increment,
// Chunk B-2): resolves which discovered hardware-tree node carries the
// bootstrap root block device, and which floor block driver
// (`driver_catalog`) binds it, through the same shared `lib/devmatch`
// policy the user-space `devmgr` autoloader uses (`AGENTS.md` §18.3 /
// §18.6). It is the storage analogue of the keyboard bring-up's bind gate
// and the front half the production root mount (`root_mount`) builds on.
// Resolution only — it never reads or mounts a volume — so it is
// architecture-neutral and host-tested on the CI host; it is gated, like
// `driver_catalog` it depends on, on the two instruction sets where that
// registry compiles (`x86_64` and `aarch64`).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod root_storage;

// The runtime hardware-inventory store (Design D, D1 —
// `.junie/next-pi-prompt.md`): the single source of truth for the
// discovered hardware tree (`AGENTS.md` §18.1 / §2.2), seeded by the boot
// path, appended to by the floor bus bring-up, and snapshotted by the
// autoload reader (the reactive generation counter / wait + node removal
// land in Design D D2/D4 with their consumers, §2.3). Architecture-neutral
// and host-tested on the CI host; gated, like the `unlock_service` that
// drives it, on the two instruction sets where the boot/autoload path
// compiles.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod hwtree_store;

// The kernel block-device sharing layer (Design D, D2a —
// `.junie/next-pi-prompt.md`): wraps the one brought-up bootstrap-floor
// block device behind a `lib/sync` lock so it can back two concurrent
// partition windows — the read-only `/System` driver-store mount and the
// encrypted-root unlock window — over a single disk (`AGENTS.md` §4 — SMP
// serialisation). Architecture-neutral and host-tested on the CI host;
// gated, like the `unlock_service` boot path that wraps the device in it,
// on the two instruction sets where the boot path compiles.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod shared_block;

// The in-kernel root-unlock service (`plans/PI.md` §3 P11 root-mount
// increment, Chunk B-2 INCREMENT (2)): the post-MMU boot stash carrying
// the resolved `root_storage` binding + the firmware DTB pointer to the
// init seam, the console-0 ownership gate that stops `login` stealing the
// passphrase bytes, and (freestanding aarch64 only) the live virtio-blk
// bring-up + unlock-policy kthread. The device-independent core is
// architecture-neutral and host-tested on the CI host; it is gated, like
// the `root_storage` binding it consumes, on the two instruction sets
// where that gate compiles. The live bring-up is further gated on
// `freestanding` + `kernel_isa = "aarch64"` (the Raspberry Pi 4 / QEMU
// `virt` boot path).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod unlock_service;

// The user-space sibling of `driver_loader` (`plans/PI.md` P10 5d-2-ii):
// admits a discovered `kind = UserSpace` driver through the same signed
// `drvhost::Host::load` gate, then **spawns** it into its own
// hardware-isolated process, minting it one device-resource grant per
// `HwResource` its matched hardware-tree node requested (`AGENTS.md` §4 —
// drivers in user space; §18.3 — only the resources the matched node
// requested). It implements `rustos_devmgr::DriverLoader`, so the device
// manager's autoload walk drives it directly; the architecture-specific
// process creation sits behind the `DriverProcessSpawn` seam, so the gate +
// resource-threading logic is host-tested on the CI host. Gated, like
// `driver_loader`, on the two instruction sets where `rustos-drvhost` /
// `rustos-devmgr` are dependencies of this crate.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod driver_spawn_loader;

// The read-only `/System` file service (`.junie/next-pi-prompt.md` Design D
// D2b-1): one object over the mounted `/System` volume that both lists the
// signed `/System/Drivers/` store and reads a bundle's bytes (a
// `drvhost::ImageSource`) through the kernel-core `DriverImageReader`. It
// consolidates the store walk and the per-bundle reads behind one seam
// (`AGENTS.md` §2.2) — the seam the D2b-2 `IPC_RECV` endpoint will wrap. The
// bin crate is the one layer that may name `drvhost` (`AGENTS.md` §17.4), so
// this delegating service lives here; gated, like the other
// `drvhost`-consuming modules, on the two instruction sets where
// `rustos-drvhost` is a dependency of this crate.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod system_files;

// The production driver-autoload boot wiring (`plans/PI.md` P10 5d-2-ii):
// composes the signed-store scan (`drvhost::store::scan_store` over the
// `/System/Drivers/` bundle paths), the `devmgr` match walk, and the
// process-spawning `driver_spawn_loader` into the one entry the boot path
// drives to autoload user-space drivers by discovery (`AGENTS.md` §4 / §18).
// It names both `rustos_devmgr` and `rustos_drvhost`, so it is gated, like
// the other `drvhost`/`devmgr`-consuming modules, on the two instruction
// sets where those crates are dependencies of this crate — `x86_64` (the CI
// host, where its unit tests run) and `aarch64` (the Raspberry Pi 4 boot
// path that will drive it once the production root volume is mounted).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod driver_autoload;

// The architecture ports. Each subtree gathers exactly one instruction
// set's `KernelArch` wrapper, fail-closed dispatch callback, production
// boot path, PID 1 (`init`) spawn seam, and runtime `spawn` producer (plus
// the x86_64 IO-APIC controller, virtio bring-up, and `DriverHost`
// composition), gated on the matching `kernel_isa` build-script name — the
// single `AGENTS.md` §17.2 selection point lives in `build.rs`, never an
// inline `target_arch` predicate. Code shared across the ports stays at the
// crate root (`dispatch_core`, `mem_map`, `stack_arena`, `spawn_layout`,
// the driver registry, …) rather than being duplicated into a port
// (`AGENTS.md` §2.2). Each port's bare-metal-only modules are further gated
// on `freestanding` inside its root module.
#[cfg(kernel_isa = "x86_64")]
pub mod x86_64;

#[cfg(kernel_isa = "aarch64")]
pub mod aarch64;

#[cfg(kernel_isa = "riscv64")]
pub mod riscv64;

// The data every port's PID 1 spawn seam and runtime spawn producer share
// by definition — the user-space layout constants (stack/MMIO-window offsets
// and page counts, canary seeds), the embedded-program registry the `spawn`
// syscall resolves (paths, capability sets, argument vectors), the
// `CAP_PROC_SPAWN` `SpawnAuthority`, and PID 1 `init`'s own grant + argument
// vector. These describe one user-space contract, not a per-architecture
// register layout, so they are defined once here rather than copy-pasted into
// each `init_spawn` / `spawn_producer` sibling (`AGENTS.md` §2.2). Gated to
// exactly the configurations whose consumers compile, so it is never dead
// code (`AGENTS.md` §2.3).
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
mod spawn_layout;

// The boot memory-map arithmetic (`plans/PI.md` P6c-1, G3b-2): the aarch64
// `/memory` → `BootMemoryMap` window translation and the shared
// guard-arena sizing / carving every port uses to reserve the kthread-stack
// guard arena. The arithmetic is free of the bare-metal-only ports, so it
// compiles — and its bounds-check unit tests run — on the CI host under
// `cargo test` as well as on each production build that consumes it
// (`aarch64::boot`, `x86_64::boot`, `riscv64::boot`). Gated to exactly those
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
// (`aarch64::init_spawn` on aarch64, `x86_64::init_spawn` on x86_64,
// `riscv64::init_spawn` on riscv64). Its bump arithmetic is free of the
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

// Shared host-test fixtures. The in-memory mock root-volume filesystem
// driver `MockRootFs` is the surface several boot-path readers delegate
// through (`system_files`, `driver_autoload`), so it is defined
// once here rather than copy-pasted into each test module (`AGENTS.md`
// §2.2). Compiled only under `cargo test`.
#[cfg(test)]
mod test_support;

pub use bumpalloc::BumpAllocator;

#[cfg(kernel_isa = "aarch64")]
pub use aarch64::arch_wrapper::{Aarch64BinArch, UartConsole, UART_CONSOLE};
#[cfg(kernel_isa = "x86_64")]
pub use x86_64::arch_wrapper::BinArch;
#[cfg(kernel_isa = "x86_64")]
pub use x86_64::dispatch::{production_dispatch, DISPATCH_SLOT};
#[cfg(kernel_isa = "x86_64")]
pub use x86_64::driver_host::{run_with_driver_host, DriverHostConfig};
#[cfg(kernel_isa = "x86_64")]
pub use x86_64::virtio_boot::{provision_and_run, VirtioBootConfig};
// The architecture-neutral virtio factory and provisioning walks now
// live in `rustos-kernel-virtio` so every architecture port can reuse
// them (`AGENTS.md` §2.2); re-exported here to keep this crate's public
// API unchanged.
pub use rustos_kernel_virtio::{
    provision_virtio_mmio, provision_virtio_pci, KernelVirtioFactory, KernelVirtioFactoryConfig,
    VirtioMmioProvision, VirtioMmioWalkError, VirtioPciWalkError, VirtioProvision, MAX_FUNCTIONS,
    MAX_SLOTS,
};

#[cfg(all(freestanding, kernel_isa = "riscv64"))]
pub use riscv64::boot::{
    boot, build_boot_memory_map, try_boot, BootError, RiscvBinArch, RiscvUartConsole,
    RISCV_UART_CONSOLE,
};
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use x86_64::boot::{boot, BootError};
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use x86_64::panic_ctx::handle_panic_via_kernel_core;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use x86_64::serial_sink::{Com1Console, SerialSink, COM1_CONSOLE, SERIAL_SINK};
