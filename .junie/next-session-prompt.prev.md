# Next session — Stage 4.D Item 2-tail.2 (x86_64 IRQ trap glue) +
# carried-over Items 3–6 + acceptance gate

## Where we are

`.junie/next-session-prompt.prev2.md` (Item 2-tail, kernel-side
substrate) landed in the preceding session and is recorded in
`PLAN.md` Stage 4.D under "Item 2-tail — kernel IRQ table +
per-handle wait queue, *complete*". The frozen `abi-v1` surface
(`CapabilityId::IRQ_BIND`, `SyscallNumber::IRQ_BIND` / `IRQ_WAIT`,
`IrqHandle`, `Errno::TimedOut`) is now backed by:

- New `kernel/irq` crate (`rustos-kernel-irq`, `no_std`) with
  `IrqTable::{bind, try_wait_step, fire, release_for, lookup}`,
  the `IrqController` seam, the placeholder
  `UnsupportedController`, and 18 in-tree unit tests including the
  `mask_is_observed_before_wake` ordering probe.
- `KernelSyscallHandlers::{irq_bind, irq_wait}` wired against
  `IrqTable` (no more `SYSCALL_FEATURE_UNAVAILABLE` deferral) and
  `KernelSyscallHandlers::exit` calling
  `IrqTable::release_for(caller.task_id)` before the capability
  record + scheduler exit.
- `KernelState` owns one `IrqTable::new(0)` and one
  `UnsupportedController`. With `max_line = 0` only line 0 is
  bindable; the production kernel binary's wiring phase below
  is responsible for installing a real controller and widening
  the line space.
- Docs: `docs/src/security/irq.md` extended with the kernel-side
  invariants + per-arch controller table; `docs/src/architecture/
  syscalls.md` handler-wiring table updated.

Baseline at the start of this session: `cargo test --workspace`
(excluding the five QEMU-only integration test crates) green;
`cargo clippy --workspace --all-targets -- -D warnings` (same
exclusion) clean; `cargo fmt --check` clean; `cargo xtask
abi-check` clean. Pinned toolchain is `nightly-2026-05-27`
(`rust-toolchain.toml`).

The preceding session's reality check turned up two facts the
prior prompt did not account for:

1. The prior prompt named scheduler primitives `block_current` /
   `wake_one` that do not exist by those names. The closest are
   `Scheduler::park(id)` / `Scheduler::unpark(id)`. Both are
   composable but susceptible to a lost-wakeup race when used
   for IRQ-side wakes; the landed `irq_wait` therefore uses a
   yield-cycle on `Scheduler::yield_current` between
   `IrqTable::try_wait_step` polls. The wait loop is correct (no
   lost wakeups, timeouts honoured) at the cost of consuming
   scheduler quanta while blocked.
2. The prior prompt said x86_64 IRQ masking happens through the
   LAPIC LVT (`LapicMmio::set_lvt_mask`). That is wrong: the LVT
   only covers LAPIC-internal sources (timer, LINT0/1, error,
   …). External GSIs are masked through the **IO-APIC
   redirection-entry mask bit** — `IoApic::set_redirection_entry(
   pin, vector, dest_apic_id, masked: bool)` already exists in
   `kernel/arch/x86_64::apic`. More importantly: the x86_64 port
   has *no* external-IRQ infrastructure today (no IDT
   external-vector range, no per-vector asm thunks, no LAPIC EOI
   prologue, no vector↔GSI map, no MADT-driven IO-APIC
   programming consumer). Building that machinery is Item 2-tail.2
   and is the lead of this session.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4.D status block — the two newest paragraphs
  ("Item 2-tail — kernel IRQ table + per-handle wait queue,
  *complete*" and the superseded "Item 2 — IRQ ABI surface").
- This file.
- `.junie/next-session-prompt.prev.md` and
  `.junie/next-session-prompt.prev2.md` for the original Item 2
  and the kernel-substrate split that this prompt builds on.
- `docs/src/security/irq.md` for the user-visible contract and
  the new "Kernel-side implementation" section.
- `kernel/irq/src/{lib,table,error}.rs` for the substrate this
  session wires a real `fire` source against.
- `kernel/arch/x86_64/src/{idt,apic,interrupts,acpi}.rs`. `idt.rs`
  currently wires `#PF`, `#GP`, `#DF` exception vectors plus a
  fail-loud default trampoline; `apic.rs` ships
  `IoApic::set_redirection_entry` and the LAPIC EOI primitive;
  `acpi.rs` parses MADT but its IO-APIC discovery is not yet
  consumed for IRQ routing.
- `kernel/rustos-kernel/src/{boot,dispatch,arch_wrapper,main}.rs`
  for the boot pipeline + `BinArch` wrapper where the production
  `KernelArch` impl lives.
- `drivers/bus/virtio/src/kernel_host.rs` for the
  `KernelVirtioHost::notify_wait` polled shim Item 2-tail.2's
  follow-on (or Item 4 below) replaces with an `IrqHandle`
  block.
- `userland/system/drvhost/src/host.rs` for `VirtioHostFactory`
  and `HostConfig::virtio_host_factory` — drvhost stays free of
  `kernel/*` deps.
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs`
  (Item 3).

## What needs doing

### Item 2-tail.2 — x86_64 IDT external-vector + IO-APIC trap glue

The kernel-neutral substrate is in place. This item wires a real
`fire` source on x86_64 so `irq_wait` actually wakes from a real
hardware interrupt.

- **IDT external-vector range.** Reserve `0x30..=0xFE` for external
  IRQs (the architectural usable range above the reserved
  exception/IPI vectors). For each vector, install an asm thunk
  in `kernel/arch/x86_64/src/interrupts.s` that pushes the
  vector number, calls a Rust `extern "C"` entry point
  (`rustos_arch_x86_64_external_irq(vector: u8) -> ()`), and
  performs the architectural IRET sequence. The thunks must be
  identical except for the immediate operand; AGENTS.md §2.2 (no
  duplication) means the assembly source generates them through
  a macro or a `.rept` loop, not by copy-paste.
- **LAPIC EOI prologue/epilogue.** The Rust entry point reads the
  vector, looks up the corresponding GSI through a table
  populated at MADT consumption time, calls into the kernel-core
  IRQ subsystem (see below), then writes the LAPIC EOI register
  before returning to the asm thunk for IRET.
- **Vector↔GSI map.** A `kernel/arch/x86_64::irq` submodule
  exposes `Routing::{install(gsi, vector), gsi_for_vector(vector)
  -> Option<u32>}`. The routing table is populated by the
  kernel binary during a new init phase (see below) from ACPI
  MADT IO-APIC entries; the routing table itself is read-only
  after init (`AGENTS.md` §2.1 — one-shot publish, no mutable
  static).
- **IO-APIC programming.** During the new init phase, walk MADT's
  IO-APIC table, for each pin allocate a vector from the
  reserved range, call `IoApic::set_redirection_entry(pin,
  vector, boot_apic_id, masked = true)` (lines start masked),
  and install the `(pin, vector)` pair in `Routing`. The
  production `IrqTable` is then constructed with `max_line =
  total_io_apic_pins`, replacing the conservative
  `IrqTable::new(0)` that `kernel/core::init` ships.
- **Trap → `IrqTable::fire`.** The Rust entry point calls
  `state.irq.fire(gsi, &state.irq_controller)?;` then EOI. The
  production `IrqController` impl on x86_64 (a new
  `IoApicController` type in `kernel/rustos-kernel`) programs
  the IO-APIC redirection-entry mask bit through the existing
  `IoApic::set_redirection_entry` interface.
- **Boot pipeline integration.** A new `Phase::Irq` step lands
  between `Phase::Sched` and `Phase::Syscall` in
  `kernel/core::init` so the IRQ table is constructed with a
  realistic `max_line` and the production controller is
  installed before any syscall can race the deferral path. The
  kernel binary populates the routing table inside this phase
  through a hook trait `KernelArch::irq_routing()` (`AGENTS.md`
  §2.4 — this is a real new contract, not creep).
- **Tests.**
  - In-tree unit tests for `IoApicController` against a mock
    `IoApicMmio` (the existing pattern in `kernel/arch/x86_64::
    apic`'s tests): assert mask writes land on the right
    redirection entry; assert `MaskError::OutOfRange` when the
    line exceeds the controller's `max_redirection_entry`.
  - In-tree unit tests for the new `Routing` table.
  - A QEMU integration test crate `tests/integration/irq_qemu_x86_64`
    mirroring `tests/integration/syscall_dispatch_qemu`'s
    layout: arms a small synthetic device (PIT- or HPET-driven
    one-shot is sufficient — they are already programmable
    from the existing arch port) at a known GSI, calls
    `irq_wait` from a synthetic in-kernel task, and verifies
    wake-up + mask. Place the test under
    `tests/integration/irq_qemu_x86_64`; add the crate to the
    workspace.
- **Coverage target.** ≥95 % for the new x86_64 IRQ submodule and
  for `IoApicController` (security-critical per AGENTS.md §7).
  Use `cargo xtask coverage`.

### Item 2-tail.3 — `KernelVirtioHost::notify_wait` rewrite

Once a real `fire` source exists, the polled cooperative shim in
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

(Unchanged from the original prompt — reproduced for continuity.)

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

Once Items 2-tail.2 / 2-tail.3 / 2-tail.4 / 3 are in place:

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

After Items 2-tail.2 / 2-tail.3 / 2-tail.4 / 3 / 4 / 5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7
  (`userland/net/icmp`, the four new QEMU integration crates,
  the new `kernel/arch/x86_64::irq` submodule).
- Confirm `kernel/sec`, `kernel/ipc`, and `kernel/irq` coverage
  remain ≥ 95 % after every addition.

## Toolchain note

The pinned `nightly-2026-05-27` is required for
`kernel/arch/x86_64` (`#[unsafe(naked)]`, inline-const). On
systems without `rustup` on PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`. The preceding session
validated the baseline on that toolchain.

## Assumptions for the next session to confirm at the top of the PR body

1. The `abi-v1` surface remains frozen: `CapabilityId::IRQ_BIND`,
   `SyscallNumber::IRQ_BIND` / `IRQ_WAIT`, `IrqHandle`,
   `Errno::TimedOut`, the two `SyscallSpec` rows, and the
   refreshed `SYSCALL_TABLE_HASH`. This session does not mutate
   them; any departure is an `abi-v2` change and is out of scope.
2. The `kernel/irq` substrate (`IrqTable`, `IrqController`,
   `UnsupportedController`) is unchanged. The x86_64 trap glue
   *composes* it; it does not modify the substrate. A new
   `IoApicController` type lives in `kernel/rustos-kernel` (or
   in a thin kernel-binary-side adapter), not in `kernel/irq`.
3. The new `Phase::Irq` init step preserves the existing
   ordering — Log / Mem / Sec / Sched **/ Irq /** Syscall / Ipc
   — so existing init-order tests pick up exactly one new
   `KERNEL_PHASE_STARTED` + `KERNEL_PHASE_READY` pair carrying
   `phase = "irq"`. Update
   `kernel/core/src/init.rs::tests::run_phases_emits_each_phase_in_documented_order`
   and the matching `docs/src/architecture/kernel.md` boot
   timeline table in the same commit.
4. The `irq_wait` polling loop is **not** replaced with a
   parking blocker in this session. The reason is documented
   in `docs/src/security/irq.md` ("Wait semantics"): the
   park-based blocker needs a table-internal interlock to
   close the lost-wakeup race between `fire` and `park`. That
   work is its own follow-up.
5. The mask-before-wake invariant remains the load-bearing
   safety property. The x86_64 `IoApicController` implementation
   must observe the same ordering: `IrqTable::fire` calls
   `controller.mask(line)` *before* setting `ready`; the
   `IoApicController` mask write must be a volatile store with
   a memory-barrier so a subsequent waker observing `ready =
   true` is guaranteed to also observe the mask. A unit test
   that violates the ordering (wake then mask) must fail the
   new test suite — write the test such that it would catch
   the regression.
6. `KernelVirtioHost::notify_wait` blocks on `IrqHandle` in the
   production path; the polled log accessor is retained only on
   `MockHost`.
7. The kernel-binary `VirtioHostFactory` impl lives in
   `kernel/rustos-kernel` (or a thin kernel-side adapter crate
   if the generic bounds force it); `userland/system/drvhost`
   stays free of `kernel/*` deps.
