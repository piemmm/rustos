# OPEN-DEFECTS — Close the remaining open core-kernel defect classes

Status: **planned**

Binding under `AGENTS.md`. This plan is the single tracker for driving
the remaining open core-kernel defect classes to closure. It assumes
`plans/FIX-SYSCALL.md` is **substantially done** — syscalls run with
interrupts enabled on the bare-metal ports, the deferred drain runs on
the syscall return path, and the console-RX lock discipline (C1) is
closed — with only the residual per-arch validation verticals still
outstanding (D1 below). It supersedes no other plan; each defect keeps
its own detailed plan where one exists and this file is the umbrella.

Read first (§15.18): `plans/FIX-SYSCALL.md`, `plans/WATCHDOG.md`,
`plans/WIRING.md` (Arch HAL parity), `plans/ARCHSUPPORT.md`
(x86_64 product parity), `plans/CODEVERIFY.md` (the §27 sweep spirit),
`PLAN.md` immediate-work P-series (P-6 at ~line 2075).

## Scope

The open items, in priority order:

- **D1 — FIX-SYSCALL residual verticals** (x86_64/riscv64 syscall-body
  tests + metal re-confirmation). The design and code are done; the
  per-arch conformance verticals are not.
- **D2 — P-6: wait-queue §27 completeness rework.** A foundational
  primitive (`kernel/core/src/waitq.rs`) shipped as a thin slice; §27
  requires the complete primitive.
- **D3 — Hard-lockup watchdog parity** on x86_64 and riscv64 (aarch64
  is the only port with hard-lockup detection wired).
- **D4 — Latent §27 audit sweep** of the other foundational primitives
  (`lib/sync`, IPC/capability structures, allocators) to find any other
  thin-slice cores before they bite.

These are **distinct in kind**: D1 finishes an interrupt-model fix, D2
and D4 are §27 foundational-completeness defects, D3 is an Arch-HAL
parity gap. Do not collapse them into one change; land each on its own
whole-project-green gate (§7).

## Coupling to be aware of

D1 (FIX-SYSCALL) and D2 (P-6) ride the **same** `request_wake` /
`waitq::drain_pending_wakes` machinery. Whoever executes D2 must not
break the lock-free-ISR + deferred-drain shape the syscall return path
now depends on, and must re-audit exactly the park sites the syscall
path made interruptible (§2.2 — one discipline, not two). Sequence D1
before or alongside D2 where practical, and re-run the FIX-SYSCALL
verticals after D2 lands.

---

## D1 — Close the FIX-SYSCALL residual verticals

**State:** design + code done (T1–T5 of `plans/FIX-SYSCALL.md`); the
aarch64 syscall-body vertical passes. **Remaining:** the same vertical
on the other bare-metal targets, and metal re-confirmation.

- **D1.1 — x86_64 syscall-body vertical.** Port the aarch64
  `preempt`-style syscall-body test to x86_64 under QEMU: a task in a
  deliberately long syscall (a) has a device IRQ / preemption tick
  **taken during** the syscall (delivery), (b) is **not** rescheduled
  mid-syscall (non-preemptibility — IRQ in ring-0 serves-and-returns),
  and (c) is rescheduled at **return-to-user** when `need_resched` is
  latched. Include the wake-timeliness case (a parked blocking syscall
  woken via the lock-free drain).
- **D1.2 — riscv64 syscall-body vertical.** The same, under the riscv64
  QEMU target (`sstatus.SIE` enabled in-syscall, `sret` re-masks).
- **D1.3 — wasm32 (C2) confirmation.** Assert the no-op entry/exit still
  satisfies the deferred-drain + reschedule-at-return semantics via the
  host yield facility.
- **D1.4 — metal re-confirmation.** Re-confirm on Pi hardware that a
  long in-kernel syscall body no longer stalls the preemption tick /
  serial drain / input pump (the 2026-06-23 failure class). Record the
  metal checklist result; do not mark FIX-SYSCALL fully closed until it
  is confirmed.

**Done when:** the syscall-body vertical is green on every bare-metal
Tier-1 target under QEMU, wasm32 is confirmed, metal is re-confirmed,
and `plans/FIX-SYSCALL.md` is updated to done-state (§13) with its
`PLAN.md` sibling-of-P-5 entry finalised.

---

## D2 — P-6: wait-queue §27 completeness rework

**State:** staged as `PLAN.md` **P-6** (2026-07-06 §27 amendment).
`kernel/core/src/waitq.rs` is the thinnest slice P-2 needed: an O(n)
`Vec` wait set, `wake_all` as the only wake path (no `wake_one`), no
FIFO/priority ordering, no fairness/anti-starvation guarantee, and O(n)
`register`/`deregister`/`sweep`/`earliest_deadline` with
`nearest_timed_deadline` re-scanning every queue on every timer arm.

**Deliverables (§27 — the complete primitive, not new surface §27.4):**

- **D2.1 — real wait-set structure** with a *stated* FIFO
  fairness/ordering discipline and O(1) (or O(log n)) register /
  deregister — never a linear `Vec` scan on this load-bearing per-park
  path (§26 load, §24.1). Use an intrusive list for O(1) removal.
- **D2.2 — deadline-ordered structure** (min-heap or timer wheel) so the
  timed sweep and one-shot arming stop re-scanning every waiter;
  `nearest_timed_deadline` becomes O(1)/O(log n).
- **D2.3 — `wake_one` path** so a single-resource event (a
  `CallEndpoint` reply, one console byte) no longer thundering-herds
  every waiter; keep `wake_all` for genuine broadcast conditions
  (§27.3 — wake-one where a wake-all is a thundering herd).
- **D2.4 — preserve P-5's discipline.** The lock-free ISR `request_wake`
  + deferred `drain_pending_wakes` shape is retained unchanged (§2.2);
  do not invent a second wake/drain path.
- **D2.5 — re-audit every park site** (`ipc_recv`, `ipc_call`,
  `call_recv`/`call_reply`, `irq_wait`, `wait`/`waitset_wait`,
  `keyboard_read`, `stream_read`, `users_db_wait`, `hw_tree_wait`) for
  which wake primitive it should use (one vs all).

**Tests (§7/§23.4):** FIFO wake order; wake-one vs wake-all; deadline
ordering; no lost wake under concurrent `request_wake` + drain; the
no-starvation property under N×M load (mirror the §17.1 style). Any
fuzz/proptest find enters the regression corpus (§19.6).

**Done when:** `waitq.rs` meets the §27 bar with the above operations,
complexities, and stated fairness discipline; all park sites re-audited;
tests green; `PLAN.md` P-6 updated to done-state.

---

## D3 — Hard-lockup watchdog parity (x86_64, riscv64)

**State:** the soft-lockup detector is cross-arch; **hard-lockup**
detection (the non-maskable buddy cadence + `WatchdogArch`) is wired
only on aarch64 (virtual generic timer `CNTV`, PPI 27). x86_64 and
riscv64 keep only the soft detector and inherit hard detection once they
wire their own non-maskable cadence (`PLAN.md` ~2046, `plans/WATCHDOG.md`).

- **D3.1 — x86_64 hard-lockup cadence.** Wire a non-maskable liveness
  sample (NMI-driven cadence via the local APIC / HPET as the arch
  dictates) behind the existing `WatchdogArch` seam, feeding the
  arch-neutral buddy detector and `request_recovery` — no new surface,
  reuse the aarch64 shape (§2.21).
- **D3.2 — riscv64 hard-lockup cadence.** The same, using the riscv64
  non-maskable/high-priority timer facility.
- **D3.3 — `stuck_interrupt` parity.** Implement `WatchdogArch::
  stuck_interrupt` for each port (the aarch64 `gic::stuck_spi` analogue)
  so the `stuck_irq`/`stuck_state`/`stuck_owner` diagnostics are emitted
  on all bare-metal targets, not just aarch64.
- **D3.4 — conformance vertical.** Extend the `WatchdogArch` conformance
  suite (§17.2) with the hard-lockup case on x86_64 and riscv64 (a
  CPU wedged with IRQs masked is detected and a recovery attempted with
  its honest outcome logged, `CPU_LOCKUP_RECOVERY` 4084).

**Done when:** hard-lockup detection + recovery + stuck-line attribution
work and are conformance-tested on all three bare-metal targets;
`plans/WATCHDOG.md` and the README support matrix updated to match.

---

## D4 — Latent §27 audit sweep of foundational primitives

**State:** the 2026-07-06 §27 amendment was a **general** rule (kernel
building blocks had been shipped as thin slices); `waitq` (D2) is the
one concrete instance staged so far. Other foundational primitives may
carry the same defect and are not yet audited.

- **D4.1 — enumerate the foundational primitives** other code builds on:
  `lib/sync` (locks, `Once`, epoch, the `IrqSafeSpinLock` family), the
  IPC/capability structures (`kernel/ipc`, `lib/caps`), the allocators
  (`kernel/mem` slab, `lib/kalloc`, `lib/rt` heap), and the core
  collections in `lib/collections`.
- **D4.2 — audit each against §27** in the spirit of `plans/CODEVERIFY.md`:
  is the *complete* abstraction implemented, or the first caller's slice?
  Right data structure / complexity for §26 load? Fairness / ordering /
  wake-one where the abstraction implies it? No O(n) scan on a
  load-bearing path?
- **D4.3 — record findings.** Each primitive that falls short is staged
  as its own §27 rework item (a `PLAN.md` P-item and, if large, its own
  `plans/*.md`) with the specific gap named — do not silently fix in
  passing and do not defer with a `// TODO` (§2.18). A primitive that
  passes is recorded as audited so the sweep is not re-run blindly.

**Done when:** every enumerated primitive is audited and either confirmed
§27-complete or has a staged rework item; the audit result is recorded
(a short table in this file), and any in-scope small fixes land with
tests (§7).

---

## Definition of done (whole plan, §7/§15/§23)

This umbrella is closed only when D1–D4 are each closed on their own
whole-project-green gate:

- D1: syscall-body verticals green on all bare-metal targets + wasm32
  confirmed + metal re-confirmed; FIX-SYSCALL marked done.
- D2: `waitq.rs` at the §27 bar with tests; P-6 marked done.
- D3: hard-lockup watchdog + diagnostics conformance-tested on all three
  bare-metal targets.
- D4: every foundational primitive audited; shortfalls staged.
- For each landing: `cargo fmt --all` (+ `--check`), `cargo xtask ci`
  (once), `cargo xtask fuzz --secs 5`, and `tools/ci/soak.sh both
  --secs 20` green and quoted; §23 self-review verdict stated.
- Housekeeping: `PLAN.md` immediate-work list reflects the closures, the
  README support matrix updated where a per-arch mark changes, and a row
  added to the `AGENTS.md` §15.18 jump-sheet:
  `Open core-kernel defect tracking → plans/OPEN-DEFECTS.md`.

## Non-goals / do not do

- Do NOT re-open the settled FIX-SYSCALL design decisions (no per-syscall
  interruptibility flag §2.3/§2.4; kernel stays non-preemptible §4;
  reuse P-5's single wake/drain discipline §2.2).
- Do NOT collapse D1–D4 into one mega-change — each is a distinct defect
  class with its own gate.
- Do NOT grow the compiled-in surface or ABI beyond what each seam
  already exposes (§2.3/§2.4).
- Do NOT mark any item done on a green compile alone — the tests and the
  §23 gate are the bar.
