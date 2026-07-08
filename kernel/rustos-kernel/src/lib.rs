//! RustOS microkernel binary support library (Stage 3a (c7-bin)).
//!
//! This is the library half of the `rustos-kernel` crate. It carries the
//! per-instruction-set boot pipelines that are reusable across the
//! production binary (`src/main.rs`) and the QEMU integration tests.
//! Pulling a pipeline into a library is the only way to satisfy
//! (no duplication) without leaking the test-only
//! audit-observer sink into the production binary through Cargo feature
//! unification.
//!
//! The build script (`build.rs`) selects the pipeline per instruction set
//! via the `kernel_isa` conditional-compilation name, so the production
//! kernel image is built for exactly one architecture at a time (the
//! single selection point): the x86_64
//! Multiboot2/ACPI pipeline or the aarch64 (Raspberry Pi 4) boot path.
//!
//! # Module map
//!
//! | Module          | Role                                                                              |
//! | --------------- | --------------------------------------------------------------------------------- |
//! | [`kalloc`]      | Freeing (coalescing free-list) `GlobalAlloc` impl shared by every bin.        |
//! | `dispatch_core` | Arch-neutral syscall-dispatch helpers shared by every port (host-tested).         |
//! | `spawn_layout` | Shared user-space layout constants for every port's spawn seam/producer. |
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
//! (fail closed; the harness never decides what
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
// build. — every `#[allow]` carries a justifying
// comment.
#[allow(unused_extern_crates)]
extern crate alloc;

// Host tests need `std` for `Box::leak` (`TestSink`) and friends. The
// crate itself remains `no_std` for production builds
// (no hacks).
#[cfg(test)]
extern crate std;

// The boot pipeline is selected per instruction set by the build script
// (`build.rs` emits the `kernel_isa` name from `CARGO_CFG_TARGET_ARCH`),
// so the crate body never names `target_arch` inline — that decision
// lives in the build glue (`cargo xtask cfg-check`).
//
// The x86_64 pipeline (the Multiboot2/ACPI boot path, the `BinArch`
// `KernelArch` wrapper over `X86_64Arch`, the IO-APIC controller, the
// virtio bring-up, the fail-closed syscall-dispatch callback) compiles
// whenever the target instruction set is x86_64 — the CI host included,
// so its host unit tests run under `cargo test`.
pub mod kalloc;

// The architecture-neutral syscall-dispatch helpers (frame read, errno
// encoding, slot forwarding) shared by every port's `production_dispatch`
// callback. Un-gated: it names only unconditional
// `kernel/*` + `lib/abi` deps, so it compiles on every target and the CI
// host, where its unit tests run.
pub mod dispatch_core;

// The production root-volume unlock + users-database load composition
// (`plans/PI.md` §3 P11 root-mount increment, Chunk A): turns the on-FAT
// `root.unlock` descriptor, the typed passphrase, and the encrypted root
// block device into the validated `users-v1` database
// `kernel/core::load_users_db_source` serves. `rustos-kernel`
// (`Layer::Tooling`) is the one layer permitted to name both the `rustfs`
// driver and `kernel/core`. It is architecture-neutral
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
// policy the user-space `devmgr` autoloader uses. It is the storage analogue of the keyboard bring-up's bind gate
// and the front half the production root mount (`root_mount`) builds on.
// Resolution only — it never reads or mounts a volume — so it is
// architecture-neutral and host-tested on the CI host; it is gated, like
// `driver_catalog` it depends on, on the two instruction sets where that
// registry compiles (`x86_64` and `aarch64`).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod root_storage;

// The runtime hardware-inventory store (Design D, D1 —
// `.junie/next-pi-prompt.md`): the single source of truth for the
// discovered hardware tree, seeded by the boot
// path, appended to by the floor bus bring-up, and snapshotted by the
// autoload reader. It also backs the `hw_tree_read` / `hw_tree_wait`
// syscalls through `HW_TREE_SOURCE` (the reactive generation counter the
// wait parks on; node removal lands in Design D D4 with its consumer). Architecture-neutral and host-tested on the CI host; gated on the
// three instruction sets whose production boot path installs it through
// `BootInfo::with_hw_tree` so the device manager can observe the discovered
// inventory.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod hwtree_store;

// The kernel block-device sharing layer (Design D, D2a —
// `.junie/next-pi-prompt.md`): wraps the one brought-up bootstrap-floor
// block device behind a `lib/sync` lock so it can back two concurrent
// partition windows — the read-only `/System` driver-store mount and the
// encrypted-root unlock window — over a single disk (SMP
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
// `HwResource` its matched hardware-tree node requested (drivers in user space; — only the resources the matched node
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
// consolidates the store walk and the per-bundle reads behind one seam — the seam the D2b-2 `IPC_RECV` endpoint will wrap. The
// bin crate is the one layer that may name `drvhost`, so
// this delegating service lives here; gated, like the other
// `drvhost`-consuming modules, on the two instruction sets where
// `rustos-drvhost` is a dependency of this crate.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod system_files;

// The on-disk application store handle (`plans/APPS.md` deliverable 8): the
// one `'static` `AppStore` carrying the build's embedded app trust anchor
// (`build.rs`-derived from the dedicated app-signing seed) and the
// readiness latch the `/System` mount install resolves. A boot path with a
// storage floor installs it into the syscall layer so the `spawn` syscall
// verifies and launches `…/<Name>.app/Run` bundles from the mounted volume.
// Architecture-neutral pure data, so it is un-gated and its trust-domain
// unit test runs on the CI host.
pub mod app_store;

// The type-erased mounted-volume driver trait (`KernelFs`) and its
// `Box<dyn KernelFs>` forwarders. Architecture-neutral (it names only
// `rustos_abi` types), so the arch-neutral unlock policy (`root_mount`)
// and the account-administration storage (`user_admin_backing`) build on
// every instruction set.
pub mod kernel_fs;

// The RustFS transform cache (`plans/SMARTRAM.md` SMART3): the production
// implementation of the driver's `ClusterCache` seam, retaining verified,
// decrypted, decompressed cluster plaintext under the `kernel/mem::reclaim`
// classification/budget model and the SMART2 pressure bands. Each mounted
// volume installs one at registration (`system_mount`, the aarch64 unlock
// path). Architecture-neutral (rustfs + kernel/mem seams only), so its unit
// and end-to-end tests run on the CI host.
pub mod transform_cache;

// The boot-time install of the read-only `/System` volume as the userland
// `fs_*` filesystem mount (`PREREQUISITES.md` P-A): the type-erased
// `KernelFs` mount driver, the `LATE_FILESYSTEM` / `FS_SERVICE` statics the
// dispatch hook serves the `fs_*` syscalls through, and `install_system_mount`
// (a second, park-safe `'static` window onto the boot disk's `/System`
// volume, published once the disk is up). The production identity half is
// installed by `root_mount` at the encrypted-root unlock. It names
// `rustos_drv_fs_rustfs` and the kernel-core mount service, and consumes the
// `shared_block` window, so it is gated like `shared_block` on the two
// instruction sets where the boot path compiles; its bounds/forwarding unit
// tests run on the CI host.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod system_mount;

// The root-volume storage the `CAP_USER_ADMIN` account-administration
// engine commits through (`plans/CAPABILITY_USE.md` CU4): the type-erased
// admin window onto the writable encrypted root, its crash-safe database
// persistence, and owned home-directory provisioning. Depends only on the
// rustfs driver and kernel/core seams, so it compiles — and its unit tests
// run — on the CI host as well as every kernel target.
pub mod user_admin_backing;

// The kernel-resident `/System` driver-store IPC *server* (Design D
// D2b-2c): the arch-neutral request→reply translation that drains a
// `rustos_kernel_ipc::CallEndpoint` and serves each
// `rustos_abi::driver_store::StoreRequest` against the `system_files`
// `SystemFileService` — a `Catalogue` op (the signed-store scan exposed as
// opaque `bundle_id` + decoded bind keys) and a `Load` op (the signed
// gate + process spawn, granting the matched node's resources). The
// user-space `devmgr` owns matching *policy*; this server keeps the load
// *mechanism* in the kernel TCB. It names both
// `rustos_devmgr` (`DriverLoader`) and `rustos_drvhost`
// (`scan_store`/`ImageSource`), so it is gated, like `system_files`, on the
// two instruction sets where those crates are dependencies of this crate —
// `x86_64` (the CI host, where its unit tests run) and `aarch64` (the
// Raspberry Pi 4 boot path that drives it).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod driver_store_server;

// The architecture ports. Each subtree gathers exactly one instruction
// set's `KernelArch` wrapper, fail-closed dispatch callback, production
// boot path, PID 1 (`init`) spawn seam, and runtime `spawn` producer (plus
// the x86_64 IO-APIC controller, virtio bring-up, and `DriverHost`
// composition), gated on the matching `kernel_isa` build-script name — the
// single selection point lives in `build.rs`, never an
// inline `target_arch` predicate. Code shared across the ports stays at the
// crate root (`dispatch_core`, `mem_map`, `stack_arena`, `spawn_layout`,
// the driver registry, …) rather than being duplicated into a port. Each port's bare-metal-only modules are further gated
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
// each `init_spawn` / `spawn_producer` sibling. Gated to
// exactly the configurations whose consumers compile, so it is never dead
// code.
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
mod spawn_layout;

// The absolute paths the embedded programs are registered under — pure
// data, free of the rxe-laden registry rows in `spawn_layout` that consume
// it, so it compiles — and its system-app-store spelling drift test runs —
// on the CI host as well as on each row-bearing bare-metal production
// build, and on no other configuration, so it is never dead code. The
// aarch64 production build carries no embedded rows (its `spawn` resolves
// on-disk store bundles), so it has no consumer for the path constants and
// is excluded.
#[cfg(any(
    all(freestanding, any(kernel_isa = "x86_64", kernel_isa = "riscv64")),
    test
))]
mod spawn_paths;

// The manifest-requested capability list of every embedded program (and
// PID 1 `init`) — the session baseline and each service/tool request
// (`plans/CAPABILITY_USE.md` CU2). Pure data, free of the rxe-laden
// registry rows in `spawn_layout` that consume it, so it compiles — and
// its exact-set pinning tests run — on the CI host as well as on each
// bare-metal production build, and on no other configuration, so it is
// never dead code.
#[cfg(any(
    all(
        freestanding,
        any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
    ),
    test
))]
mod program_manifests;

// The discovered-RAM anonymous-heap-window sizing policy
// (`anon_window_pages`): pure arithmetic, free of the bare-metal ports and
// of the rxe-laden layout constants, so it compiles — and its unit tests
// run — on the CI host as well as on each bare-metal production build whose
// spawn seam consumes it, and on no other configuration, so it is never
// dead code. Kept separate from `spawn_layout` precisely so the host test
// build pulls in only this testable arithmetic, not the freestanding-only
// layout constants — which would otherwise be unused on host and trip the
// dead-code lint.
#[cfg(any(
    all(
        freestanding,
        any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
    ),
    test
))]
mod anon_layout;

// The boot memory-map arithmetic (`plans/PI.md` P6c-1, G3b-2): the aarch64
// `/memory` → `BootMemoryMap` window translation and the shared
// guard-arena sizing / carving every port uses to reserve the kthread-stack
// guard arena. The arithmetic is free of the bare-metal-only ports, so it
// compiles — and its bounds-check unit tests run — on the CI host under
// `cargo test` as well as on each production build that consumes it
// (`aarch64::boot`, `x86_64::boot`, `riscv64::boot`). Gated to exactly those
// configurations so it is never dead code; the per-port
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
// on no other configuration, so it is never dead code.
#[cfg(any(
    all(
        freestanding,
        any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
    ),
    test
))]
mod stack_arena;

// The build script's pure target-selection logic, compiled into the
// host test build so its rules are unit tested.
#[cfg(test)]
#[path = "build_support.rs"]
mod build_support;

// Shared host-test fixtures. The in-memory mock root-volume filesystem
// driver `MockRootFs` is the surface several boot-path readers delegate
// through (`system_files`, `driver_store_server`), so it is defined
// once here rather than copy-pasted into each test module. Compiled only under `cargo test`.
#[cfg(test)]
mod test_support;

pub use kalloc::FreeListAllocator;

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
// them; re-exported here to keep this crate's public
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
