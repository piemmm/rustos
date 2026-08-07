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

SP0–SP4 are the P6d tranche (the spawn syscall + the multitasking it
needs). **SP5 is a follow-on beyond P6d**, kept in this file because it is
the natural next `abi-v1` process-runtime syscall and shares P6d's
precondition (a process running in its own isolated address space): a
dynamic per-process memory map/unmap (`brk`/`sbrk`/`mmap`-equivalent)
pair. It is scheduled after the spawn tranche, not before SP3b/SP4.

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
   (`tairix_arch_api::context::{ContextSwitch, TaskContext, TaskEntry,
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
- `cpu_state`: one fallibly allocated table sized from the validated
  scheduler CPU count, owning each CPU's resume handle, live address-space
  pointer, preemption latch, and preemption counter. Its set-once owned-slice
  publication gives interrupt paths O(1) lock-free lookup with no
  compile-time CPU ceiling; each resume/live-space slot remains independently
  locked and never cross-CPU contended. `kthread` supplies the
  `C,S`-monomorphised suspend thunks over one shared `suspend_with` body
  (reuses `Yielder::suspend`, §2.2) — `suspend_thunk_syscall` (brackets
  the port's cooperative-park convention, the user mid-handler path) and
  `suspend_thunk_body` (no bracket, a kernel kthread's own body) — and
  the public **`reschedule_current(cpu, action) -> bool`** the arch trap
  path and in-kernel blocking primitives call — it lifts the handle out
  from under the lock *before* the suspending switch (no lock across the
  hand-off) and **fails closed** (`false`) for an unpublished/out-of-range
  CPU (the pre-dispatch boot flow, a host test).
- `kthread`: a `pre_resume: Option<PreResume>` hook on `ThreadControl`
  whose `Some`-ness marks a **user** kthread; `dispatch_step` now takes
  `cpu`, runs `pre_resume` (the per-task address-space reactivation seam)
  and publishes the resume handle immediately before the switch-in — for
  **every** kthread: user tasks with the syscall thunk, kernel kthreads
  with the body thunk — and clears it the instant the task switches back.
  Kernel kthreads being suspendable is load-bearing: a kthread contending
  on a `SleepLock` whose holder parked across a block-device completion
  wait parks too instead of spinning in-kernel and starving the dispatch
  loop. New `spawn_user_kthread[_with_stack]` carry the hook.
- Known follow-up: a kthread stack-guard violation detected at
  `dispatch_step`'s switch-back still fails the task closed **silently**
  (no log — the dispatcher has no sink seam); the termination must reach
  the system log per the fail-loud rule once a logging seam exists there.
- `tairix-kernel::dispatch_core::dispatch_via_slot`: the `Reschedule` arm
  (suspend via `reschedule_current`, then encode the result on resume).
- **Tests:** `kernel/core` host tests (publish→suspend records the
  `task→dispatch` switch + action; no-handle and out-of-range fail closed;
  `pre_resume` fires every step and the handle is cleared after; a kernel
  kthread publishes a body handle for its step and it is cleared after;
  a body suspend skips the cooperative-park bracket while a syscall
  suspend enters and leaves it; action mapping) + a bin `dispatch_core`
  test (Reschedule with no user task falls back to an encoded return).

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

- A new pure-Rust EL0 fixture program `tairix-test-el0-yielder`
  (`tests/integration/el0_yielder_program`) links `tairix-rt` (§1) and
  yields `TAIRIX_EL0_YIELDS` times then exits 0. The new
  `tairix_rt::yield_now()` wrapper (over the existing `abi-v1` `yield`
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

### SP3 — `spawn` syscall + embedded-program registry `[x]`

Too large for one safe landing, so staged SP3a (the ABI surface + the
fail-closed handler/seam, host-proven) and SP3b (the arch producer +
registry population + the `-M virt` vertical), mirroring the P6a→P6c
`console_write` precedent (`abi-v1` syscall + fail-closed `NULL_*` seam
landed first; the real device/producer wired in a following increment).
**Both SP3a and SP3b are landed.**

#### SP3a — `spawn` syscall #12 + fail-closed handler/seam `[x]`

**Done (host-proven).**

- `lib/abi`: `SyscallNumber::SPAWN` (**12**) + its `SyscallSpec` row gated
  on `CapabilityId::PROC_SPAWN` (id 17), `audit: true`. Args: program-path
  user pointer + length; returns the new PID (`U64`) or a stable `Errno`.
  Frozen-number + frozen-capability tests added.
- `lib/abi-sys`: the `tairix_sys_spawn` C stub (`#[export_name]`, panic-free,
  `AGENTS.md` §9) + the drift-registry row + a marshalling test. C header
  regenerated (`cargo xtask c-header --write` → `TAIRIX_SYS_SPAWN 12u`,
  `uint64_t tairix_sys_spawn(void*, uintptr_t)`); `abi-check` + `c-header`
  drift guards green. `SYSCALL_TABLE_HASH` recomputed (`791b08…`).
- `kernel/syscall`: `SyscallHandlers::spawn` trait method + the
  `SyscallNumber::SPAWN` dispatch arm + `MockHandlers::spawn` + the
  reachability test's `CAP_PROC_SPAWN` grant.
- `kernel/core`: the capability-agnostic, path-keyed `ProgramRegistry`
  (path → validated `rxe`; default `EMPTY_PROGRAM_REGISTRY`) + the arch
  `ProcessSpawn` seam (`spawn(rxe, &dyn SpawnCtx) -> Result<u64, Errno>`;
  default fail-closed `NULL_PROCESS_SPAWN` → `NotImplemented`) + the
  core-side `SpawnCtx` (`frames`/`audit`/`admit_process`). The `spawn`
  handler copies the path in through the validated `copy_from_user`
  boundary, resolves it, and delegates to the producer; `SpawnCtx`'s
  `HandlerSpawnCtx` impl admits the child **parked**
  (`spawn_user_kthread*(…, parked = true)`), registers its caps + frozen
  address space + streams + limits + grants under the returned id, and only
  then `unpark`s it — so on an SMP machine no core can dispatch the child
  and take its first syscall before that per-task state exists (a Ready
  admission raced the installs and the child's first syscall found no
  capability record). It returns the PID **without** entering/draining (the
  caller keeps running). Handler fields default in `new`, with `with_frames` /
  `with_spawn` builders (mirroring `with_consoles`), so the kernel binary
  needs no change yet and production `spawn` fails closed with
  `NotImplemented`.
- Host tests (8 new): happy path through a host `ProcessSpawn` double that
  builds a frozen host space + calls `admit_process` (asserts the child is
  admitted + caps/aspace registered + PID returned); no-producer →
  `NotImplemented`; no-frames → `NotImplemented`; unknown path →
  `NotFound`; bad pointer → `BadAddress`; empty/over-long path →
  `NotFound`; dispatcher denial without `CAP_PROC_SPAWN`. `docs/src`
  syscall table + handler-wiring + capability matrix updated.

#### SP3b — aarch64 `ProcessSpawn` producer + registry + `-M virt` vertical `[x]`

**Landed.** The real aarch64 `ProcessSpawn` producer lives in the kernel
binary (`kernel/tairix-kernel/src/spawn_producer.rs`, sibling of
`init_spawn`): it draws the child's stage-1 page tables from the kernel's
live `FrameAllocator` through a boot-cached `kernel/mem` `FrameTableSource`
(the §24.1 allocator-backed source — no fixed `MAX_SPAWNS` reserve, so the
spawn capacity scales with discovered RAM and fails closed `NoSpace` only on
genuine OOM), builds a fresh isolated user space identity-mapping the
mask-derived window (`paging::configured_identity_gigapages` — 2 GiB on
`virt`, 4 GiB on the Pi 4) **without**
switching `TTBR0_EL1` (the
spawning caller keeps running — the build writes through the identity
`physmap`, so the child space need not be active), parses the `rxe` against
`SYSCALL_TABLE_HASH`, calls `spawn_image` (re-asserts `CAP_PROC_SPAWN`,
audits), freezes, builds the `pre_resume`/`enter` closures, and calls
`ctx.admit_process`. `boot_aarch64` wires `.with_spawn(&spawn_layout::PROGRAM_REGISTRY,
&AARCH64_PROCESS_SPAWN)` (the registry is the one shared `spawn_layout`
definition every port installs, §2.2); `kernel_core` threads
`programs`/`spawn_service` +
`&state.frame_allocator` through `BootInfo::with_spawn` → `run_phases` →
`KernelDispatchHook::new` → `with_frames`/`with_spawn`. The kernel `build.rs`
was generalised to build **both** `init` and the `Shell` session program
through one `elf2rxe` helper (§2.2; TAIRiX stays Rust-only, §1), embedding
`SHELL_RXE` registered under `/System/Commands/elsh.app/Run`. `AdmitError`,
build, and parse failures map onto stable `Errno`s (`NoSpace` /
`AlreadyExists` / `PermissionDenied` / `BadMagic`); page-table and image
frames are handed out monotonically (not reclaimed this stage), so a failed
spawn leaks nothing user-visible (§2.9).
- `-M virt` vertical landed: `tairix-test-spawn-session-qemu-aarch64` boots
  the production pipeline, PID 1 `init` spawns `/System/Commands/elsh.app/Run` through
  the `spawn` syscall, both run (proving SP2 timesharing), the session writes
  a gated banner + `exit`s, and `init` observes the returned PID. PASS keys
  on two `ProcessSpawned` (4030) + three audited syscalls (5000) — `init`'s
  `spawn`, `init`'s `exit`, and the session's necessarily-last gated `exit`.

**Done when (SP3 overall):** a userland process can spawn a separate,
isolated, runnable process via `abi-v1` on aarch64 `-M virt`; siblings
follow. **SP3a + SP3b are landed.**

### SP4 — `init` launches the `session` program `[x]` (folds into PI.md P6e)

**Landed alongside SP3b.** `init`'s startup config (`session
/System/Commands/elsh.app/Run`, already parsed, `plans/PI.md` P6b) is now launched
through the SP3 `spawn` syscall (`tairix_rt::spawn`) as a separate, isolated
process; `init` keeps running when a spawn is refused — the refusal is
reported on `stderr` (`Sessions::report_launch_failure`) and that entry's
slot abandoned while the remaining entries boot on (§2.24) — rather than
being replaced. `init`'s effective set gained
`CAP_PROC_SPAWN`; the child receives only its own stream authority,
`{CAP_CONSOLE_WRITE, CAP_CONSOLE_READ}` (no ambient authority, §4). The
minimal `session` program is a banner+exit `Run` stub in the `Shell`
bundle; growing it into a real shell REPL (wiring
in the `tairix-elsh` interpreter library) is `plans/PI.md` P6e. Per the
binding §20 stream model, the REPL does its text I/O over its **inherited
standard streams (fd 0/1/2/3 — `stdin`/`stdout`/`stderr`/`stdinfo`)**, never
the kernel-discovered console (`console_*` is only the bootstrap stream
*backing*, AGENTS.md §20). Supervising the session across its lifetime
(restart, reap) is also P6e.

**Done when:** PID 1 `init` spawns the `session` process via the spawn
syscall and both run concurrently on `-M virt` — proven by the SP3b vertical.
The real Pi is the on-metal acceptance item.

### SP5 — `mem_map`/`mem_unmap`: dynamic per-process anonymous memory `[x]` (beyond P6d)

**SP5-0, SP5a, SP5b-1, and SP5b-2 are all landed.** The `abi-v1`
surface, the C-callable stubs + generated header, the dispatcher arms, the
fail-closed `kernel/core` seam, the reusable `kernel/mem` live-address-
space producer, **and** the aarch64 `-M virt` EL0 vertical that wires the
producer through the `kernel/core` `MemMap` seam are all proven. The
sibling riscv64 **and x86_64** `-M virt`/QEMU verticals are now landed too
(the x86_64 one also closed a production ring-3 fault-delivery gap — boot
now installs the dedicated `#PF` entry and `TSS.RSP0`). The arch-neutral
**live-address-space retention mechanism** the production `mem_map` producer
and the `mmio_map` facility mutate a running task's own space through is now
landed (`plans/PI.md` 5d-0-ii (b′)-1): `kernel/mem::live`
(`LiveUserSpace`/`LiveSpace`), the per-CPU `USER_LIVE_SPACE` publication +
`with_current_live_space` accessor + `spawn_user_kthread_with_stack_live`
admission entry in `kernel/core::kthread`, and the `LiveMemMap`/`LiveMmioMap`
producers (`kernel/core::live_producer`) — all host-proven. The remaining
follow-on is wiring it into production per arch (the `admit_*` seam threading
the live space, the aarch64 `spawn_producer`/`init_spawn` building a
`LiveSpace`, the boot install, the `-M virt` vertical) plus the non-`FIXED`
per-task user-VA placement allocator (`plans/PI.md` 5d-0-ii (b′)-2).

The natural follow-on abi-v1 process-runtime capability, scheduled here
after the spawn tranche because it has the same precondition: a process
running in its **own** hardware-isolated address space (SP2/SP3). Today a
spawned process gets exactly its fixed spawn-time image (code/data/bss + a
fixed `UserStack`) and `abi-v1` has **no** `brk`/`sbrk`/`mmap`-equivalent,
so a process cannot obtain a heap or any additional pages at runtime —
its memory is bounded only by that static image. SP5 closes that gap with
a modern anonymous-memory **map/unmap** pair.

**Binding decisions (settle the open ones in the SP5-0 design note,
§15.2 — do not invent the capability/ABI before then):**

1. **Clean-slate shape, no legacy single break (§2.13).** TAIRiX has no
   installed base, so SP5 adds an `mmap`-style *anonymous region* map +
   unmap, **not** a `brk`/`sbrk` single-heap-break model. The libc/heap
   allocator a program links (`lib/rt`, future) layers its `malloc` over
   this pair; the kernel ABI is the region primitive only (§2.3 — no
   convenience surface in the kernel).
2. **W^X, RW only (§19.2).** `mem_map` returns `RW` anonymous pages and
   **never** `RWX`. An executable (JIT) mapping is a *separate, later*
   `CAP_JIT_MAP_EXEC`-gated `mprotect`-equivalent RW→RX flip — explicitly
   **not** bundled into SP5 (§2.4 — no interface creep). SP5 does not add
   `mprotect`.
3. **Per-process, never global (§4).** A region is mapped only into the
   **caller's own** isolated address space. No cross-process mapping;
   shared memory stays the capability-checked IPC object (§4). There is no
   global user heap.
4. **Deterministic OOM, no artificial limit (§4 / §2.9).** A frame- or
   page-table-allocation failure returns a stable `Errno` (`OutOfMemory`),
   never a panic. Consistent with the standing position, SP5 adds **no**
   per-process memory quota/`rlimit`; a process is bounded only by
   available physical frames.
5. **Zero on map and on free (§4 — secret hygiene).** Pages handed to
   userland are zeroed before the mapping is visible (no stale kernel /
   other-process bytes); `mem_unmap` zeroes-on-free the frames it reclaims
   (the existing zero-on-free guarantee extends to user anonymous memory).
6. **Capability gating — open question for SP5-0.** Ordinary anonymous
   `RW` growth of one's *own* address space is the candidate **unprivileged
   baseline** (mirroring §16.6 "list my own processes" needing no
   capability), but whether a capability is required is decided in the
   design note *before* the `SyscallSpec` row is written; the decision is
   recorded there, not pre-empted here (§5.4 still applies — checked
   before state, fail closed).

**The kernel gap SP5b must close.** Post-spawn an address space is captured
as an **immutable** `FrozenAddressSpace` snapshot (P6c-3 follow-up), so the
copy/permission path can read it from the `Send+Sync` registry. SP5 needs
the *running* user space to be **mutable** at runtime: `kernel/mem` grows a
capability to map fresh zeroed frames into a live user address space (and
unmap + TLB-shootdown via the §17.2 `TlbShootdown` / `CrossCpuTlbShootdown`
HAL slices), keeping the registry view consistent — never a second
parallel address-space model (§2.2).

**Staging (mirrors the SP3a→SP3b precedent — one fully-gated increment per
landing):**

- **SP5-0 — design note `[x]`.** Extend `docs/src/architecture/` (the
  syscall + multitasking pages) with the map/unmap ABI shape (`mem_map`:
  length + flags + optional addr hint → base `U64` or `Errno`; `mem_unmap`:
  base + length → `Errno`), the live-address-space-mutation + TLB-shootdown
  design, the W^X and zero-on-map/free invariants, the OOM-as-`Result`
  contract, and the resolved capability-gating decision. Pure docs; lands
  with SP5a. **Landed:** `docs/src/architecture/memory.md` §7c is the design
  note and `syscalls.md` carries the ABI/handler rows. **Capability
  decision: unprivileged** — growing one's *own* isolated address space
  grants no authority over anything else (§16.6 "list my own processes").
- **SP5a — abi-v1 surface + fail-closed seam (host-proven) `[x]`.**
  `lib/abi`: `SyscallNumber::MEM_MAP` (**14**) + `MEM_UNMAP` (**15**) +
  their `SyscallSpec` rows (capability per SP5-0), with frozen-number tests.
  (Numbers are **14/15**, not the 13/14 sketched below: `STREAM_READ` took
  13. `MapFlags` carries a `FIXED` bit; `addr_hint` is advisory unless
  `FIXED`. `Errno::OutOfMemory` = **20** was appended.)
  `lib/abi-sys`: the `tairix_sys_mem_map` / `tairix_sys_mem_unmap` C stubs
  (`#[export_name]`, panic-free) + drift-registry rows + marshalling tests;
  regenerate the C header (`cargo xtask c-header --write`) and recompute
  `SYSCALL_TABLE_HASH`; `abi-check` + `c-header` drift guards green.
  `kernel/syscall`: the two `SyscallHandlers` trait methods + dispatch arms
  + `MockHandlers` impls + reachability tests. `kernel/core`: a fail-closed
  arch-neutral seam (default → `NotImplemented`, mirroring `NULL_CONSOLE` /
  `NULL_PROCESS_SPAWN`), with `with_*` builders so the kernel binary needs
  no change yet. Host-tested (validation, fail-closed, no-producer paths).
  **Landed:** both syscalls are **unprivileged + unaudited**; the handler
  rejects a zero `len` (`LengthOutOfRange`) and a reserved flag bit
  (`OutOfRange`); the seam is `kernel/core`'s `MemMap` (`NULL_MEM_MAP` /
  `with_mem_map`), so the kernel binary is unchanged and production
  `mem_map`/`mem_unmap` fail closed with `NotImplemented` until SP5b.
- **SP5b-1 — reusable `kernel/mem` producer (host-proven) `[x]`.** The
  architecture-neutral `kernel/mem::anon` module: `map_anonymous` maps fresh
  `RW|USER` zeroed frames into a live `AddressSpace<P>` (the single
  `ANON_FLAGS` set — never executable, W^X §19.2), zeroing each frame
  through the kernel direct map before the mapping is visible, and unwinds
  every page already mapped if a later page cannot be backed (fail-closed,
  all-or-nothing, §2.9). `reserve_anonymous` (placed) / `reserve_anonymous_at`
  (`FIXED`) reserve address space with no commit — the demand-paged map
  path that `mem_map` now uses, so a large mapping never eager-commits.
  `unmap_anonymous` **sparsely** tears the region down — zeroing every
  reclaimed frame on free (§4) and skipping the pages the fault path never
  backed (the caller validates the reservation first) — and folds an
  allocator exhaustion onto one OOM type. The per-page
  TLB flush rides the existing `AddressSpace::map`/`unmap` (`TlbShootdown`
  slice). Host-proven over `HostPageTable` + `SimPhysMap`.
- **SP5b-2 — `-M virt` EL0 vertical `[x]`.** **Landed.** The SP5b-1
  producer is wired through the `kernel/core` `MemMap` seam in a
  self-contained aarch64 `-M virt` vertical
  (`tests/integration/mem_map_qemu_aarch64`): the test builds one isolated
  EL0 space with `spawn_image`, **retains** it live behind a `MemMap`
  producer (an `UnsafeCell<Option<LiveSpace>>` reached only from the
  single-CPU cooperative dispatch path) backed by `map_anonymous` /
  `unmap_anonymous` over its frame pool + `DirectPhysMap`, admits the
  program as a resumable user kthread, and routes the program's `mem_map` /
  `mem_unmap` `svc`s through the producer. The pure-Rust EL0 fixture
  (`tests/integration/mem_map_program`, linking the new `tairix_rt::mem_map`
  / `mem_unmap` wrappers) `mem_map`s a region (FIXED), writes and reads back
  a pattern, `mem_unmap`s it, then touches the released range; the fault
  handler reports the use-after-unmap data abort as PASS (id 4282), and a
  verification failure exits early with a distinct finisher (fail-loud,
  `AGENTS.md` §7). Verified green under QEMU on `-M virt`. The **riscv64
  sibling** (`tests/integration/mem_map_qemu_riscv64`) is now landed: it
  reuses the same pure-Rust `mem_map_program` fixture and the same SP5b-1
  `kernel/mem` producer (its own `AnonProducer` over an Sv39 U-mode space),
  but — having a single task that only direct-returns from its `ecall`s —
  drops into the program through `spawn_image` + a direct
  `EnterUser::enter_user` rather than the scheduler, keeping the riscv64
  cooperative-switch trap-save path off the critical path; its fault handler
  reports the use-after-unmap page fault as PASS on `-M virt` (ids 4284–4287,
  verified green on this host). The **x86_64 sibling**
  (`tests/integration/mem_map_qemu_x86_64`) is now landed: it reuses the same
  pure-Rust `mem_map_program` fixture and SP5b-1 producer (its own
  `AnonProducer` over an x86_64 four-level space), but — needing the GDT
  ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry — boots the
  production `tairix-kernel` pipeline (like `spawn_program_qemu_x86_64`), then
  `iretq`s into the program through `EnterUser::enter_user`; its `fault`
  observer reports the use-after-unmap `#PF` as PASS on the QEMU boot
  (ids 4274–4277, verified green on this host). Reaching it required closing a
  **production x86_64 gap**: a ring-3 CPU exception (or a hardware IRQ that
  preempts a user task) is delivered through the IDT, for which the CPU loads
  `TSS.RSP0` — but boot only ever set the *syscall* entry stack (loaded via
  `swapgs`), leaving `TSS.RSP0 = 0`, so a user fault's frame-push faulted and
  escalated to `#DF` (the trap was *undeliverable*, a security gap, §2.9/§2.17).
  Boot now installs the dedicated, error-code-aware `#PF` entry
  (`tairix_arch_x86_64::fault`, the x86_64 analogue of the riscv64/aarch64
  `fault` hooks) **and** programs `TSS.RSP0` (`percpu::install_tss_rsp0`, reusing
  the one shared stack-pivot validator `validate_kernel_rsp0`, §2.2) to the
  same already-mapped per-CPU kernel stack the syscall path uses, bringing
  x86_64 to the ring-3 fault-delivery parity the other ports already had.
  Production per-task live-space retention is wired on **every** bare-metal
  port: each port's `init_spawn` (PID 1) and `spawn_producer` (the `spawn`
  syscall's children) retains the just-built arch space as a `LiveSpace`
  behind the `LiveUserSpace` boundary and admits the task with it, so a
  production EL0/ring-3/U-mode process's `mem_map` / `mmio_map` / `dma_alloc`
  mutate its own space through the `live_producers` per-CPU slot (the shared
  MMIO/ANON/DMA window offsets live once in `spawn_layout`, §2.2; the stack
  and startup-block placement is *derived per spawn* from the image's mapped
  top with guard gaps — `tairix_kernel_mem::derive_user_layout` bound by
  `spawn_layout::user_layout`, one definition for every port — so an image
  of any size below the device window spawns, never capped by a fixed
  offset). wasm32's linear-memory model is an honest n/a (declared).

**Done when (SP5 overall):** an EL0 process can obtain and release anonymous
`RW` memory at runtime via `abi-v1` on aarch64 `-M virt`, zeroed on map and
on free, OOM surfaced as an `Errno`; the immutable-`FrozenAddressSpace` gap
is closed by a single live-space mutation path (§2.2); `virt` + the headless
build stay green.

**Anonymous memory is demand-paged (landed).** `mem_map` reserves address
space only and each page faults in lazily on first touch
(`resolve_anon_fault` backs one zeroed reserve-gated `RW|USER` page,
recorded via `AddressSpaceRegistry::record_anon_region`; `mem_unmap`
validates the reservation and sparsely reclaims). This replaced the eager
per-`mem_map` commit, whose single non-preemptible zeroing loop over a large
region monopolised the CPU under `stress --vm` and starved interrupts
(`plans/STRESSTEST.md`; the fix keeps per-fault work to one page so the task
stays preemptible between faults). The copy path (`copy_in_user`) offers a
staging miss to the same resolver.

### SP6 — `wait`: reap a child + read its exit code `[x]` (beyond P6d)

The process-lifecycle counterpart of `spawn`: a parent blocks until one of
its children exits, reaps the zombie, and reads back the child's exit code.
It is the missing prerequisite for **P6e-3b** — both the shell's foreground
job control (`ProcessHost::wait`) and PID 1 `init` supervising the session
(reap, restart) need it — and `spawn` today is a spawn-and-forget with no way
to observe a child's exit. Staged like SP5 (one fully-gated increment per
landing).

**Binding decisions:**

1. **`waitpid`-style, not a global reaper (§2.13).** `wait(pid: i32,
   status: *mut i32) -> i64`: `pid` selects a specific child or
   `tairix_abi::WAIT_PID_ANY` (`-1`) for any child; on success the kernel writes
   the child's exit code to `status` and returns the reaped child's PID
   (`< 0` is `-errno`, the standard signed-result convention).
2. **Own children only (§4 / §16.6).** A process may only reap children it
   spawned, so `wait` grants no authority over any other principal and needs
   **no capability** — the same unprivileged baseline as `mem_map` and "list
   my own processes". It **is** audited (a principal disappears), like
   `spawn`/`exit`; it blocks rather than polls, so the per-call record does
   not drown the log.
3. **Block, never busy-poll (§2.1).** The blocking is the
   scheduler-side producer's job (a cooperative park/unpark on the child's
   exit, mirroring the `irq_wait` wait loop), not a spin in the handler.
4. **Fail closed (§2.9).** A `pid` that is not a child of the caller →
   `NotFound`; no producer wired → `NotImplemented`; a faulting `status`
   pointer → `BadAddress`. The exit code is copied out only on success.

**Staging:**

- **SP6a — abi-v1 surface + fail-closed seam (host-proven) `[x]`.**
  `lib/abi`: `SyscallNumber::WAIT` (**16**) + `WAIT_PID_ANY` const + the
  `SyscallSpec` row (`wait(I32 pid, UserPtr status) -> U64`, **unprivileged,
  audited**) + frozen-number test. `lib/abi-sys`: the `tairix_sys_wait` C stub
  (`#[export_name]`, panic-free) + drift-registry row + marshalling tests;
  regenerate the C header (`cargo xtask c-header --write`); `SYSCALL_TABLE_HASH`
  re-derives from `ENCODED_TABLE`; `abi-check` + `c-header` guards green.
  `lib/rt`: the `wait(pid, &mut status) -> i64` wrapper + marshalling tests.
  `kernel/syscall`: the `SyscallHandlers::wait` trait method + dispatch arm
  (I32-pid recovery, UserPtr status) + the three test-double impls
  (`MockHandlers`/`AcceptingHandlers`/`CountingHandlers`) + decode/forward
  tests. `kernel/core`: a fail-closed arch-neutral `procwait::ProcessWait`
  seam (`wait(parent: TaskId, pid, flags) -> Result<WaitedChild, Errno>`; default
  `NULL_PROCESS_WAIT` → `NotImplemented`, mirroring `NULL_MEM_MAP` /
  `NULL_PROCESS_SPAWN`), the `wait` handler (forward → `copy_out` the exit
  code → return pid), and a `with_process_wait` builder, so the kernel binary
  needs no change yet. Host-tested (forward+copy_out success, no-producer
  `NotImplemented`, producer-error propagation, unregistered-caller
  `BadAddress`). **Landed (this increment).**
- **SP6b — scheduler-side blocking producer + `-M virt` EL0 vertical `[x]`.**
  The `ProcessWait` trait grew two default-no-op bookkeeping hooks —
  `register_child(parent, child)` (called from the `spawn` admit path) and
  `record_exit(task, code)` (called from the `exit` handler) — so the
  fail-closed `NullProcessWait` and the host-test doubles inherit inert
  bodies and `KernelState`/`KernelSyscallHandlers::new` need no churn. The
  real producer, `kernel/core::procwait::KernelProcessWait<A: SchedulerArch>`,
  owns a `SpinLock<ProcessTable>` (child id → `{parent, exit}`) and blocks a
  waiting parent by cooperatively parking it back on the scheduler through
  the free `reschedule_current(current_cpu, Yield)` until a matching child is
  reapable, then reaps it (fail-closed `NotImplemented` if no resumable user
  kthread is published — never a busy-spin, §2.1/§2.9). `run_phases` builds +
  leaks it over the `'static` `KernelState` arch handle and installs it via
  the hook's new `process_wait` param + `with_process_wait`. `exit` now feeds
  the recorded code (it was previously dropped), and the `spawn` admit path
  records the parent→child link. Host-proven (9 `ProcessTable`/producer
  tests + the `exit`→`record_exit` and spawn-admit→`register_child` wiring
  tests). The aarch64 `-M virt` vertical (`tests/integration/wait_qemu_aarch64`
  + the two-role `tests/integration/wait_program` fixture, `build.rs` the §2.2
  source of truth for `CHILD_EXIT_CODE`) builds an isolated child + parent EL0
  space, registers the link with a live `KernelProcessWait`, and drives the
  cooperative `step` loop: the child `exit`s with the agreed code, the
  parent's `wait` parks then reaps it, the kernel copies the code out to the
  parent's `status`, and the parent verifies it and exits 0 — PASS.
  **Verified green under QEMU on `-M virt`.** This unblocks the P6e-3b shell
  REPL + `init` supervision.

**Done when (SP6 overall):** a parent process can block on, reap, and read
the exit code of its own child via `abi-v1` on aarch64 `-M virt`; waiting on
a non-child fails closed; `virt` + the headless build stay green. **SP6 is
complete:** SP6a landed the abi surface + fail-closed seam; SP6b landed the
scheduler-side blocking producer + the `-M virt` vertical.

**Non-blocking poll (`WaitFlags::NONBLOCK`).** `wait` carries a third
`flags: u32` argument (a `tairix_abi::WaitFlags` set; the ABI is unfrozen so
the row was extended in place, no `v2`). With `NONBLOCK` set the call polls:
it reaps an already-exited child, or returns `Errno::WouldBlock` for a
still-running child **without parking** — the reap the shell's job control
performs before each prompt and PID 1 `init` uses to reap the session without
blocking. The producer serves it through the same single `ProcessTable::reap`
primitive the blocking loop uses (`ProcessWait::poll`), so the two can never
diverge; `WouldBlock` is the `abi-v1` "nothing yet, retry" signal the
dispatcher records below the error level, so a polling loop never floods the
audit log. First-party wrapper `tairix_rt::try_wait`; C stub `tairix_sys_wait`
(header macro `TAIRIX_WAIT_FLAG_NONBLOCK`). Host-proven end to end (producer
poll ready/would-block/not-found; handler nonblock-poll + would-block; abi-sys
+ rt marshalling); the blocking-reap `-M virt` vertical already proves the
shared `reap` primitive on hardware.

---

## SP7 — the `signal` syscall (job-control signal delivery) `[x]`

**SP7a + SP7b are landed, so SP7 is complete on aarch64.** A first-party
program can deliver a control signal to its own child through `abi-v1`; the
signal producer authorises the target against the sender's own children and
delivers it by driving the live scheduler. The x86_64 + riscv64 sibling
verticals follow when convenient (the producer is arch-neutral; only the
`-M virt` proof is per-arch).

Backs `ProcessHost::signal` (`.junie/PREREQUISITES2.md` P2), the seam the
shell's `fg`/`bg`/kill job control drives. A parent delivers one of a small,
closed set of control signals to a child it spawned; a process may signal
only its **own** children, so — like `wait` — the authority is inherent in
the parent/child relationship and needs no capability (§5.2: no capability
with no live enforcement point). No cross-process or process-group signalling
exists yet: that is a later stage with its own capability when a holder and
an enforcement point exist together.

**Signal set (closed, `tairix_abi::Signal`).** The minimal set job control
needs, mirroring the shell's own `job::Signal` vocabulary: `Continue`
(resume a stopped child, discriminant 1), `Terminate` (ask a child to end,
2), `Kill` (force a child to end, 3). Discriminant 0 is reserved and never
valid, so a zeroed register fails closed. `Signal::from_u32` rejects any
other value with `Errno::OutOfRange` (validate every input).

Split into two increments, exactly as SP6 was (surface+seam, then producer):

- **SP7a — abi-v1 surface + fail-closed seam (host-proven).**
  `lib/abi`: `SyscallNumber::SIGNAL` (**64**) + the closed `Signal` enum
  (`process.rs`) + the `SyscallSpec` row (`signal(I32 pid, U32 signal) ->
  Errno`, **unprivileged, audited** — signalling a process is a
  security-relevant lifecycle decision, and own-children-only grants no
  authority over another principal) + frozen-number test. `lib/abi-sys`: the
  `tairix_sys_signal` C stub (`#[export_name]`, panic-free) + drift-registry row
  + marshalling tests; regenerate the C header
  (`cargo xtask c-header --write`); `abi-check` + `c-header` guards green.
  `lib/rt`: the `signal(pid, Signal) -> i64` wrapper + marshalling tests.
  `kernel/syscall`: the `SyscallHandlers::signal` trait method (default
  `NotImplemented` body, so the test doubles need no churn) + dispatch arm
  (I32-pid recovery, `Signal::from_u32` validation) + decode tests.
  `kernel/core`: a fail-closed arch-neutral `procsignal::ProcessSignal` seam
  (`signal(sender: TaskId, pid, Signal) -> Result<(), Errno>`; default
  `NULL_PROCESS_SIGNAL` → `NotImplemented`, mirroring `NULL_PROCESS_WAIT`),
  the `signal` handler (forward → return `Ok(0)`), and a `with_process_signal`
  builder, so the kernel binary needs no change yet. The shell's
  `RtProcessHost::signal` is wired to the real `tairix_rt::signal` wrapper
  (mapping `job::Signal` → `tairix_abi::Signal`), replacing its explicit
  `NotImplemented` stub with the genuine syscall path (fail-closed until the
  producer lands). Host-tested (null seam fail-closed; handler forwards;
  dispatch decodes/validates; marshalling).
- **SP7b — scheduler-side producer + `-M virt` vertical `[x]`.** **Landed.**
  `kernel/core::procsignal::KernelProcessSignal<A, P>` is the concrete
  producer: it composes over the `KernelProcessWait` producer (the one owner
  of the parent/child + exit-status bookkeeping — no second copy, §2.2) and a
  `&'static P: SchedulerPolicy`. It authorises the target through the new
  `KernelProcessWait::authorise_child` (a live child of the sender, else
  fail-closed `NotFound`; a zombie is not signallable), then delivers:
  `Continue` → `SchedulerPolicy::unpark` (a continue to a non-stopped child is
  a harmless no-op — `InvalidState` is folded to `Ok`); `Terminate` / `Kill` →
  `SchedulerPolicy::exit` + `KernelProcessWait::record_signalled_exit`, which
  records the signal's POSIX-familiar termination status (the shared
  `Signal::termination_status` in `lib/abi`, so kernel and program agree —
  since SP9: `Interrupt` → 130, `Kill` → 137, `Terminate` → 143) so the
  parent's `wait` reaps it. A termination never lands *inside* a syscall:
  a victim between syscall entry and return may hold kernel state only its
  own unwind can release (a mount's `SleepLock`, an in-flight block-I/O
  descriptor), so the `procsignal` kill gate records the kill pending,
  wakes the victim (every in-kernel park loop unwinds with
  `Errno::Interrupted` when a kill is pending, so no wait is unkillable),
  and the syscall dispatch boundary lands it — recording the same
  `128 + n` status and running the one shared reclaim — once the handler
  has unwound. A victim in user mode is terminated immediately.
  Installed in `init.rs::run_phases` over the concrete wait producer +
  `state.scheduler` and threaded through a hook-level `with_process_signal`
  forwarder. Six host tests cover it over a real `Scheduler<TestArch>`
  (non-child fail-closed, Terminate/Kill status, Continue no-op, no
  double-signal). The aarch64 `-M virt` vertical
  (`tests/integration/signal_qemu_aarch64` + the two-role
  `tests/integration/signal_program` fixture) builds an isolated child + parent
  EL0 space, admits the child, threads its scheduler-assigned PID into the
  parent's startup arguments, installs the wait + signal producers, and drives
  the cooperative `step` loop: the child yields forever, and since SP9 the
  parent drives the full job-control sequence (`Stop` → `STOPPED` wait
  observes the stop → `Continue` → `Terminate` → reap, verifying the 143
  status) and exits 0 — verified green under QEMU on `-M virt`.

**Done when (SP7):** a first-party program can issue `signal` through `abi-v1`
and terminate its own child under the live scheduler on aarch64 `-M virt`;
signalling a non-child fails closed; the C header, `abi-check`, `deps-check`,
`cfg-check`, the host test matrix, and the QEMU matrix stay green. **SP7a
landed the surface + fail-closed seam; SP7b landed the scheduler-side producer
+ the `-M virt` vertical.**

---

## SP8 — startup strings: caller-supplied argv + environment `[x]`

The `spawn` syscall carries the child's startup strings (`plans/APPS.md` §8
— the shell's launch form, and the prerequisite for `man <cmd>` and every
argv-taking command app):

- **ABI.** `spawn` is a 6-argument syscall: `(path, path_len, console,
  target_uid, strings, strings_len)`. `strings` (0 = absent) names an
  encoded `tairix_abi::process` `PSV1` startup-vector block — the **same**
  format the kernel writes into a child's image, so there is exactly one
  strings encoding and one fuzz-covered decoder, no second codec. The
  kernel bounds `strings_len` against `PROCESS_START_MAX_TOTAL_LEN`
  *before* staging, copies the block in through the validated
  `copy_from_user` boundary, and parses it fail-closed (`ProcessStart::
  parse`); the block's canary field is ignored — the kernel mints the
  child's canary itself. Strings are data: they carry no authority and
  never influence the child's credential, manifest, or capability set.
- **Semantics.** A present block governs the child's argument vector and
  environment verbatim (a shell passing the typed words and its exported
  variables, `NAME=value` entries split at the first `=`); no block means
  the program's registered default arguments and an empty environment —
  every pre-existing caller's exact behaviour. Boot-floor driver spawns
  pass an empty environment deliberately.
- **Plumbing.** `ProcessSpawn` was consolidated to the single entry point
  `spawn_with(rxe, ctx, caps, args, env)` (the old `spawn(program, ctx)`
  delegator was deleted; the handler resolves the effective strings and
  calls `spawn_with` directly); all three arch producers thread `env` into
  the shared `kernel/mem` startup-vector build. Userland:
  `tairix_rt::spawn_with(path, console, uid, args, env)` encodes the block
  via the shared `process_start_*` helpers, `tairix_rt::{env, env_count,
  env_var}` read the child-side environment, `tairix_sys_spawn` carries the
  two new C-ABI parameters, and elsh's `RtProcessHost` passes the
  command's words plus exported env (with `NAME=v cmd` prefix overrides);
  pipes/redirections still fail closed pending descriptor plumbing.
- **Proof.** Kernel host tests cover the override/default/malformed/shape
  paths (`kernel/core/src/syscalls.rs`); rt marshal + accessor tests cover
  the userland encoding and env lookup; the session-ceiling QEMU vertical
  types `ps --bogus` and keys on the resulting usage line — output only a
  delivered `argv[1]` can produce.

---

## SP9 — foreground job control: `^C`/`^Z`, `Signal::{Interrupt,Stop}`, stopped wait reports `[x]`

The elsh interactive work (`.junie/plan-session-shell.md` Part 3,
`plans/SHELL.md` "Job control"): while the shell is blocked in `wait()` on a
foreground child, `^C` must interrupt and `^Z` must stop that child — the
kernel console line discipline delivers the signal; the shell only marks and
clears the foreground job. Binding design decisions:

- **Signal set.** The closed `tairix_abi::Signal` gains `Interrupt` (4, the
  `^C` interrupt request; default disposition terminates) and `Stop` (5, the
  `^Z` stop; parks the child, never terminates it). `Signal::
  termination_status` follows the POSIX-familiar `128 + <signal a script
  expects>` codes — `Interrupt` → 130, `Terminate` → 143, `Kill` → 137
  (`Continue`/`Stop` → `None`) — because §16.7 familiarity binds the codes a
  shell user scripts against, not our wire discriminants (in-place evolution
  of SP7's 130/131).
- **Stopped wait reports.** `WaitFlags` gains `STOPPED` (1 << 1, the
  `WUNTRACED` analogue): with it set, `wait` also reports a child freshly
  stopped by `Signal::Stop` — without reaping it. A stop is reported once
  (edge-triggered, re-armed by `Continue`). The `status` out-pointer now
  names a typed two-field record, `tairix_abi::WaitStatusRecord`
  (`#[repr(C)]`: `kind: u32` — 1 exited, 2 stopped, 0 reserved — plus
  `value: i32` — the exit code, or the stopping signal's discriminant),
  decoded fail-closed into `tairix_abi::WaitStatus::{Exited(i32),
  Stopped(Signal)}`; no POSIX bit-packing. Every caller updates in place.
- **Kernel bookkeeping.** `ProcessTable`'s `ChildEntry` gains
  `stop_pending: Option<Signal>`; `KernelProcessWait` gains
  `record_stop`/`record_continue`, its `wait`/`poll` take the decoded
  `WaitFlags` and return `WaitedChild { pid, status: WaitStatus }` (the
  `ReapedChild` successor), and stop events wake `PROCWAIT_WAITQ` exactly as
  exits do. `KernelProcessSignal` delivers `Stop` →
  `SchedulerPolicy::park(child)` + `record_stop`, `Continue` → `unpark` +
  `record_continue`, `Interrupt` → the terminate path with its 130 status.
- **Console line discipline.** `ConsoleDevice` owns an atomic `foreground`
  slot (lock-free — the filter runs in the UART RX interrupt handler, where
  spinning on a lock held by the interrupted task would deadlock a single
  CPU) and an atomic `InputMode` mirror, and implements `ConsoleInput`:
  every producer (the aarch64 UART RX drain — both its ISR and
  reader-context entry — via `arch_wrapper::uart_console_device()`, and the
  seat registry's text sink, now the video console device) pushes through
  the device. In **cooked** mode with a foreground task set and the
  delivery hook installed, the filter consumes `0x03`/`0x1A` and **queues**
  `Interrupt`/`Stop` in `procsignal`'s single atomic pending slot
  (`queue_foreground_signal`, newest wins), nudging the dispatch loop
  (`console_wake`); the scheduler-driving delivery runs at dispatcher
  context (`drain_pending_foreground`, called beside `drain_pending_wakes`
  and in the idle guard) through the boot-installed `ForegroundSignal` hook
  (implemented by `KernelProcessSignal`, installed beside
  `with_process_signal`). All other bytes — and every byte in raw/secret
  mode, or with no foreground set — flow to the input queue exactly as
  before. A delivery whose target has already exited is dropped fail-closed
  (task ids are never reused, so a stale slot can never reach a different
  task).
- **Stop overlay.** The scheduler's park/unpark state is shared with every
  blocking wait, so a broadcast wake (a console byte waking all parked
  readers) could resume a "stopped" task. `procsignal` owns a
  `STOPPED_TASKS` overlay set: `Stop` marks before parking, the kthread
  dispatch shim re-parks an overlay-held task instead of running it, and
  only `Continue` (or termination) lifts the entry — so a stopped job stays
  genuinely stopped across spurious wakes.
- **Foreground marking.** New unprivileged-beyond-console syscall
  `SyscallNumber::CONSOLE_FOREGROUND` (**70**): `(fd: u32, pid: i32)`; `fd`
  must be a `StreamMode::Read` descriptor of the caller's own table (the
  same fd-scoped authority `stream_input_mode` uses, same dispatcher
  capability gate), `pid` must be a **live child of the caller**
  (authorised through the one `KernelProcessWait::authorise_child`), and
  `pid == 0` clears the slot. No new capability (§5.2 minimalism): the
  authority is the inherited console descriptor plus the parent/child
  relation.
- **Shell wiring.** `RtProcessHost::wait` marks the child foreground on
  fd 0 (`console_foreground`), issues the blocking wait with
  `WaitFlags::STOPPED`, clears the slot on return, and decodes the record —
  `Stopped` maps to the shell's `WaitOutcome::Stopped` with the familiar
  POSIX numbers (`Stop` → 20, so `$?` = 148) — feeding the already-landed
  elsh job table (`launch_foreground` stopped-job handling, `fg`/`bg`
  resume). `ProcessHost` itself is unchanged; scripts and pipes see no
  difference.
- **Proof.** Host tests: ABI round-trips (signal values, flags, record
  encode/decode + byte codec, fail-closed kinds), `ProcessTable`
  stop/report-once/continue/reap interleavings (exit supersedes a stop, a
  zombie wins over a pending stop), `KernelProcessSignal`
  stop/continue/interrupt/kill-while-stopped + foreground-deliver over a
  real `Scheduler<TestArch>`, console-device filter (cooked-only,
  foreground-only, hook-gated, replace semantics, short push, clear
  restores pass-through), `console_foreground` handler fail-closed paths
  (bad fd, non-child/negative/zombie pid, no console, no producer), rt +
  abi-sys marshalling and the rt fail-closed record decode, dispatcher
  decode + capability-gate tests, and the fuzz/proptest mirrors. QEMU: the
  SP7 signal vertical now drives Stop → `wait(STOPPED)` observes the stop →
  `Continue` → `Terminate` → reap 143.

**Done when (SP9):** elsh's foreground `^C` terminates and `^Z` stops the
running child on an interactive console with the shell reporting
`[N] Stopped …` and `fg`/`bg` resuming it; a stopped child is reported
through `wait` only when `STOPPED` is requested; every fail-closed path above
is host-tested; the C header, `abi-check`, the host matrix, and the QEMU
matrix stay green. **Landed: the kernel/ABI/rt/elsh path above is complete;
elsh marks its foreground child around every blocking wait and maps a stop
to `$?` = 148 (SIGTSTP's POSIX number).**

---

## SP10 — spawn-time descriptor wiring + pipes (`cmd > file`, `cmd | cmd`)

The elsh Part 4 work (`.junie/plan-session-shell.md`): redirections and
pipelines need the host/kernel half the shell's final
`RedirAction::{Open,Dup,Close,HereString,Multi}` lowering already targets.
The parent pre-opens every target in its **own** descriptor table and hands
the child an explicit fd 0/1/2/3 wiring block at spawn; a pipe object
connects `cmd | cmd`. Binding design decisions:

- **One handle space, one I/O vocabulary.** `pipe_create`
  (`SyscallNumber::PIPE_CREATE`, **73**, unprivileged, unaudited) mints one
  kernel pipe object and returns **two descriptors in the caller's existing
  open-descriptor table** (the same `OpenFileTable` allocator
  `fs_open`/`resource_open` draw from): a read end (`OpenFlags::READ`) and a
  write end (`OpenFlags::WRITE`), written to a caller out-pointer as two
  `u32`s. Pipe ends are read/written through the existing `fs_read` /
  `fs_write` (the offset is meaningless and ignored, exactly as for a
  resource backing) and closed through `fs_close` — no second handle table
  and no pipe-specific I/O syscalls. No new capability (§5.2 minimalism): a
  pipe reaches only the caller's own table, and handing an end to a child
  rides the existing `CAP_PROC_SPAWN` spawn gate.
- **The pipe object** (`kernel/core::pipe`) is a bounded byte ring
  (`PIPE_CAPACITY` = 64 KiB — a deliberate flow-control bound like the §22
  random reserve, not a §24.1 scaling capacity: back-pressure is the point)
  with reference-counted ends. End lifetimes are Drop-based
  (`PipeEndHandle` clones count, drops decrement and wake), so `fs_close`,
  a failed spawn's unwind, and task exit (the registry `withdraw` dropping
  the table) all close ends through one path — nothing leaks and nothing is
  double-counted. Semantics: a read on an empty pipe **parks** the caller
  on the new `PIPE_WAITQ` (never a busy loop) until bytes or writer
  exhaustion; all writers closed + empty ⇒ EOF (`0`). A write to a full
  pipe parks until space; all readers closed ⇒ the new
  `Errno::BrokenPipe` (**27**), so `yes | head`-style pipelines terminate.
  Wakes ride `pipe_wake` (dispatcher-context `wake_all`, the
  `procwait_wake` pattern).
- **The spawn attach block.** `spawn` keeps its six registers but slots 2/3
  become `attach`/`attach_len` (in-place `abi-v1` evolution; the old
  `console` and `target_uid` registers move *into* the block): a
  fixed-length `tairix_abi::SpawnAttach` block carrying `target_uid`
  (`SPAWN_UID_INHERIT` sentinel), the console selector (`CONSOLE_INHERIT`
  or an installed index — the base table exactly as before), and four typed
  per-fd wires (`tairix_abi::FdWire`): `Inherit` (whatever backs the
  parent's own descriptor — the base table's slot, or a clone of the
  parent's open entry where one is wired behind it), `InheritSlot(n)` (the
  same, taken from slot *n* — how `2>&1` is spelled), `Closed`, and
  `Handle(fd)` (a descriptor of the **parent's own** open table: a file,
  resource, or pipe/pty end).
  `attach == 0` means full inherit — exactly today's
  `CONSOLE_INHERIT`/`SPAWN_UID_INHERIT` semantics, so every pre-existing
  caller keeps its behaviour. The block is copied through the validated
  boundary and parsed fail-closed **before any state is touched**; every
  `Handle` is resolved owner-checked against the kernel-trusted caller id
  (a forged or foreign fd is `NotFound`, never a probe oracle).
- **Wired child streams live in the child's own open table at fd 0–3.**
  For each `Handle` wire the kernel clones the parent's open entry into the
  child's `OpenFileTable` at the standard fd number itself (the entries
  share the open-file description: one `Arc`'d offset cursor, so two dup'd
  sinks append interleaved output POSIX-correctly, and a cloned pipe end
  counts as one more reader/writer). An **inheriting** wire clones the
  parent's entry the same way when the base is the parent's own table: the
  console table records console-backed slots only and a wired slot's is
  `Closed`, so reading the table alone would hand a child of a pty-hosted
  shell a denied stdout and lose every byte it writes. An explicitly
  selected console index is the whole base, so the parent's entries stay
  out of that child's reach. The child's console
  `DescriptorTable` slot for a wired fd is `Closed`, so exactly one
  authority backs each descriptor. `stream_read`/`stream_write` resolve
  the caller's open table **first** (a wired standard stream routes to its
  pipe/file/resource backing — a file stream reads/writes at the shared
  cursor, honouring `APPEND`), then fall back to the console table.
  `stream_input_mode`, `console_foreground`, and `terminal_size` stay
  console-only: against a wired fd they fail closed `NotFound` (a pipe is
  not a terminal), which is exactly what a program probing "am I on a
  tty?" needs.
- **The console capability gate moves into the handler's console arm.**
  `stream_read`/`stream_write` were dispatcher-gated on
  `CAP_CONSOLE_READ`/`CAP_CONSOLE_WRITE` — a bootstrap artefact from when
  every backing was the console. With pipe/file backings that blanket gate
  is wrong (a child writing its redirected stdout needs no console
  authority), so the dispatcher rows drop it and the handler checks the
  console capability only when the descriptor actually resolves to a
  console backing — the exact evolution `fs_read` made when
  `CAP_FS_ACCESS` stopped being a blanket dispatcher check. Authority is
  the descriptor table (§5.4); the capability gates the device class.
- **Fail closed, unwind whole.** A malformed block, unknown wire kind,
  out-of-range slot, wrong-direction handle use, or forged fd refuses the
  spawn before the child exists; Drop-based end handles make the
  error-path unwind release every cloned end. OOM stays a `Result`.

Staged like SP3/SP5/SP6/SP7 (one fully-gated increment per landing):

- **SP10a — abi-v1 surface + kernel pipes + spawn wiring (host-proven) `[x]`.**
  **Landed.** `lib/abi`: `Errno::BrokenPipe` (27), `SyscallNumber::PIPE_CREATE`
  (73) + its unprivileged spec row, the `SpawnAttach`/`FdWire` block
  (fixed-length LE encode/parse, fail-closed; the block also carries a
  `flags` word whose sole defined bit `SPAWN_FLAG_SANDBOX` requests the
  parser-sandbox spawn mode — canonical only with fully explicit
  `Closed`/`Handle` wires, an inherited credential, and no console index;
  kernel enforcement and the dispatcher allow-list are documented in
  `docs/src/security/sandbox.md`. A sandbox spawn may pass the reserved
  path token `SPAWN_SELF` (`@self`): the kernel substitutes the path it
  admitted the *caller* from — the `spawn_path` attested on its
  capability record at admission, never `argv[0]`, which is data the
  spawner chose — then runs the ordinary resolution and load gate over
  it. The token serves any spawn of the caller's own binary, sandboxed
  or plain (`plans/STRESSTEST.md` ST5's worker re-entry is the plain
  consumer), and only when the caller carries a spawnable path; a
  caller without one fails closed `NotFound`) + `SPAWN_ATTACH_LEN`, the
  `SPAWN` row's slots 2/3 → `attach`/`attach_len`, and the
  `stream_read`/`stream_write` rows' dispatcher gate dropped (checked
  in-handler for console backings). `lib/abi-sys`: `tairix_sys_spawn` carries
  `(path, path_len, attach, attach_len, strings, strings_len)`,
  `tairix_sys_pipe_create` added; C header regenerated, drift guards green.
  `kernel/syscall`: `SyscallHandlers::spawn` re-shaped, `pipe_create`
  added, dispatch arms + decode tests. `kernel/core`: the `pipe` module
  (ring + Drop-counted ends + `PIPE_WAITQ`/`pipe_wake` + park-loop
  blocking I/O), `OpenBacking::Pipe` + the shared stream cursor on
  `OpenFile`, `pipe_create` handler, `fs_read`/`fs_write` pipe arms,
  `stream_read`/`stream_write` open-table-first routing with the console
  capability checked in the console arm, the spawn handler's attach-block
  decode + owner-checked wire resolution, and `KernelSpawnCtx` installing
  wired entries at the child's fd 0–3 beside `set_streams`. `lib/rt`:
  `pipe_create`, `SpawnAttach` re-exports, and `spawn_attached(path,
  &SpawnAttach, args, env)` beside the preserved `spawn`/`spawn_at`/
  `spawn_as`/`spawn_with` wrappers. Host tests cover the pipe object
  (fill/drain/EOF/broken-pipe/close-idempotence), the attach codec
  (round-trip + every fail-closed shape), owner-checked wiring (happy
  path, forged fd, foreign fd, direction enforcement at use, closed
  slots, dup-shared cursor), the stream routing fallbacks, and the
  handler decode paths.
- **SP10b — elsh `RtProcessHost` wiring + `-M virt` pipeline vertical `[x]`.**
  **Landed.** The lowering is a pure, host-tested planner,
  `tairix_elsh::wireplan`: `lower` turns a `LaunchSpec` into a `WirePlan`
  — the ordered `PlannedOpen` list (path/resource opens with
  `OpenMode`-derived flags, pipe pairs), one fd 0–3 `PlannedWire` map per
  member (pipeline joints wired fd 1 → next fd 0; redirections applied in
  source order over them; `Dup` copies the source's *current* wire, so
  `> out 2>&1` shares one open description and a bare `2>&1` is
  `InheritSlot(1)`), and the `PumpTask` list (`HereString` → pipe +
  `WriteContent`; multios → all targets opened plus a shell-side pipe,
  `FanOut`/`Concat`). Fail closed: a redirection or dup source outside
  fd 0–3 (the `{var}` dynamic forms the attach block cannot express), a
  mixed-direction or undersized multios, and a half-lowerable pipeline
  all refuse whole before anything opens. The `run.rs` executor opens the
  plan all-or-nothing, spawns each member via `spawn_attached` (candidate
  resolution per member, env overrides per command), kills + reaps
  already-spawned members and closes every descriptor on a mid-pipeline
  refusal, closes the transferred ends after the last spawn, runs the
  pumps on the shell's retained ends (blocking `fs_write`/`fs_read`;
  `BrokenPipe` ends a feed silently — the `yes | head` shape; other pump
  errors are reported on fd 2, fail loud, without killing the job), and
  records non-leader PIDs per leader — `wait` reaps them after the
  leader's terminal status (a stopped leader keeps its entry for
  `fg`/`bg`). The `-M virt` vertical
  (`tests/integration/pipeline_qemu_aarch64`) drives the production boot
  + login + shell through `yes | head -n 2` (broken-pipe teardown +
  member reap), `seq 1 1000 | wc -c` (byte-exact `3893` over the pipe),
  and a `> file` / `< file` round trip; the audit sink arms on `cat`'s
  audited exit and PASSes on the shell's scripted exit after the content
  marker. The vertical exposed and now pins two latent kernel defects the
  host matrix could not reach: (1) process death never reclaimed the
  task's kernel state — the `exit` handler skipped the address-space
  registry withdrawal (open pipe ends leaked, so a pipe peer never saw
  EOF/broken-pipe and parked forever) and the signal-terminate path
  reclaimed nothing at all (a killed task also leaked its capability
  record, IRQ bindings, endpoints, and shared memory); both paths now
  drive the one `reclaim_task_resources` definition, the kill side
  through the per-instance `TaskReclaim` seam
  (`KernelProcessSignal::install_task_reclaim`, installed at boot with
  the leaked dispatch hook), with host regression tests on both paths.
  (2) `stream_read`/`stream_write`'s wired-descriptor branch held the
  address-space registry's read guard across a blocking pipe park (the
  `if let` scrutinee temporary), wedging every registry writer — a
  sibling member's startup `mem_map` — on the non-preemptible kernel;
  the entry is now cloned out before the wired call, and the QEMU
  pipeline vertical is the regression test (it deadlocked before the
  fix and passes after).

**Done when (SP10):** `elsh` runs `cmd > file`, `cmd < file`, `2>&1`,
`cmd <<< here`, multios, and `cmd | cmd` end to end on `-M virt` through
the spawn attach block and kernel pipes; every fail-closed path above is
host-tested; the C header, `abi-check`, the host matrix, and the QEMU
matrix stay green. **Landed: the full SP10 path above is complete; the
remaining launch-ABI gap is the `{var}` dynamic descriptors (fd ≥ 10),
which the host still refuses closed (`NotImplemented`) because the
attach block wires only the standard fd 0–3.**

---

## SP11 — demand-grown user stack (remove the fixed stack capacity ceiling)

The spawned-process user stack is today an eagerly committed, hand-picked
288-page (1.125 MiB) constant — the §24.1 fixed-capacity defect twice over:
a scaling cliff (deep recursion faults the guard page and dies with no
`ulimit`/manifest remedy) and a wasted reservation (288 zeroed frames per
process up front, §26.2/§26.3). The guard pages and the fail-closed spawn
refusal are correct and stay; the target is a demand-grown stack inside a
reserved virtual span, bounded by the settable §24.3 `StackBytes` limit
(the `LimitKind` exists, inherits, and is settable via `ulimit`, but is
enforced nowhere yet). Session-to-session working notes:
`.junie/fix-fixed-stack-size.md`.

**Binding decisions:**

1. **The span is the structural bound; the limit is the settable bound
   within it.** `derive_user_layout` places a `stack_reserve_pages` span one
   guard page above the image top and eagerly maps only its top
   `stack_commit_pages`; the guard page below the span never maps, so a
   true overrun still faults deterministically (§2.17 — the structural
   control is untouched).
2. **Growth is fault-driven through the existing seams — no new seam.**
   The arch ports' shared `set_user_fault_resolver` →
   `KernelDispatchHook::resolve_user_fault` path gains a stack case
   (offered for reads *and* writes, before the "any write is fatal" file
   rule): a fault inside the task's recorded span below the committed
   bottom maps one zeroed `RW` page through the installed `MemMap`
   producer (`MapFlags::FIXED` at the faulting page — `LiveSpace::
   map_anonymous` already accepts any unmapped page-aligned VA and fails
   closed on the already-resident race), then re-freezes the registry
   snapshot, exactly as `resolve_file_fault` does.
3. **`StackBytes` is enforced at the growth path, fail closed.** Growth
   stops at the effective soft bound (per-task `LimitSet`, inherited and
   intersected at spawn); a fault past the limit or below the span stays
   fatal with its own audited fault class. Frame exhaustion is the typed
   OOM outcome, never a panic (§4).
4. **The span record lives in the one registry.** A per-task stack-span
   record (reserve base / committed bottom / span top) joins
   `AddressSpaceRegistry` (same lifecycle as `LimitSet`/file regions),
   recorded at admission by threading the span through
   `SpawnCtx::admit_process` / `InitSpawnCtx::admit_init` (in-place
   signature evolution, every port updated together, §2.13).
5. **wasm32 is an honest n/a** (linear memory, no MMU fault growth),
   declared like the SP5 precedent.

**Staging (one fully-gated increment per landing):**

- **SP11a — layout mechanism (host-proven) `[x]`.** **Landed.**
  `tairix_kernel_mem::derive_user_layout` takes the
  `(stack_reserve_pages, stack_commit_pages)` pair and `UserLayout` carries
  `stack_reserve_base` beside the committed `stack_base`; fail-closed on a
  zero commit, a commit exceeding the reserve, the ceiling, and overflow
  (host-tested, including the committed-top-of-a-wider-reserve shape).
  `spawn_layout` splits the one policy constant into
  `USER_STACK_RESERVE_PAGES` / `USER_STACK_COMMIT_PAGES` — **equal (288)
  in this increment**, a compile-time assert pinning commit ≤ reserve —
  so production behaviour is byte-identical until the growth path exists;
  the six port `init_spawn`/`spawn_producer` files consume the committed
  constant. `memory.md` §7c describes the reserve/commit shape.
- **SP11b — kernel/core growth path (host-proven) `[x]`.** **Landed.**
  `StackSpan` (fail-closed constructor: page-aligned bounds, committed ≥
  reserve, non-empty committed top) lives in `AddressSpaceRegistry` with
  the per-process lifecycle (recorded at admission, withdrawn at exit);
  the span threads through `SpawnCtx::admit_process` /
  `InitSpawnCtx::admit_init` (in-place signature evolution, every port +
  double updated together) from the one shared derivation
  `spawn_layout::stack_span`. `resolve_stack_fault` sits beside
  `resolve_file_fault` and is offered first in
  `KernelDispatchHook::resolve_user_fault` — for reads *and* writes, so
  the write-fatal file rule can never kill a growth fault — backing
  **every page from the committed base down to the faulting page** with
  zeroed `RW` pages through the installed `MemMap` producer
  (`MapFlags::FIXED`, `Errno::BadAddress` folded per page as the benign
  resident race), lowering the committed base per page (truthful even on
  a mid-walk frame exhaustion), and re-freezing the snapshot once.
  Contiguous growth is the invariant that makes a large frame's
  first-touch-anywhere order safe: no unmapped hole can ever strand above
  the low-water mark (the single-faulting-page walk it replaced wrongly
  killed a later touch inside the skipped pages; host-regression-tested).
  The `copy_in_user` retry offers stack growth too. `StackBytes` soft bound
  enforced before any page maps; committed bytes are the live usage the
  `TaskLimits` introspect report carries (`AddressSpaceBytes` usage now
  reports `mapped_aspace_bytes` there too — a noticed drift fixed in the
  same change). The audited kill classifies `stack_limit` / `stack`
  beside `file_region` / `wild`. Host tests cover span shapes, record
  lifecycle, growth, bounds, the limit gate, the race/OOM folds, and the
  fault classes; `resource-limits.md`'s "not yet wired" list is down to
  `OpenStreams` / `Processes`.
- **SP11c — policy flip + `-M virt` verticals `[x]`.** **Landed.**
  `USER_STACK_RESERVE_PAGES` is 2048 pages (8 MiB), derived from the one
  default stack policy value — `tairix_kernel_core::DEFAULT_STACK_LIMIT_BYTES`
  (`kernel/core/src/rlimit.rs`), which `LimitSet::DEFAULT` carries as the
  `StackBytes` bound (soft and hard: the span is the structural bound, so
  a wider grant is meaningless without `CAP_RLIMIT_RAISE` *and* a wider
  span) — so the settable default and the structural span share one
  definition. `USER_STACK_COMMIT_PAGES` is 32 (128 KiB): ample for
  `tairix-rt` startup (the 1 MiB "scratch carve" is the C-compat `crt0`'s,
  reached only under the production growth path, and the c-program
  verticals map their own bespoke fully-eager stacks). QEMU verticals
  landed on aarch64 + riscv64: `tests/integration/stack_grow_program`
  (four argv-selected roles — parent / grow / limit / guard, numeric
  parameters passed via chassis argv derived from the one `spawn_layout`
  policy) driven by `stack_grow_qemu_aarch64` / `…_riscv64` through the
  production `KernelDispatchHook` + user-fault resolver + `LiveMemMap` +
  production spawn/wait: growth past the commit is transparent and
  byte-verified, a `rlimit_set`-lowered `StackBytes` bound fault-kills
  (exit 139), and the below-span guard page stays fatal (exit 139).
  `spawn_layout` is now a public module so the verticals import the
  policy instead of copying it.
- **SP11d — docs finish `[x]`.** **Landed.** `memory.md` §7c describes
  the landed design (reserve/commit values, contiguous growth invariant,
  vertical coverage); `resource-limits.md` carries the finite default
  `StackBytes` prose. No README matrix row: the demand-grown stack is
  arch-neutral kernel behaviour on every MMU port (wasm32 linear memory
  stays the honest n/a), not a per-arch feature split.
- **SP11e — x86_64 QEMU vertical `[x]`.** **Landed.** The x86_64 twin of
  the SP11c verticals, unblocked by factoring the x86_64 bring-up into a
  composable piece rather than forking it (§2.2): `x86_64/boot.rs` now
  exposes `bring_up_bsp(boot_info, log_sink) -> BspBringUp` — the whole
  shared board bring-up (per-CPU/GDT/TSS/IDT ordering, the dedicated
  `#PF` entry + uaccess window, NXE, park root, LAPIC calibration, the
  firmware memory map + guard-arena carve, the MADT walk, the production
  dispatch callback **and** user-fault resolver, syscall TLS + `syscall`/
  TSS entry, masked IO-APIC routing; every set-once install lives here,
  and a second resolver occupant now refuses the boot with the typed
  `BootError::UserFaultResolverInstall` instead of a silent park) —
  returning the discovered facts (`bsp_lapic_id`, `cpu_to_lapic`,
  `calibration`, `memory_map`, `irq_routing`); the private `try_boot` is
  now `bring_up_bsp` + arch-handle construction + `BootInfo` assembly.
  Because `production_dispatch`/`production_user_fault` resolve through
  the bin-crate `DISPATCH_SLOT`, the chassis
  (`tests/integration/stack_grow_qemu_x86_64`) runs the same bring-up and
  installs its **own** production `KernelDispatchHook` into that same
  slot (no `kernel_main`, no second dispatch shim) — two `BinArch`
  handles from the returned facts, the production `LiveMemMap` +
  `KernelProcessWait` + `X86_64_PROCESS_SPAWN` + `KernelInitSpawner`
  composition, the same four-role fixture and policy-derived argv as the
  SP11c twins (failure sites are logged messages + a `parent_exit_code`
  field: x86_64's `isa-debug-exit` carries no per-site code). Grow /
  limit / guard all proven on the QEMU x86_64 machine. The same
  `bring_up_bsp` seam unblocks the staged `file_map`/`mem_map` x86_64
  resolver siblings.

**Done when (SP11):** a first-party EL0 program can recurse past the
spawn-time committed stack and keep running, bounded by a `ulimit`-settable
`StackBytes` limit that kills it (audited) when exceeded; no process pays
more eager stack frames than the committed top; the guard page below the
span still faults; host + QEMU matrices and the headless build stay green.
**Met end to end on all three MMU ports (SP11c aarch64/riscv64, SP11e
x86_64); wasm32 linear memory stays the honest n/a.**

---

## 2. Cross-cutting requirements (apply to every stage)

- **No new HAL trait unless deliberate (§17.2).** SP1–SP5 reuse the closed
  HAL set (SP5's TLB shootdown uses the already-landed `TlbShootdown` /
  `CrossCpuTlbShootdown` slices — the §17.2 burn-down is complete). A
  genuinely new arch primitive lands in `kernel/arch/api` with a
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
tools/ci/soak.sh both --secs 20   # developer machine (that's us): max 20 s;
                                  # the unbounded 24 h soak is the CI host's job
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
