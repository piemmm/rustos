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
