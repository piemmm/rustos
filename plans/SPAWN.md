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
- `lib/abi-sys`: the `ros_sys_spawn` C stub (`#[export_name]`, panic-free,
  `AGENTS.md` §9) + the drift-registry row + a marshalling test. C header
  regenerated (`cargo xtask c-header --write` → `ROS_SYS_SPAWN 12u`,
  `uint64_t ros_sys_spawn(void*, uintptr_t)`); `abi-check` + `c-header`
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
  `HandlerSpawnCtx` impl admits the child as a **Ready** resumable user
  kthread (`spawn_user_kthread`) + registers its caps + frozen address
  space, returning the PID **without** entering/draining (the caller keeps
  running). Handler fields default in `new`, with `with_frames` /
  `with_spawn` builders (mirroring `with_console`), so the kernel binary
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
binary (`kernel/rustos-kernel/src/spawn_producer.rs`, sibling of
`init_spawn`): a static `PageTablePool` reserve + monotonic cursor
(`MAX_SPAWNS = 8`, fail-closed `NoSpace` on exhaustion), builds a fresh
2 GiB-identity isolated user space **without** switching `TTBR0_EL1` (the
spawning caller keeps running — the build writes through the identity
`physmap`, so the child space need not be active), parses the `rxe` against
`SYSCALL_TABLE_HASH`, calls `spawn_image` (re-asserts `CAP_PROC_SPAWN`,
audits), freezes, builds the `pre_resume`/`enter` closures, and calls
`ctx.admit_process`. `boot_aarch64` wires `.with_spawn(&AARCH64_PROGRAM_REGISTRY,
&AARCH64_PROCESS_SPAWN)`; `kernel_core` threads `programs`/`spawn_service` +
`&state.frame_allocator` through `BootInfo::with_spawn` → `run_phases` →
`KernelDispatchHook::new` → `with_frames`/`with_spawn`. The kernel `build.rs`
was generalised to build **both** `init` and the `Shell` session program
through one `elf2rxe` helper (§2.2; RustOS stays Rust-only, §1), embedding
`SHELL_RXE` registered under `/Apps/Shell.app/Run`. `AdmitError`,
build, and parse failures map onto stable `Errno`s (`NoSpace` /
`AlreadyExists` / `PermissionDenied` / `BadMagic`); the partial pool/frame
reserves are monotonic, so a failed spawn leaks nothing user-visible (§2.9).
- `-M virt` vertical landed: `rustos-test-spawn-session-qemu-aarch64` boots
  the production pipeline, PID 1 `init` spawns `/Apps/Shell.app/Run` through
  the `spawn` syscall, both run (proving SP2 timesharing), the session writes
  a gated banner + `exit`s, and `init` observes the returned PID. PASS keys
  on two `ProcessSpawned` (4030) + three audited syscalls (5000) — `init`'s
  `spawn`, `init`'s `exit`, and the session's necessarily-last gated `exit`.

**Done when (SP3 overall):** a userland process can spawn a separate,
isolated, runnable process via `abi-v1` on aarch64 `-M virt`; siblings
follow. **SP3a + SP3b are landed.**

### SP4 — `init` launches the `session` program `[x]` (folds into PI.md P6e)

**Landed alongside SP3b.** `init`'s startup config (`session
/Apps/Shell.app/Run`, already parsed, `plans/PI.md` P6b) is now launched
through the SP3 `spawn` syscall (`rustos_rt::spawn`) as a separate, isolated
process; `init` keeps running and reacts fail-closed (`EXIT_SESSION_FAILED`)
to a failed spawn rather than being replaced. `init`'s effective set gained
`CAP_PROC_SPAWN`; the child receives only its own `CAP_CONSOLE_WRITE`
(no ambient authority, §4). The minimal `session` program is a banner+exit
`Run` stub in the `Shell` bundle; growing it into a real shell REPL (wiring
in the `rustos-shell` interpreter library) is `plans/PI.md` P6e. Per the
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
sibling riscv64 **and x86_64** `-M virt`/OVMF verticals are now landed too
(the x86_64 one also closed a production ring-3 fault-delivery gap — boot
now installs the dedicated `#PF` entry and `TSS.RSP0`); production per-task
live-space retention is the remaining follow-on (tracked beyond SP5).

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

1. **Clean-slate shape, no legacy single break (§2.13).** RustOS has no
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
  `lib/abi-sys`: the `ros_sys_mem_map` / `ros_sys_mem_unmap` C stubs
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
  all-or-nothing, §2.9). `unmap_anonymous` validates the whole range is
  mapped before tearing any of it down, zeroes every reclaimed frame on free
  (§4), and folds an allocator exhaustion onto one OOM type. The per-page
  TLB flush rides the existing `AddressSpace::map`/`unmap` (`TlbShootdown`
  slice). Host-proven over `HostPageTable` + `SimPhysMap` (8 unit tests:
  zeroed RW|USER map, zero-on-free, OOM unwind, already-mapped unwind,
  validate-all unmap, zero-length/misaligned rejection, page-count rounding).
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
  (`tests/integration/mem_map_program`, linking the new `rustos_rt::mem_map`
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
  production `rustos-kernel` pipeline (like `spawn_program_qemu_x86_64`), then
  `iretq`s into the program through `EnterUser::enter_user`; its `fault`
  observer reports the use-after-unmap `#PF` as PASS on the OVMF/GRUB-ISO boot
  (ids 4274–4277, verified green on this host). Reaching it required closing a
  **production x86_64 gap**: a ring-3 CPU exception (or a hardware IRQ that
  preempts a user task) is delivered through the IDT, for which the CPU loads
  `TSS.RSP0` — but boot only ever set the *syscall* entry stack (loaded via
  `swapgs`), leaving `TSS.RSP0 = 0`, so a user fault's frame-push faulted and
  escalated to `#DF` (the trap was *undeliverable*, a security gap, §2.9/§2.17).
  Boot now installs the dedicated, error-code-aware `#PF` entry
  (`rustos_arch_x86_64::fault`, the x86_64 analogue of the riscv64/aarch64
  `fault` hooks) **and** programs `TSS.RSP0` (`percpu::install_tss_rsp0`, reusing
  the one shared stack-pivot validator `validate_kernel_rsp0`, §2.2) to the
  same already-mapped per-CPU kernel stack the syscall path uses, bringing
  x86_64 to the ring-3 fault-delivery parity the other ports already had.
  Production per-task live-space retention still follows; wasm32's
  linear-memory model is an honest n/a (declared).

**Done when (SP5 overall):** an EL0 process can obtain and release anonymous
`RW` memory at runtime via `abi-v1` on aarch64 `-M virt`, zeroed on map and
on free, OOM surfaced as an `Errno`; the immutable-`FrozenAddressSpace` gap
is closed by a single live-space mutation path (§2.2); `virt` + the headless
build stay green.

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
   `rustos_abi::WAIT_ANY` (`-1`) for any child; on success the kernel writes
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
  `lib/abi`: `SyscallNumber::WAIT` (**16**) + `WAIT_ANY` const + the
  `SyscallSpec` row (`wait(I32 pid, UserPtr status) -> U64`, **unprivileged,
  audited**) + frozen-number test. `lib/abi-sys`: the `ros_sys_wait` C stub
  (`#[export_name]`, panic-free) + drift-registry row + marshalling tests;
  regenerate the C header (`cargo xtask c-header --write`); `SYSCALL_TABLE_HASH`
  re-derives from `ENCODED_TABLE`; `abi-check` + `c-header` guards green.
  `lib/rt`: the `wait(pid, &mut status) -> i64` wrapper + marshalling tests.
  `kernel/syscall`: the `SyscallHandlers::wait` trait method + dispatch arm
  (I32-pid recovery, UserPtr status) + the three test-double impls
  (`MockHandlers`/`AcceptingHandlers`/`CountingHandlers`) + decode/forward
  tests. `kernel/core`: a fail-closed arch-neutral `procwait::ProcessWait`
  seam (`wait(parent: TaskId, pid) -> Result<ReapedChild, Errno>`; default
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
