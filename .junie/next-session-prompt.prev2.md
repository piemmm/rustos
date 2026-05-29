# Next session — Stage 4.D Item 2-tail (kernel IRQ subsystem) + Items 3–6

## Where we are

The ABI-half of Item 2 from `.junie/next-session-prompt.prev.md`
landed in the preceding session and is recorded in `PLAN.md`
Stage 4.D under "Item 2 — IRQ ABI surface, *complete in part*".
The frozen `abi-v1` surface is now:

- `CapabilityId::IRQ_BIND = 11` (`lib/abi/src/capability.rs`),
  mirrored in `kernel/sec::is_known_capability` and the audit-
  frozen-id test.
- `SyscallNumber::IRQ_BIND = 8`, `IRQ_WAIT = 9`, opaque
  `IrqHandle(u64)` newtype with `INVALID = 0`, frozen
  `Errno::TimedOut = 13` (all in `lib/abi/src/syscall.rs` and
  `lib/abi/src/error.rs`).
- Two `SyscallSpec` rows in `lib/abi/src/syscalls.rs`
  (`irq_bind`: `U32 -> Handle`, audited; `irq_wait`: `Handle, U64
  -> Errno`, unaudited); refreshed `SYSCALL_TABLE_HASH` in
  `kernel/syscall/src/table.rs`.
- `SyscallHandlers::irq_bind`/`irq_wait` trait methods with
  `Dispatcher::invoke` arms; production
  `KernelSyscallHandlers` (`kernel/core/src/syscalls.rs`) routes
  both to a `SYSCALL_FEATURE_UNAVAILABLE(feature =
  irq_subsystem) + Errno::NotImplemented` deferral, the same
  pattern `cap_delegate` uses for `user_memory_copyin`.
- New [`docs/src/security/irq.md`](../docs/src/security/irq.md)
  locks down the user-visible contract (per-architecture line
  namespaces, wake-up sequence, mask-before-wake invariant,
  failure-mode table).

Baseline at the start of this session: `cargo test -p rustos-abi -p
rustos-kernel-syscall -p rustos-kernel-sec -p rustos-kernel-core`
all green, `cargo xtask abi-check` clean. Pinned toolchain is
`nightly-2026-05-27` (`rust-toolchain.toml`).

The preceding session's survey turned up a structural reality the
prior prompt's "Item 2 in full" wording did not account for: the
kernel-binary `boot()` pipeline reaches `SyscallInit` /
`PreemptInit` phases but does **not** yet wire user-space tasks
on real hardware, and `KernelVirtioHost::notify_wait` is still
the polled `MockHost` shim. Item 2-tail is therefore split out so
the kernel-side work can land at AGENTS.md's no-hacks bar
without dragging Items 3–6 with it.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4.D status block — in particular the new
  "Item 2 — IRQ ABI surface" paragraph and the existing
  "Item 0-tail" / "Item 0a" / "Item 0" paragraphs the IRQ work
  builds on.
- This file.
- `.junie/next-session-prompt.prev.md` for the ABI-half wording
  this prompt supersedes; `.junie/next-session-prompt.prev3.md`
  for the historical Items 2–6 text those prompts inherit from.
- `docs/src/security/irq.md` for the user-visible contract Item
  2-tail must implement.
- `lib/abi/src/{capability.rs, syscall.rs, syscalls.rs, error.rs}`
  for the frozen surface.
- `kernel/syscall/src/table.rs` for the `SyscallHandlers` trait
  and `Dispatcher::invoke` arms (do **not** mutate the
  `irq_bind`/`irq_wait` rows or the table hash — they are frozen).
- `kernel/core/src/syscalls.rs` for the production
  `KernelSyscallHandlers` impl that currently routes both calls
  to the `irq_subsystem` deferral. Item 2-tail removes that
  deferral and wires the real subsystem.
- `kernel/sched/src/{scheduler.rs, runqueue.rs, task.rs}` for the
  scheduler primitives Item 2-tail will compose into a per-handle
  wait queue.
- `drivers/bus/virtio/src/kernel_host.rs` for the
  `KernelVirtioHost::notify_wait` polled body Item 2-tail
  replaces.
- `kernel/rustos-kernel/src/{boot.rs, dispatch.rs}` for the
  kernel binary's existing init phases — the kernel-side
  `VirtioHostFactory` wires through the same surface.
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs`
  (bus driver side of Item 3).
- `userland/system/drvhost/src/host.rs` for `VirtioHostFactory`
  and the per-driver virtio plumbing.

## What needs doing

### Item 2-tail — kernel IRQ table + per-handle wait queue

The ABI surface is frozen; the kernel-side wake-up plumbing is
not. Land it now, end-to-end:

- New `kernel/irq/` crate (or `kernel/sched::irq` submodule —
  decide on the smaller surface; `AGENTS.md` §2.3 — no bloat)
  carrying:
  - A kernel-side IRQ table: `BTreeMap<u32 line, IrqEntry>`
    behind a `kernel/sync::RwLock`, mirroring the `CapTable`
    lock-ordering policy. Each `IrqEntry` holds the bound
    `SecTaskId`, the minted `IrqHandle`, a ready flag, and a
    short wait-queue (at most one waiter per `(task, line)`
    binding per the contract in `docs/src/security/irq.md`).
  - A `bind(line, task)` API that mints a fresh `IrqHandle`,
    refuses duplicate bindings, refuses lines outside the
    platform's allowable range, and records the binding against
    the calling task.
  - A `wait(handle, timeout_ns, caller)` API that re-checks the
    `(task, handle)` mapping (forgery defence), atomically
    consumes the ready flag if set, otherwise parks the caller
    on the per-entry wait queue using the existing
    `Scheduler::block_current` / `wake_one` primitives, and
    returns `Errno::TimedOut` on timeout.
  - A `fire(line)` API the per-architecture trap dispatcher
    calls. The sequencing — mask line at controller, then mark
    ready, then wake one — is the load-bearing invariant from
    `docs/src/security/irq.md`. Implement and unit-test it under
    a deterministic mock controller.
  - A `release_for(task)` API the scheduler calls on
    `Scheduler::exit` to drop every binding the exiting task
    held.
- Replace the `irq_subsystem` deferral in
  `KernelSyscallHandlers::{irq_bind, irq_wait}` with real
  forwarding to the new APIs. The audit-event mapping is in the
  failure-mode table of `docs/src/security/irq.md`; do not invent
  new event IDs.
- Per-architecture trap glue:
  - **x86_64**: `kernel/arch/x86_64::idt` already vectors LAPIC
    interrupts; route GSI numbers reported by ACPI MADT through
    `irq::fire(gsi)`. The mask step is the LAPIC's mask-bit in
    the LVT (`LapicMmio::set_lvt_mask`).
  - **aarch64** / **riscv64**: stub with `Errno::NotImplemented`
    and a kernel-init audit record naming the architecture; the
    relevant arch ports are themselves not yet wired (see
    Stage 3 in `PLAN.md`), so emitting a deferral here is the
    only honest landing.
  - **wasm32**: WASM userlands cannot bind IRQs; both syscalls
    return `Errno::NotImplemented` per
    `docs/src/security/irq.md`.
- Plumb the handle through `KernelVirtioHost::notify_wait`,
  replacing the polled cooperative shim from `MockHost`. The
  polled log accessor is retained only on `MockHost`; the
  production path blocks on `IrqHandle` via the new kernel
  subsystem.
- **Kernel-binary factory.** Wire a `VirtioHostFactory` impl in
  the kernel binary (`kernel/rustos-kernel/src/main.rs`-ish
  surface — locate the existing per-process `DmaPool` carve
  point that lands as part of Item 2-tail) that mints a fresh
  `KernelVirtioHost` per loaded driver and passes it through
  `HostConfig::virtio_host_factory`. The drvhost seam already
  accepts it. **Do not** add a `kernel-host` feature to
  `userland/system/drvhost` itself — the factory abstraction is
  designed so drvhost stays free of `kernel/*` deps.
- Tests:
  - In-tree unit tests covering: bind / duplicate-bind refusal /
    out-of-range refusal / fire-wakes-waiter / fire-then-wait
    consumes ready / timeout returns `TimedOut` / forged-handle
    rejected with `NotFound` / mask-before-wake ordering /
    release_for evicts on exit.
  - One QEMU integration test that arms an IRQ from a small
    in-tree mock device and verifies wake-up + mask. Place it
    under `tests/integration/irq_qemu_x86_64` mirroring
    `tests/integration/syscall_dispatch_qemu`.
  - Coverage targets per AGENTS.md §7: ≥ 95 % for the new
    kernel/irq crate (it is security-critical) and for
    `kernel/sec` after the binding-on-exit hook. Use
    `cargo xtask coverage`.
- Docs: extend `docs/src/security/irq.md` with the kernel-side
  invariants (lock ordering, scheduler interaction); update
  `docs/src/architecture/kernel.md` to add an "IRQ phase" entry
  to the init order; remove the *deferred* rows from the
  handler-wiring table in `docs/src/architecture/syscalls.md`.

### Item 3 — Bus-handle hand-off from `drivers/bus/{pci,mmio}`

(Unchanged from the prior prompt; reproduced for continuity.)

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

Once Items 2-tail + 3 are in place:

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

After Items 2-tail + 3–5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7
  (`userland/net/icmp`, the four new QEMU integration crates).
- Confirm `kernel/sec` and `kernel/ipc` coverage remain ≥ 95 %
  after the IRQ-plumbing additions and the
  `Scheduler::exit` ↔ `irq::release_for` interaction.

## Toolchain note

The pinned `nightly-2026-05-27` is required for
`kernel/arch/x86_64` (`#[unsafe(naked)]`, inline-const). On
systems without `rustup` on PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`. The preceding session
validated the baseline on that toolchain.

## Assumptions for the next session to confirm at the top of the PR body

1. The `abi-v1` surface added by the ABI-half landing —
   `CapabilityId::IRQ_BIND`, `SyscallNumber::IRQ_BIND/IRQ_WAIT`,
   `IrqHandle`, `Errno::TimedOut`, the two `SyscallSpec` rows, and
   the refreshed `SYSCALL_TABLE_HASH` — is **frozen**. Item 2-tail
   wires the kernel-side subsystem against that surface; it does
   not mutate it. Any departure from this is an `abi-v2` change
   and is out of scope.
2. `KernelSyscallHandlers::irq_bind`/`irq_wait` lose their
   `irq_subsystem` deferral in Item 2-tail; the
   `SYSCALL_FEATURE_UNAVAILABLE` audit record stops appearing in
   production. Update the deferral test in
   `kernel/core/src/syscalls.rs` accordingly (do **not**
   `#[ignore]` it — `AGENTS.md` §2.5).
3. The per-handle wait queue composes the existing scheduler
   primitives (`Scheduler::block_current` / `wake_one`); it does
   **not** introduce a new global mutable static
   (`AGENTS.md` §2.1).
4. `Scheduler::exit` gains a single call to
   `irq::release_for(task)` to evict every binding the exiting
   task held. The kernel unmasks no lines on task exit (a freshly
   created task that wants the same line must re-issue
   `irq_bind`).
5. The mask-before-wake invariant from
   `docs/src/security/irq.md` is the load-bearing safety
   property. A test that violates the ordering (wake then mask)
   must fail the new unit-test suite — write the test such that
   it would catch the regression.
6. `KernelVirtioHost::notify_wait` blocks on `IrqHandle` in the
   production path; the polled log accessor is retained only on
   `MockHost`.
7. The kernel-binary `VirtioHostFactory` impl lives in
   `kernel/rustos-kernel` (or a thin kernel-side adapter crate
   if the generic bounds force it); `userland/system/drvhost`
   stays free of `kernel/*` deps.
