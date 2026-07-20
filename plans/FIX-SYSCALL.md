# FIX-SYSCALL — Run syscalls with interrupts enabled

Status: **code + docs done, host-proven; per-arch QEMU syscall-body
verticals staged** (see "Remaining" below)

Binding under `AGENTS.md`. This plan closes the remaining half of the
"no cooperative dispatch loop" defect class (§17.1): **P-5 made the
in-kernel dispatch loop / kthreads preemptible, but the user→kernel
syscall entry path still runs with device interrupts and the preemption
timer masked for the whole syscall.** A long, non-blocking syscall body
therefore monopolises the CPU exactly as the pre-P-5 dispatch loop did
(the 2026-06-23 charter-amendment failure: a single in-kernel MMIO read
stalled ~4.3 s with nothing else running).

Read first (§15.18): `plans/WIRING.md` (Arch HAL parity), `plans/PI.md`
(the metal-stall failure this generalises), `docs/src/architecture/syscalls.md`.

## Goal / invariants

The correct model — the one Linux uses and the one that survives review:

1. **Device interrupts are ENABLED for the body of every syscall**, on
   every Tier-1 target. Enable once, uniformly, in the arch trap glue —
   never as a per-syscall flag.
2. **The kernel stays non-preemptible (§4).** An IRQ taken *while a
   syscall runs* (i.e. taken in EL1/S-mode/ring-0) services its source
   and returns to the **same** syscall; it does not reschedule. The
   reschedule is latched (`need_resched`) and honoured at
   **return-to-user**. "Interrupts on" ≠ "preemptible kernel"; only the
   former changes here.
3. **Mask only around genuine critical sections** — the
   run-queue/context-switch window and any held `lib/sync` lock — briefly.
   This is the "narrow" enforcement §17.1 demands.
4. **No per-syscall "runs uninterruptible" flag.** That is ABI/complexity
   creep (§2.3/§2.4) and the wrong axis: the thing that must be
   uninterruptible is the *critical section*, not the *syscall*. If one
   specific region genuinely needs a longer uninterruptible window it
   masks locally with a documented reason; the syscall as a whole still
   enters interruptible.

## Why this is safe today (audit result — carried forward)

Safety with IRQs on is a property of **lock discipline**, not of any
syscall's function. With IRQs enabled the only new hazard is an ISR
firing mid-syscall on the same CPU and contending for a lock the
interrupted syscall holds (single-CPU self-deadlock). By construction
that cannot happen, given two invariants that P-5 already established:

- **Invariant 1 — every ISR is lock-free w.r.t. scheduler/wait state.**
  `IrqTable::fire` touches only per-line atomics (its doc: taking `Inner`
  "would deadlock a single CPU whose parked task already holds it in
  `try_wait_step`"). All interrupt-context wakes (`irq_wake`,
  `console_wake`, `timed_wake_sweep`, `call_wake`, …) call only
  `WaitQueue::request_wake`, which sets one `AtomicBool`; the real
  `wake_all` (wait-queue `SpinLock` → scheduler `unpark`) runs later in
  dispatcher context via `waitq::drain_pending_wakes`. So a syscall may
  hold the wait-queue / run-queue plain locks with IRQs enabled with no
  deadlock risk.
- **Invariant 2 — the one genuinely ISR↔task-shared structure is
  IRQ-gated.** The UART receive ring (`ConsoleInputQueue` / `UART_INPUT`,
  a `SpinLock<InputRing>`) is shared by the RX ISR
  (`drain_uart_into_console_queue`) and `stream_read`/`keyboard_read`.
  On aarch64 both sides take `UART_RX_GATE` first — an
  `IrqSafeSpinLock<(), DaifIrqControl>` masking `DAIF.I` for the short
  one-FIFO-drain hold. That is the narrow §17.1 masking done right.

Given those, **no syscall is intrinsically unsafe** to run interruptible:
fast/non-blocking calls touch per-process or lock-free state; blocking
calls park on a `WaitQueue` and are woken by the lock-free deferred drain
(this class *benefits* most); console I/O is the gated-ring case;
bootstrap-floor storage (`fs_*` → in-kernel virtio-blk / EMMC2) is the
long-MMIO case IRQs-on is meant to fix, and its completion parks on a
bound GIC line woken through the lock-free `IrqTable::fire`.

## The three things to CONFIRM/ENFORCE before enabling (real risk surface)

These are verification, not redesign — and each is a task below:

- **C1 — console-RX lock discipline on riscv64 and x86_64.** The
  `UART_RX_GATE` interlock was found only under
  `kernel/tairix-kernel/src/aarch64/`. Confirm the riscv64/x86_64 console
  input paths either are synchronous (no RX ISR sharing the ring) or gain
  the same `IrqSafeSpinLock` gate. A plain-`SpinLock` ring shared with an
  RX ISR and *not* IRQ-gated is the exact deadlock this change would
  introduce.
- **C2 — wasm32.** No hardware IRQ masking; "interrupts" are the host
  yield facility. Confirm the entry glue degrades to a sane no-op and the
  deferred-drain model still holds.
- **C3 — standing rule for future drivers.** "Safe with IRQs on" is
  preserved only if enforced going forward: any lock shared between a
  driver's ISR and its syscall-reachable body MUST be an
  `IrqSafeSpinLock` (or the ISR side must be lock-free with a deferred
  drain). Add this to the §23 review checklist and a doc note in
  `lib/sync`.

## What landed

**T1 — arch entry glue (interruptible syscall body).** Each bare-metal
port unmasks device IRQs around the syscall dispatch call only and
re-masks before restoring the user frame; the trampoline's full-frame
save + frame-resident return state (aarch64 `ELR/SPSR/SP_EL0`, riscv64
`sepc/sstatus`) plus nested-trap support make a mid-body IRQ safe:

- `kernel/arch/aarch64/src/exceptions.rs`: `enable_irq()` / `mask_irq()`
  (`DAIF.I`) around `dispatch_svc`.
- `kernel/arch/riscv64/src/trap.rs`: `set_supervisor_interrupts(true/false)`
  (`sstatus.SIE`) around `dispatch_ecall`; the stale "kernel runs with
  `SIE == 0`" comment in `preempt.rs` corrected — the saved-`SPP` gate is
  now load-bearing, not defence-in-depth.
- `kernel/arch/x86_64/src/syscall_entry.rs`: `sti` / `cli` around the
  `call {dispatch}` in `syscall_entry_stub` (entry still clears
  `IF`/`IOPL`/`AC`/… via `IA32_FMASK`; only `IF` is re-enabled, in kernel
  context after `swapgs`+pivot). The ISRs identify the CPU via the LAPIC,
  not GS, and gate preemption on the saved ring-3 `CS`, so a body-taken
  IRQ runs at ring 0 with kernel GS and never reschedules.
- `kernel/arch/wasm32`: no-op (no hardware interrupts) — documented in
  `syscall_entry.rs`.

Every port keeps its interrupted-privilege preempt gate (`from_el0` /
`SPP` / ring-3 `CS`), so a body-taken IRQ serves-and-returns without a
mid-syscall switch — the kernel stays non-preemptible (§4).

**T2 — deferred drain at return-to-user (one arch-neutral definition).**
`completion_outcome` (`kernel/core/src/syscalls.rs`) calls
`waitq::drain_pending_wakes()` then suspends the caller with `Yield` when a
preemption tick was latched *or* the drain unparked a task; otherwise it
returns straight to user space. This reuses P-5's machinery — no
per-syscall flag, no second wake discipline.

**C1/C2/C3 closed.** C1: only aarch64 has an ISR-shared console RX ring
(already `UART_RX_GATE`-interlocked); riscv64 and x86_64 console reads are
the fail-closed `NULL_CONSOLE_READ` (no RX ISR) — synchronous, nothing to
gate. C2: wasm32 entry is inherently a no-op; the shared drain still runs.
C3: the standing rule ("any ISR-shared lock is `IrqSafeSpinLock` or the
ISR side is lock-free + deferred drain") is in `lib/sync`'s `irq` rustdoc
and the AGENTS.md §23.2 review checklist.

**T5 — docs.** `docs/src/architecture/syscalls.md` gained the "Interrupts
during a syscall" section; PLAN.md has the P-5b entry and the
Charter-Amendments line; the §15.18 jump-sheet has its row. The README
support matrix needs no per-arch mark change (preemption/interruptibility
is not a matrix row).

## Why this is safe — see "Why this is safe today" above

The lock-discipline argument (Invariants 1 & 2) is unchanged and is the
correctness basis: every ISR is lock-free (`request_wake`), and the one
ISR↔task-shared ring is IRQ-gated.

## Remaining (QEMU syscall-body verticals — staged, §2.19/§15.7)

The dedicated per-arch **syscall-body** QEMU verticals are staged, not yet
landed. Each mirrors `preempt_inkernel_qemu_aarch64` for the syscall path:
an EL0/U-mode/ring-3 task issues a syscall whose test dispatch shim
busy-loops until it observes a timer tick, asserting the tick is taken
*during* the body (IRQs deliverable in-kernel) while the EL0-preempt
callback fires **zero** times (kernel not preempted), and a latched tick
yields at return-to-user.

What already covers the behaviour today, so this staging is confirmation
rather than an unproven claim:

- the in-kernel IRQ-delivery + non-preemption property is proven directly
  by `preempt_inkernel_qemu_aarch64` (enabling device IRQs while an
  in-kernel task runs and taking a tick mid-run without rescheduling — the
  same mechanism the syscall body now uses);
- interrupt-return preemption / the need-resched latch is proven per arch
  by `preempt_el0_qemu_{aarch64,riscv64,x86_64}`;
- the arch-neutral return-to-user decision (drain + latched-tick /
  unparked-task yield) is host-tested via `completion_outcome_*` in
  `kernel/core`.

Landing the three dedicated syscall-body verticals is the outstanding
follow-up for full per-arch coverage of the IRQ-during-body property
specifically.

## Non-goals / do not do

- Do NOT add a per-syscall interruptibility flag or table (§2.3/§2.4).
- Do NOT make the kernel preemptible mid-syscall (§4 stands).
- Do NOT invent a second wake/drain discipline — reuse P-5's (§2.2).
- Do NOT widen the ABI beyond what the seam already exposes.
