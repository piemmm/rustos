# THREADS.md — lightweight threads (multiple threads per process)

Status: **T1, T2 and T3a done; T3b planned.**

Binding under `AGENTS.md`. Read `plans/SPAWN.md` (the process model this builds
on), `plans/PI.md` RV1 (the riscv64 trap protocol T1 reworked), and
`docs/src/architecture/multitasking.md` first.

## The gap

TAIRiX has real processes: `spawn` (syscall 12) builds a child in its own
hardware-isolated address space, admitted as a resumable user kthread
(`plans/SPAWN.md` SP1–SP5). It has **no** second thread of execution inside a
process, so a program wanting concurrency must `spawn` a whole separate address
space and talk over IPC/shm. Every server, compositor, and driver runtime needs
threads over one heap instead.

T2 removed the state-model obstacle: process-scoped state is keyed by
`ProcessId` and thread-scoped state by `TaskId`, so the kernel can hold several
threads per process. T3a removed the address-space obstacle: the live space is
a shared, locked `ProcessSpace` rather than one task's property. What remains
(T3b) is the mechanism:

- `BuiltImage::pre_resume` is a single-use `Box<dyn FnMut>`, so a second thread
  of a process has no way to obtain its own switch-in hook. Decision 9 makes it
  a cloneable per-process hook and hands `kernel/core` the port's `EnterUser`.
- No per-thread thread pointer: `UserEntry` has no `tls_base`, so every thread
  of a process would share one (decision 7).
- No `thread_create`/`thread_exit`/futex ABI, and no `lib/rt` thread runtime.

## Binding decisions

1. **A process is a thread group, identified by its leader's `TaskId`** — the
   `tgid` model. That id is the PID; each thread is its own scheduler task with
   its own `TaskId` (the TID). No new id space.
2. **One capability record per process, with a thread alias map.** `CapTable`
   holds `threads: BTreeMap<TaskId, ProcessId>`; `caps_for` resolves through it.
   Credentials, `proc_id`, attested name, and I/O counters are therefore
   genuinely process-wide, and a `cap_revoke` by one thread binds its siblings —
   a per-thread copy would make revocation incomplete, which is a security
   defect, not a behavioural nuance. `TaskCapabilities::task()` becomes
   `process()`, so the caps snapshot the dispatcher already takes yields the
   process id with **no extra lookup and no extra lock** on the syscall path.
3. **`ProcessId` is a newtype** (`kernel/sec`, beside `TaskId` — kernel-internal
   state, not ABI surface), so the compiler finds every call
   site. Process-scoped registry accessors take `ProcessId`; per-thread
   accessors keep `TaskId`. This is what catches the misuse class that matters —
   `kernel/virtio/src/kernel_host.rs` used `caps.task()` for its completion
   wait, which the retype forced a decision on (it is the owning *process*,
   since an interrupt binding is a process resource any of its threads may wait
   on).
4. **The live address space becomes a shared, locked process object.**
   `ProcessSpace { space: SpinLock<Box<dyn LiveUserSpace + Send>> }` behind an
   `Arc`, cloned into each thread's `ThreadControl`; the per-CPU slot publishes a
   raw `*const ProcessSpace` (no refcount traffic on the switch path) and
   `with_current_live_space` takes the lock. A **spin** lock, not `SleepLock`:
   the demand-paging fault resolver runs with IRQ masked and cannot park. The
   discipline is *never park while holding it*, and every `LiveUserSpace` method
   is park-free (frame alloc + page-table write + TLB flush). File content is
   already read into a kernel buffer *before* the mapping call
   (`resolve_file_fault`), so the critical section stays short.
5. **Futex-based synchronisation.** `thread_create` carries a `clear_on_exit`
   user word the kernel zeroes and futex-wakes on thread death, so join, detach,
   `Mutex`, and `Condvar` are all built in userland over one generic blocking
   primitive and an uncontended lock never enters the kernel. Userland therefore
   never busy-waits (§2.23).
6. **Futex key = `(ProcessId, user VA)`.** Address spaces are per-process and
   isolated, so this is sound and unforgeable. Cross-process (shm-backed)
   process-shared futexes are a *different* abstraction and deliberately out of
   scope — an absent feature, not a stub.
7. **Per-arch TLS is honest about what the silicon owns.** `UserEntry` gains
   `tls_base`. On aarch64 (`TPIDR_EL0`) and riscv64 (`tp`) the register is
   architecturally user-writable, so it is saved/restored in the thread's own
   trap frame — which lives on that thread's kernel stack, so it is per-thread
   and context-switch-safe by construction — and a user write is respected. On
   x86_64 `FS_BASE` is privileged (CR4.FSGSBASE stays off), so the kernel holds
   the per-thread value and reloads it at switch-in.
8. **No `thread_tls_set` syscall.** A thread's initial thread pointer comes from
   `thread_create`'s `tls_base`, and the register being per-thread is a
   *correctness* property of creating a thread, not a feature. A syscall to
   change it later would have no consumer today (§2.4); the two user-writable
   ports can already do it themselves, and adding the x86_64 path belongs with
   the first real TLS consumer. Thread-local *storage* — `PT_TLS` loading and
   the per-arch variant layouts — is the next layer up, not part of this one.
9. **Thread creation is arch-neutral.** `BuiltImage::pre_resume` becomes a
   cloneable, per-process factory `Arc<dyn Fn(u64 /*kernel stack top*/,
   u64 /*tls base*/) + Send + Sync>` and `BuiltImage` gains
   `user_entry: &'static dyn EnterUser`, so `kernel/core` builds a new thread's
   `pre_resume`/`enter` closures itself. **No new per-arch producer** (§2.21).
10. **Thread-group exit/signal semantics.** `exit(code)` is a *group* exit: it
    terminates every sibling through the existing `procsignal` deferred-teardown
    machinery, then reclaims the process. `thread_exit` ends one thread; the last
    thread ending is a process exit. A signal to a PID is process-directed
    (terminate/stop/continue hit all threads); `wait` reports a child only when
    its whole thread group is gone.
11. **`LimitKind::Threads`** is added, so the per-thread capacity is a settable
    soft/hard rlimit whose live usage is reported through the existing
    `ResourceLimitRecord { limit, usage }` path (§24.3) — never a fixed
    `MAX_THREADS` const (§24.1).

## Stages

### T1 — riscv64 `tp`/`sscratch` trap-protocol fix `[x]`

**Done.** `tp` is both the psABI thread pointer U-mode writes freely and this
port's per-hart kernel identity anchor, and the trap vector never touched it, so
any unprivileged program could steer the kernel onto another hart's per-CPU
state. `sscratch` now points at a per-task 16-byte **trap anchor** carrying the
running hart's kernel `tp`; the from-U prologue spills the user's value into the
frame's new `user_tp` slot and reloads the kernel's before any other register is
touched, and the U-return path publishes the current hart's value and restores
the user's. That also makes the thread pointer per-task — decision 7's
prerequisite on riscv64. Full write-up and regression cover:
`plans/OPEN-DEFECTS.md` D43; witness
`tests/integration/tp_isolation_qemu_riscv64`.

### T2 — process/thread-group split in the kernel state model `[x]`

**Done.** Behaviour-preserving: every process still has exactly one thread, so
nothing user-visible changed. The model can now hold more.

`ProcessId` landed in **`kernel/sec`**, beside `TaskId`, rather than in
`lib/abi` as first sketched: it is kernel-internal state, the PID already
crosses the ABI as a plain integer, and putting it in `lib/abi` would have been
ABI surface with no ABI consumer.

What the split covers:

- `kernel/sec/src/captable.rs` — one record per process, the thread→process
  map, the process→threads index, `register_thread`/`remove_thread`
  (fail closed on an unknown process or a duplicate thread, so a live thread's
  authority can never be re-pointed), `threads_of`/`thread_count`, and
  `task()` → `process()`.
- `kernel/core/src/aspace.rs` — process-scoped maps keyed by `ProcessId`; the
  user-stack span keyed by `TaskId` (per thread) with an incrementally
  maintained per-process committed total, so a multi-threaded process reports
  its whole stack footprint without the registry needing a thread index;
  `withdraw_thread` beside `withdraw`.
- Retyped to `ProcessId` because the resource is the process's, not one
  thread's: IRQ bindings (`kernel/irq`), IPC port/endpoint ownership and
  message provenance (`kernel/ipc`), the virtio host's completion wait
  (`kernel/virtio` — the site the newtype was meant to catch), shared-memory
  mappings, console/pty foreground ownership, the process-wait table, and
  signal targets. `reclaim_task_resources` became
  `reclaim_process_resources`, and `reclaim_process_bookkeeping` now fans the
  per-thread gates out over the process's threads.
- `procsignal`'s deferred teardown carries the **process** to tear down while
  staying keyed by the still-executing **thread** — the dispatch loop can only
  recognise the latter, and reclaiming an address space is the former's job.
- `kernel/syscall` — `CallerContext::process()`, read off the capability
  snapshot the dispatcher already takes, so resolving a caller's process costs
  no extra lookup and no extra lock.

Two grant-ownership security tests changed meaning and were rewritten rather
than adjusted: a caller with a different thread id but the *same* capability
record is now a sibling thread, which shares the grant by design, so the
"foreign" caller in those tests was given its own record (its own process) and
a companion test now pins the sibling-sharing half explicitly.

### T3a — the process address space `[x]`

**Done** (decision 4), behaviour-preserving: every process still has exactly
one thread, so nothing user-visible changed.

`kernel/core/src/procspace.rs` holds `ProcessSpace`, a `SpinLock` over the
boxed `LiveUserSpace`. Each thread's `ThreadControl` holds an `Arc` clone; the
per-CPU slot publishes a borrowed `*const ProcessSpace` (no refcount traffic on
the switch path) and `with_current_live_space` reborrows it and takes the lock,
so the old "only one task per space" `&mut` argument is replaced by real
exclusion. The lock order is `ProcessSpace` before the address-space registry
(the snapshot publication reads translations out of the live space under the
registry write guard); the discipline is never park while holding it, which
holds because every `LiveUserSpace` method is park-free.

The handle threads through `InitSpawnCtx::admit_init`, `BuiltImage::live`,
`Yielder::become_user`, and `spawn_user_kthread_with_stack_live` as
`Option<Arc<ProcessSpace>>`, and each of the six per-arch producers wraps its
own `LiveSpace` in one.

### T3b — threads end to end `[ ]`

**ABI** (`lib/abi`, `lib/abi-sys`, regenerated `include/`, recomputed
`SYSCALL_TABLE_HASH`): `SyscallNumber` 109–112 — `THREAD_CREATE`, `THREAD_EXIT`,
`FUTEX_WAIT`, `FUTEX_WAKE` — with their `SyscallSpec` rows, the `thread_create`
request struct (entry, arg, stack base/len, tls base, clear-on-exit pointer,
flags), `FutexFlags`, `LimitKind::Threads`, the C stubs, and marshalling tests.
All four are **unprivileged**: creating a thread in one's own address space
grants no authority over anything else — the reasoning that made `mem_map`
unprivileged — and the capacity is bounded by `LimitKind::Threads` instead.
`thread_create` is audited; the futex pair is not (a hot, non-security-relevant
blocking primitive).

**Arch HAL** (`kernel/arch/api/src/userentry.rs` + three ports + conformance):
`UserEntry::tls_base`; aarch64 `TPIDR_EL0` framed in `vectors.s` beside
`ELR_EL1`/`SPSR_EL1`/`SP_EL0`; riscv64 `tp` seeded at entry (already framed by
T1); x86_64 `IA32_FS_BASE` set at entry and reloaded from `pre_resume`.

**`kernel/core`**:
- decision 9 — `BuiltImage::pre_resume` becomes `Arc<dyn Fn(u64 /*kernel stack
  top*/, u64 /*tls base*/) + Send + Sync>`, `BuiltImage` carries
  `user_entry: &'static dyn EnterUser` plus the `UserEntry` register state
  instead of a boxed `enter` thunk, and the six per-arch producers stop
  building an entry closure — `kernel/core` builds each thread's from the port
  handle, so a new thread needs no new per-arch producer (§2.21).
- `threads.rs` — `thread_create`: limit check *before* state, validate entry,
  stack, tls and clear-word all lie in the caller's own space through the
  existing uaccess boundary, reserve the kernel stack, `spawn_parked`, register
  the thread (caps alias, stack span, TLS), then `unpark` — the same
  parked-then-install-then-unpark discipline `spawn` uses, so no CPU can
  dispatch a thread before its state exists. `thread_exit`: zero the
  clear-on-exit word, futex-wake it, withdraw per-thread state, reap; last
  thread ⇒ process exit. Group exit and signal fan-out over `procsignal`'s
  existing deferred teardown.
- `futex.rs` — a bucket array sized from discovered CPUs (§24.1), each bucket a
  `BTreeMap<FutexKey, WaitQueue>` created on demand and dropped when empty, so
  the FIFO wake-one fairness, the O(log n) deadline index, and the lost-wake-up
  discipline are `waitq.rs`'s existing tested definitions rather than a second
  wait implementation. Enrols in `run_timed_sweep` and `nearest_timed_deadline`
  so a timed `futex_wait` cannot be dropped. `futex_wait` faults the word in
  through the existing `resolve_anon_fault` and retries once, so a first-touch
  futex word is not a spurious `BadAddress`.
- `syscalls.rs` — the four handlers, seam wiring, host tests.
- `kernel/syscall/src/table.rs` — dispatch arms, `MockHandlers`, reachability
  tests, and the sandbox allow-list decision (threads and futex are
  self-scoped, so a parser sandbox may use them).

**`lib/rt`** — `thread.rs`: `Thread::spawn(FnOnce)` (stack via `mem_map` with a
guard page, thread control block, closure ownership transfer),
`JoinHandle::join`/`detach`; `sync.rs`: futex `Mutex` + `Condvar` whose
uncontended paths are pure userland atomics. The TCB address is passed as both
the entry argument and the thread's `tls_base`, so the thread is psABI-conforming
even before a TLS layer exists.

**Tests** — host unit tests in every touched crate, plus a `threads_program`
fixture and `threads_qemu_{aarch64,riscv64,x86_64}` verticals modelled on
`tests/integration/mem_map_qemu_*`: N threads over one address space increment a
shared counter under a futex `Mutex`, a `Condvar` rendezvous proves a blocking
wake rather than a spin, `join` proves clear-on-exit plus futex wake, the
dispatch callback proves each thread presents its own thread-pointer value, and
a group `exit` proves every sibling dies. Fail-loud finishers throughout.

**Docs** — `docs/src/architecture/threads.md`, plus `multitasking.md`,
`syscalls.md`, `memory.md`, `resource-limits.md`, `security.md` (the futex key
and the thread-group credential model), the per-arch TLS notes in
`docs/src/platform/*.md`, the `README.md` matrices, and a `PLAN.md` stage entry.

**wasm32** is an honest declared n/a: it has no user mode at all (no
`userentry.rs`, no `context_hal.rs`), so `thread_create` fails closed with
`NotImplemented` exactly as `spawn` already does.

## Non-goals

- Do NOT add a `v2` of any type or a compatibility shim: `abi-v1` is unfrozen,
  so every change is made in place with all callers updated (§2.13).
- Do NOT add thread-local *storage* (`PT_TLS` loading, per-arch variant
  layouts, `__tls_get_addr`) in T3b — decision 8. The kernel's per-thread
  thread-pointer contract is what T3b owes.
- Do NOT add process-shared (shm-backed) futexes — decision 6.
- Do NOT collapse the prerequisite stages into T3b: T2 (the state-model split)
  and T3a (the process address space) each land green on their own, because
  nothing in them is user-visible and their blast radius is the whole syscall
  surface.
