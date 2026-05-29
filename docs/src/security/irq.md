# Hardware interrupts: capability-gated user-space wake-ups

This page documents the `abi-v1` user/kernel contract for delivering
hardware interrupts to user-space drivers. The contract is exercised
by the `irq_bind` / `irq_wait` syscall pair and gated by the
`CAP_IRQ_BIND` capability (`lib/abi/src/capability.rs`).

Wake-up plumbing on the kernel side — the per-line wait queue, the
controller-level mask / unmask sequence, and the `KernelVirtioHost::
notify_wait` rewrite that consumes this ABI — lands in a follow-up
session. This page locks down the contract those pieces must respect.

## ABI surface

| `SyscallNumber` | name       | args                              | ret           | required capability |
| --------------- | ---------- | --------------------------------- | ------------- | ------------------- |
| 8               | `irq_bind` | `line: u32`                       | `IrqHandle`   | `CAP_IRQ_BIND`      |
| 9               | `irq_wait` | `handle: IrqHandle, timeout_ns: u64` | `Result<(), Errno>` | `CAP_IRQ_BIND` |

* `line` is the architecture-defined IRQ identifier:
  * **x86_64** — Global System Interrupt (GSI) reported by ACPI MADT
    (`kernel/arch/x86_64::acpi::Madt`).
  * **AArch64** — GIC `IntId` (SGI / PPI / SPI namespace per the
    GICv3 architecture reference).
  * **riscv64** — PLIC source number reported by the device tree's
    `interrupts-extended` property.
  * **wasm32** — reserved; the WASM host has no concept of hardware
    interrupts. A call from a WASM userland returns
    `Errno::NotImplemented`.
* `IrqHandle` is the opaque `u64` newtype in
  [`lib/abi/src/syscall.rs`](../../../lib/abi/src/syscall.rs). The
  kernel reserves `IrqHandle::INVALID` (`0`) so a caller-zeroed
  argument cannot be mistaken for a live handle.
* `timeout_ns` is interpreted relative to the kernel monotonic clock
  (`clock_get`'s reference). The kernel must round up to its tick
  granularity, never down — a caller asking for a 1 ns wait must not
  receive a zero-duration wait.

## Capability check

`CAP_IRQ_BIND = 11` is the single gate on both syscalls. The
dispatcher refuses with `Errno::PermissionDenied` and emits one
`SyscallPermissionDenied` audit record before any state is touched
(`AGENTS.md` §5.4 step 2). There is no "open by default" path; a task
holding no capabilities cannot observe a hardware interrupt under any
circumstances.

The same capability gates both ends deliberately: a task that may
bind a line must be able to wait on it, and the implementation of
`irq_wait` re-checks that `handle` was minted for the calling task
before any state transition, so a forged handle from another task is
rejected with `Errno::NotFound`. Splitting the gate into separate
`CAP_IRQ_BIND` / `CAP_IRQ_WAIT` would add ABI surface without making
the policy any tighter — the binding step is the security-relevant
authority.

The dispatcher emits a `SyscallInvoked` audit record on every
successful `irq_bind` (the spec row sets `audit: true`); `irq_wait`
is **not** audited on success, otherwise a busy driver would drown
the audit log.

## Wake-up contract

A producer of an interrupt — the per-architecture trap dispatcher
that fields the line at the controller — performs the following
sequence in this order:

1. **Mask the line at the controller** (LAPIC EOI inhibit / GIC
   `ICACTIVE` / PLIC `claim`) **before** waking the user-space
   waiter. This is the *only* ordering that prevents the same edge
   from re-firing while the driver is still draining its completion
   queue, and it must happen on the same CPU that took the trap so
   the controller-side state is consistent.
2. **Mark the line ready** in the kernel-side IRQ table entry
   associated with `handle`.
3. **Wake at most one** waiter on the per-handle wait queue. The
   driver is expected to bind one task per line; the kernel must not
   broadcast.
4. The woken task observes `Ok(())` returned from `irq_wait`. After
   draining its queues the driver re-issues `irq_wait`, which clears
   the ready flag and re-arms the line at the controller.

If `timeout_ns` elapses before step 2 occurs the kernel resumes the
waiter with `Err(Errno::TimedOut)`. The handle stays bound and the
caller may immediately re-issue `irq_wait`; `TimedOut` is **not** an
error in the IRQ subsystem (`lib/abi/src/error.rs`).

## Failure modes

| `Errno`             | when                                                            | audit event                |
| ------------------- | --------------------------------------------------------------- | -------------------------- |
| `PermissionDenied`  | caller lacks `CAP_IRQ_BIND`                                     | `SyscallPermissionDenied`  |
| `OutOfRange`        | `line` exceeds the platform's allowable range                   | `SyscallBadArguments`      |
| `OutOfRange`        | `irq_bind` `line` argument carries non-zero upper 32 bits       | `SyscallBadArguments`      |
| `NotFound`          | `irq_wait` `handle` was not minted for the calling task         | `SyscallHandlerRejected`   |
| `TimedOut`          | `irq_wait` timeout expired before the line fired                | none (per the audit policy)|
| `NotImplemented`    | called from a WASM userland or before the IRQ subsystem is wired up | none                     |

Every audit event listed above already exists in
`kernel/sec::audit`; the IRQ subsystem reuses them rather than minting
new event IDs.

## Why the kernel masks the line on wake-up

A user-space driver cannot reliably mask its own line: the
controller-level register write is privileged on every Tier-1
architecture (LAPIC MSR, GIC system register, PLIC MMIO behind a
kernel-only page mapping). If the kernel left the line unmasked on
wake-up, an edge-triggered device that fires faster than the driver
can drain its completion queue would re-enter the trap path and
either spin the CPU (level-triggered) or back-pressure the controller
into dropping subsequent edges (edge-triggered). The kernel therefore
masks the line *before* it wakes the waiter and unmasks it on the
driver's next `irq_wait` call, which is the moment the driver tells
the kernel "I am ready for the next edge". This sequencing is the
load-bearing safety invariant of the entire user-space IRQ path; the
follow-up session's kernel-side test plan exercises it directly.

## Out of scope for this contract

* **IRQ raising / masking by user space.** Neither syscall grants the
  ability to assert or mask a line. Both remain kernel-only.
* **Sharing a line between drivers.** `abi-v1` mints at most one
  binding per `(task, line)` pair. Shared-IRQ devices (PCI legacy
  pin-based interrupts) are out of scope; the virtio family uses
  MSI/MSI-X or PCIe message-signalled interrupts, which assign one
  GSI per queue and do not share.
* **Re-binding after task exit.** `Scheduler::exit` releases every
  binding the exiting task held; the kernel unmasks no lines on
  task exit (a freshly created task that wants the same line must
  re-issue `irq_bind`).

## Kernel-side implementation (Stage 4.D Item 2-tail)

The kernel-side substrate that backs the contract above lives in
the `kernel/irq` crate (`rustos-kernel-irq`). The crate is `no_std`,
holds no global mutable state, and exposes one type, `IrqTable`,
together with an `IrqController` seam the architecture port
implements:

```text
                  bind(line, caller)                ┌────────────┐
   syscall ─────────────────────────────────────────►            │
   irq_bind                                         │  IrqTable  │
                  try_wait_step(handle, …) ─────────►            │
   syscall                                          │  (RwLock-  │
   irq_wait                                         │   guarded  │
                                                    │   binding  │
   trap from arch         fire(line, &controller) ──►   table)   │
   port's IDT vector ───────────────────────────────►            │
                                                    └────────────┘
                                                          │
                                            controller.mask(line)
                                                          ▼
                                              architecture port
                                              (x86_64 IO-APIC,
                                               aarch64 GIC, …)
```

### Invariants

1. **Mask-before-wake.** `IrqTable::fire(line, controller)` calls
   `controller.mask(line)` *before* it sets the per-entry `ready`
   flag. The Rust source orders the two operations in that
   sequence; the unit test
   `kernel/irq::table::tests::mask_is_observed_before_wake`
   installs a probe controller whose `mask` impl reads the table's
   own `ready` flag through a borrow and asserts it is still
   `false` while `mask` is in flight. A regression that reorders
   the writes fails the test deterministically.
2. **Forgery defence in the table.** `IrqTable::try_wait_step`
   re-verifies the `(handle, caller)` mapping before any state
   transition. The syscall handler does not need to re-check,
   and the dispatcher does not see forged handles — the
   distinction surfaces only through the standard
   `SYSCALL_HANDLER_REJECTED` audit record carrying the syscall
   name.
3. **Lock ordering.** The table's interior `RwLock<Inner>` mirrors
   the `CapTable` lock-ordering policy: the syscall handler
   acquires the IRQ-table write lock for the duration of one
   `bind` / `try_wait_step` / `release_for` call and never holds
   the capability-table lock at the same time. The dispatcher
   releases the cap-table read lock before invoking the handler
   body, so a concurrent `cap_revoke` cannot deadlock against an
   in-flight `irq_wait`.
4. **Idempotent release.** `IrqTable::release_for(task)` returns
   the number of bindings it dropped; a second call against the
   same task returns zero. The `exit` syscall handler invokes
   `release_for` unconditionally before evicting the capability
   record, so a task that holds no IRQ bindings still terminates
   cleanly.

### Wait semantics

The handler implements `irq_wait` as a polling loop on top of
`IrqTable::try_wait_step`, composing two existing primitives:

* `KernelArch::monotonic_ns(arch.current_cpu())` — non-decreasing
  per-CPU clock; `irq_wait` reads it once at entry to compute the
  deadline (`start + saturating(timeout)`) and again on every
  iteration to detect timeout.
* `Scheduler::yield_current(caller.task_id)` — invoked between
  iterations to surrender the rest of the quantum. The handler
  tolerates `SchedError::InvalidState` (in host-side tests the
  calling task is not marked Running) and re-loops; the loop
  always terminates because the per-CPU monotonic clock is
  strictly monotonic.

This design composes existing scheduler primitives only; no new
scheduler interface is introduced (`AGENTS.md` §2.4 — no interface
creep). A power-efficient variant that parks the caller and
relies on a controller-tick to wake on timeout is queued for a
future landing alongside the per-arch trap glue; today's wait
loop is correct but consumes scheduler quanta while blocked.

### Architecture-port glue

Each architecture port supplies an `IrqController` impl. The
kernel/core default is `UnsupportedController`, whose `mask`
returns `MaskError::Unsupported`; the kernel binary is expected
to swap in a real controller during its post-`run_phases` wiring
phase:

| Architecture | Production controller (planned)              | Status today |
| ------------ | -------------------------------------------- | ------------ |
| `x86_64`     | IO-APIC redirection-entry mask via `IoApic::set_redirection_entry`; trap source from the per-vector ISR | **Not wired** in the kernel binary — the IDT external-vector range and the per-vector asm thunks are the next session's lead. `kernel/irq::UnsupportedController` is installed; `irq_bind` succeeds, `irq_wait` will time out, no real `fire` arrives. |
| `aarch64`    | GIC `ICACTIVE` / distributor mask            | Not wired; `UnsupportedController` installed. |
| `riscv64`    | PLIC `claim` / `complete` priority gating    | Not wired; `UnsupportedController` installed. |
| `wasm32`     | No hardware-interrupt concept                | Permanently `UnsupportedController` (per the contract above). |

The kernel binary records one `KERNEL_PHASE_STARTED` /
`KERNEL_PHASE_READY` pair around the controller installation
phase once it lands; the kernel/core `Sched` phase remains the
final init step for the architecture-neutral subsystems.

### Test coverage

* `kernel/irq` ships 18 unit tests covering bind / duplicate
  refusal / out-of-range refusal / ready-after-fire / timeout /
  forgery (no binding, wrong caller) / mask-before-wake ordering /
  controller errors / stray-IRQ containment / release_for
  semantics / handle-uniqueness across rebinds.
* `kernel/core::syscalls::tests` adds 6 syscall-handler tests
  (mint / out-of-range / duplicate / forgery / timeout / pre-fired
  ready) plus an `exit_releases_every_irq_binding_owned_by_task`
  test that asserts the `exit` ↔ `release_for` ordering.
