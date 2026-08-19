# THREADS.md — lightweight threads (multiple threads per process)

Status: **done.** Every stage has landed; this file records the design the
implementation now carries, not work that remains.

Binding under `AGENTS.md`. Read `plans/SPAWN.md` (the process model this builds
on), `plans/PI.md` RV1 (the riscv64 trap protocol T1 reworked), and
`docs/src/architecture/multitasking.md` first.

## What it delivers

`spawn` (syscall 12) builds a child in its own hardware-isolated address space
(`plans/SPAWN.md` SP1–SP5); threads add a second flow of control *inside* one.
A program that wants concurrency no longer has to `spawn` a separate address
space and talk over IPC/shm — which is what every server, compositor, and driver
runtime actually needs.

The user-facing surface is `abi-v1` syscalls 109–112 (`thread_create`,
`thread_exit`, `futex_wait`, `futex_wake`) and `lib/rt`'s `thread` + `sync`
modules; the reference documentation is `docs/src/architecture/threads.md`.

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
5a. **The kernel owns a thread's user stack, guard page and all.**
   `thread_create` takes a `stack_len`, never a caller-supplied base: the
   kernel reserves `guard + stack` out of the process's own anonymous window,
   leaves the guard page unreserved so an overrun faults instead of being
   demand-backed, bounds the reservation by `LimitKind::StackBytes`, and
   releases the whole region at thread teardown. The reservation is *address
   space only* (`reserve_anonymous_growable`): a stack's depth is unknown and
   mostly untouched, so its no-overcommit headroom is taken one growth step at a
   time exactly as the first thread's growth room is, and the release credits
   back nothing it never charged. Charging the whole extent up front (the
   `mem_map` rule) would bill a thread the RAM of its worst case — with the
   default `stack_len` below, the process's entire `stack-bytes` bound. A
   caller-supplied base cannot
   carry a real guard page under the demand-paged anonymous model (every page of
   a `mem_map` reservation is backed on touch, and the page below one is free for
   an unrelated mapping), it would need an overlap check against every sibling's
   stack span to stop two threads sharing one stack, and it leaks a detached
   thread's stack because nothing observes that death. Kernel ownership
   forecloses all three by construction, so `lib/rt` owns no stack memory and
   needs no retired-stack cache. A `stack_len` of `THREAD_STACK_DEFAULT` (`0`)
   asks for the caller's effective `LimitKind::StackBytes` soft bound, so the
   default is the one live policy rather than a second constant in userland
   (§24.2). Because the five values then fit the argument registers, there is no
   `thread_create` request struct and no `flags` word: a flags word with no
   defined bit would be the speculative surface §2.4 forbids, and the same
   reasoning drops `FutexFlags` (the futex key is always `(ProcessId, VA)`, the
   timeout is always relative nanoseconds with `u64::MAX` for none).
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
    its whole thread group is gone. **The group is the unit of every death**, so
    the driver-store unload stops a user-space driver's whole group too — one
    sibling left running would keep executing against the state that teardown
    withdraws, with no path left to stop it. The per-thread half of all of them is
    one rule (`threads::retire`), so the group's member count can only ever fall
    through a single definition.
11. **`LimitKind::Threads`** is added, so the per-thread capacity is a settable
    soft/hard rlimit whose live usage is reported through the existing
    `ResourceLimitRecord { limit, usage }` path (§24.3) — never a fixed
    `MAX_THREADS` const (§24.1).

## What each part now guarantees

Named by their stage letters, because the surrounding documentation and
`PLAN.md` refer to them that way.

### T1 — the riscv64 `tp`/`sscratch` trap protocol

`tp` is both the psABI thread pointer U-mode writes freely and this port's
per-hart kernel identity anchor, and the trap vector used not to touch it — so
any unprivileged program could steer the kernel onto another hart's per-CPU
state. `sscratch` now points at a per-task 16-byte **trap anchor** carrying the
running hart's kernel `tp`; the from-U prologue spills the user's value into the
frame's `user_tp` slot and reloads the kernel's before any other register is
touched, and the U-return path publishes the current hart's value and restores
the user's. That also makes the thread pointer per-task, which is decision 7's
prerequisite on riscv64. Write-up: `plans/OPEN-DEFECTS.md` D43; witness
`tests/integration/tp_isolation_qemu_riscv64`.

### T2 — the process/thread-group split in the kernel state model

Process-scoped state is keyed by `ProcessId`, thread-scoped state by `TaskId`,
so the compiler rejects a site that scopes one to the other. `ProcessId` lives in
**`kernel/sec`** beside `TaskId` rather than in `lib/abi`: it is kernel-internal
state, and the PID already crosses the ABI as a plain integer.

`kernel/sec/src/captable.rs` holds one record per process plus the
thread→process alias map, the process→threads index, `register_thread` /
`remove_thread` (fail closed on an unknown process or a duplicate thread, so a
live thread's authority can never be re-pointed), `threads_of`, `thread_count`,
and `process()`. `kernel/core/src/aspace.rs` keys its process maps by
`ProcessId` and the user-stack span by `TaskId`, with an incrementally
maintained per-process committed total so a multi-threaded process reports its
whole stack footprint without the registry needing a thread index. Retyped to
`ProcessId` because the resource is the process's: IRQ bindings, IPC port and
endpoint ownership and message provenance, the virtio host's completion wait,
shared-memory mappings, console/pty foreground ownership, the process-wait
table, and signal targets.

### T3a — the process address space

`kernel/core/src/procspace.rs` holds `ProcessSpace`: a `SpinLock` over the boxed
`LiveUserSpace`, behind an `Arc` each thread's `ThreadControl` clones. The
per-CPU slot publishes a borrowed `*const ProcessSpace` (no refcount traffic on
the switch path) and `with_current_live_space` reborrows it and takes the lock,
so the old "only one task per space" `&mut` argument is replaced by real
exclusion. The pointer's `Arc` provenance is a *type* invariant —
`LiveSpacePtr::borrowed` is its only constructor — because
`current_process_space` reconstructs an owning handle out of the publication's
own strong count; a publisher able to name a `ProcessSpace` that was never
inside an `Arc` turns that increment into an out-of-bounds write
(`plans/OPEN-DEFECTS.md` D45). A dispatch step that refuses a task publishes
nothing, so no per-CPU slot can name a control block the scheduler then reaps. A **spin** lock, not a `SleepLock`: the demand-paging fault resolver
runs with IRQ masked and cannot park. The discipline is *never park while holding
it*, which holds because every `LiveUserSpace` method is park-free (frame alloc +
page-table write + TLB flush) and file content is read into a kernel buffer
*before* the mapping call. Lock order: `ProcessSpace` before the address-space
registry.

`ProcessSpace` is the process's whole shared *execution context* — the locked
live space plus its `ProcessResume` hook and the port's `EnterUser` handle — so
`thread_create` reaches all three from one handle.

### T3b-k — the kernel half

**ABI:** `SyscallNumber` 109–112 with their `SyscallSpec` rows, the
`THREAD_STACK_DEFAULT` selector, `LimitKind::Threads`, the generated C stubs.
`thread_create(entry, arg, stack_len, tls_base, clear_on_exit)` returns the new
TID. All four are unprivileged (decision 5a's reasoning); the lifecycle pair is
audited, the futex pair is not.

**Arch HAL:** `UserEntry::tls_base`, with each port programming its own psABI
thread-pointer register — aarch64 frames `TPIDR_EL0` in `vectors.s` at offset 800
(inside the existing 816-byte frame's tail padding), riscv64 seeds `tp` (T1
already framed it), and x86_64 writes `IA32_FS_BASE` at entry and **reloads it
from the switch-in hook**, because that register is privileged and the kernel
therefore owns the value. Each port exposes a `'static USER_MODE` singleton, so a
thread created later is entered through the same transition its process's first
thread was.

**`kernel/mem`:** `AddressSpace::unmap_single_page` and
`LiveUserSpace::unmap_kernel_stack_guard`, so a thread's kernel-stack guard page
can be re-expressed as unmapped in the process's **live** root. The first thread
gets that during the image build while its root is still inactive; a thread
created later has no such moment, and without it an overrun of its kernel stack
would silently corrupt the neighbouring arena region (`ArenaStack` has no canary
— the unmapped page *is* its guard).

Plus `LiveUserSpace::reserve_anonymous_growable` for decision 5a's user-stack
reservation: address space with no up-front no-overcommit charge. `LiveSpace`
records such a region's base, and a release credits back nothing rather than the
`page_count - resident` a fully-charged `mem_map` region owes — crediting pages
that were never committed would hand the pool budget it never received and let
the machine overcommit.

**`kernel/core`:** decision 9's cloneable per-process `ProcessResume` hook and
`UserThreadEntry`, so no per-arch producer is involved in creating a thread;
`threads.rs` (the reservation, the recorded span, the born-parked ordering, and
the teardown); `futex.rs` (a per-CPU-sized bucket array of on-demand `WaitQueue`s,
`wake_n`, enrolment in the timed sweep and the nearest-deadline arming, and
per-process key teardown — the bucket table is fixed by its first use, so boot
sizes it before any thread can wait and a later sizing is refused rather than
stranding a live key's waiters in a bucket no waker looks in); the four
handlers, each validating its addresses through the fault-aware copy boundary;
and decision 10's group fan-out.

**Defects fixed on the way:** `copy_in_user` offered the compressed-tier
(`ramzip`) resolver **nowhere**, so a syscall staging a buffer from a parked page
got `resolve_anon_fault`'s freshly zeroed frame instead of the data — silent
corruption. Both copy directions now share one `resolve_user_miss` that offers
the resolvers in the same order the hardware fault path does, and `copy_out_user`
was added as its write counterpart (the thread-exit clear word cannot tolerate a
dropped store: a joiner would wait forever).

The stack release additionally publishes the region's teardown into the process's
registry snapshot, as `mem_unmap` does. Without it a *surviving* sibling's syscall
buffer still resolved through the dead thread's stack pages, so a frame the
allocator had since handed to another principal was readable and writable across
the isolation boundary. The release runs on the dying thread's own CPU, the only
context whose published live space is that process's.

### T3b-u — the userland half and the verticals

**`lib/rt`:** `thread.rs` — `Thread::spawn` and `thread::Builder`
(`stack_bytes`, `thread_pointer`), `JoinHandle::join`/`detach`, over a
**rendezvous cell** carrying the kernel's clear-on-exit word, the two-party
handshake that decides which side owns the outcome, and the outcome slot. The
cell's address crosses into the kernel, so a cell is never returned to the heap:
it is recycled, and only once its word reads zero, which is the proof the
kernel's one write has already happened. That is what makes a *detached* thread
cost nothing permanent. The runtime owns no stack memory (decision 5a), and the
thread pointer defaults to the cell — per-thread, stable, and outliving the
thread — so a thread is psABI-conforming before a TLS layer exists.

`sync.rs` — a three-state futex `Mutex` (free / held / held-with-waiters, so a
release pays for a wake syscall only when someone is parked, and contention parks
rather than spins; no poison state, because a panic ends the process) and a
`Condvar` over a monotonic notification counter read *before* the mutex is
released, which is what closes the lost-wake-up race.

**Defects fixed on the way** (§2.18): the process teardown could run while a
sibling thread was still executing on another CPU, freeing its page-table root
from under it — `exit` reclaimed immediately after stopping siblings even when one
was reported still-executing, the fault kill did not fan out at all, and the
gate-deferred kill reclaimed the whole process for whichever thread reached its
boundary first. All four death paths (plus the signal terminate, which was
already correct) now funnel through one landing rule,
`KernelSyscallHandlers::land_thread_down`: retire this thread's per-thread state
and tear the process down only when it was the group's **last**. The terminal
status is carried through both deferral channels (the kill gate and the
running-kill set) as a status rather than a signal, so a sibling's synthesised
`128 + n` can never overwrite a real `exit` code. `thread_create` additionally
refuses the second thread of a process whose signal producer cannot stop a group,
since such a group could never be torn down.

The driver-store unload stopped only the leader task, so a user-space driver that
had created threads left them running with their capability record gone and no
path left to stop them — an unkillable runaway holding a core. It now drives the
whole group through the same retire-then-defer shape, so the process teardown
still lands exactly once, when the last of them is down.

**Tests:** handler-level host tests for the four syscalls (every `thread_create`
address and bound refusal, the `futex_wait` compare-and-block, the `futex_wake`
alignment gate), for the landing rule, for the released stack leaving nothing
translating in the surviving process's snapshot, for growth charging headroom only
for the pages it backs, and for the driver unload stopping every thread of its
group; the pure policy is host-tested beside its code. End to end:
`tests/integration/threads_program` in six argv-selected
roles, driven by `threads_qemu_{aarch64,riscv64,x86_64}` over the production
dispatch hook — N threads incrementing a shared counter under a futex mutex and
each joined for its tally, a `Condvar` rendezvous that completes only because the
wait genuinely parked, each thread reading its own magic through its psABI thread
pointer before and after a trap, a thread that ends itself releasing its joiner,
and a group `exit` reaching a sibling parked in the kernel.

**wasm32** is an honest declared n/a: it has no user mode at all (no
`userentry.rs`, no `context_hal.rs`), so `thread_create` fails closed with
`NotImplemented` exactly as `spawn` already does.

## Non-goals

- Do NOT add a `v2` of any type or a compatibility shim: `abi-v1` is unfrozen,
  so every change is made in place with all callers updated (§2.13).
- Do NOT add thread-local *storage* (`PT_TLS` loading, per-arch variant
  layouts, `__tls_get_addr`) here — decision 8. The kernel's per-thread
  thread-pointer contract is what this plan owes; the storage layout above it is
  its own work.
- Do NOT add process-shared (shm-backed) futexes — decision 6.
