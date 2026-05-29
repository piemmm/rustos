# Next session — Stage 4.D Items 4 / 6
# (carried over after Item 2-tail.4 — kernel-binary VirtioHostFactory — landed)

## Where we are

`PLAN.md` Stage 4.D records the following as **complete**:

- Item 1 — kernel per-process-heap `DmaPool` + `kernel/sec::dma`
  gate (`CAP_MEM_DMA`, audit 1030/1031).
- Item 2-tail.2 (+ QEMU validation) — live IRQ end-to-end on
  x86_64 QEMU.
- Item 2-tail.3 — `KernelVirtioHost::notify_wait` blocks on a
  pre-bound `IrqHandle` through `kernel/irq::block_until_ready`.
- Item 3 — capability-checked register-window hand-off
  (`RegisterWindow` / `MmioMapper` ABI seam, `kernel/mem::MmioMap`,
  `kernel/sec::map_mmio`, `Pci::map_bar_window` /
  `Mmio::map_slot_window`, `KernelMmioMapper`).
- Item 5 — userland ARP / IP / ICMP responder
  (`userland/net/icmp`, `rustos-net-icmp`).
- **Item 2-tail.4 — kernel-binary `VirtioHostFactory`** (this
  session). `KernelVirtioHost::new` now takes its `DmaPool` **by
  value** (`RefCell<DmaPool<'a, P>>`), which is what makes a
  `&self` factory sound. New module
  `kernel/rustos-kernel/src/virtio_factory.rs`:
  `KernelVirtioFactory<'k, P, F>` + `KernelVirtioFactoryConfig<'k>`,
  implementing `rustos_drvhost::VirtioHostFactory`. `mint` fails
  closed without `CAP_MEM_DMA`, else mints a fresh `AddressSpace`
  (via a `make_table: Fn() -> P` closure) + `DmaPool` and a fresh
  per-driver `KernelVirtioHost`. `rustos-kernel` gained
  `rustos-caps` / `rustos-drvhost` / `rustos-drv-bus-virtio`
  (`kernel-host`) deps + a `host-tests` dev-dep on `kernel/mem`.

Baseline host tests after this session (all green): `rustos-abi`
77, `rustos-kernel-mem` 101, `rustos-kernel-sec` 52,
`rustos-drv-bus-virtio` 50 (default and `--features kernel-host`),
`rustos-kernel --lib` 37 (+3), `rustos-drvhost` 19 lib; no
regressions elsewhere. Clippy (`-D warnings`), `cargo fmt --check`,
and `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` are clean on
every touched crate. The freestanding lib **and** the production
`rustos-kernel` bin both build for `x86_64-unknown-none`.
Toolchain is pinned `nightly-2026-05-27`; QEMU
(`qemu-system-x86_64` / `qemu-system-riscv64`) is available on the
Linux host.

`cargo xtask test`/`ci` (which build the freestanding kernel
targets and run the QEMU integration crates) were **not** run this
session — the changes are host-testable and additive — so the
acceptance gate (Item 6) must run them.

## Assumptions to confirm at the top of the PR body

1. `abi-v1` stays frozen. The owned-pool reshape of
   `KernelVirtioHost::new` is an internal driver-crate change, not
   an ABI change; the `VirtioHost` / `VirtioHostFactory` trait
   signatures are unchanged.
2. `KernelVirtioFactory` lives in the kernel binary (not `drvhost`)
   so `userland/system/drvhost` keeps zero `kernel/*` dependencies.
   The factory's `make_table` closure must return a **fresh, empty**
   page table per call (per-process isolation, `AGENTS.md` §4).
3. The production kernel binary does not yet construct a live
   `drvhost::Host` (there is no filesystem / `.rxe` load path in
   ring 0 yet). The factory is therefore exercised by unit tests and
   will be threaded into a live `Host` by the Item 4 QEMU crates,
   which is where boot + driver-host + signed `.rxe` wiring exists.

## What needs doing

### Item 4 — QEMU integration tests

These need full boot wiring: kernel + driver host + signed `.rxe`,
plus real device bring-up (walk PCI/DTB → `map_bar_window` /
`map_slot_window` → `KernelMmioMapper` → `PciBackend`/`MmioBackend`,
a per-device `DmaPool` via `KernelVirtioFactory`, and the IRQ line
bound alongside the register window). Model them on
`tests/integration/drvhost_qemu` (boot to `AuditEvent::BootCompleted`,
then drive the host) and `tests/integration/irq_qemu_x86_64`.

- `tests/integration/virtio_blk_pci_x86_64` — attach `virtio-blk` to
  a backing qcow2, read sector 0 (planted by `tools/qemu`), write a
  known pattern to sector 1, read it back, verify checksum. **Also
  satisfies Item 3's deferred "walk PCI and hand a working window to
  the virtio transport" check.**
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
