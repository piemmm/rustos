# Next session — Stage 4.D Items 2-tail.4 / 4 / 5 / 6
# (carried over after Item 3 — register-window hand-off — landed)

## Where we are

`PLAN.md` Stage 4.D records the following as **complete**:

- Item 1 — kernel per-process-heap `DmaPool` + `kernel/sec::dma`
  gate (`CAP_MEM_DMA`, audit 1030/1031).
- Item 2-tail.2 (+ QEMU validation) — live IRQ end-to-end on
  x86_64 QEMU.
- Item 2-tail.3 — `KernelVirtioHost::notify_wait` blocks on a
  pre-bound `IrqHandle` through `kernel/irq::block_until_ready`.
- **Item 3 — capability-checked register-window hand-off** (this
  session). New ABI seam in `lib/abi/src/driver/mmio.rs`:
  `RegisterWindow` (unsafe-mint, bounds/aligned volatile r/w),
  `MmioMapper` trait, `MmioMapError`, `CapabilityId::MMIO_MAP = 12`,
  `DriverHost::mmio_mapper()` (default `None`). Kernel facility:
  `kernel/mem::MmioMap` (guard-bracketed, `NO_CACHE`, device-frame
  mapping) + `kernel/sec::map_mmio`/`unmap_mmio` gate (audit
  1040/1041). Virtio: `PciBackend`/`MmioBackend` now own a
  `RegisterWindow`; `kernel-host` `KernelMmioMapper` mints windows
  through the gate. Bus hand-off:
  `Pci::map_bar_window(bdf, bar_index, &dyn MmioMapper)` and
  `Mmio::map_slot_window(base, &dyn MmioMapper)`.

Baseline host tests after this session (all green): `rustos-abi`
77, `rustos-kernel-mem` 101, `rustos-kernel-sec` 52,
`rustos-drv-bus-virtio` 50 (default and `--features kernel-host`),
`rustos-drv-bus-pci` 16, `rustos-drv-bus-mmio` 9; no regressions in
`drvhost` / `virtio_blk` / `virtio_net` / `kernel-core` /
`kernel-syscall`. Clippy (`-D warnings`, incl. `--all-features`),
`cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings" cargo doc
--no-deps` are clean on every touched crate. Toolchain is pinned
`nightly-2026-05-27`; QEMU is available on the Linux host.

`cargo xtask test`/`ci` (which build the freestanding kernel
targets and run the QEMU integration crates) were **not** run this
session — the changes are host-testable and additive — so the
acceptance gate (Item 6) must run them.

## Assumptions to confirm at the top of the PR body

1. `abi-v1` stays frozen except the deliberate additive surface:
   `CapabilityId::MMIO_MAP = 12`, the `RegisterWindow` /
   `MmioMapper` / `MmioMapError` / `WindowError` types, and the
   default-`None` `DriverHost::mmio_mapper()` accessor. The window
   constructor `RegisterWindow::from_mapping` is `unsafe` and is
   only ever called by the kernel mapper.
2. `kernel/mem::MmioMap` maps the device's own physical frames
   (it does **not** allocate from the `FrameAllocator`) and never
   reclaims them on `RegisterWindow` drop — a register window lives
   for the whole driver load and is released when the driver's
   `MmioMap`/address space is torn down.
3. `userland/system/drvhost` stays free of `kernel/*` deps; the
   kernel-side `MmioMapper` impl (`KernelMmioMapper`) lives behind
   `rustos-drv-bus-virtio`'s `kernel-host` feature, mirroring
   `KernelVirtioHost`.

## What needs doing

### Item 2-tail.4 — Kernel-binary `VirtioHostFactory` impl

Install a `VirtioHostFactory` in the kernel binary that mints a
fresh `KernelVirtioHost` per loaded driver and passes it through
`HostConfig::virtio_host_factory`. Item 1's `DmaPool` and Item
2-tail.3's `IrqHandle` wait path both exist now; the per-device GSI
comes from the bus-driver registration path (the
`KernelMmioMapper` / bus hand-off landed in Item 3 supplies the
register window, and the IRQ line binding rides alongside it).
**Do not** add a `kernel-host` feature to `userland/system/drvhost`.

### Item 4 — QEMU integration tests

Once 2-tail.4 is in place (Item 3's bus hand-off is done):

- `tests/integration/virtio_blk_pci_x86_64` — boot kernel + driver
  host + signed `.rxe`, attach `virtio-blk` to a backing qcow2,
  read sector 0 (planted by `tools/qemu`), write a known pattern to
  sector 1, read it back, verify checksum. **This crate also
  satisfies Item 3's deferred "walk PCI and hand a working window
  to the virtio transport" check** — assert the
  `Pci::map_bar_window` → `KernelMmioMapper` → `PciBackend` path
  drives real device registers.
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`, exercising
  `Mmio::map_slot_window`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP echo
  round-trip against `qemu user net`. Depends on Item 5.
- Add an unload → reload → reuse test for each driver.

### Item 5 — Userland ARP / IP / ICMP responder

New crate `userland/net/icmp/` implementing only ARP request +
reply, IP + ICMP echo, and a minimal main loop on top of the `Net`
trait. Out of scope: TCP, UDP, IPv6, routing (Stage 6).

### Item 6 — Acceptance gate

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` (incl. `--qemu`) and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate (`userland/net/icmp`,
  the four new virtio QEMU integration crates) per `AGENTS.md` §7.
- Confirm `kernel/sec`, `kernel/mem`, `kernel/ipc`, `kernel/irq`,
  `lib/caps` coverage remain ≥ 95 % after every addition.

## Verification commands

```
# Item 3 regression (this session's surface):
cargo test -p rustos-abi -p rustos-kernel-mem -p rustos-kernel-sec \
           -p rustos-drv-bus-pci -p rustos-drv-bus-mmio
cargo test -p rustos-drv-bus-virtio --features kernel-host

# Items 2-tail.4 / 4 / 5 / 6:
cargo xtask test --qemu
cargo xtask ci
cargo xtask test
```
