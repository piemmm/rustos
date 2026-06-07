# SPAWN.md — True concurrent userland multitasking + the process-spawn syscall

This is the staged build plan for `plans/PI.md` **P6d** done *properly*
(the standing direction, confirmed for this work): a real
`CAP_PROC_SPAWN`-gated `abi-v1` process-spawn syscall whose child is a
**genuinely separate, schedulable process**, not an `exec`-style control
hand-off. Reaching that requires real kernel-thread context switching
between EL0 tasks, which the kernel does not have yet — so the work is
staged.

`AGENTS.md` is binding — read it, `PLAN.md`, `plans/WIRING.md`, and
`plans/PI.md` first. Every rule in this file is binding too. One
fully-gated increment (one `SP`-stage) per landing.

**Note:** `abi-v1` is *not* frozen, despite what `AGENTS.md` / `PLAN.md`
say — the standing task direction supersedes that language. Adding the
spawn syscall changes a `lib/abi` type; it requires regenerating the C
header (`cargo xtask c-header --write`), which the drift guard enforces.

---

## 0. Scope and binding decisions

1. **Option B — true concurrent spawn.** `spawn` builds a new process in
   its own isolated address space (§4 — memory isolation is enforced by
   hardware), registers it as a runnable task, and returns its PID to the
   caller, which keeps running. The child runs when the scheduler next
   picks it. A control hand-off (`exec`/replace-the-caller) was explicitly
   rejected: it is not "correct for an OS" and would not survive a senior
   kernel review (§2.6).

2. **The gap this plan closes.** Today a user task is entered through a
   scheduler *closure body* that calls the arch `EnterUser::enter_user`
   (`eret`/`sret`/`iretq`) and **never returns**; EL0 runs with interrupts
   masked, so the only kernel re-entry is the `svc`/`ecall`/`syscall`
   trap, after which the arch vector returns to the **same** task in the
   **same** address space. There is no mechanism for the kernel to regain
   control from a running/blocked EL0 task and run a **different** one.
   That mechanism — resumable per-task kernel threads — is what SP1/SP2
   build; SP3 then adds the syscall that creates a second such task.

3. **The primitive already exists; it is not wired in.** The §17.2
   Arch-HAL **context-switch** slice
   (`rustos_arch_api::context::{ContextSwitch, TaskContext, TaskEntry,
   PrepareError}`) is implemented and conformance-tested on every
   bare-metal port (`kernel/arch/*/src/context_hal.rs`,
   `plans/WIRING.md` W5a/W7). `ContextSwitch::prepare` seeds a never-run
   task's first kernel-stack frame; `ContextSwitch::switch` performs the
   port's native callee-saved save/restore + kernel-stack swap. `switch`
   is exercised by the W7 `sched_drive_*` verticals as a bidirectional
   round-trip but is **not** on any production scheduling path. SP1 makes
   it one. **No new HAL trait is expected** for SP1–SP3 — the closed set
   (`ContextSwitch` + `EnterUser` + `Timer` + the SMP/IPI primitives) is
   sufficient; if a stage seems to need a new arch primitive, stop and
   extend `kernel/arch/api` deliberately (§17.2), never a `cfg(target_*)`
   fork (§2.2, enforced by `cargo xtask cfg-check`).

4. **The `SchedulerPolicy` closure contract is preserved (§2.4 / §17.1).**
   `SchedulerPolicy::spawn` takes a body `FnMut(&mut TaskContext) ->
   TaskAction`. A real kernel-thread task is layered *on top* of that
   contract, not by changing it: the body becomes a thin **kthread shim**
   owned by `kernel/core` that, on each dispatch, `switch`es into the
   task's own kernel stack and, when the task cooperatively switches back
   (yield/block/exit), returns the matching `TaskAction`. The scheduler
   policy crates (`kernel/sched/*`) are untouched. This keeps the §17.1
   modularity contract intact and avoids a frozen-interface change.

5. **Security is not relaxed for multitasking (§4 / §5).** Each spawned
   process gets its own hardware-isolated address space; the spawn gate is
   `CAP_PROC_SPAWN` (id 17, already defined), checked *before* any state
   is touched (§5.4) and audited (`ProcessSpawned`/`ProcessSpawnDenied`/
   `ProcessSpawnFailed` already exist). The child receives only the
   intersection of its `rxe` manifest request and its user's grants
   (§16.5); spawn authority does not widen the child's authority.

6. **Fail closed, no hacks (§2.1 / §2.9 / §5.4).** No
   `unwrap`/`expect`/`panic!` in production paths; every `unsafe` carries a
   `// SAFETY:` block and a test; a kernel stack that cannot be allocated,
   an unrunnable context, an absent program, or a malformed `rxe` fails the
   syscall with a stable `Errno`, never a panic or a half-built task.

7. **Two proving grounds.** Every stage that *can* be proven under QEMU is
   proven on `-M virt` (aarch64) and the sibling boards, mirroring the
   existing `*_qemu_*` verticals. There is still no usable `raspi*` QEMU
   vertical (`plans/PI.md` P2 lesson); the real Pi stays an on-metal
   acceptance item. The headless build (§17.3) and `virt` stay green
   throughout.

8. **Per-arch ordering.** aarch64 (the PI target) leads each stage; the
   x86_64 and riscv64 siblings follow so their `spawn_program_*` verticals
   keep passing and gain the second-task capability. wasm32 has no EL0/MMU
   process model — its multitasking is the W8 cooperative worker model and
   is an honest n/a for the EL0-isolation stages (declared, not faked).

---

## 1. Stages

### SP0 — Facts of record + design note `[x]`

- **Done.** Added `docs/src/architecture/multitasking.md` (linked from
  `docs/src/SUMMARY.md` after the scheduler page) recording the
  kthread-shim model (decision §0.4), per-task kernel-stack ownership +
  reclaim/UAF, the raw-pointer aliasing discipline across the switch, the
  fail-closed behaviour, and — as recorded decisions for the EL0 work — the
  trap-return reschedule decision point and the EL0 save-area layout. Pure
  documentation; landed together with SP1.

### SP1 — Kernel-thread task runtime in `kernel/core` `[x]`

**Done (all three bare-metal ports).** The `kernel/core::kthread` runtime
exists: `KernelStack` (+ `BoxStack`), the `Yielder`, the boxed
`ThreadControl`, the generic `trampoline`, the `dispatch_step` shim, and the
public `spawn_kthread` / `spawn_kthread_with_stack` layered over
`SchedulerPolicy::spawn` (the `kernel/sched/*` policy crates are untouched,
§2.4/§17.1). Seven host tests pass (first-run prepare-then-switch, prepare
happens once, fail-closed `Exit` on a bad stack, terminal `Exit`, the
`Yielder` action+switch, a live-`Scheduler` spawn+step smoke test, and the
slab-backed stack reclaim + `SlabError::TagMismatch` UAF check, §19.10). The
two-kthread ping-pong vertical is enrolled in `cargo xtask test --qemu` and
QEMU-green on **every** bare-metal port —
`tests/integration/kthread_switch_qemu_{aarch64,riscv64,x86_64}` — so two
kthreads ping-pong N times through the **real** `ContextSwitch::switch`,
making it a *production* scheduling path for the first time on each arch.
`cargo xtask ci`, `fuzz --secs 5`, the QEMU matrix, and the soak are all
green. wasm32 has no EL0/MMU model — n/a (declared, §0.9).

**Bug fixed in passing (x86_64):** the riscv64 + x86_64 verticals are the
*first* on-metal exercisers of each port's first-resume into a real Rust
trampoline (the W7 `sched_drive_*` round-trip never first-resumed, and
`context::conformance` is a host no-op). That surfaced a latent x86_64
`TaskCtx::prepare` bug: it seeded the first-run argument at the suspend
half's *push* offset, but `context.s` `popq %rdi` **first**, so the
trampoline entered with `rdi`(=the control-block pointer)`=0` → null deref;
the synthesised frame was also misaligned by 8 vs System V AMD64 §3.2.2.
`prepare` now seeds `arg` at frame offset 0 and adds a trailing 16-byte
alignment pad (`FRAME_BYTES` 64→72); the host layout test and the stale
`context.s` comment were corrected to match the real `popq` order (§2.2).

The foundation: make a scheduler task a *resumable kernel thread* with its
own kernel stack, driven through the existing `ContextSwitch` HAL, without
any new syscall and without yet touching EL0.

- A `kernel/core` kthread runtime: per-task owned kernel stack (guard-paged
  where the allocator supports it, §4) + a `TaskContext`, plus a **kthread
  shim** closure the runtime hands to `Scheduler::spawn`. On first
  dispatch the shim `prepare`s the task's first frame at a runtime
  trampoline and `switch`es into it; the trampoline runs the task's real
  work and, at each cooperative reschedule point, `switch`es back to the
  scheduler's (dispatch's) saved context, so the shim returns the right
  `TaskAction` (`Yield`/`Park`/`Exit`).
- Host-tested with a faithful `ContextSwitch` double (the same pattern as
  `context::conformance`), covering: first-run prepare, a yield round-trip,
  park/unpark, and exit (stack reclaimed, no use-after-free — exercised
  against the `kernel/mem` slab UAF tag-check, §19.10).
- One bare-metal QEMU vertical (aarch64 `-M virt` first) that builds the
  live `Scheduler`, spawns **two** kthreads that ping-pong via the real
  `ContextSwitch::switch`, and PASSes once both have run N times — making
  `ContextSwitch::switch` a *production* path for the first time.

**Done when:** the kthread runtime is host-green and the two-kthread switch
vertical is QEMU-green on at least aarch64; x86_64 + riscv64 siblings land
in the same stage or an immediately-following SP1 sibling increment so
every bare-metal port has the runtime.

### SP2 — EL0 tasks become resumable kernel threads `[x]`

Bring EL0 into the SP1 model so two **user** tasks can timeshare the CPU.
Like P6c, the work is too large for one safe landing, so it is staged
SP2a/SP2b/SP2c (one fully-gated increment each, §0 / DoD below).

The shape (decided, no backward compatibility required — the standing
direction): a user EL0 task becomes an SP1 kthread whose work diverges
into EL0 via `EnterUser::enter_user`. Because an EL0 `svc` traps onto
*that task's own* EL1 kernel stack (= its kthread stack), the trap path
can suspend the task with the ordinary `ContextSwitch::switch` back to the
scheduler's dispatch context and resume exactly there (re-`eret`) on the
next dispatch — **no separate EL0-frame save area and no new HAL trait**
(§0.3); the kthread's kernel stack already *is* the save area.

#### SP2a — core EL0-reschedule machinery (no EL0 yet) `[x]`

**Done (host-proven, arch-neutral).** Landed entirely in `kernel/core`
(+ the bin dispatch glue), with no syscall-semantics change yet:

- `dispatch_slot`: a new `DispatchOutcome::Reschedule { result, action,
  cpu }` plus a self-contained `RescheduleAction { Yield, Park, Exit }`
  (kept off `kernel/sched`'s vocabulary; mapped to `TaskAction` at one
  boundary, §2.2). Re-exported from the crate root.
- `kthread`: a `KTHREAD_MAX_CPUS`-sized per-CPU resume table
  (`SpinLock<Option<UserResumeHandle>>`, never cross-CPU contended), the
  `C,S`-monomorphised `suspend_thunk` (reuses `Yielder::suspend`, §2.2),
  and the public **`reschedule_current(cpu, action) -> bool`** the arch
  trap path calls — it lifts the handle out from under the lock *before*
  the suspending switch (no lock across the hand-off) and **fails closed**
  (`false`) for an unpublished/out-of-range CPU.
- `kthread`: a `pre_resume: Option<PreResume>` hook on `ThreadControl`
  whose `Some`-ness marks a **user** kthread; `dispatch_step` now takes
  `cpu`, runs `pre_resume` (the per-task address-space reactivation seam)
  and publishes the resume handle immediately before the switch-in, and
  clears it the instant the task switches back. New
  `spawn_user_kthread[_with_stack]` carry the hook; plain `spawn_kthread`
  is unchanged and never publishes.
- `rustos-kernel::dispatch_core::dispatch_via_slot`: the `Reschedule` arm
  (suspend via `reschedule_current`, then encode the result on resume).
- **Tests:** 6 new `kernel/core` host tests (publish→suspend records the
  `task→dispatch` switch + action; no-handle and out-of-range fail closed;
  `pre_resume` fires every step and the handle is cleared after; a kernel
  kthread never publishes; action mapping) + a bin `dispatch_core` test
  (Reschedule with no user task falls back to an encoded return). Whole-
  project DoD green: `cargo xtask ci` (incl. `test --qemu`), `fuzz
  --secs 5`, and `tools/ci/soak.sh both` all pass.

#### SP2b — aarch64: enter EL0 as a user kthread + wire the producer `[x]`

**Done.** PID 1 now reaches EL0 as a resumable user kthread, and the
producer is wired arch-neutrally:

- `kernel/arch/aarch64/src/paging.rs` gained `activate_user_root(root_phys:
  u64)` (MMU already on: `msr TTBR0_EL1` + `dsb`/`tlbi vmalle1`/`dsb`/`isb`),
  with a host no-op `cfg` arm. It takes only the `u64` root, so the
  `pre_resume` hook stays `Send`.
- `KernelArch` (`kernel/core`) grew an associated `type Cs: ContextSwitch +
  Copy + Send + 'static` + `context_switch()` accessor; all three bin
  wrappers (`Aarch64BinArch`, x86_64 `BinArch`, riscv64 `RiscvBinArch`) and
  the host `TestArch` (new `TestContextSwitch` double) implement it.
- `InitSpawnCtx::admit_init` now takes a boxed `pre_resume` hook alongside
  `enter` and admits PID 1 via `spawn_user_kthread` (own kernel stack);
  `kernel_main` drains the boot CPU's run queue until no task is live, then
  halts. The aarch64 `init_spawn` seam builds the `pre_resume` hook over the
  captured `init_root_phys` (`activate_user_root`).
- Producer: `KernelDispatchHook::dispatch` maps `yield`/`exit` to
  `DispatchOutcome::Reschedule { result, action, cpu }` (via the new
  `reschedule_action_for`); the `yield_now`/`exit` handlers no longer drive
  the scheduler (`exit` keeps only the IRQ/caps cleanup), reconciling the
  double-handling. The bin-side `dispatch_via_slot` already calls
  `reschedule_current`, so the aarch64 trap callback needed no change.
- Whole-project DoD green: `cargo fmt --all --check`, `cargo xtask ci`,
  `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both --secs 10`, and the
  full `cargo xtask test --qemu` matrix (49 verticals, incl.
  `spawn-init-qemu-aarch64` reaching EL0 through the production hook +
  producer and all three `kthread-switch` verticals).

#### SP2c — `-M virt` EL0↔EL0 timeshare vertical `[x]`

**Done.** The new `tests/integration/spawn_el0_timeshare_qemu_aarch64`
vertical (mirrors `kthread_switch_qemu_aarch64` + the `spawn_program` EL0
recipe) proves two EL0 user tasks timeshare one CPU under the live
scheduler on the `virt` board:

- A new pure-Rust EL0 fixture program `rustos-test-el0-yielder`
  (`tests/integration/el0_yielder_program`) links `rustos-rt` (§1) and
  yields `RUSTOS_EL0_YIELDS` times then exits 0. The new
  `rustos_rt::yield_now()` wrapper (over the existing `abi-v1` `yield`
  syscall) is its cooperative reschedule point; the vertical's `build.rs`
  owns the yield count, injecting it into the program build *and* emitting
  the matching `YIELDS_PER_TASK` constant the kernel asserts against, so the
  two halves can never disagree (§2.2).
- The freestanding test kernel reads the GICv2 base + timer rate from the
  embedded `virt` DTB (P3/P4), brings up the EL1 vectors + GICv2, and builds
  **two** hardware-isolated EL0 address spaces (two `PageTablePool`s, a
  shared monotonic `FRAME_POOL`, §4) from the same `rxe` through the
  production capability-checked, audited `spawn_image`. It admits each as a
  resumable user kthread via `spawn_user_kthread` — each task's `pre_resume`
  hook reactivates *its* page-table root before every switch-in — and drains
  the cooperative `step` loop. A dispatch callback maps each task's
  `yield`/`exit` `svc` to `reschedule_current`, suspending the running task
  back to the dispatcher exactly as the production bin callback does.
- PASS once both tasks yielded their full count (`2 × YIELDS_PER_TASK`) and
  exited (`live_task_count() == 0`); distinct failure finishers and the
  harness timeout keep it fail-loud (§7). **Verified green under QEMU on
  this host** (two `ProcessSpawned` into two isolated spaces → drain → PASS).
  Enrolled in `tools/xtask/src/commands/qemu_tests.rs`.

**Done when (SP2 overall):** two EL0 user tasks timeshare one CPU under
the live scheduler on aarch64 `-M virt`, each isolated; siblings follow;
`virt` and the headless build stay green. (SP2a is the host-proven core;
SP2b wires aarch64 EL0 + the producer; SP2c is the QEMU proof.) **SP2a +
SP2b + SP2c are landed, so SP2 is complete on aarch64; the x86_64 +
riscv64 sibling EL0 ports follow when their `spawn_program_*` verticals
gain the second-task capability (§0.8).**

### SP3 — `spawn` syscall + embedded-program registry `[ ]`

- `lib/abi`: new `SyscallNumber::SPAWN` (next free number, **12**) + its
  `SyscallSpec` row gated on `CapabilityId::PROC_SPAWN` (id 17, already
  defined), `audit: true`. Args: the program-path user pointer + length
  (and an argv/flags shape kept minimal and frozen-once-shipped, §2.4).
  Returns the new PID (or a stable `Errno`). Regenerate the C header
  (`cargo xtask c-header --write`); `abi-check` + `c-header` drift guards
  must pass. `ros_sys_spawn` stub follows the `ros_sys_*` convention (§9).
- `kernel/syscall`: the dispatch arm + the recomputed `SYSCALL_TABLE_HASH`;
  a `SyscallHandlers::spawn` trait method; table tests.
- An **embedded-program registry**: a capability-agnostic, path-keyed map
  of validated `rxe` programs the kernel can launch, threaded into
  `kernel/core` like the `ConsoleWrite` seam (a `BootInfo` field +
  builder; default empty → spawn of an unknown path fails closed with a
  stable `Errno`). The kernel binary registers the embedded `session`
  (and, for the vertical, a test child) program under its absolute path,
  via the host-only `elf2rxe` build glue already used for `init` (§2.2;
  RustOS stays Rust-only, §1).
- The `kernel/core` spawn handler: §5.4 capability check (the dispatcher
  already gates on `required_capability`; the handler re-asserts via the
  existing `spawn_image` caller), copy-in the path from the caller's
  address space, look it up in the registry, build a fresh isolated
  address space (`build_process_image` + freeze + register in the
  `AddressSpaceRegistry`), allocate a kernel stack + seed the EL0 frame
  (SP1/SP2 runtime), register the task **Ready** with caps = manifest ∩
  user grant, and return its PID. Every failure path frees what it built
  and returns a stable `Errno` (§2.9).
- `-M virt` vertical: PID 1 spawns an embedded child program; both run
  (proving SP2 timesharing), the child writes a banner + `exit`s, and the
  parent observes the child's PID. Host tests for the handler cover denial
  (no `CAP_PROC_SPAWN`), unknown path, bad pointer, malformed `rxe`, and
  the happy path.

**Done when:** a userland process can spawn a separate, isolated, runnable
process via `abi-v1` on aarch64 `-M virt`; siblings follow.

### SP4 — `init` launches the `session` program `[ ]` (folds into PI.md P6e)

- `init`'s startup config (`session <absolute-path>`, already parsed,
  `plans/PI.md` P6b) is launched through the SP3 spawn syscall as a
  separate process; `init` continues running (e.g. as the session
  supervisor) rather than being replaced. The minimal shell/`session`
  program itself is `plans/PI.md` P6e.

**Done when:** PID 1 `init` spawns the `session` process via the spawn
syscall and both run concurrently on `-M virt`; the real Pi is the
on-metal acceptance item.

---

## 2. Cross-cutting requirements (apply to every stage)

- **No new HAL trait unless deliberate (§17.2).** SP1–SP3 reuse the closed
  HAL set. A genuinely new arch primitive lands in `kernel/arch/api` with a
  conformance vertical for every port, never a `cfg(target_*)` fork
  (`cargo xtask cfg-check` stays clean; grandfather lists stay empty).
- **`SchedulerPolicy` is not changed (§2.4 / §17.1).** The kthread model is
  layered over the closure body; `kernel/sched/*` stays untouched.
- **Isolation + capabilities + audit (§4 / §5).** Each process gets its own
  hardware-isolated address space; spawn is `CAP_PROC_SPAWN`-gated,
  checked-before-state-touch, and audited with the existing
  `ProcessSpawn*` events.
- **Fail closed (§2.1 / §2.9).** No panics on the spawn/switch paths; every
  resource a failed spawn allocated is reclaimed.
- **`virt` + headless stay green.** Every change serves both boards as
  discovered/neutral data; the headless build (§17.3) excludes no kernel
  capability used here.

## 3. Definition of done (per stage, run over the whole project, never `-p`)

```
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all && cargo fmt --all --check
cargo xtask ci            # clippy -D warnings, deps-check, cfg-check, test matrix,
                          # docs-check, deny, c-header drift, proptest/fuzz --quick,
                          # model-check, spec-review, abi-check
cargo xtask fuzz --secs 5
tools/ci/soak.sh both --secs 10
```

The QEMU verticals are **not** in the host-only `cargo xtask ci` gate; run
the enrolled matrix separately (the real proof of the switch):

```
cargo xtask test --qemu
```

`cfg-check` / `deps-check` grandfather lists must stay empty. Any failure
found — new or pre-existing — is fixed or reverted before the increment is
done (§2.5 / §7). One increment per landing: finish the `SP`-stage, update
`PLAN.md` + `plans/PI.md` + this file, refresh
`.junie/next-pi-prompt.md`, then start the next.
