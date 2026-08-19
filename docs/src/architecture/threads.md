# Threads within a process

This page is the source of truth for TAIRiX's **lightweight threads**: two or
more flows of control inside one process, over one address space, one heap, one
descriptor table, and one capability record. It records the design fixed in
[`plans/THREADS.md`](../../../plans/THREADS.md) and is the companion to the
[process model](./multitasking.md), the [syscall
surface](./syscalls.md), the [memory page](./memory.md), and
[`AGENTS.md`](../../../AGENTS.md) §4 (kernel rules), §5 (the security model),
§17.1 (the pluggable scheduler), and §24 (resource limits).

## The model: a process is a thread group

A process is a **thread group** named by its leader's `TaskId` — the `tgid`
model. That id is the PID. Each thread is a scheduler task of its own with its
own `TaskId` (the TID), its own kernel stack, its own user stack, and its own
psABI thread pointer. There is no second id space: a PID *is* a TID, the leader's.

What a thread has of its own, and what belongs to the group:

| Per thread | Per process (shared by every thread) |
|---|---|
| Scheduler task, priority, run state | Address space and every mapping in it |
| Kernel stack (guarded arena slot) | Capability record, credentials, attested name |
| User stack + its unbacked guard page | Descriptor table and standard streams |
| psABI thread pointer | Resource limits, working directory, I/O counters |
| Signal-intake and kill-gate state | IPC ports, IRQ bindings, shared memory, wait-sets |

The single shared capability record is a **security** decision, not a
convenience: a `cap_revoke` by one thread binds its siblings immediately,
because there is only one record to revoke from. A per-thread copy would make
revocation incomplete.

`kernel/sec`'s `CapTable` holds one record per `ProcessId` plus a
`thread → process` alias map, so `caps_for(thread)` resolves through it and the
capability snapshot the syscall dispatcher already takes yields the caller's
process id with no extra lookup and no extra lock.

## Creating a thread

`thread_create(entry, arg, stack_len, tls_base, clear_on_exit)` (`abi-v1`
syscall 109) returns the new TID. It is **unprivileged**: the new thread runs in
the caller's own hardware-isolated address space under the caller's own
capability record, so it grants no authority over anything else — the same
reasoning that makes `mem_map` unprivileged. It is audited, because a new
schedulable principal is a lifecycle event exactly as `spawn` is.

### The kernel owns the stack

There is no caller-supplied stack base. The kernel reserves `[guard | stack]`
out of the process's own anonymous window, records **only** the stack half as a
growable span, and leaves the guard page reserved-but-unrecorded — so a fault
there resolves through neither the stack-growth nor the anonymous handler and
stays fatal. One page at the top is backed eagerly (a span must name a committed
page, and the thread's first instruction may push); everything below faults in
through the same `resolve_stack_fault` path a process's first thread uses, which
already bounds growth by `stack-bytes`.

The reservation is **address space only** (`reserve_anonymous_growable`), unlike
a `mem_map` reservation, which takes no-overcommit headroom for its whole extent
up front. A stack is a span whose depth is unknown and mostly untouched, so its
physical headroom is taken one growth step at a time and fails closed there —
exactly as a process's first thread's growth room does. Charging the whole extent
would bill a thread the RAM of its worst case, which with the default
`stack_len` is the process's entire `stack-bytes` bound. The release therefore
credits back nothing: a page that never faulted in was never committed.

Kernel ownership is what makes three properties hold by construction rather than
by convention:

* **A real guard page.** Under the demand-paged anonymous model every page of a
  `mem_map` reservation is backed on touch, and the page below one is free for an
  unrelated mapping — so a caller-supplied base could not carry a guard page at
  all.
* **No two threads on one stack.** A caller-supplied base would need an overlap
  check against every sibling's span; a kernel-chosen reservation cannot collide.
* **No leaked stack.** A *detached* thread has nobody watching its death, so
  only the kernel can release its stack — which it does at thread teardown.

`stack_len` of `THREAD_STACK_DEFAULT` (`0`) asks for the caller's effective
`stack-bytes` soft bound, so the default is the one live policy rather than a
second constant in userland. A larger request is refused, never clamped.

### Bounds before state

Every bound is checked and every user-supplied address validated before the
first state change:

1. `entry`, `clear_on_exit`, and `tls_base` are each probed by reading a word
   through the fault-aware `copy_from_user` boundary, which proves each names
   mapped memory of the *caller's own* space. A misaligned `clear_on_exit` is
   refused now rather than discovered at thread death, when there would be
   nobody left to report it to.
2. The `threads` and `stack-bytes` bounds are read from one snapshot of the
   process's limit set.
3. The process must have a signal producer that can drive a whole group to its
   stopping point; a group whose siblings cannot be stopped could never be torn
   down, so a build without one refuses the *second* thread rather than
   constructing that process.

Probing `tls_base` is a real defence, not hygiene: on x86_64 the thread-pointer
register is privileged, so the kernel writes that value to `IA32_FS_BASE`
itself — a non-canonical one would `#GP` **inside the kernel** on every switch
into the thread.

The thread is then admitted **parked**, its capability alias, stack span, and
owned-stack record are installed under the returned id, and only then is it
unparked — the discipline `spawn` established, so no CPU can dispatch a thread
before the kernel knows what it is. Every failure path releases the reservation.

### The per-thread thread pointer

`UserEntry` carries a `tls_base`, and each port programs its own psABI register:

| Target | Register | Who owns the value |
|---|---|---|
| `aarch64` | `TPIDR_EL0` | Framed in the thread's own trap frame; a user write is respected |
| `riscv64` | `tp` | Framed as `user_tp` in the thread's own trap frame; a user write is respected |
| `x86_64` | `IA32_FS_BASE` | **The kernel**: the register is privileged (`CR4.FSGSBASE` stays off), so the kernel reloads it from the switch-in hook |
| `wasm32` | — | No user mode at all; `thread_create` fails closed with `NotImplemented` |

Framing is what makes the register per-thread on the two ports where user code
may write it: the frame lives on that thread's own kernel stack, so it is
per-thread and context-switch-safe by construction. There is deliberately **no**
`thread_tls_set` syscall: a thread's initial thread pointer comes from
`thread_create`, and the register being per-thread is a *correctness* property of
creating a thread rather than a feature. Thread-local *storage* — `PT_TLS`
loading and the per-arch variant layouts — is the next layer up.

## The futex

`futex_wait(uaddr, expected, timeout_ns)` / `futex_wake(uaddr, count)` (syscalls
111 and 112) are the one generic blocking primitive userland builds its mutex,
condition variable, and thread join over. A userland lock is a word in the
process's own memory: while it is uncontended, acquiring and releasing it is a
pair of atomic operations and the kernel never hears about it at all. Only a
thread that must actually *wait* enters the kernel, names the word, and parks —
which is what keeps a lock cheap while still letting a waiter give the CPU up
instead of spinning.

* **The key is `(ProcessId, user VA)`.** Address spaces are per-process and
  hardware-isolated, so the same virtual address in two processes names two
  unrelated words, and a key is unforgeable: the process half comes from the
  kernel-attested capability record, never from the caller. Cross-process
  (shared-memory-backed) futexes are a *different* abstraction and deliberately
  absent.
* **Waiters live in the one shared `WaitQueue`** — its FIFO wake-one fairness,
  its `O(log n)` deadline index, and its register-before-retest lost-wake
  discipline. Queues are held in a bucket array sized from the discovered CPU
  count (four buckets per CPU), created on demand per live key and dropped once
  the last waiter leaves, so an idle process holds no futex state.
* **No futex lock is ever held across a scheduler lock.** A waker clones the
  queue handle out from under the bucket lock and releases it before any
  `unpark`, so the bucket locks cannot participate in a lock cycle with the
  scheduler's.
* **Registration precedes the re-test.** `futex_wait` joins the key's queue,
  *then* reads the word; a wake landing in that window finds the waiter already
  registered. A word that no longer holds `expected` returns `WouldBlock` — the
  lost-wake-up race closing, not a failure — and the caller re-tests and retries.
* **A wake is advisory.** One park returns `Ok(0)` and the caller re-tests its
  own condition, which is the contract `futex_wait` publishes; looping inside the
  kernel would hide a genuine wake from the userland lock that has to see it.
* **Timed waits are tickless.** A relative `timeout_ns` becomes an absolute
  monotonic deadline, clamped one nanosecond short of the "no deadline" sentinel
  so a saturating span still names a deadline the sweep fires. The module enrols
  in the kernel's deadline sweep and its nearest-armed-wakeup, so a
  `futex_wait(timeout)` fires even on an otherwise-idle CPU.

Live keys are bounded without a cap of their own: a thread blocks on at most one
futex at a time, so a process can hold no more keys than it has threads, which
`LimitKind::Threads` already bounds.

## Ending a thread, and ending a process

Five things can end a thread, and they all funnel through **one** landing rule
(`KernelSyscallHandlers::land_thread_down`), which is the single point that
decides "the process is gone":

* `thread_exit` (syscall 110) — this thread only;
* `exit(code)` — a **group** exit: every sibling is driven to its stopping point;
* a fatal user fault — likewise a group death, carrying the crash status;
* a terminating signal to the PID — process-directed, so it reaches every thread;
* a driver unload — the device manager tearing down a user-space driver whose
  hardware-tree node vanished, which stops that driver's whole group.

The per-thread half of every one of them is the single `threads::retire` rule:
retire the dying thread's own state (its signal-intake, kill-gate and
running-kill overlays, its user-stack span, its capability alias) and report how
many threads of the group are still live. Dropping the capability alias is what
makes that count fall, so it can only ever fall through one definition. **Only
when it was the group's last thread still executing** is the process's terminal
status recorded for the parent's `wait` and the process reclaimed.

That gate is load-bearing, not tidiness. A process's address space, capability
record, endpoints, and open files may be released only when no thread of it is
executing any longer — reclaiming while a sibling still runs on another CPU
would free its page-table root from under it. A thread that cannot stop
immediately (one inside a syscall, whose own unwind must release a mount lock or
an in-flight block-I/O descriptor; or one still executing in user mode on another
CPU) therefore has the death *deferred* against it, carrying the terminal status
the first dying thread declared, and whichever thread lands last performs the
teardown. Carrying the status through the deferral is what stops a sibling's
synthesised `128 + n` from overwriting a real `exit` code.

`wait` therefore reports a child only when its whole thread group is gone.

A thread that is *not* the group's last also returns its own stack reservation,
which its surviving siblings' syscalls must stop being able to reach: the release
drops the region from the process's registry snapshot as `mem_unmap` does, or a
frame the allocator has since handed to another principal would still translate
for a sibling's syscall buffer. It runs on the dying thread's own CPU, which is
the only context whose published live space is that process's.

### Releasing a joiner

`thread_create`'s `clear_on_exit` word is zeroed and futex-woken by the
**kernel** at thread death, before anything is torn down. That is what makes a
join robust: a thread that never reaches its own epilogue still releases its
joiner, and the joiner then observes that no value was published rather than
waiting forever.

## The userland runtime

`lib/rt`'s `thread` and `sync` modules are the program-facing surface. Nothing in
them spins.

* **`Thread::spawn(body)`** (and `thread::Builder`, which names the stack size or
  the thread pointer) moves a closure into a boxed payload, hands the kernel the
  payload address as the entry argument, and returns a `JoinHandle<T>`.
  `JoinHandle::join` yields the closure's value; dropping the handle detaches the
  thread. The runtime owns **no** stack memory — the kernel reserves and releases
  it — so `spawn` passes only a length and `detach` needs no retired-stack cache.
* **The rendezvous cell** carries the kernel's clear-on-exit word, a two-party
  handshake deciding which side owns the outcome, and the outcome slot. Its
  address crosses into the kernel, so a cell is never returned to the heap: it is
  **recycled**, and only once its word reads zero — which is exactly the proof
  that the kernel's one write has already happened. A detached thread's cell
  therefore costs nothing permanent, and the next `spawn` reuses it.
* **The thread pointer** defaults to that cell: per-thread, at a stable address,
  and outliving the thread, so a thread is psABI-conforming before any
  thread-local storage layer exists.
* **`sync::Mutex`** is a three-state lock word — free, held, held-with-waiters —
  so a release pays for a `futex_wake` syscall only when a thread is actually
  parked on it, and contention parks rather than spins. There is no poison state:
  a TAIRiX program has no unwinder, so a panic ends the *process* and a lock can
  never be left held by a thread that unwound out of its critical section.
* **`sync::Condvar`** holds a monotonic notification counter rather than a waiter
  list. Reading the counter *before* releasing the mutex closes the lost-wake-up
  race: a notification landing in the window between the release and the park
  bumps the counter, and the kernel's own compare then declines to park. A wait
  may return spuriously, so callers re-test their predicate in a loop.

## Limits and observability

`LimitKind::Threads` is a settable soft/hard `rlimit` like any other, so the
per-process thread capacity is policy rather than a compiled-in `MAX_THREADS`,
and its live usage is reported through the existing `ResourceLimitRecord`
path — see the [resource-limits page](./resource-limits.md). Its default is
unlimited, so the real bound is the growable kernel-stack arena failing closed.

## Tests

* Pure policy, host-tested in `kernel/core`: stack-size resolution, the
  guard-page span derivation, the futex key isolation and FIFO wake ordering, the
  deadline arithmetic and sweep, and per-process key teardown.
* Handler level, host-tested: every `thread_create` address and bound refusal,
  the `futex_wait` compare-and-block, the `futex_wake` alignment gate, the
  group-teardown landing rule (a process torn down only once its last thread is
  down, and a landing that owes no `wait` reap), a dying thread's stack leaving
  nothing translating in its surviving process's snapshot, growth charging
  headroom only for the pages it backs, and a driver unload stopping every thread
  of its group.
* End to end, under QEMU on all three native Tier-1 targets
  (`tests/integration/threads_qemu_{aarch64,riscv64,x86_64}`): N threads over one
  address space incrementing a shared counter under a futex mutex and each joined
  for its own tally; a `Condvar` rendezvous that completes only because the wait
  genuinely parked; each thread reading its own magic through its psABI thread
  pointer before and after a trap; a thread that ends itself releasing its joiner
  through the kernel's word; and a group `exit` reaching a sibling parked in the
  kernel.
