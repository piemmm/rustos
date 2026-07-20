# FIX-SYSCALL — Run syscalls with interrupts enabled

Status: **planned**

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

## Work items

### T1 — Arch entry glue: enable IRQs on syscall entry, re-mask on exit
Do this behind the existing `KernelArch` seam so the shared
reschedule/drain logic stays one definition (§2.21). For each bare-metal
port, after the trampoline has saved the user register frame and
established a well-defined kernel context (kernel stack, `swapgs`/per-CPU,
and the §19.1 side-channel entry barrier — all already done), unmask
device IRQs; re-mask on the exit path before restoring the user frame.

- `kernel/arch/aarch64/src/exceptions.rs` (`tairix_aarch64_trap_handler` /
  `dispatch_svc` path): clear `DAIF.I` after frame save; set before `eret`.
- `kernel/arch/riscv64/src/trap.rs` (`ecall` path): set `sstatus.SIE`
  after frame save; clear before `sret`. Update the now-stale comments
  that assert "the kernel runs with `sstatus.SIE == 0`".
- `kernel/arch/x86_64/src/syscall_entry.rs`: `sti` once in a safe kernel
  context (keep `IA32_FMASK` clearing `IF`/`IOPL`/`AC`/etc. on *entry*;
  re-enable only `IF`); `cli` before `sysret`. Update the module doc.
- `kernel/arch/wasm32/*`: entry glue is a no-op (C2).

Keep every port's existing preemption gate on the interrupted privilege
(`from_el0` / saved `SPP` / `cs_is_ring3`) so an IRQ taken in
EL1/S-mode/ring-0 serves-and-returns without a mid-syscall switch.

### T2 — Deferred drain on the syscall return path
Once syscalls run with IRQs on, an IRQ landing mid-syscall latches a wake
via `request_wake`. Run `waitq::drain_pending_wakes` (and honour
`need_resched`) at a safe point on the **syscall return-to-user path**
(before `eret`/`sret`/`sysret`), not only in the dispatch loop. Reuse
P-5's machinery; do not invent a second discipline (§2.2).

### T3 — Close C1 (riscv64/x86_64 console RX)
Audit both console input paths. If either shares a plain-`SpinLock` ring
between an RX ISR and `stream_read`/`keyboard_read`, add the
`IrqSafeSpinLock` gate (the aarch64 `UART_RX_GATE` shape). Otherwise
document that the path is synchronous.

### T4 — Close C2 (wasm32) and C3 (standing rule)
- Verify the wasm32 no-op entry/exit and deferred-drain behaviour.
- Add the "ISR-shared lock must be IRQ-safe (or lock-free + deferred
  drain)" rule to the §23 review checklist and a rustdoc note in
  `lib/sync` (`irq.rs`).

### T5 — Docs + housekeeping
- Update `docs/src/architecture/syscalls.md` to state syscalls run with
  interrupts enabled, the kernel is non-preemptible, and reschedule
  happens at return-to-user.
- Add a `PLAN.md` entry (as the sibling of P-5) recording this as the
  syscall-entry half of the §17.1 fix, and a one-line "Charter
  Amendments" rationale if the review checklist rule (C3) is added.
- Add a row to the `AGENTS.md` §15.18 jump-sheet:
  `Syscall interruptibility / IRQ-on-entry → plans/FIX-SYSCALL.md`.
- Update the `README.md` support matrix only if a per-arch mark changes.

## Tests (mandatory — part of this change, §7)

Extend the §17.1 conformance / `preempt_*` verticals with a
**syscall-body** case, mirroring `preempt_inkernel_qemu_aarch64` for the
syscall path, on every bare-metal Tier-1 target:

- **Delivery-during-syscall.** A task enters a deliberately long syscall
  (a CPU-bound busy body, or a bootstrap-floor `fs_*` MMIO wait); assert a
  device IRQ / preemption tick is **taken during** the syscall (proving
  IRQs are enabled and deliverable in-kernel).
- **Non-preemptibility.** Assert the task is **not** rescheduled
  mid-syscall (no context switch while in EL1/S-mode/ring-0), and that a
  held lock / in-flight syscall is never abandoned.
- **Reschedule-at-return.** Assert a latched `need_resched` is honoured on
  return-to-user (the task yields the CPU at syscall exit, not before).
- **Wake timeliness.** A parked blocking syscall (`ipc_recv`/`wait`/
  `stream_read`) is woken via the lock-free `request_wake` → deferred
  drain while another CPU/ISR runs — no busy-poll (§2.23).
- **Console-RX no-deadlock (C1).** On each port, drive the RX ISR while a
  `stream_read`/`keyboard_read` holds the ring; assert no single-CPU
  self-deadlock (the gate must be present where the ring is ISR-shared).
- **wasm32 (C2).** The no-op entry/exit still satisfies the deferred-drain
  and reschedule-at-return semantics via the host yield facility.

Add any fuzzer/proptest find to the regression corpus (§19.6). A bug found
en route gets its own regression test (§7).

## Definition of done (§7, §15, §23)

- All work items T1–T5 complete; C1/C2/C3 closed (confirmed or gated).
- New syscall-body preemption/delivery tests pass on every Tier-1 target
  under QEMU; the §17.1 conformance suite is green.
- Whole-project gate green and quoted: `cargo fmt --all` (+ `--check`),
  `cargo xtask ci` (once), `cargo xtask fuzz --secs 5`, and
  `tools/ci/soak.sh both --secs 20`.
- §23 self-review verdict stated: security (entry hardening unchanged —
  still clear `IF`/barriers on entry, re-enable only `IF`; capability
  checks fail-closed and unchanged, §5.4/§19.1), multi-arch (§23.2 — one
  definition behind `KernelArch`, no `cfg` leakage), no dead code / no
  compat shims (§2.13/§2.14), docs updated in the same change.

## Non-goals / do not do

- Do NOT add a per-syscall interruptibility flag or table (§2.3/§2.4).
- Do NOT make the kernel preemptible mid-syscall (§4 stands).
- Do NOT invent a second wake/drain discipline — reuse P-5's (§2.2).
- Do NOT widen the ABI beyond what the seam already exposes.
