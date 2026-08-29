//! TAIRiX microkernel binary support library (Stage 3a (c7-bin)).
//!
//! This is the library half of the `tairix-kernel` crate. It carries the
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
//! the architecture-neutral [`tairix_kernel_core::BootInfo`] hand-off
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
// `kernel/core::load_users_db_source` serves. `tairix-kernel`
// (`Layer::Tooling`) is the one layer permitted to name both the `arxfs`
// driver and `kernel/core`. It is architecture-neutral
// (it consumes only the `lib/abi` `Block` seam and the `arxfs`/`kernel/core`
// APIs), so it is un-gated — it compiles on every target and its host unit
// tests run on the CI host; the aarch64 boot path supplies the discovered
// root block device, the FAT-read descriptor, and the console passphrase
// in the following increment (`plans/PI.md` P11 Chunk B).
pub mod root_mount;

// Pure validation/ordering of a boot path's discovered CPU list into the
// dense-`CpuId` → hardware-affinity map the arch handle and every per-CPU
// storage are sized from. Architecture-neutral by design (device-tree
// `/cpus` and ACPI MADT consumers alike), so it is un-gated and its
// fail-closed rules are host-unit-tested.
pub mod cpu_topology;

// The in-kernel driver registry (`plans/PI.md` P10 5c / PLAN Stage 4.HW
// item 5): the single source of truth pairing each in-kernel driver's
// canonical `BIND_KEYS`, `/System/Drivers/` image path, build-signed
// manifest image, and `register()` entry, and resolving a discovered
// hardware node against them through the shared `lib/devmatch` policy — the
// data-driven replacement for hand-sequenced bring-up. It carries each
// driver's `register()` entry (a `tairix-drvhost` type) and `include!`s the
// signed manifest images `build.rs` bakes, so it is gated on the two
// instruction sets where `tairix-drvhost` is a dependency of this crate —
// `x86_64` (the CI host, where its unit tests run) and `aarch64` (the
// Raspberry Pi 4 boot path that consumes it to gate the live VL805 bring-up
// on a match).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod driver_catalog;

// The in-kernel signed-driver-load gate (`plans/PI.md` P10 5c-ii): admits
// any driver in the `driver_catalog` registry through `drvhost::Host::load`
// (Ed25519 signature against the build's embedded driver-signing key + the
// `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` gates), generic over hardware. Gated on
// the three instruction sets where `tairix-drvhost` is a dependency of this
// crate and the boot path drives autoload — `x86_64` (the CI host, where its
// unit tests run), `aarch64` (the Raspberry Pi 4 boot path), and `riscv64`
// (the QEMU `virt` / SiFive boot path, `plans/NETWORK.md` N4e-riscv64).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
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
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod root_storage;

// The arch-neutral virtio-MMIO hardware-discovery observers: the pure
// walks that probe an enumerated virtio-MMIO bus and emit each populated
// block / input / network slot as a discovered `HwNode`. Kept apart from
// `root_storage`'s root-block catalogue resolution (which links
// `driver_catalog` / `drvhost`) so discovering hardware never drags the
// driver-signing trust anchor in with it — an architecture whose boot path
// builds a hardware tree reuses these observers without linking the
// catalogue. Bus injected through the frozen `lib/abi` seams, so this names
// no concrete `drivers/bus/*` type; host-tested on the CI host. Gated on
// the three instruction sets whose boot path assembles a hardware tree and
// runs the bootstrap-floor virtio-MMIO probe (x86_64, aarch64, and the
// riscv64 `virt`-board discovery, `plans/NETWORK.md` N4e-riscv64).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod hwdiscovery;

// The reserved synthetic hardware-tree node-id address space: one shared,
// disjoint-by-construction home for the high node-id bases the bootstrap-floor
// virtio-MMIO probes (`hwdiscovery`) and the boot-display shim
// (`boot_display`) mint their nodes from, plus the compile-time guard that a
// probe walk never overruns its region. Pure `lib/abi`/`kernel/virtio`
// constants, so it is host-tested on the CI host and gated, like its
// `hwdiscovery` consumer, on the three instruction sets whose boot path
// assembles the tree and runs the virtio-MMIO probe.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod hwtree_node_ids;

// The boot-display publication step (`plans/DISPLAY.md` D7d): turns the
// architecture port's discovered framebuffer-boot-console scan-out facts
// into a display-class hardware-tree node carrying the geometry-carrying
// `Framebuffer` grant request and the canonical `simple-framebuffer`
// match key, so the user-space display service autoloads against the boot
// display exactly like any other discovered device. Architecture-neutral
// (plain discovered values in, `lib/abi` node out) and host-tested on the
// CI host; gated, like `root_storage` whose buffered tree it feeds, on
// the two instruction sets where the boot tree assembly compiles.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64"))]
pub mod boot_display;

// The runtime hardware-inventory store (Design D, D1 —
// `plans/PI.md`): the single source of truth for the
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

// The arch-neutral boot-time hardware-tree collection sink: the growable
// `HwNodeSink` every architecture whose boot path builds a hardware tree
// collects discovered `HwNode`s into before publishing them to `HW_TREE`,
// defined once so the trivial collect-into-`Vec` logic cannot diverge
// between the aarch64 and riscv64 boot paths. Pure `alloc`/`lib/abi` glue
// over the frozen `PlatformDiscovery` seam. Consumed by the aarch64 and
// riscv64 boot paths that seed the tree; gated, like `hwtree_store` it
// feeds, on the three instruction sets whose kernel compiles, so it is
// host-tested on the CI host (x86_64).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod boot_hwtree;

// The synthetic virtual bus (`plans/FIX-IO.md` `IO6d`): the one always-present
// hardware-tree node beneath the root that a *composed* block device — a RAID
// array built out of its member disks — hangs from, because it can hang
// neither from a member (pulling that disk would orphan an array the
// survivors still serve) nor from the root (no driver is matched to the
// root). Published at the one arch-neutral seam where the discovered boot
// tree becomes the live inventory, so every port gets it and none can forget
// it. Pure `lib/abi` node construction, host-tested on the CI host; gated,
// like the `hwtree_store` it is published into, on the three instruction sets
// whose boot path installs that store.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod virtual_bus;

// The kernel block-device sharing layer (Design D, D2a —
// `plans/PI.md`): wraps the one brought-up bootstrap-floor
// block device behind a `lib/sync` lock so it can back two concurrent
// partition windows — the read-only `/System` driver-store mount and the
// encrypted-root unlock window — over a single disk (SMP
// serialisation). Architecture-neutral and host-tested on the CI host;
// gated, like the `unlock_service` boot path that wraps the device in it,
// on the two instruction sets where the boot path compiles.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
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
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod unlock_service;

// The user-space sibling of `driver_loader` (`plans/PI.md` P10 5d-2-ii):
// admits a discovered `kind = UserSpace` driver through the same signed
// `drvhost::Host::load` gate, then **spawns** it into its own
// hardware-isolated process, minting it one device-resource grant per
// `HwResource` its matched hardware-tree node requested (drivers in user space; — only the resources the matched node
// requested). It implements `tairix_devmgr::DriverLoader`, so the device
// manager's autoload walk drives it directly; the architecture-specific
// process creation sits behind the `DriverProcessSpawn` seam, so the gate +
// resource-threading logic is host-tested on the CI host. Gated, like
// `driver_loader`, on the two instruction sets where `tairix-drvhost` /
// `tairix-devmgr` are dependencies of this crate.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod driver_spawn_loader;

// The read-only `/System` file service (`plans/PI.md` Design D
// D2b-1): one object over the mounted `/System` volume that both lists the
// signed `/System/Drivers/` store and reads a bundle's bytes (a
// `drvhost::ImageSource`) through the kernel-core `DriverImageReader`. It
// consolidates the store walk and the per-bundle reads behind one seam — the seam the D2b-2 `IPC_RECV` endpoint will wrap. The
// bin crate is the one layer that may name `drvhost`, so
// this delegating service lives here; gated, like the other
// `drvhost`-consuming modules, on the two instruction sets where
// `tairix-drvhost` is a dependency of this crate.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
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
// `tairix_abi` types), so the arch-neutral unlock policy (`root_mount`)
// and the account-administration storage (`user_admin_backing`) build on
// every instruction set.
pub mod kernel_fs;

// The ARXFS transform cache (`plans/SMARTRAM.md` SMART3): the production
// implementation of the driver's `ClusterCache` seam, retaining verified,
// decrypted, decompressed cluster plaintext under the `kernel/mem::reclaim`
// classification/budget model and the SMART2 pressure bands. Each mounted
// volume installs one at registration (`system_mount`, the aarch64 unlock
// path). Architecture-neutral (arxfs + kernel/mem seams only), so its unit
// and end-to-end tests run on the CI host.
pub mod transform_cache;

// The whole-disk block-level LRU cache (`plans/SMARTRAM.md` SMART11):
// wraps the one brought-up boot device *below* the block-sharing layer,
// so every window onto the disk reads through one coherent cache of
// recently used device blocks under the `kernel/mem::reclaim`
// classification/budget model and the SMART2 pressure bands (class
// `CleanFileData` — reclaimed from mild pressure, before any `ramzip`
// handoff). Architecture-neutral (block-ABI + kernel/mem seams only),
// so its unit tests run on the CI host.
pub mod block_cache;

// The boot-time install of the read-only `/System` volume as the userland
// `fs_*` filesystem mount (`PREREQUISITES.md` P-A): the type-erased
// `KernelFs` mount driver, the `LATE_FILESYSTEM` / `FS_SERVICE` statics the
// dispatch hook serves the `fs_*` syscalls through, and `install_system_mount`
// (a second, park-safe `'static` window onto the boot disk's `/System`
// volume, published once the disk is up). The production identity half is
// installed by `root_mount` at the encrypted-root unlock. It names
// `tairix_drv_fs_arxfs` and the kernel-core mount service, and consumes the
// `shared_block` window, so it is gated like `shared_block` on the two
// instruction sets where the boot path compiles; its bounds/forwarding unit
// tests run on the CI host.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod system_mount;

// The root-volume storage the `CAP_USER_ADMIN` account-administration
// engine commits through (`plans/CAPABILITY_USE.md` CU4): the type-erased
// admin window onto the writable encrypted root, its crash-safe database
// persistence, and owned home-directory provisioning. Depends only on the
// arxfs driver and kernel/core seams, so it compiles — and its unit tests
// run — on the CI host as well as every kernel target.
pub mod user_admin_backing;

// Runtime volume attach/detach service behind the `volume_attach` /
// `volume_detach` syscalls (`plans/DEVICES.md` D3b). It builds on
// `system_mount`'s mount cell and names the filesystem driver crates, so
// it is gated like `system_mount` on the ports with a storage floor; its
// host tests (the full attach/read/detach lifecycle) run on the CI host.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod volume_service;

// The removable-volume mount policy (`plans/DEVICES.md` D3d): the
// storage-group identity map an ownerless filesystem (FAT32) is mounted
// under, and the set-once gid cell the root unlock resolves the
// well-known `storage` group into. Architecture-neutral (abi/sec/sync
// seams only) because the unlock path that installs the gid compiles on
// every port even where the attach path does not; its unit tests run on
// the CI host.
pub mod volume_policy;

// The kernel-resident `/System` driver-store IPC *server* (Design D
// D2b-2c): the arch-neutral request→reply translation that drains a
// `tairix_kernel_ipc::CallEndpoint` and serves each
// `tairix_abi::driver_store::StoreRequest` against the `system_files`
// `SystemFileService` — a `Catalogue` op (the signed-store scan exposed as
// opaque `bundle_id` + decoded bind keys) and a `Load` op (the signed
// gate + process spawn, granting the matched node's resources). The
// user-space `devmgr` owns matching *policy*; this server keeps the load
// *mechanism* in the kernel TCB. It names both
// `tairix_devmgr` (`DriverLoader`) and `tairix_drvhost`
// (`scan_store`/`ImageSource`), so it is gated, like `system_files`, on the
// two instruction sets where those crates are dependencies of this crate —
// `x86_64` (the CI host, where its unit tests run) and `aarch64` (the
// Raspberry Pi 4 boot path that drives it).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod driver_store_server;

// The architecture-neutral root-unlock / driver-store orchestration
// (`plans/PI.md` design B): the two-task tail every port's bootstrap-floor
// block bring-up feeds — the spawned interactive encrypted-root unlock and
// the persistent capability-gated driver-store serve loop the user-space
// `devmgr` autoloads through. A port injects only its console-0 seam
// (`UnlockConsole`) and its `ProcessSpawn` producer, so the tail is never
// copied into a `kernel/arch/<target>/` sibling. Gated, like its
// driver-store dependencies, on the three instruction sets that drive it —
// `x86_64` (the CI host, where its dependencies' unit tests run), `aarch64`
// (the Raspberry Pi 4 boot path), and `riscv64` (the QEMU `virt` / SiFive
// boot path, `plans/NETWORK.md` N4e-riscv64).
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod unlock_orchestrate;

// The write-back flusher kthread: the one task above the filesystem drivers
// that publishes a volume whose batched transaction has aged out
// (`plans/ARXFS-WRITEBACK.md` §10). Admitted from `unlock_orchestrate`'s
// shared tail, so it is gated on the same three instruction sets that reach
// it — a port with no storage floor registers no volume to publish.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod writeback_service;

// The binding kernel's `SupervisorHost` — the consumer wiring the arch-neutral
// pre-boot Supervisor engine (`lib/supervisor`) to the real bootstrap-floor
// state (`plans/NEW-SUPERVISOR.md`): each command reaches its one existing
// source of truth (the published `SupervisorSystem`, the boot audit-log ring,
// the hardware tree, the shared boot disk, the real unlock path). Built in
// `unlock_orchestrate`'s unlock kthread body and consumed by the ESC
// boot-screen window in `root_mount`, so it is gated on exactly the three
// instruction sets that drive that path, like its dependencies.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", kernel_isa = "riscv64"))]
pub mod supervisor_host;

// The one definition of an interrupt-driven UART console's receive drain:
// the lossless, backpressured loop that moves bytes from a hardware receive
// FIFO into a `kernel/core` `ConsoleInputQueue`, shared verbatim by the
// aarch64 PL011 and x86_64 16550 console paths so the subtle flow-control /
// clear-then-recheck logic lives in exactly one place. It is pure and
// arch-neutral — every hardware touch (the FIFO read, the receive-latch
// clear, the flow-control brake) is an injected closure — so it host-tests
// against a fake FIFO and compiles on the two ports whose console is a
// discovered UART. riscv64 has no interrupt-driven UART receive (its boot
// console is the SBI console), so it is excluded.
#[cfg(any(kernel_isa = "x86_64", kernel_isa = "aarch64", test))]
pub mod console_uart;

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

// The riscv64 PLIC `IrqController` bridge (`plans/NETWORK.md` N4e-riscv64): the
// smallest local newtype implementing `kernel/irq`'s `IrqController` over the
// arch port's `plic::PlicController` (orphan rules keep it out of the arch
// port), mirroring how the x86_64 `IoApicController` lives in this crate. It is
// generic over `PlicMmio`, so it is host-buildable: it lives at the crate root
// — gated on the riscv64 image build *or* a host `cargo test` — rather than
// inside the freestanding-only `riscv64` port module, so its mask-before-wake /
// re-arm regression test runs under `cargo test` on the CI host. The
// `virt`-board QEMU verticals re-export it from here (one definition).
#[cfg(any(kernel_isa = "riscv64", test))]
pub mod riscv64_plic_irq;

// The data every port's PID 1 spawn seam and runtime spawn producer share
// by definition — the user-space layout constants (stack/MMIO-window offsets
// and page counts, canary seeds), the embedded-program registry the `spawn`
// syscall resolves (paths, capability sets, argument vectors), the
// `CAP_PROC_SPAWN` `SpawnAuthority`, and PID 1 `init`'s own grant + argument
// vector. These describe one user-space contract, not a per-architecture
// register layout, so they are defined once here rather than copy-pasted into
// each `init_spawn` / `spawn_producer` sibling. Public because the QEMU
// stack-growth verticals derive their role parameters from the one stack
// policy defined here rather than carrying a copy. Gated to
// exactly the configurations whose consumers compile, so it is never dead
// code.
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
pub mod spawn_layout;

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

// The discovered-RAM dynamic-window sizing policy (`user_windows`): the
// anonymous-heap / file-mapping split, pure arithmetic, free of the
// bare-metal ports and of the rxe-laden layout constants, so it compiles —
// and its unit tests run — on the CI host as well as on each bare-metal
// production build whose spawn seam consumes it, and on no other
// configuration, so it is never dead code. Kept separate from
// `spawn_layout` precisely so the host test build pulls in only this
// testable arithmetic, not the freestanding-only layout constants — which
// would otherwise be unused on host and trip the dead-code lint.
#[cfg(any(
    all(
        freestanding,
        any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
    ),
    test
))]
mod user_windows;

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

// Shared host-test fake virtio-MMIO bus (`FakeBus`): the enumeration stand-in
// both the `hwdiscovery` observer tests and the `root_storage` root-block
// resolution tests drive, defined once here rather than copy-pasted into each
// test module. Compiled only under `cargo test`.
#[cfg(test)]
mod discovery_test_bus;

pub use kalloc::FreeListAllocator;

/// Publish this binary's `#[global_allocator]` with the kernel core so the
/// boot path can wire the frame-backed growth source into it (the growable
/// kernel heap). Each arch bin's `kernel_main` calls this once with its
/// `&'static FreeListAllocator` before entering `boot`.
pub use tairix_kernel_core::kheap::register_global_heap;

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
// live in `tairix-kernel-virtio` so every architecture port can reuse
// them; re-exported here to keep this crate's public
// API unchanged.
pub use tairix_kernel_virtio::{
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
pub use tairix_arch_x86_64::serial::{SerialSink, SERIAL_SINK};
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use x86_64::boot::{boot, BootError};
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use x86_64::panic_ctx::handle_panic_via_kernel_core;
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
pub use x86_64::serial_sink::{Com1Console, COM1_CONSOLE};
