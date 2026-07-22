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
- **D2 — P-6: wait-queue §27 completeness rework — DONE.** The
  foundational primitive (`kernel/core/src/waitq.rs`) shipped as a thin
  slice; §27 required the complete primitive. Landed (three-index
  O(log n) `WaitSet` with a stated FIFO no-starvation discipline).
- **D3 — Hard-lockup watchdog parity** on x86_64 and riscv64 (aarch64
  is the only port with hard-lockup detection wired).
- **D4 — Latent §27 audit sweep — DONE.** The other foundational
  primitives (`lib/sync`, IPC/capability structures, allocators,
  `lib/collections`) were audited against §27. All are complete; `waitq`
  (D2) was the sole thin slice. One latent watch-item (the slab
  free-slot scan) is recorded and staged (not a live defect).
- **D5 — `mem-pin-migration` intermittent multi-vCPU-TCG stall — DONE.**
  Root-caused to a lost-wakeup in the vertical's own secondary-CPU idle
  loop and fixed structurally (not a load artifact, not a budget bump).
- **D6 — `docs-check` cross-crate rustdoc failure documenting
  `tairix-kernel` — NON-REPRODUCING.** Formerly a `cargo xtask ci`
  `docs-check` failure (`error[E0432]: unresolved import
  tairix_abi::driver::virtio_pci::virtio_pci_window_resource`); it does
  not reproduce on the pinned toolchain from a full `cargo clean`. Kept on
  record with its reproduction procedure in case it recurs.

These are **distinct in kind**: D1 finishes an interrupt-model fix, D2
and D4 are §27 foundational-completeness defects, D3 is an Arch-HAL
parity gap, D5 was a test-harness idle-loop lost-wakeup (fixed), and D6
is a rustdoc/docs-build failure. Do not collapse them into one change;
land each on its own whole-project-green gate (§7).

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

## D2 — P-6: wait-queue §27 completeness rework — DONE (host-proven)

**State:** landed. `kernel/core/src/waitq.rs`'s O(n) `Vec` wait set is
replaced by a three-index `WaitSet` (all `BTreeMap`, `const`-constructible
so the `static` queues keep `const fn new()`), meeting the §27 bar.

**Deliverables (§27 — the complete primitive, not new surface §27.4):**

- **D2.1 — real wait-set structure — DONE.** `by_task: BTreeMap<TaskId,
  Waiter>` gives O(log n) `register`/`deregister`/`wake_task` membership,
  and `order: BTreeMap<seq, TaskId>` (a monotonic arrival sequence) gives
  a *stated* FIFO first-come-first-served no-starvation discipline: the
  oldest `seq` is the head `wake_one`/`oldest_task` release, and a
  re-`register` keeps its `seq` so a looping waiter is never overtaken. No
  linear scan on the per-park path. (An `alloc`-only `BTreeMap` was chosen
  over an intrusive list because the latter needs per-task node storage in
  the scheduler — a far larger change for the same O(log n) removal and no
  `unsafe`.)
- **D2.2 — deadline-ordered structure — DONE.** `deadlines: BTreeMap<
  (deadline_ns, seq), TaskId>` holds only finite-deadline waiters, so
  `earliest_deadline` is O(log n) (the front key) and `sweep` visits only
  the already-expired prefix in deadline order — O(log n + woken), not a
  scan of every waiter per timer expiry. `nearest_timed_deadline` is a
  fixed-arity min over the five timed queues' O(log n) fronts.
- **D2.3 — `wake_one` path — DONE.** `wake_one` (FIFO head) and
  `wake_task` (addressed) are the single-target paths; `wake_all` is kept
  for genuine broadcast conditions only (cancellation, a shared latch
  resolving).
- **D2.4 — preserve P-5's discipline — DONE.** The lock-free ISR
  `request_wake` + deferred `drain_pending_wakes` shape is unchanged
  (§2.2); no second wake/drain path.
- **D2.5 — park sites re-audited — DONE.** Single-target events use
  `wake_task` (`CALL_WAITQ`/`SERVE_WAITQ`/`SIGNAL_INTAKE_WAITQ`); genuine
  broadcasts use `wake_all` (`CONSOLE`/`PROCWAIT`/`PIPE`/`HW_TREE`/
  `USERS_DB`/`APP_STORE`/`SEAT_INPUT`). The rework preserves each choice.

**Tests (§7/§23.4):** host tests cover FIFO wake order + re-register
position preservation, deadline ordering + expired-prefix sweep,
deregister across every index, the wake-one round-robin no-starvation
loop, and the unchanged lock-free `request_wake`/drain race. All 15
`waitq` tests green.

**Done:** `waitq.rs` meets the §27 bar with the above operations,
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

## D4 — Latent §27 audit sweep of foundational primitives — DONE

**State:** completed. Every foundational primitive `kernel/*`, `lib/*`,
and userland code builds on was read and judged against the §27 bar
(complete abstraction, right structure/complexity for §26 load,
fairness/ordering/wake-one where the abstraction implies it, no O(n) scan
on a load-bearing path). `waitq` (D2) was and remains the **only** thin
slice; every other primitive is §27-complete. One latent structural
watch-item (the slab free-slot scan) is recorded below — it is not a live
defect (its sole production caller uses one slot) and is staged, not
fixed in passing (D4.3).

**D4.1 — primitives enumerated and audited.** The full set below.

**D4.2/D4.3 — audit result (each primitive read, not assumed):**

| Primitive | Structure / complexity | Verdict |
|---|---|---|
| `lib/sync::SpinLock` / `IrqSafeSpinLock` | test-and-set acquire spin (charter's brief-hold carve-out); `new`/`try_lock`/`lock`/`is_locked`/`get_mut`/`into_inner`/guards | §27-complete |
| `lib/sync::McsLock` | canonical MCS queue lock — strict FIFO fairness, per-waiter local spin, O(1)/op | §27-complete (the fair lock the plain spinlock defers fairness to) |
| `lib/sync::RwLock` | writer-preference; stated fairness invariant (`pending_writers>0` blocks new readers) with a property test | §27-complete |
| `lib/sync::SeqLock` | read-mostly seqlock — `read`/`write`/`sequence`, retry-on-odd | §27-complete |
| `lib/sync::OnceCell` / `Once` | full once-init: `get`/`set`/`get_or_try_init`/`take`/`call_once`(+infallible), poison handling | §27-complete |
| `lib/sync::Epoch` | epoch-based reclamation — `register`/`pin`/`defer_free`/`defer`/`advance`; complete lifecycle | §27-complete |
| `lib/collections::BitSet256` | 4×u64; full set algebra + subset + popcount + ascending fused iter, all O(1) | §27-complete |
| `lib/caps::CapabilitySet` | 256-bit; full algebra + subset-enforcing `delegate` + `revoke` + wire round-trip; delegation-never-widens property-tested (§19.7) | §27-complete |
| `lib/caps::CapToken` | unforgeable token vocabulary (`token.rs`) | §27-complete |
| `kernel/ipc::PortRegistry` | `BTreeMap` endpoint + name indexes — O(log n) `lookup`/`resolve`/`register`/`unregister`; bulk `teardown_owned_by` O(n) only on process exit (not a hot path) | §27-complete |
| `kernel/ipc` `call`/`port`/`notify` | reply/mailbox/notification queues over the shared `waitq` wake/drain discipline (D2) | §27-complete |
| `lib/kalloc::FreeListAllocator` | coalescing address-sorted first-fit free list; growable/shrinkable via `HeapSource`; deterministic OOM (null, never panic) | §27-complete (first-fit is O(free-blocks); coalescing bounds the count — the standard general-purpose design, not a thin slice) |
| `lib/rt` heap | first-fit over a coalesced, address-sorted free-**span** list; growable `SpanStore` (§24.1/§25); realloc grow/shrink in place | §27-complete (same standard first-fit design; §25-bound) |
| `kernel/mem::Slab` | guard-page + tag-rotation + zero-on-free + double-free/dirty-slot hardened fixed-size slab | §27-complete for its use — **watch-item** below |

**Slab free-slot scan — recorded, staged, not fixed in passing (D4.3).**
`Slab::alloc` finds a free slot with an `O(slot_count)` linear scan of the
`in_use` bitmap rather than an `O(1)` free-index. This is **not a live
§27 defect**: the sole production constructor (`kernel/core/src/kthread.rs`
kthread-stack slab) uses `slot_count == 1`, so the scan is O(1) in
practice, and the slab's purpose is guard/tag hardening of small, few-slot
object classes, not a high-fan-out hot-path allocator. It is recorded as a
latent structural watch-item: **should a large-`slot_count` consumer ever
be introduced, `Slab` must first gain an O(1) free-slot index (a free-slot
stack/head) so the allocation hot path does not become O(n) under §26
load.** Staged as a `PLAN.md` note rather than reworked here, per D4.3 (do
not fix in passing; the abstraction is complete and correct for every
present caller).

**Done:** every enumerated foundational primitive audited against §27 and
confirmed complete (table above); the one latent structural concern (slab
free-slot scan) recorded and staged with its specific trigger; no other
thin-slice core found; no in-scope code fix was required (all present
callers are served correctly), so the sweep lands as the recorded audit.

---

## D5 — `mem-pin-migration` intermittent multi-vCPU-TCG stall — DONE

**Root cause.** A lost-wakeup in the vertical's *own* secondary-CPU idle
loop (`tests/integration/mem_pin_qemu_aarch64/src/kernel.rs`
`migration_secondary`), not the scheduler or the CI runner. The re-rolled
secondary loop parked on a bare `wfi` with IRQ taking **enabled** and
without re-checking the run queue: when a placement/reschedule IPI landed
in the window between `step` returning `Idle` and the `wfi`, the handler
took and acknowledged the SGI, so the following `wfi` then slept with a
just-readied task already on this CPU's run queue (`wake_from_parked`
enqueues *before* `send_ipi`). During the parent phase no further IPI is
sent, so the CPU slept indefinitely and the guest made no progress until
the wall-clock budget fired. It reproduces only under QEMU-TCG timing
jitter, hence the isolation-passes / full-matrix-stalls signature.

**Fix.** `migration_secondary`'s idle and paused branches now use the
same race-free park the production dispatch loop uses
(`kernel/core/src/init.rs` `run_dispatch_loop`): mask IRQ taking, drain
flagged wakes and re-check `Scheduler::has_ready_work(cpu)` (and the pause
flag) under the mask, `wfi` only if still genuinely idle, then re-enable —
so an IPI arriving in the check→park window stays pending-but-masked and
wakes the `wfi`. No budget bump, no retry.

**Regression coverage.** The mirrored protocol is guarded host-
deterministically by `run_dispatch_loop`'s
`idle_commit_rechecks_work_published_after_the_idle_step` unit test
(work published inside the masked idle-commit window must not let the
dispatcher sleep). A per-window micro-reproducer is not feasible for a
hardware-timing race; the fix removes the harness's divergence from that
tested protocol, and the vertical now runs it.

---

## D6 — `docs-check` cross-crate rustdoc failure documenting `tairix-kernel` — NON-REPRODUCING (monitoring)

**State:** does **not** reproduce on the pinned toolchain
(`nightly-2026-07-03`); `docs-check` is green. Kept on record — not
deleted — so that a recurrence has its prior context and its
reproduction procedure to hand.

**Prior symptom (historical).** `cargo xtask ci` → `docs-check`
(`cargo doc --workspace --no-deps --document-private-items
-Z rustdoc-mergeable-info`, `RUSTDOCFLAGS="-D warnings"`) was reported to
fail while documenting `tairix-kernel`:

```
error[E0432]: unresolved import
  tairix_abi::driver::virtio_pci::virtio_pci_window_resource
 --> kernel/tairix-kernel/src/hwdiscovery.rs:24:5
```

`virtio_pci_window_resource` is a real, unconditional `pub fn` in
`lib/abi/src/driver/virtio_pci.rs` (no `cfg`, no feature gate), so the
failure was a cross-crate rustdoc resolution issue under the unstable
`-Z rustdoc-mergeable-info` mergeable-info model, never a kernel-logic
defect.

**Reproduction attempted, could not reproduce.** The exact CI command was
run standalone — from a warm cache, from `cargo clean --doc`, and from a
full `cargo clean` (cold compile of every dependency's rmeta) — and each
run documented all 373 crates and merged cleanly (`cargo doc -p
tairix-kernel --no-deps` succeeds too). `cargo xtask docs-check`
(rustdoc + mdBook + link check) passes end to end. The host carries no
`sccache`/`RUSTC_WRAPPER` and no shared `CARGO_TARGET_DIR`, so this is not
a stale-cache artefact.

**If it recurs.** Treat it as a real cross-crate-rustdoc / mergeable-info
defect (not a load flake, §7): capture whether it appears only under the
concurrent `cargo xtask ci` static-gate group (memory pressure) vs.
standalone, and the structural fix is to drop `-Z rustdoc-mergeable-info`
from `run_docs_check` (`tools/xtask/src/commands.rs`) — the mergeable-info
model is a doc-build *speed* optimisation, and correctness of the doc
build takes precedence (§2.16). Do not reinstate the flag until the
resolution failure is root-caused in rustdoc.

---

## D7 — x86_64 disk-completion interrupt triple-faulted the boot — DONE

**State:** fixed. The live x86_64 disk bring-up now delivers the
virtio-blk-PCI completion interrupt, wakes the scheduler-parked bring-up
repeatedly, and mounts the read-only `/System` volume — proven by
`tests/integration/root_unlock_admission_qemu_x86_64` (keys PASS on
`root_mount::SYSTEM_VOLUME_MOUNTED_MESSAGE`).

**Root cause (two x86_64 defects, both fixed).** The symptom looked like
"the parked kthread never wakes" (the serial stalls with no prompt), but
the guest was actually **triple-faulting** (`qemu -d int`: a ring-3 `#PF`
storm, then a kernel `#PF` in `syscall_entry_stub` with `RSP=0` /
`CR2=-8`, → `#DF`). Two independent bugs:

1. **External-IRQ ISR read the interrupted CPU frame at the wrong stack
   offset.** `external_irq.s` pushes a synthetic *vector qword* between the
   15-GPR `SavedRegs` block and the CPU-pushed `InterruptStackFrame`, but
   the shared `preempt::preempt_ring3_if_pending` located the frame at
   `regs + size_of::<SavedRegs>()` — correct only for the *timer* stub,
   which pushes no vector qword. On a device IRQ it read the vector qword
   as the interrupted `CS`, mis-decided ring-3, and ran an unbalanced
   `swapgs`; a later `syscall` then loaded `kernel_rsp0` from the wrong GS
   base (0) → push into a null stack → `#DF`. Fix:
   `preempt_ring3_if_pending` now takes the `InterruptStackFrame` pointer,
   and each ISR computes it at its own offset (the external path adds
   `EXTERNAL_VECTOR_QWORD_BYTES`). Timer-driven preemption (used by
   `spawn_session_qemu_x86_64`, which passed) was unaffected, which is why
   only the disk (external-IRQ) path crashed. Host guard:
   `irq::tests::external_irq_frame_sits_one_vector_qword_above_saved_regs`.
2. **The MSI-X source shared an IO-APIC pin's vector.** `virtio_blk_unlock`
   reused the PCI interrupt-line GSI's vector for the device's MSI and
   drove that pin's *level* `IoApicController` for an *edge* MSI. Fixed by
   `kernel/tairix-kernel/src/x86_64/msi.rs`: a dedicated MSI vector +
   virtual `MSI_LINE_BASE` line space with an edge no-op
   `CompositeIrqController` (the Linux / aarch64-`MSI_LINE_BASE` model), so
   an MSI-X source is never bound to a shared IO-APIC pin. Boot pre-installs
   the free external vectors as MSI lines; `root_unlock` allocates a
   dedicated `(vector, line)`.

## D8 — x86_64 encrypted-root / users-DB read loop stalls the interactive unlock

**State:** open, uncovered by the D7 fix (the disk interrupt now works, so
boot proceeds far enough to hit this). **Not** an interrupt defect.

**Symptom.** With the disk interrupt fixed,
`root_unlock_admission_qemu_x86_64` reaches the `ARXFS passphrase:` prompt,
accepts the passphrase, and mounts the read-only `/System` volume, then
stalls **deterministically** (identical at 120 s and 300 s — not slowness)
before `USERS_DB_INSTALLED_MESSAGE`. Tracing shows the disk `notify_wait`
enters and *returns* thousands of times (5000+, each completion woken
cleanly — the interrupt path is solid) with **no** log-visible forward
progress: an unbounded disk-read loop in the encrypted-root / users-DB
read path (the interactive unlock kthread). `devmgr` is not the culprit
(it logs its catalogue-retry once and parks on `hw_tree_wait`).

**Not the observer path.** `root_unlock_login_qemu_x86_64` reads the same
users DB over the same fixture and PASSES, so the loop is specific to the
full-pipeline admission path (concurrency between the interactive-unlock
kthread and the driver-store serve kthread over the shared disk, and/or
the block/transform-cache behaviour under the production scheduler on a
256 MiB guest), not the users-DB read itself.

**What remains.** Root-cause the read loop (add a per-read LBA trace to
confirm same-block re-read vs. advancing thrash; audit the `block_cache` /
`transform_cache` and the two-kthread disk sharing under memory pressure),
fix it, then extend `root_unlock_admission_qemu_x86_64` to key PASS on the
users-DB install (`unlock_service::USERS_DB_INSTALLED_MESSAGE`) with the
`serial: &[("ARXFS passphrase: ", …, UNLOCK_PASSPHRASE_LINE)]` step
restored — the full kthread-admission witness the vertical is scoped to
reach once D8 is closed.

## Definition of done (whole plan, §7/§15/§23)

This umbrella is closed only when D1–D8 are each closed on their own
whole-project-green gate:

- D1: syscall-body verticals green on all bare-metal targets + wasm32
  confirmed + metal re-confirmed; FIX-SYSCALL marked done.
- D2: **DONE** — `waitq.rs` at the §27 bar (three-index O(log n)
  `WaitSet`, stated FIFO no-starvation) with tests; P-6 marked done.
- D3: hard-lockup watchdog + diagnostics conformance-tested on all three
  bare-metal targets.
- D4: **DONE** — every foundational primitive audited against §27
  (findings table recorded); all complete, the one latent structural
  concern (slab free-slot scan) staged; no in-scope code fix required.
- D5: **DONE** — the `mem-pin-migration` multi-vCPU-TCG stall root-caused
  to a lost-wakeup in the vertical's secondary idle loop and structurally
  fixed (production masked-park protocol); a full `cargo xtask ci` is
  whole-project-green.
- D6: **NON-REPRODUCING** — the cross-crate rustdoc `docs-check` failure
  documenting `tairix-kernel` does not reproduce on the pinned toolchain
  (verified from a full `cargo clean`); `cargo xtask ci` (`docs-check`
  included) is whole-project-green. Recorded with its reproduction
  procedure in case it recurs.
- D7: **DONE** — the x86_64 disk-completion-interrupt triple fault
  root-caused (external-IRQ frame offset + shared IO-APIC-pin MSI vector)
  and fixed; `root_unlock_admission_qemu_x86_64` reaches the `/System`
  mount over the dedicated MSI-X vector.
- D8: the x86_64 encrypted-root / users-DB read loop root-caused and
  fixed; `root_unlock_admission_qemu_x86_64` extended to key on the
  users-DB install.
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
