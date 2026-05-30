# Next session — Stage 4.D Item 6 (acceptance gate)

## Where we are

`PLAN.md` Stage 4.D Item 4 is **complete**. The most recent landings
(newest first):

- **riscv64 virtio-MMIO QEMU verticals + arch-neutral virtio crate —
  landed (latest session).** `tests/integration/virtio_blk_mmio_riscv64`
  and `virtio_net_mmio_riscv64` boot the riscv64 `virt`-board pipeline to
  `AuditEvent::BootCompleted`, then drive a real virtio device over the
  board's virtio-mmio bus end-to-end — the MMIO analogues of the gated
  x86_64 PCI verticals. Both reach `SiFive` Test PASS under
  `qemu-system-riscv64` (blk: sector-0 verify + sector-1 round-trip; net:
  ARP-resolve `10.0.2.2` + ICMP echo; both after a `load → reload` cycle
  and a clean `unload`), verified deterministic across repeated runs.
  - **Crate extraction.** The arch-neutral `KernelVirtioFactory` + the
    virtio-PCI / virtio-MMIO provisioning walks moved out of the
    x86_64-only `rustos-kernel` bin crate (it can't build for riscv64)
    into a new `kernel/virtio` (`rustos-kernel-virtio`) crate that names
    no architecture port; `rustos-kernel` re-exports every item, so its
    public API is unchanged (`AGENTS.md` §2.2 / §6).
  - **Shared bring-up.** `tests/integration/virtio_qemu_support` is now
    arch-generic: a `common` module owns the `QemuEnv` seam, the
    signed-`.rxe` inputs, the generic `drive_driver_lifecycle<Tr>`, and
    the generic device tails `virtio_blk_round_trip<Tr>` /
    `virtio_net_ping<Tr>`; `imp_pci` (x86_64) + `imp_mmio` (riscv64)
    supply the arch-specific bring-up and a `define_*_boot_harness!`
    macro. Both arches re-export their transport as `ScenarioTransport`,
    so the device-tail invocation text is identical. The x86_64
    verticals were refactored onto the shared tails.
  - **riscv64 MMIO scaffold (`imp_mmio`).** Consumes
    `published_dtb`/`published_memory_map`; builds the bus via the new
    public `rustos_drv_bus_mmio::virtio_mmio_bus_from_dtb` (`unsafe`;
    concrete `Mmio` type stays private behind `impl VirtioMmioBus`);
    provisions the `MmioTransport` through the `CAP_MMIO_MAP`-gated
    `KernelMmioMapper`; walks the DTB for the PLIC base + `riscv,ndev`
    and the device's `interrupts` source; builds a `PlicController` +
    `IrqTable`, arms the source, installs the S-mode trap dispatch (PLIC
    claim → virtio-MMIO `InterruptACK` → `IrqTable::fire` → complete) +
    `init_traps`; mints a `KernelVirtioHost`; runs the shared lifecycle.
    The IRQ park is a race-free `wfi` (unmask source, clear `sstatus.SIE`,
    re-check `IrqTable::ready_for`, `wfi` only if not ready, restore
    `SIE`) — no lost wake-up, no bounding timer. The virtio-MMIO
    `InterruptACK` in the dispatch is load-bearing (a level-high source
    never re-edges otherwise).
  - **Runner / enrolment.** The riscv64 QEMU runner passes `-global
    virtio-mmio.force-legacy=false` (RustOS only drives modern
    virtio-mmio); both verticals are enrolled in `cargo xtask test
    --qemu` (blk: planted 2048-sector disk; net: SLIRP + pcap).
  - Verified: both verticals PASS under QEMU; host `cargo test`
    (`rustos-kernel-virtio` 12, `rustos-drv-bus-mmio` 13, `rustos-kernel`
    33, `rustos-qemu` 51); `cargo clippy -- -D warnings` (host + x86_64 +
    riscv64), `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` (host +
    riscv64), `cargo fmt --check`, `cargo build --workspace` all clean.
    Docs: `docs/src/platform/riscv64.md` ("virtio-MMIO QEMU verticals");
    `AGENTS.md` §3 (`kernel/virtio`). **Not run here:** the mdBook half
    of `cargo xtask docs-check` (mdbook not installed) and `cargo deny
    check`.

- Earlier Item 4 landings (riscv64 boot-state publication, the PLIC +
  S-mode trap glue, the virtio unload→reload→reuse cycle, the shared
  virtio bring-up scaffolding, the x86_64 `virtio_blk_pci` /
  `virtio_net_pci` verticals, the riscv64 boot port, the MMIO/PCI
  transports, `provision_virtio_*`, the `KernelVirtioFactory`, Items
  1/2-tail/3) — all complete (see `PLAN.md` Stage 4.D).

## What needs doing — Item 6 (acceptance gate)

This is the only Stage 4.D item left. Run it on a host that has `mdbook`
and `cargo deny` installed (this environment had neither):

- Run `cargo xtask ci` and paste verbatim output in the PR body
  (`cargo xtask docs-check` needs `mdbook`; the advisory/license audit
  needs `cargo deny`).
- Run `cargo xtask test` **including** `cargo xtask test --qemu` and
  paste verbatim output. The `--qemu` matrix now includes the two new
  riscv64 MMIO verticals (`rustos-test-virtio-blk-mmio-riscv64`,
  `rustos-test-virtio-net-mmio-riscv64`) alongside the x86_64 PCI ones.
- Confirm coverage with `cargo xtask coverage`: ≥ 75 % on each new QEMU
  integration crate per `AGENTS.md` §7, and ≥ 95 % on `kernel/sec`,
  `kernel/mem`, `kernel/ipc`, `kernel/irq`, `lib/caps`, `lib/crypto`.

## Verification commands

```
# This session's surface:
cargo build --workspace
cargo test -p rustos-kernel-virtio -p rustos-drv-bus-mmio -p rustos-kernel --lib -p rustos-qemu
cargo clippy -p rustos-test-virtio-qemu-support \
             -p rustos-test-virtio-blk-mmio-riscv64 \
             -p rustos-test-virtio-net-mmio-riscv64 \
             -p rustos-drv-bus-mmio -p rustos-kernel-virtio -p rustos-arch-riscv64 \
             --target riscv64gc-unknown-none-elf -- -D warnings

# Manual single-vertical reproduction under QEMU:
qemu-system-riscv64 -M virt -no-reboot -display none -serial stdio -m 256M \
    -smp 1 -bios default -global virtio-mmio.force-legacy=false \
    -kernel target/riscv64gc-unknown-none-elf/debug/rustos-test-virtio-net-mmio-riscv64 \
    -netdev user,id=net0 -device virtio-net-device,netdev=net0

# Item 6:
cargo xtask test --qemu
cargo xtask ci
```
