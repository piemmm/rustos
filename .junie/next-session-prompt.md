# Next session — Stage 4.D Items 2-tail.3 / 2-tail.4 / 3 / 4 / 5 / 6
# (carried over after Item 2-tail.2 QEMU validation landed)

## Where we are

`.junie/next-session-prompt.prev.md` (Item 2-tail.2 QEMU
validation) landed in the preceding session and is recorded in
`PLAN.md` Stage 4.D under "Item 2-tail.2 QEMU validation — live
IRQ end-to-end on x86_64 QEMU, *complete*". The full Item
2-tail.2 deliverable is now hardware-validated: the new
freestanding `tests/integration/irq_qemu_x86_64` crate boots the
production kernel, programs the legacy IRQ-0 line through the
IO-APIC controller, arms PIT channel 0 as a one-shot, observes
`WaitStep::Ready` through `IrqTable::try_wait_step`, and asserts
the post-fire mask bit. The crate is enrolled in `tools/xtask`'s
QEMU workload and runs as part of `cargo xtask test --qemu`.

New public seams the session added (read-only-after-init
publications of state that was already produced by boot; no new
writable surface — `AGENTS.md` §2.4 honoured):

- `rustos_kernel::arch_wrapper::published_irq_table` /
  `published_irq_controller`.
- `rustos_kernel::ioapic_controller::publish_typed` (called by
  `try_boot::discover_and_program_io_apics`) /
  `published_typed`.
- `IoApicController::unmask(gsi)` —
  symmetric counterpart of `IrqController::mask` that re-applies
  cached `(vector, dest)` with `masked = false`.
- `IoApicController::read_pin_low(gsi)` —
  observes the redirection-entry low half (mask bit 16) via the
  same MMIO seam `program_pin` / `mask` write through.
- `IoApic::read_redirection_entry_low(pin)` in the arch crate.

Baseline at the start of this session: `cargo test -p
rustos-kernel --lib` → 34 passing; `cargo run -p rustos-xtask
-- test --qemu` → six QEMU crates pass
(`rustos-test-memory-isolation`,
`rustos-test-scheduler-stress-qemu`,
`rustos-test-kernel-arch-boot`,
`rustos-test-syscall-dispatch-qemu`,
`rustos-test-drvhost-qemu`,
`rustos-test-irq-qemu-x86-64`). Pinned toolchain is
`nightly-2026-05-27` (`rust-toolchain.toml`); QEMU is available
on the Linux session host.

The user-confirmed scope for the preceding session was the QEMU
validation only (option "A" of the two presented). Items
2-tail.3 / 2-tail.4 / 3 / 4 / 5 / 6 from the original prompt
remain deferred and are reproduced below verbatim.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4.D status block — the two newest paragraphs
  ("Item 2-tail.2 QEMU validation, *complete*" and the
  superseded "Item 2-tail.2 — x86_64 IDT external-vector +
  IO-APIC trap glue").
- This file.
- `.junie/next-session-prompt.prev.md` for the QEMU-validation
  plan that just landed.
- `.junie/next-session-prompt.prev2.md` (the original Item
  2-tail.2 host-substrate prompt) and
  `.junie/next-session-prompt.prev3.md` for the
  kernel-substrate split.
- `docs/src/security/irq.md` for the user-visible contract and
  the new "x86_64 QEMU validation" section.
- `tests/integration/irq_qemu_x86_64/src/main.rs` as a reference
  for the audit-sink-driven test pattern used end-to-end against
  the published `IrqTable` / `IoApicController`.
- `kernel/rustos-kernel/src/{arch_wrapper,ioapic_controller,
  boot}.rs` for the new accessor + publication wiring.
- `drivers/bus/virtio/src/kernel_host.rs` for the
  `KernelVirtioHost::notify_wait` polled shim Item 2-tail.3
  rewrites onto `IrqHandle`.
- `userland/system/drvhost/src/host.rs` for `VirtioHostFactory`
  and `HostConfig::virtio_host_factory` — drvhost stays free of
  `kernel/*` deps.
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs`
  (Item 3).

## What needs doing

### Item 2-tail.3 — `KernelVirtioHost::notify_wait` rewrite

A real `fire` source now exists end-to-end, so the polled
cooperative shim in
`drivers/bus/virtio/src/kernel_host.rs::KernelVirtioHost::notify_wait`
is no longer the only available wake-up path. Land:

- A per-virtio-host pre-bound `IrqHandle` (the bus-driver
  registration path — Item 3 below — supplies the GSI).
- `notify_wait` now calls into the kernel IRQ subsystem to
  block until the bound handle fires. The polled log accessor
  is retained only on `MockHost`; the production path becomes
  the canonical wake-up.
- Tests covering: virtio-host blocks on a fresh `IrqHandle`,
  `fire(gsi)` from a trap dispatcher releases the wait, mask
  is honoured before the driver observes the wake-up.

### Item 2-tail.4 — Kernel-binary `VirtioHostFactory` impl

Once the kernel binary has a per-process `DmaPool` (the Stage
4.D Item 1 work that lands in parallel), install a
`VirtioHostFactory` in the kernel binary that mints a fresh
`KernelVirtioHost` per loaded driver and passes it through
`HostConfig::virtio_host_factory`. The drvhost seam already
accepts it. **Do not** add a `kernel-host` feature to
`userland/system/drvhost`; the factory abstraction is designed
so drvhost stays free of `kernel/*` deps.

### Item 3 — Bus-handle hand-off from `drivers/bus/{pci,mmio}`

(Unchanged from the prior prompts — reproduced for continuity.)

- Extend the `PciBackend` / `MmioBackend` constructors in
  `drivers/bus/virtio` to receive a capability-checked register
  window rather than the bare identification tuple they carry
  today.
- The PCI and MMIO bus drivers obtain the window from the kernel
  via the DMA / future MMIO-map facility (the *kernel* allocates
  the window; the bus driver does not synthesise pointers).
- Per-bus unit tests with mock register windows; a QEMU
  integration test that walks PCI / DTB and hands a working
  window through to the virtio transport.
- Update `docs/src/drivers/bus.md` with the hand-off sequence
  and the capability flow.

### Item 4 — QEMU integration tests

Once Items 2-tail.3 / 2-tail.4 / 3 are in place:

- `tests/integration/virtio_blk_pci_x86_64` — boots the kernel +
  driver host + signed `.rxe`, attaches `virtio-blk` to a backing
  qcow2, reads sector 0 (planted by `tools/qemu`), writes a known
  pattern to sector 1, reads it back, verifies checksum.
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP echo
  round-trip against `qemu user net`'s built-in DHCP/ARP/ICMP
  responder. Depends on Item 5.
- Add an unload → reload → reuse test for each driver. The
  drvhost `VirtioHostFactory` is the seam through which a fresh
  `KernelVirtioHost` (and therefore a fresh per-driver
  `DmaPool`) is minted on each load.

### Item 5 — Userland ARP / IP / ICMP responder

The virtio-net QEMU integration tests need a small userland
stack:

- New crate `userland/net/icmp/` implementing only ARP request +
  reply, IP + ICMP echo, and a minimal main loop sitting on top
  of the `Net` trait.
- Out of scope: TCP, UDP, IPv6, routing — those are Stage 6 work.

### Item 6 — Acceptance gate

After Items 2-tail.3 / 2-tail.4 / 3 / 4 / 5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7
  (`userland/net/icmp`, the four new virtio QEMU integration
  crates).
- Confirm `kernel/sec`, `kernel/ipc`, and `kernel/irq` coverage
  remain ≥ 95 % after every addition.

## Toolchain note

The pinned `nightly-2026-05-27` is required for
`kernel/arch/x86_64` (`#[unsafe(naked)]`, inline-const). On
systems without `rustup` on PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`.

## Assumptions for the next session to confirm at the top of the PR body

1. The `abi-v1` surface remains frozen: `CapabilityId::IRQ_BIND`,
   `SyscallNumber::IRQ_BIND` / `IRQ_WAIT`, `IrqHandle`,
   `Errno::TimedOut`, the two `SyscallSpec` rows, the
   `SYSCALL_TABLE_HASH`, **and** the new kernel-internal
   `KernelArch::irq_routing` / `KernelArch::install_irq_dispatch`
   hooks (both have safe no-op defaults). The new
   `rustos_kernel::arch_wrapper::published_irq_table` /
   `published_irq_controller` accessors and the
   `rustos_kernel::ioapic_controller::{publish_typed,
   published_typed, unmask, read_pin_low}` surface are
   kernel-bin-internal — they read state already published into
   set-once slots at boot and are not part of the `abi-v1`
   user-visible surface. Any departure is an `abi-v2` change and
   is out of scope.
2. The `kernel/irq` substrate (`IrqTable`, `IrqController`,
   `UnsupportedController`) and the x86_64 trap glue
   (`external_irq.s`, `kernel/arch/x86_64::irq`,
   `IoApicController`) are unchanged. Item 2-tail.3 composes
   them via `IrqHandle`; it does not mutate either.
3. The `Phase::Irq` init step preserves the existing ordering —
   Log / Mem / Sec / Sched / **Irq** / Syscall / Ipc.
4. The mask-before-wake invariant remains the load-bearing
   safety property. The QEMU integration test
   `tests/integration/irq_qemu_x86_64` re-reads the IO-APIC
   redirection entry after wake-up and asserts the mask bit is
   set; any regression that violates the ordering must fail
   that test.
5. `KernelVirtioHost::notify_wait` blocks on `IrqHandle` in the
   production path; the polled log accessor is retained only on
   `MockHost`.
6. The kernel-binary `VirtioHostFactory` impl lives in
   `kernel/rustos-kernel` (or a thin kernel-side adapter crate
   if the generic bounds force it); `userland/system/drvhost`
   stays free of `kernel/*` deps.

## Verification commands for each Item

```
# Item 2-tail.3.
cargo test -p rustos-drv-bus-virtio --features kernel-host
cargo test -p rustos-drv-bus-virtio

# Item 3 / 4 / 5.
cargo xtask test --qemu
cargo xtask ci
cargo xtask test
```
