# Hardware interrupts: capability-gated user-space wake-ups

This page documents the `abi-v1` user/kernel contract for delivering
hardware interrupts to user-space drivers. The contract is exercised
by the `irq_bind` / `irq_wait` syscall pair and gated by the
`CAP_IRQ_BIND` capability (`lib/abi/src/capability.rs`).

Wake-up plumbing on the kernel side — the per-line wait queue and the
controller-level mask / unmask sequence — is wired and
QEMU-validated (see *Kernel-side implementation* below). The first
in-kernel consumer of the wait loop, `KernelVirtioHost::notify_wait`,
blocks a loaded virtio driver on its device's pre-bound `IrqHandle`
through the same shared primitive (Stage 4.D Item 2-tail.3). This
page locks down the contract those pieces respect.

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

The polling loop on top of `IrqTable::try_wait_step` lives in one
place — `rustos_kernel_irq::block_until_ready` — so every in-kernel
waiter drives the same implementation rather than re-deriving it
(`AGENTS.md` §2.2 — no duplication). The loop computes the deadline
once (`start + saturating(timeout)`, so `u64::MAX` does not wrap to a
tiny value), polls `try_wait_step`, and suspends between polls until
the line fires, the deadline elapses, or the binding disappears.

The clock and the suspend step are inverted behind the
two-method `IrqWaiter` trait, which keeps `kernel/irq` free of any
scheduler or architecture dependency:

* `IrqWaiter::now_ns()` — non-decreasing per-CPU clock reading.
* `IrqWaiter::yield_now()` — suspends until the next poll (a real park
  off the run queue on the syscall path, a cooperative yield / `wfi`
  on the kthread path — the implementation's choice) and returns `Ok`
  to re-loop, or `Err(IrqWaitAbort)` to abort (e.g. the task can no
  longer be scheduled).

Three implementations exist:

* **`irq_wait` syscall handler** (`kernel/core::syscalls`): registers
  the caller on `rustos_kernel_core::IRQ_WAITQ` *before* the first
  poll, then **parks** off the run queue (`SyscallIrqWaiter` calls
  `reschedule_current(Park)` — `AGENTS.md` §2.1, no busy yield). The
  device-IRQ dispatch path's `irq_wake` (after `IrqTable::fire`) and the
  architecture one-shot's `timed_wake_sweep` are **lock-free**: they only
  flag a pending wake on `IRQ_WAITQ`, and the actual `unpark` runs at the
  next dispatcher-context `waitq::drain_pending_wakes` (the fully preemptive
  kernel runs in-kernel code with device IRQs enabled, so an ISR must never
  take the scheduler lock — `AGENTS.md` §17.1). The waiter re-checks its own
  bound line after every wake and deregisters when the wait ends. The
  register-before-poll order plus the scheduler wake-pending token
  closes the park/unpark race, exactly as `hw_tree_wait` does
  (`AGENTS.md` §2.2). In host-side tests (no live dispatch loop)
  `reschedule_current` returns `false` and it falls back to
  `Scheduler::yield_current`, tolerating `SchedError::InvalidState`,
  mapping `NoSuchTask` to `Errno::NotFound`, and failing closed to
  `Errno::OutOfRange` on any other scheduler error; the loop still
  terminates because the per-CPU monotonic clock is strictly
  monotonic.
* **`KthreadIrqWaiter`** (`kernel/core::kthread_irq`): the waiter an
  in-kernel **service kthread** drives — it has no syscall frame or
  `Scheduler` borrow, so it suspends through the object-safe
  `YieldHandle` the core hands its body. It wraps that handle (in a
  `RefCell`, because `IrqWaiter::yield_now` is `&self` while
  `YieldHandle::yield_now` is `&mut self`) plus a monotonic-clock
  closure, and like the syscall path it *yields* (re-enqueues,
  staying runnable, or `wfi`-parks on metal) rather than parking on the
  scheduler run queue, so there is no lost-wakeup window. This is the
  path the P11 root-unlock kthread takes to drive
  interrupt-driven block I/O before login; it is proven end-to-end by
  the `tests/integration/irq_kthread_qemu_aarch64` vertical, where a
  device SPI wakes a parked kthread under the live scheduler.
* **`KernelVirtioHost::notify_wait`** (`kernel/virtio`): waits on
  the device's pre-bound
  `IrqHandle` against the owning task (`caller.task()`) with an
  unbounded (`u64::MAX`) timeout. A virtio device signals completion
  on a single MSI / MMIO line, not per-queue, so the wait key is the
  handle, not `queue_index`; the driver re-scans every used ring on
  wake-up. Because the wake-up is the ready flag that `fire` sets
  *after* masking, the mask-before-wake invariant is observed before
  the driver returns from `notify_wait` — exercised by
  `kernel_host::tests::notify_wait_observes_mask_before_wake`.

This design composes existing scheduler primitives only; no new
scheduler interface is introduced (`AGENTS.md` §2.4 — no interface
creep). The `irq_wait` syscall path now **parks** off the run queue
rather than busy-yielding: the device-IRQ dispatch path's `irq_wake`
(after `IrqTable::fire`) unparks a waiter on a fire, and `IRQ_WAITQ`
is swept by `timed_wake_sweep` for finite timeouts, so a blocked
user-space driver consumes no scheduler quanta. The in-kernel kthread
path still suspends through its own race-free wait (a cooperative
yield under the QEMU verticals, a `wfi` park on metal); migrating it
to the same scheduler park is the device two-task split, queued for a
future landing alongside the per-arch trap glue.

### Architecture-port glue

Each architecture port supplies an `IrqController` impl. The
kernel/core default is `UnsupportedController`, whose `mask`
returns `MaskError::Unsupported`; the kernel binary is expected
to swap in a real controller during its post-`run_phases` wiring
phase:

| Architecture | Production controller                                                                                  | Status today |
| ------------ | ------------------------------------------------------------------------------------------------------ | ------------ |
| `x86_64`     | `kernel/rustos-kernel::ioapic_controller::IoApicController` — IO-APIC redirection-entry mask via `IoApic::set_redirection_entry`; trap source from the `0x30..=0xFE` per-vector ISR thunks (`kernel/arch/x86_64/src/external_irq.s`) and Rust dispatcher (`kernel/arch/x86_64::irq`). | **Wired and QEMU-validated** (Stage 4.D Item 2-tail.2 + QEMU validation). `BinArch::irq_routing` returns the controller; `try_boot` walks MADT's IO-APIC entries, installs one IDT vector per pin, and programs every redirection entry `masked = true`. The `tests/integration/irq_qemu_x86_64` integration crate drives a live PIT-channel-0 one-shot through GSI 2 and asserts both `WaitStep::Ready` and the post-fire mask bit. |
| `aarch64`    | `kernel/rustos-kernel::aarch64::gic_irq::GicIrqController` — the downstream `IrqController` bridge over the arch port's `kernel/arch/aarch64::gic::GicController`, whose HAL `mask` clears the distributor `ICENABLER` enable bit + SeqCst-fences; the EL1 IRQ vector (`kernel/arch/aarch64::exceptions`) acknowledges via `IAR`, forwards a non-timer INTID to the set-once `set_device_irq_dispatch` hook, and bridges to `IrqTable::fire`. | **Wired into the boot path and QEMU-validated** (P11 Chunk B-2 INCREMENT (1)). `Aarch64BinArch::irq_routing` returns the GICv2-backed routing and `install_irq_dispatch` publishes the `IrqTable` into the EL1 vector seam, so the kernel/core `irq` phase builds the table against the real controller. Device SPIs are discovered from the device tree (`kernel/arch/aarch64::fdt::gic_device_intid` decodes a node's `interrupts` triple → INTID, no board constant) and a parked **kthread** is woken through `KthreadIrqWaiter`; proven end-to-end by `tests/integration/irq_kthread_qemu_aarch64` (RTC SPI → parked kthread → `WaitOutcome::Ready` + post-fire masked bit) alongside the delivery-path vertical `tests/integration/irq_qemu_aarch64`. The boot path does not yet *bind/route* a device SPI — that arrives with INCREMENT (2)'s root-unlock kthread; the arch port owns no `kernel/irq` dependency — the bridge lives downstream (`AGENTS.md` §17.2). |
| `riscv64`    | `tests/integration/riscv64_boot::PlicIrqController` — the downstream `IrqController` bridge over the arch port's `kernel/arch/riscv64::plic::PlicController`, whose inherent `mask` writes the source's PLIC priority register to zero; S-mode trap vector (`kernel/arch/riscv64::trap`) claims/completes via the PLIC and bridges to `IrqTable::fire`. | **Implemented and host-tested**, not yet armed in the boot path (Stage 4.D Item 4 — riscv64 external-IRQ controller). The PLIC register driver, the `scause` decode, the one-shot dispatch slot, and the `PlicIrqController` bridge (incl. mask-before-wake through `IrqTable`) are unit-tested; the boot pipeline does not call `trap::init_traps` until the virtio-mmio verticals wire it. The arch port owns no `kernel/irq` dependency — the bridge lives downstream (`AGENTS.md` §17.2). |
| `wasm32`     | No hardware-interrupt concept                                                                          | Permanently `UnsupportedController` (per the contract above). |

There are two `IrqController` traits and they are deliberately
distinct. The one in this section is the **consumer-side**
`rustos_kernel_irq::IrqController` (just `mask`) that `IrqTable::fire`
calls during a wake. Separately, the §17.2 Arch HAL
(`rustos_arch_api`, `plans/WIRING.md` Stage W3) defines an
architecture-facing `IrqController` (`mask` + `unmask`, fail-closed with
`IrqControlError::OutOfRange`) and an `InterruptEntry` (the `claim` →
`complete` prologue/epilogue) so the architecture-neutral kernel can name
one controller surface across every port. Each port implements the HAL
traits over its real controller behind an MMIO seam — riscv64
`PlicController` (`PlicMmio`), aarch64 `GicController` (`GicMmio`,
`ICENABLER`/`ISENABLER` masking + `IAR`/`EOIR`), and x86_64
`IoApicController` (`IoApicMmio`) — and runs the host
`irq::conformance::run_controller` (+ `run_entry` for the claim-based
ports) vertical over it. x86_64 is **vectored** and implements
`IrqController` only (no claim register to model, so no `InterruptEntry`,
`AGENTS.md` §2.1). The downstream `kernel/irq` bridge each port's boot
pipeline installs forwards the consumer-side `mask` to the same
controller, so the arch port still owns no `kernel/irq` dependency
(`AGENTS.md` §17.2).

The kernel binary records one `KERNEL_PHASE_STARTED` /
`KERNEL_PHASE_READY` pair with `phase = "irq"` strictly between
the `sched` and `syscall` phase markers. The kernel/core init
order is therefore `log → mem → sec → sched → irq → syscall →
ipc`, pinned by
`kernel/core::init::Phase::ORDER` and the
`run_phases_emits_each_phase_in_documented_order` and
`irq_phase_lands_between_sched_and_syscall` regression tests.

### x86_64 trap glue (Stage 4.D Item 2-tail.2)

The x86_64 trap path threads an external IRQ end-to-end through:

1. **IDT vectors `0x30..=0xFE`.** Reserved for external IRQs.
   Per-vector asm stubs are emitted by an `.altmacro` / `.rept`
   loop in `kernel/arch/x86_64/src/external_irq.s`; the stub
   addresses are published as a `.rodata` `.quad` table and
   exposed through `kernel/arch/x86_64::irq::external_isr_addr`.
2. **Shared trampoline.** Each per-vector stub pushes the vector
   immediate and jumps to `rustos_arch_x86_64_external_irq_common`,
   which saves the 15 GPRs into a `SavedRegs` block and calls
   `rustos_arch_x86_64_external_irq_dispatch(*mut SavedRegs, u64)`.
3. **Rust dispatcher.** Reads the installed `ExternalIrqDispatchFn`
   from a set-once `AtomicUsize` and writes the LAPIC EOI register
   before returning. The asm trampoline pops GPRs, drops the
   vector qword, and `iretq`s.
4. **Vector↔GSI routing.** A read-only-after-init `Routing` table
   (lock-free, one `AtomicU32` per reserved vector) maps the IDT
   vector to a GSI. Populated by the kernel binary's
   `try_boot::discover_and_program_io_apics` during the
   `Phase::Irq` step.
5. **IO-APIC programming.** The kernel binary walks every MADT
   `IoApic { id, address, gsi_base }` entry, allocates a vector
   per pin from the reserved range, calls
   `percpu::install_vector` to wire the IDT, calls
   `Routing::install(gsi, vector)`, and calls
   `IoApicController::program_pin(gsi, vector, bsp_lapic_id,
   masked = true)`. Lines start masked; a follow-up driver-host
   commit unmasks them when a userland driver binds.
6. **Mask-before-wake.** `IoApicController::mask` re-writes the
   IO-APIC redirection entry with the cached `(vector, dest)`
   and `masked = true`, then issues a `core::sync::atomic::fence`
   with `Ordering::SeqCst`. The fence pairs with the SeqCst
   load `IrqTable::try_wait_step` performs on `ready`,
   guaranteeing every CPU that observes `ready = true` also
   observes the masked redirection entry. The host test
   `ioapic_controller_mask_before_wake_ordering` in
   `kernel/rustos-kernel::ioapic_controller` drives this exact
   path against a `RecordingMmio` mock and asserts the mask write
   completes before `IrqTable::fire` returns `Marked`.

The `KernelArch` trait extension surface is two new methods:
`irq_routing(&self) -> IrqRouting` (consulted during
`Phase::Irq`) and `install_irq_dispatch(&self, &'static
IrqTable)` (called by `kernel/core::init` immediately after the
table is constructed, used by the arch port to publish the
table pointer into its dispatcher slot). Both have safe default
impls so non-x86_64 ports inherit the conservative
`IrqRouting::unsupported` behaviour without source-level change.

### x86_64 QEMU validation (Stage 4.D Item 2-tail.2 QEMU)

`tests/integration/irq_qemu_x86_64` is the end-to-end regression
bound for the x86_64 trap path. The crate is a freestanding
`x86_64-unknown-none` kernel binary that reuses
`rustos_kernel::boot` verbatim and installs a custom audit Sink.
On observing `AuditEvent::BootCompleted` the sink:

1. Reads the published `IrqTable` through
   `rustos_kernel::arch_wrapper::published_irq_table` and the
   typed `IoApicController<VolatileIoApicMmio>` through
   `rustos_kernel::ioapic_controller::published_typed`. Both
   accessors expose state already published into set-once slots
   during boot; they perform no new writes (`AGENTS.md` §2.1).
2. Resolves the IDT vector assigned to GSI 2 (legacy ISA IRQ 0
   under QEMU's PIIX/Q35 `InterruptSourceOverride { source: 0,
   gsi: 2 }` mapping) via
   `rustos_arch_x86_64::irq::global_routing().vector_for_gsi(2)`.
3. Binds GSI 2 in the `IrqTable` against the synthesised
   `TaskId(0)`, masks the legacy 8259 PIC, unmasks the line via
   `IoApicController::unmask`, and arms PIT channel 0 in mode 0
   as a one-shot (architectural 1.193182 MHz × 2000-tick reload
   ≈ 1.68 ms).
4. `sti`s, then spin-polls `IrqTable::try_wait_step` with `hlt`
   between polls and a 1 s deadline. The asm trampoline +
   `production_external_irq_dispatch` chain delivers the IRQ →
   the dispatcher calls `IrqTable::fire(2, controller)` → the
   controller masks the line + SeqCst-fences → `ready` flips →
   `try_wait_step` observes `WaitStep::Ready`.
5. `cli`s and re-reads the IO-APIC redirection-entry low half via
   `IoApicController::read_pin_low(2)`. Asserts `low & (1 << 16)
   != 0` — the load-bearing evidence that the controller's mask
   write reached the IO-APIC MMIO window before the wake.
6. Flips `qemu_exit::exit_success`. Any deviation —
   missing slot, no vector bound, `WaitStep::TimedOut`,
   `WaitStep::NotFound`, mask bit clear — flips
   `qemu_exit::exit_failure` with the QEMU serial log attached
   by `tools/qemu::Runner`.

The crate is enrolled in `tools/xtask::commands::qemu_tests::TESTS`
with a 60 s budget. `cargo xtask test --qemu` builds and runs it
alongside the other five freestanding integration crates.

### riscv64 trap glue (Stage 4.D Item 4 — riscv64 external-IRQ controller)

The riscv64 port supplies the same `IrqController` seam through a
PLIC, plus the S-mode trap vector that turns a hardware external
interrupt into an `IrqTable::fire`. The PLIC register driver and trap
glue are a pure Arch HAL implementation and own no `kernel/irq`
dependency (`AGENTS.md` §17.2); the `IrqController` bridge
(`PlicIrqController`) lives downstream in
`tests/integration/riscv64_boot`. The pieces are implemented and
host-tested; the boot-to-`BootCompleted` slice does not yet arm them
(the `virt` board reaches `BootCompleted` with interrupts disabled),
so the virtio-mmio verticals are the first consumer.

1. **PLIC controller.** `kernel/arch/riscv64::plic::PlicController`
   wraps a `Plic<M>` register driver over the `PlicMmio` access seam
   (`VolatilePlicMmio` on the freestanding target, an in-memory mock
   in host tests). `arm(source)` enables the source in the boot
   hart's S-mode context bitmap, drops the context threshold to zero,
   and sets a delivering priority; `claim` / `complete` wrap the
   per-context claim register.
2. **Mask-before-wake.** The inherent `PlicController::mask` masks a
   source by writing its **priority register to zero** — a single
   32-bit MMIO store, after which the source can never out-prioritise
   the (zero) threshold and so cannot re-fire — then issues a
   `core::sync::atomic::fence` with `Ordering::SeqCst`. The downstream
   `PlicIrqController` bridge's `IrqController::mask` forwards here. The
   single-store strategy is lock-free: it never read-modify-writes a
   shared word, so it races neither the trap handler's claim/complete
   nor a concurrent arm/unmask on another source. The host test
   `mask_before_wake_through_irq_table` (in `tests/integration/riscv64_boot`)
   drives `IrqTable::fire` with the bridge and asserts the priority-zero
   write is the last register write before `fire` returns `Marked`.
3. **S-mode trap vector.** `kernel/arch/riscv64::trap` publishes
   `rustos_riscv64_trap_vector` (`trap.s`) and installs it into
   `stvec` (direct mode) via `init_traps`, which also sets
   `sie.SEIE` and `sstatus.SIE`. The vector saves the interrupted
   context's caller-saved registers, calls the Rust handler, restores,
   and `sret`s.
4. **Dispatch.** The Rust handler reads `scause`. A synchronous
   exception is unexpected in this slice and fails closed by parking
   the hart (rather than `sret`-looping the faulting instruction). A
   supervisor external interrupt
   (`is_supervisor_external_interrupt`) forwards to the one-shot
   `set_trap_dispatch` callback, which — like the x86_64 dispatcher —
   holds the controller reference and performs the PLIC claim →
   `IrqTable::fire` (mask-before-wake) → complete handshake.

The `scause` decode, the `sie`/`sstatus`/`scause` bit constants, and
the set-once dispatch slot build on the host so their unit tests run
under `cargo test`; the trap vector, `init_traps`, the handler, and
`VolatilePlicMmio` are gated to `riscv64-unknown-none-elf`.

### aarch64 GICv2 device-IRQ glue (Stage W3-B)

The aarch64 port supplies the same `IrqController` seam through a
GICv2, plus the EL1 IRQ vector path that turns a device's
shared-peripheral interrupt (SPI) into an `IrqTable::fire`. The GICv2
driver and exception glue are a pure Arch HAL implementation and own no
`kernel/irq` dependency (`AGENTS.md` §17.2); the `IrqController` bridge
(`GicBridge`) lives downstream in `tests/integration/irq_qemu_aarch64`,
mirroring riscv64's `PlicIrqController`. The boot-to-`BootCompleted`
slice does not yet arm a device IRQ (the timer PPI has its own path), so
the QEMU vertical is the first consumer.

1. **SPI target-routing.** GICv2 SPIs reset to *no* CPU target, so a
   device interrupt is never delivered until its `GICD_ITARGETSR` byte
   names a CPU. `kernel/arch/aarch64::gic::Gicv2::route_spi(intid,
   cpu_targets)` writes that byte — the SPI analogue of the x86_64
   IO-APIC redirection-entry destination field. INTIDs below
   `MIN_SPI_INTID` (32) are SGIs/PPIs whose target bytes are read-only
   and banked per CPU, so the routing write is skipped for them (a
   no-op rather than a silently-ignored read-only store).
2. **Mask-before-wake.** The HAL `GicController::mask` clears the
   source's distributor enable bit (`ICENABLER`) and issues a
   `core::sync::atomic::fence` with `Ordering::SeqCst`. The downstream
   `GicBridge::mask` forwards here. The fence pairs with the SeqCst
   load `IrqTable::try_wait_step` performs on `ready`, so every CPU
   that observes `ready = true` also observes the masked line.
3. **EL1 IRQ vector.** `kernel/arch/aarch64::exceptions` installs the
   EL1 vector table (`VBAR_EL1`) and, on an IRQ, acknowledges the GIC
   (`IAR`). The timer PPI dispatches to the scheduler-tick path; **any
   other** acknowledged INTID is a device interrupt and is forwarded to
   the set-once device-IRQ dispatcher. The GIC end-of-interrupt
   handshake (`EOIR`) stays in the vector path.
4. **Dispatch.** `exceptions::set_device_irq_dispatch` publishes a
   one-shot `extern "C" fn(u32)` into a fail-closed (set-once)
   `AtomicUsize` slot — the EL1 analogue of riscv64's
   `set_trap_dispatch`. The installed dispatcher services the device
   source and forwards the line to `IrqTable::fire` (mask-before-wake)
   over the `GicBridge`.

The `GICD_ITARGETSR` arithmetic, the `MIN_SPI_INTID` boundary, and the
set-once dispatch slot build on the host so their unit tests run under
`cargo test`; the `route_spi` free function, `init_vectors`, the IRQ
handler, and `VolatileGicMmio` are gated to `aarch64-unknown-none`.

### aarch64 QEMU validation (Stage W3-B)

`tests/integration/irq_qemu_aarch64` is the end-to-end regression bound
for the aarch64 device-IRQ path — the EL1/SPI analogue of the x86_64
crate above. The freestanding `aarch64-unknown-none` kernel binary:

1. Builds a kernel-neutral `IrqTable` and binds the PL031 RTC's GICv2
   SPI (INTID 34 = `MIN_SPI_INTID + 2`) against the synthesised
   `TaskId(0)`, then publishes a pointer to the table for the
   interrupt-context dispatcher.
2. Installs the `rtc_dispatch` device-IRQ dispatcher via
   `set_device_irq_dispatch` (which forwards to `IrqTable::fire` over
   the `GicBridge`), installs the EL1 vectors, and brings up the GICv2.
3. Routes the RTC SPI to CPU 0 (`gic::route_spi`), enables it at the
   distributor, arms the RTC match register one tick (~1 s) out, and
   unmasks IRQs at the PE.
4. Parks on `wfi` and spin-polls `IrqTable::try_wait_step`. When the
   RTC fires, the GIC delivers the SPI to EL1 → the vector
   acknowledges and calls `rtc_dispatch` → the dispatcher clears the
   RTC and calls `IrqTable::fire(34, &BRIDGE)` → the bridge masks the
   line + SeqCst-fences → `ready` flips → `try_wait_step` observes
   `WaitStep::Ready`.
5. Re-reads `GICD_ISENABLER` for INTID 34 and asserts the enable bit
   is clear — the load-bearing evidence that the mask write reached the
   distributor MMIO before the wake.
6. Flips `qemu_exit::exit_success`. Any deviation — `bind` refusal,
   duplicate dispatcher, `WaitStep::TimedOut`/`NotFound`, or an
   un-masked line — flips `qemu_exit::exit_failure`; a line that never
   fires never reaches PASS, so the run times out (the documented
   fail-loud behaviour).

The crate is enrolled in `tools/xtask::commands::qemu_tests::TESTS`
with a 60 s budget. `cargo xtask test --qemu` builds and runs it
alongside the other freestanding integration crates.

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
* `kernel/rustos-kernel::ioapic_controller` adds host tests for
  `program_pin`, `mask`, `unmask`, `read_pin_low`, multi-IO-APIC
  routing, the mask-before-wake ordering against a
  `RecordingMmio` mock, and out-of-range / unprogrammed-pin
  fail-closed paths.
* `kernel/rustos-kernel::arch_wrapper` adds host tests pinning
  the set-once semantics of the `published_irq_controller` slot
  and the "still-None until installed" invariant of the
  `published_irq_table` slot.
* `kernel/arch/riscv64::plic` adds host tests for the SiFive PLIC
  register-offset arithmetic, the S-mode context interleaving,
  `arm` / `unmask` / out-of-range refusal, `claim` / `complete`
  round-trip, the enable-bitmap toggle, and the mask-before-wake
  ordering through `IrqTable` against an in-memory `MockPlicMmio`.
* `kernel/arch/riscv64::trap` adds host tests for the `scause`
  supervisor-external-interrupt decode (incl. rejecting the same
  code as a synchronous exception and other interrupt causes), the
  `sie`/`sstatus`/`scause` bit constants, and the set-once
  fail-closed semantics of the trap-dispatch slot.
* `kernel/arch/aarch64::gic` adds host tests for the `GICD_ITARGETSR`
  offset arithmetic, the `MIN_SPI_INTID` boundary, `route_spi` writing
  the target byte for an SPI, and `route_spi` skipping SGIs/PPIs
  (read-only banked target bytes), on top of the existing
  `GicController` mask/claim conformance coverage.
* `kernel/arch/aarch64::exceptions` adds host tests for the set-once
  device-IRQ dispatch slot: fail-closed on a second install and the
  installed-fn address round-trip.
* `tests/integration/irq_qemu_x86_64` and
  `tests/integration/irq_qemu_aarch64` are the QEMU-validated
  end-to-end regression bounds described above.
