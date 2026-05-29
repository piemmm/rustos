# Next session — Stage 4.D Items 4 / 6
# (carried over after the hardware-real direct-map DMA/MMIO data path
#  landed, on top of the ring-0 virtio-PCI provisioning walk +
#  `VirtioPciBus` ABI seam, the virtio-1.x PCI capability decode +
#  register-window hand-off, and the modern virtio-PCI `PciTransport`)

## Where we are

`PLAN.md` Stage 4.D records the following as **complete**:

- **PCI MSI-X interrupt routing** (latest session). Scoping
  `virtio_blk_pci_x86_64` surfaced a hard blocker: a real
  virtio-blk-pci round-trip cannot complete without delivering the
  device's interrupt (`VirtioBlk::run_request` parks on
  `host.notify_wait()` → `block_until_ready` on a *pre-bound*
  `IrqHandle`), yet the tree had **no** PCI interrupt-routing path
  (MSI/MSI-X were *discovered* but never enabled; INTx would need an
  ACPI `_PRT`/AML interpreter that does not exist). Landed the modern,
  host-testable half: a frozen `abi-v1` seam
  `lib/abi/src/driver/msix.rs` (`MsiMessage { address, data }` +
  `MsixBus: Bus` with `route_msix(bdf, entry, message, mapper)`);
  `Pci::route_msix` (find MSI-X cap → bounds-check entry → map the
  16-byte table entry through the `CAP_MMIO_MAP`-gated `MmioMapper` →
  write addr/data + unmask vector control → set MSI-X Enable + clear
  function mask; fails closed NotFound/OutOfRange/Unsupported/
  PermissionDenied) + `MsixBus for Pci<C>`; and
  `rustos_arch_x86_64::irq::msi_message(vector, destination)` building
  the x86 LAPIC message (physical/fixed/edge, SDM §11.11). Host-tested
  only: `rustos-abi` 84 (+3), `rustos-drv-bus-pci` 26 (+5),
  `rustos-arch-x86_64` 133 (+2); clippy/fmt/doc clean; abi+pci build
  for `x86_64-unknown-none`. Legacy MSI and INTx routing remain
  unimplemented.

- **MSI-X routing wired into boot provisioning** (latest session).
  `provision_and_run` now actually *calls* `route_msix`.
  `provision_virtio_pci` returns `VirtioProvision { transport, bdf }`;
  `VirtioBootConfig` gained `msix: &dyn MsixBus`, `msix_entry`, and the
  arch-built `msi_message`, and `provision_and_run` routes MSI-X through
  the same `KernelMmioMapper` after the four register windows are mapped
  and before the driver host is built, failing closed with the new
  `VirtioPciWalkError::RouteMsix`. The arch caller still owns binding
  the line in `kernel/irq::IrqTable` and encoding the `MsiMessage`
  (`virtio_boot` stays arch-neutral). Host-tested only: `rustos-kernel
  --lib` 45 (+1); clippy/fmt/doc clean; `x86_64-unknown-none` lib build
  clean. **Still TODO (the remaining IRQ-binding the QEMU test needs):**
  in `boot.rs`, allocate a free external vector + bind it in
  `IrqTable`, build the `MsiMessage` via `msi_message` from that
  vector, and pass the `IrqHandle` + `msi_message` + `msix_entry` into
  `provision_and_run` from the live boot pipeline.

- **Live virtio-PCI boot wiring** (earlier session). The ring-0 walk
  (`provision_virtio_pci`) and the per-driver DMA factory
  (`KernelVirtioFactory`) existed but were reachable only from unit
  tests; nothing joined them to a live `drvhost::Host`. New module
  `kernel/rustos-kernel/src/virtio_boot.rs`: `VirtioBootConfig` (the
  borrowed boot resources) + `provision_and_run(config, make_table,
  body)` — it builds a `KernelMmioMapper`, provisions the
  `PciTransport`, constructs a `KernelVirtioFactory`, and hands a live
  `drvhost::Host` (factory wired into
  `HostConfig::virtio_host_factory`) plus the transport to a `body`
  closure. The scope/callback keeps the mapper/factory/host + every
  minted per-driver `DmaPool` on one boot frame (reclaimed on return;
  `AGENTS.md` §4); fails closed with `VirtioPciWalkError` without
  building the host. Host-tested only: `rustos-kernel --lib` 44 (+2 —
  a happy path that provisions the four register windows over a
  `SimPhysMap`-backed `MmioMap`, loads a signed `.rxe` whose `register`
  allocates a zeroed DMA slab through the minted `VirtioHost`, asserts
  `mmio.live() == 4`; and a missing-device `NoVirtioFunction` path).
  `rustos-crypto` is now a dep + `ed25519-dalek` a dev-dep (test
  signing). Clippy/fmt/doc + `x86_64-unknown-none` lib+bin clean.
  **What this unblocks:** the `tests/integration/virtio_blk_pci_x86_64`
  kernel test bin now only has to *call* `provision_and_run` from the
  boot pipeline against the live `Pci` + real `KernelMmioMapper` +
  `DirectPhysMap`, and drive the loaded `virtio-blk` `.rxe` inside the
  `body` closure. **Still TODO:** construct a `DirectPhysMap` in the
  test bin (e.g. `DirectPhysMap::identity(4 GiB)` matching the boot
  identity map) and a real `MmioMap`/`FrameAllocator` from `BootInfo`,
  bind the device IRQ line, and feed it all into `provision_and_run`.

- **Hardware-real direct-map DMA/MMIO data path** (earlier session).
  Before this, the kernel DMA/MMIO primitives could not drive a *real*
  device: `kernel/mem::DmaPool` served the driver's CPU-visible bytes
  from a heap `Vec<u8>` decoupled from the physical frames it handed
  the device, and both `DmaPool` and `MmioMap` mapped into a
  freshly-minted `AddressSpace` never loaded into CR3. New
  `kernel/mem::phys` module: `PhysMap` trait + production
  `DirectPhysMap` (identity/offset over the boot low-memory direct
  map — the x86_64 trampoline identity-maps 0..4 GiB) + test-only
  `SimPhysMap`. `DmaPool::new` / `MmioMap::new` now take `&dyn
  PhysMap`; `DmaPool::{bytes,bytes_mut,slot_base}` + zero-on-
  alloc/free and `MmioMap::region_base` resolve the device `phys`
  through it, so CPU view == device frame. The `0xCC` guard-byte
  *simulation* is gone (unmapped guard pages remain the real
  mechanism); new `DmaError::DirectMap` / `MmioError::DirectMap` fail
  closed. Threaded through `kernel/sec::{dma,mmio}`,
  `KernelMmioMapper` (`&'a mut MmioMap<'p,P>`), and
  `KernelVirtioFactoryConfig` (new `phys` field). All host tests +
  `x86_64-unknown-none` build + clippy/fmt/doc clean. **What this
  unblocks:** a real virtio device can now round-trip data; the
  remaining Item 4 work is purely *wiring* (construct a
  `DirectPhysMap` in `boot.rs` — e.g. `DirectPhysMap::identity(4
  GiB)` matching the boot identity map — and thread it into the live
  `KernelMmioMapper` + `KernelVirtioFactoryConfig.phys` used by the
  `virtio_blk_pci_x86_64` test).

- **virtio-blk backing storage in the QEMU runner** (earlier session —
  the `virtio_blk_pci_x86_64` prerequisite). `tools/qemu` could only
  build a GRUB ISO + attach OVMF; it had no way to give the guest a
  block device or to know its contents before boot. New module
  `tools/qemu/src/disk.rs`: `plant_raw_disk(path, size_sectors,
  sectors)` (+ `SECTOR_BYTES = 512`) lays down a zero-filled raw image
  and stamps `(lba, bytes)` at `lba * SECTOR_BYTES` (raw, not qcow2, so
  the host can re-read a guest-written block by byte offset), failing
  closed on a zero-sector image / out-of-range `lba` / over-long slice.
  `Spec` gained a `BlockDevice` field + `with_virtio_blk(image)`
  builder; the x86_64 backend emits `-drive if=none,format=raw,id=blkN`
  + `-device virtio-blk-pci,drive=blkN` per device, and `Runner::run`
  fails closed (`NotFound`) on a missing image before spawning QEMU.
  `rustos-qemu` 27 host tests (+10); clippy `-D warnings` / fmt / doc
  clean; `docs/src/platform/x86_64.md` updated. **The kernel-side
  `tests/integration/virtio_blk_pci_x86_64` crate that consumes this is
  still TODO** (boot + driver host + signed `.rxe` + live
  `KernelVirtioFactory`/`PciTransport`).

- **Ring-0 virtio-PCI provisioning walk + `VirtioPciBus` ABI seam**
  (latest session). The per-`cfg_type` window hand-offs were
  `pub(crate)` on the concrete `Pci` type, so ring 0 (whose only
  sanctioned PCI surface is `register`, §8) had no way to call them.
  New frozen `abi-v1` seam `lib/abi/src/driver/virtio_pci.rs`:
  `VirtioPciBus: Bus` (`map_virtio_window` + `notify_off_multiplier`)
  + `VIRTIO_PCI_CFG_*` / `VIRTIO_PCI_VENDOR_ID` consts. `Pci<C>`
  implements it (forwarding to its inherent methods); the PCI
  `VIRTIO_CFG_*` consts now bind to the abi source of truth (§2.2).
  New kernel module `kernel/rustos-kernel/src/virtio_pci_walk.rs`:
  `provision_virtio_pci(bus: &dyn VirtioPciBus, device_id, mapper)`
  enumerates into a bounded stack table (`MAX_FUNCTIONS = 64`, fails
  closed on overflow), finds the virtio function, maps the four
  windows through the `CAP_MMIO_MAP`-gated mapper, and builds a
  `PciTransport` — driver-agnostic ring 0, no ambient authority, no
  panics (`VirtioPciWalkError`). Host-tested only (mock
  `VirtioPciBus` + `MmioMapper`); **not yet wired into a live
  `drvhost::Host`** and **not yet run against a real QEMU device**.
  `rustos-abi` 81 (+4), `rustos-kernel --lib` 42 (+5); full `cargo
  test --workspace`, clippy/fmt/doc, and the `x86_64-unknown-none`
  kernel build all clean.

- Item 1 — kernel per-process-heap `DmaPool` + `kernel/sec::dma`
  gate (`CAP_MEM_DMA`, audit 1030/1031).
- Item 2-tail.2 (+ QEMU validation) — live IRQ end-to-end on
  x86_64 QEMU.
- Item 2-tail.3 — `KernelVirtioHost::notify_wait` blocks on a
  pre-bound `IrqHandle` through `kernel/irq::block_until_ready`.
- Item 2-tail.4 — kernel-binary `VirtioHostFactory`
  (`kernel/rustos-kernel/src/virtio_factory.rs`:
  `KernelVirtioFactory<'k, P, F>` + `KernelVirtioFactoryConfig<'k>`,
  implementing `rustos_drvhost::VirtioHostFactory`; `mint` fails
  closed without `CAP_MEM_DMA`, else mints a fresh `AddressSpace` via
  a `make_table: Fn() -> P` closure + `DmaPool` + per-driver
  `KernelVirtioHost`). `KernelVirtioHost::new` takes its `DmaPool`
  **by value** (`RefCell<DmaPool<'a, P>>`), which is what makes a
  `&self` factory sound.
- Item 3 — capability-checked register-window hand-off
  (`RegisterWindow` / `MmioMapper` ABI seam, `kernel/mem::MmioMap`,
  `kernel/sec::map_mmio`, `Pci::map_bar_window` /
  `Mmio::map_slot_window`, `KernelMmioMapper`).
- Item 5 — userland ARP / IP / ICMP responder
  (`userland/net/icmp`, `rustos-net-icmp`).
- **virtio-1.x PCI capability decode + register-window hand-off**
  (latest session — the boot-PCI-walk prerequisite). The PCI
  capability walker decoded MSI / MSI-X but not the vendor-specific
  virtio capability (`cap_id = 0x09`), so a boot-time walk had no way
  to turn a device's virtio-1.x capabilities into `(BAR, offset,
  length)` triples. `drivers/bus/pci` now decodes them into
  `Capability::Virtio` / `Capability::VirtioNotify` (+ `VIRTIO_CFG_*`
  / `CAP_ID_VENDOR` consts) and exposes
  `Pci::map_virtio_window(bdf, cfg_type, mapper)` (resolves a config
  structure to `bar.base + offset`, bounds-checks `offset + length`
  against the BAR size, maps exactly `length` bytes through the
  `CAP_MMIO_MAP`-gated `MmioMapper`) plus
  `Pci::virtio_notify_off_multiplier(bdf)`. The four windows + the
  multiplier are exactly what `PciTransport::new` consumes; the
  ring-0 boot walk now only has to *call* these. `map_bar_window`
  was refactored onto a shared `resolve_bar` helper (no
  duplication). 26 `rustos-drv-bus-pci` tests (+5 new against a
  `virtio-blk-pci` `1AF4:1042` fixture); clippy / fmt / doc /
  `x86_64-unknown-none` build clean; PCI README +
  `docs/src/drivers/bus.md` updated.
- **Modern virtio-MMIO `MmioTransport`** (latest session — the
  riscv64 / `AArch64` transport prerequisite). `PciTransport` covered
  the `x86_64` bus, but the `-M virt` / device-tree MMIO path had no
  concrete `Transport`. New module
  `drivers/bus/virtio/src/transport_mmio.rs`: `MmioTransport` over the
  single kernel-mapped `RegisterWindow` a bus driver resolves from the
  boot DTB and maps through the `CAP_MMIO_MAP`-gated MMIO-map facility
  (virtio 1.1 §4.2.2 register layout). Drives the §3.1 init sequence,
  64-bit feature negotiation, per-queue `Low`/`High` address
  programming + `QueueReady`, and single-register `QueueNotify`
  notification — no pointer arithmetic, no ambient authority
  (`AGENTS.md` §4). Fallible `new` validates magic/version/device-id
  and a full-length window so the infallible `Transport` methods touch
  only in-bounds constant offsets and never panic (§2.9). MMIO-only:
  no num-queues register (`num_queues` = 16-bit max, probe via
  `QueueNumMax`) and no notify offset/multiplier. Exported from
  `lib.rs`; 12 new unit tests against a `RegisterWindow`-backed
  `FakeMmioDevice`; `docs/src/drivers/virtio.md` + PLAN.md Stage 4.D
  updated. `rustos-drv-bus-virtio` host tests now **73** (default and
  `--features kernel-host`).
- **Modern virtio-1.x PCI `PciTransport`** (earlier session — the
  Item 4 transport prerequisite). Investigation found the only `Transport`
  impl in-tree was the in-process `MockTransport`; `PciBackend` /
  `MmioBackend` were thin `RegisterWindow` wrappers that decoded no
  virtio capability layout, so a kernel-mapped BAR could not
  actually drive a device. New module
  `drivers/bus/virtio/src/transport_pci.rs`: `PciTransport` +
  `PciTransportWindows` — a concrete `Transport` over four
  capability-checked `RegisterWindow`s (common-cfg / notify / ISR /
  device-cfg) plus `notify_off_multiplier`. It drives the virtio
  §3.1 init sequence, 64-bit feature negotiation (u32 halves),
  per-queue programming/enable, and `queue_notify_off *
  multiplier` notification — no pointer arithmetic, no ambient
  authority (`AGENTS.md` §4). Fallible `new` validates the
  common-cfg window ≥ `0x38` and reads `num_queues`, so infallible
  `Transport` methods touch only in-bounds constant offsets and
  never panic (§2.9); the device-supplied notify offset is
  bounds-checked on the fallible `queue_set` path and `notify`
  fails closed for unprogrammed queues. Exported from `lib.rs`; 11
  new unit tests against a `RegisterWindow`-backed `FakeDevice`;
  `docs/src/drivers/virtio.md` + PLAN.md Stage 4.D updated.

Baseline host tests after this session (all green): `rustos-abi`
77, `rustos-kernel-mem` 101, `rustos-kernel-sec` 52,
`rustos-drv-bus-virtio` **73** (default and `--features
kernel-host`), `rustos-kernel --lib` 37, `rustos-drvhost` 19 lib;
no regressions elsewhere (`rustos-drv-storage-virtio-blk`,
`rustos-drv-network-virtio-net` green). Clippy (`-D warnings`,
both feature sets), `cargo fmt --check`, and `RUSTDOCFLAGS="-D
warnings" cargo doc --no-deps` are clean on every touched crate.
The `rustos-drv-bus-virtio` crate builds for `x86_64-unknown-none`
(default and `--features kernel-host`). Toolchain is pinned
`nightly-2026-05-27`; QEMU (`qemu-system-x86_64` /
`qemu-system-riscv64`) is available on the Linux host.

`cargo xtask test`/`ci` were **not** run this session — the change
is host-testable and additive — and the mdBook half of
`docs-check` cannot run because `mdbook` is not installed in this
environment (the rustdoc half passed). The acceptance gate (Item
6) must run the full `xtask` matrix.

## Assumptions to confirm at the top of the PR body

1. `abi-v1` stays frozen. `PciTransport` is a new concrete
   implementation of the existing `Transport` trait; no trait
   signature changed.
2. `KernelVirtioFactory` lives in the kernel binary (not `drvhost`)
   so `userland/system/drvhost` keeps zero `kernel/*` dependencies.
   The factory's `make_table` closure must return a **fresh, empty**
   page table per call (per-process isolation, `AGENTS.md` §4).
3. The production kernel binary does not yet construct a live
   `drvhost::Host` (there is no filesystem / `.rxe` load path in
   ring 0 yet). The factory and `PciTransport` are therefore
   exercised by unit tests and will be threaded into a live `Host`
   by the Item 4 QEMU crates, which is where boot + driver-host +
   signed `.rxe` wiring exists.
4. `PciTransport` is host-verified only (register-window-backed
   `FakeDevice`); it has **not** yet driven a real QEMU device.
   First on-hardware bring-up happens in the Item 4 vertical below,
   so budget for first-try MMIO/DMA debugging there.

## What needs doing

### Item 4 — QEMU integration tests

The `PciTransport` (this session) supplies the missing
real-hardware transport. What still does **not** exist:

- The **boot-time PCI walk** now exists as
  `rustos_kernel::provision_virtio_pci` (`virtio_pci_walk.rs`),
  reaching the PCI driver through the `VirtioPciBus` ABI seam. The
  remaining work is to **wire it into the live boot pipeline**:
  construct a `Pci` (via the PCI driver's `register` path / driver
  host), pass it as `&dyn VirtioPciBus` plus the real
  `KernelMmioMapper` into `provision_virtio_pci`, and feed the
  resulting `PciTransport` + a `KernelVirtioFactory`-minted
  `KernelVirtioHost` into the `drvhost::Host` that runs the signed
  `virtio-blk` `.rxe`. That wiring is what `tests/integration/
  virtio_blk_pci_x86_64` exercises.
- The **MMIO `Transport`** for `riscv64 -M virt` / `AArch64` now
  exists (`drivers/bus/virtio::MmioTransport`, latest session). The
  remaining MMIO work is the ring-0 DTB walk that resolves the
  `virtio-mmio` slot, maps its register block via
  `Mmio::map_slot_window` → `KernelMmioMapper`, and feeds the window
  into `MmioTransport::new`.
- riscv64 support in the **QEMU runner** (`tools/qemu` is x86_64-only
  today: single `Arch::X86_64`, GRUB-ISO boot, `isa-debug-exit`).

These need full boot wiring: kernel + driver host + signed `.rxe`,
plus real device bring-up (walk PCI/DTB → `map_bar_window` /
`map_slot_window` → `KernelMmioMapper` → `PciTransport` (PCI) /
the new MMIO transport, a per-device `DmaPool` via
`KernelVirtioFactory`, and the IRQ line bound alongside the
register window). Model them on `tests/integration/drvhost_qemu`
(boot to `AuditEvent::BootCompleted`, then drive the host) and
`tests/integration/irq_qemu_x86_64`.

Recommended order: land `tests/integration/virtio_blk_pci_x86_64`
first — it is the one fully on the existing x86_64 runner and
proves the `PciTransport` against a real device — then build the
riscv64 runner + MMIO transport, then the net tests.

- `tests/integration/virtio_blk_pci_x86_64` — the runner half now
  exists: `Spec::with_virtio_blk(image)` attaches a `virtio-blk-pci`
  function and `rustos_qemu::disk::plant_raw_disk` plants a **raw**
  backing image (sector 0 = known pattern) before boot. Remaining: the
  kernel-side test bin — boot to `BootCompleted`, run
  `provision_virtio_pci` against the live `Pci` + `KernelMmioMapper`,
  mint a `KernelVirtioHost` via `KernelVirtioFactory`, load the signed
  `virtio-blk` `.rxe` through `drvhost::Host`, read sector 0, write a
  known pattern to sector 1, read it back, verify checksum, then exit
  via `qemu_exit`. Enrol it in `tools/xtask/src/commands/qemu_tests.rs`
  and have its runner spec call `with_virtio_blk` + `plant_raw_disk`.
  **Also satisfies Item 3's deferred "walk PCI and hand a working
  window to the virtio transport" check.**
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`, exercising
  `Mmio::map_slot_window`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP echo
  round-trip against `qemu user net`, driving `rustos-net-icmp`
  (Item 5) over the live device.
- Add an unload → reload → reuse test for each driver.

Wiring the kernel-binary side: thread `KernelVirtioFactory` into the
test bin's `HostConfig::virtio_host_factory` (the factory needs the
device's bound `IrqHandle` + a `FrameAllocator` + the task's
`TaskCapabilities` + an `IrqWaiter` — all already available in the
boot pipeline). `make_table` returns the arch page table.

### Item 6 — Acceptance gate

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` (incl. `--qemu`) and paste verbatim output.
- Confirm coverage ≥ 75 % on each new QEMU integration crate per
  `AGENTS.md` §7.
- Confirm `kernel/sec`, `kernel/mem`, `kernel/ipc`, `kernel/irq`,
  `lib/caps` coverage remain ≥ 95 % after every addition.

## Verification commands

```
# Item 2-tail.4 regression (this session's surface):
cargo test -p rustos-kernel --lib
cargo test -p rustos-drv-bus-virtio --features kernel-host
cargo test -p rustos-abi -p rustos-kernel-mem -p rustos-kernel-sec \
           -p rustos-drvhost

# Items 4 / 6:
cargo xtask test --qemu
cargo xtask ci
cargo xtask test
```
