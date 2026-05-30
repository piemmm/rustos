# Next session — Stage 4.D Items 4 / 6 (remaining)
# (carried over after the virtio unload → reload → reuse cycle landed in
#  the shared bring-up, on top of the gated `virtio_net_pci_x86_64` +
#  `virtio_blk_pci_x86_64` verticals)

## Where we are

`PLAN.md` Stage 4.D records the following as **complete** (most recent
first):

- **virtio unload → reload → reuse — landed (latest session).** The
  shared `run_virtio_scenario` previously loaded the signed `.rxe` once
  and dropped the `rustos_drvhost::Host` before the device-tail ran. The
  host lifecycle now lives in a `drive_driver_lifecycle(cfg, &dyn
  VirtioHostFactory, transport, vhost, body)` helper
  (`tests/integration/virtio_qemu_support/src/imp.rs`) that drives
  `load → snapshot → reload → unload` against the live
  `KernelVirtioFactory`, running the device-tail closure *after* the
  reload and *before* the unload. Both the blk and net verticals funnel
  through it, so each proves a reloaded driver still brings its device
  online and round-trips I/O — with no duplicated per-driver reload test
  (`AGENTS.md` §2.2). Verified: `cargo xtask test --qemu` green (all 8
  enrolled tests), `clippy -D warnings`, `RUSTDOCFLAGS="-D warnings"
  cargo doc --no-deps`, host `cargo build --workspace`. Docs:
  `docs/src/platform/x86_64.md`.

- **Shared virtio bring-up scaffolding + `virtio_net_pci_x86_64`
  vertical — landed and gated (latest session).** The ~430 lines of
  device-agnostic bring-up that were inline in
  `tests/integration/virtio_blk_pci_x86_64/src/kernel.rs` now live once
  in a new freestanding-only library crate
  `tests/integration/virtio_qemu_support`
  (`rustos-test-virtio-qemu-support`):
  - `run_virtio_scenario(cfg, body)` carves the high-RAM per-device DMA
    region from `published_memory_map()`, builds `DirectPhysMap` +
    `MmioMap` + `KernelMmioMapper`, runs `provision_virtio_pci`, binds a
    masked IO-APIC GSI + routes MSI-X (`msi_message` from the
    boot-assigned vector), mints a `KernelVirtioHost` over the carved
    pool, loads the signed `.rxe` through `rustos_drvhost::Host`, enables
    MSI-X, then hands the `PciTransport` + `&dyn VirtioHost` to a
    device-specific closure (`FnOnce(PciTransport, &dyn VirtioHost) ->
    Result<(), &'static str>`).
  - `define_boot_harness!(scenario)` generates the boot-observer `Sink`,
    the `#[panic_handler]` bridge, and `kernel_main`; the crate owns the
    shared bump `#[global_allocator]`. Everything is gated to
    `x86_64-unknown-none`, so a host `cargo build --workspace` compiles
    the crate to an empty library.
  - `virtio_blk_pci_x86_64`'s `kernel.rs` was refactored onto it (device
    tail only: `ToVirtioBlk` resolver + sector-0 verify + sector-1
    write/read-back) and re-gated green — no regression.
  - New `tests/integration/virtio_net_pci_x86_64` reuses the support
    crate; its tail opens `VirtioNet`, builds a `rustos_net_icmp::Client`
    from the device MAC + guest `10.0.2.15`, ARP-resolves the SLIRP
    gateway `10.0.2.2`, then `ping`s it and asserts the echo reply.
    First-try bring-up passed; the run's `<binary>.pcap` confirms the
    on-wire `ARP request/reply` + `ICMP echo request/reply (id 0x1234,
    seq 1)`.
  - `rustos-qemu-run` gained `--virtio-net` / `--virtio-net-pcap`;
    `tools/xtask/src/commands/qemu_tests.rs` gained a `virtio_net` field
    and enrols the net test (single CPU, 60 s, frame dump to
    `<binary>.pcap`).
  - Verified: `cargo xtask test --qemu` (all 8 enrolled tests green
    incl. blk + net), `cargo xtask clippy` / `test` / `abi-check`,
    `cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings" cargo doc
    --no-deps` (the three new crates, `x86_64-unknown-none`) all clean.
    Docs: `docs/src/platform/x86_64.md` ("virtio QEMU verticals (shared
    bring-up)"). **Not run in this environment:** the mdBook half of
    `cargo xtask docs-check` (mdbook not installed) and `cargo deny
    check`; the Item 6 acceptance gate must run the full `xtask` matrix
    on a host where both are available.

- **`virtio_blk_pci_x86_64` real round-trip — landed and gated.** Boot →
  `x86_mechanism_one()` PCI walk → map four virtio register windows →
  `route_msix` → mint a `KernelVirtioHost` over a carved per-device DMA
  pool → load the signed virtio-blk `.rxe` → read sector 0 (verify
  planted `byte[i] = i mod 256`) → write+read-back sector 1 → `qemu_exit`.
  The earlier ~30 % single-CPU MSI completion hang was eliminated by the
  lock-free `IrqTable::fire` / `try_wait_step` rewrite; enrolled in
  `cargo xtask test --qemu` with `disk_sectors: Some(2048)`.

- The `rustos-net-icmp` `Client` initiator + virtio-net `&'h dyn
  VirtioHost` lifetime loosening; the virtio-net user-networking surface
  in the QEMU runner (`Spec::with_virtio_net[_pcap]`); the riscv64 QEMU
  runner (`tools/qemu/src/riscv64.rs`) + `kernel/arch/riscv64::qemu_exit`;
  the modern virtio-MMIO `MmioTransport` and virtio-PCI `PciTransport`;
  the ring-0 `provision_virtio_pci` walk + `virtio_boot.rs` wiring; the
  `KernelVirtioFactory`; the capability-gated MMIO register-window
  hand-off (Item 3); and Items 1 / 2-tail.* — all complete (see PLAN.md
  Stage 4.D).

## What needs doing

### Item 4 — remaining QEMU integration tests

The x86_64 PCI verticals (blk + net) are done and gated. What remains:

- **riscv64 boot port + ring-0 DTB walk.** The riscv64 QEMU runner half
  (`-M virt`, `virtio-*-device` on virtio-mmio, SiFive-test exit decode)
  and `kernel/arch/riscv64::qemu_exit` already exist, and
  `drivers/bus/virtio::MmioTransport` is the transport. The kernel
  riscv64 boot pipeline (to `AuditEvent::BootCompleted`) and the ring-0
  DTB walk that resolves the `virtio-mmio` slot and maps its register
  block via `Mmio::map_slot_window` → `KernelMmioMapper` →
  `MmioTransport::new` are what's missing. This is the large piece.
- `tests/integration/virtio_blk_mmio_riscv64` and
  `virtio_net_mmio_riscv64` — once the riscv64 boot port lands, these are
  the MMIO analogues of the x86_64 verticals. Note the shared
  `virtio_qemu_support` crate is currently **x86_64-only** (it uses
  `x86_mechanism_one`, x86 MSI-X, `sti/hlt/cli`); the riscv64 verticals
  will need either an arch-gated sibling module in that crate or a
  parallel `*_mmio` support path (MMIO transport, DTB-resolved window, no
  MSI-X). Keep the device-tail closures identical to the x86_64 ones
  (blk: sector round-trip; net: `Client` resolve + ping) so the
  device-specific code is not duplicated across arches (`AGENTS.md`
  §2.2).
- **unload → reload → reuse** — *done* this session for the x86_64 PCI
  verticals (see "Where we are"). The riscv64 MMIO verticals will inherit
  the same `drive_driver_lifecycle` path once the riscv64 boot port lands
  (keep them on the shared helper — do not re-implement the cycle).

### Item 6 — Acceptance gate

- Run `cargo xtask ci` on a host with `mdbook` + `cargo deny` available
  and paste verbatim output in the PR body (this environment lacks both,
  so only the rustdoc half of `docs-check` was run here).
- Run `cargo xtask test` (incl. `--qemu`) and paste verbatim output.
- Confirm coverage ≥ 75 % on each new QEMU integration crate per
  `AGENTS.md` §7, and ≥ 95 % on `kernel/sec`, `kernel/mem`, `kernel/ipc`,
  `kernel/irq`, `lib/caps`.

## Verification commands

```
# This session's surface (host + freestanding):
cargo build --workspace
cargo clippy -p rustos-test-virtio-qemu-support \
             -p rustos-test-virtio-blk-pci-x86-64 \
             -p rustos-test-virtio-net-pci-x86-64 \
             --target x86_64-unknown-none -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps \
             -p rustos-test-virtio-qemu-support \
             -p rustos-test-virtio-blk-pci-x86-64 \
             -p rustos-test-virtio-net-pci-x86-64 \
             --target x86_64-unknown-none

# Manual single-vertical reproduction (the runner plants/attaches):
cargo run -p rustos-qemu --bin rustos-qemu-run -- \
    --kernel target/x86_64-unknown-none/debug/rustos-test-virtio-net-pci-x86-64 \
    --virtio-net-pcap /tmp/net.pcap --timeout-secs 60

# Items 4 / 6:
cargo xtask test --qemu
cargo xtask ci
cargo xtask test
```
