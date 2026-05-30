# Per-task capability registry

`kernel/sec::captable` ships two layers of capability state:

* `TaskCapabilities` — one record per task: the intersection of the
  owning user's grant and the task's signed manifest request, plus
  delegate / revoke / signed-token application. This layer landed in
  Stage 2.4 and is documented in
  [`architecture/security.md`](../architecture/security.md).
* `CapTable` — the `TaskId → TaskCapabilities` lookup the syscall
  dispatcher consults after the per-CPU current-task slot
  ([`Scheduler::current_task`](../architecture/scheduler.md#current-task-slot))
  has named the caller. This is Stage 2.7 follow-up (f2) and is
  described below.

## Per-task registry

`CapTable` owns a flat `BTreeMap<TaskId, TaskCapabilities>`. It carries
no interior mutability: the owning scope — `KernelState` in
`kernel/core::init` — composes it with the scheduler under a single
lock-ordering policy and provides whichever reader/writer
synchronisation primitive is appropriate. The same shape of access
pattern already drives `Scheduler::tasks` (many concurrent syscall-
context readers, occasional task-creation writers), so a
reader-preferring `lib/sync::RwLock` mirrors that policy.

### Lifecycle

| event                                | registry effect                                       |
| ------------------------------------ | ----------------------------------------------------- |
| task creation (manifest verified, caps derived) | `CapTable::insert(caps)` registers the record  |
| syscall `cap_query` / handler reads  | `CapTable::caps_for(task_id) -> &TaskCapabilities`    |
| syscall `cap_delegate` / `cap_revoke`| `CapTable::caps_for_mut(task_id) -> &mut TaskCapabilities` then mutate via `TaskCapabilities::{delegate,revoke,apply_token}` |
| `Scheduler::exit(task_id)` returned  | `CapTable::remove(task_id)` evicts the record         |

`CapTable::insert` returns the previously-registered record for the
same `TaskId` if any, surfacing the duplicate rather than silently
overwriting. `kernel/sched` does not recycle task ids within a single
scheduler instance (see
[`architecture/scheduler.md`](../architecture/scheduler.md) §
Invariants), so a non-`None` return is an anomaly the caller is
expected to audit / refuse.

`CapTable::remove` returns the evicted record so the caller can zero
out any sensitive capability material in line with the kernel
allocator's "zero-on-free for credential-holding memory" requirement
(`AGENTS.md` §4).

### No ambient authority on lookup

Lookups never widen the stored capability set. `caps_for(task)`
returns `Some(&TaskCapabilities)` only when `insert` has stored a
record for that exact id; there is no implicit grant, no "fall back to
root" branch, and no policy hook that consults the numeric uid.
`TaskCapabilities::derive` enforces the user-grant ∩ manifest-request
invariant on the way in, so the registry's stored value is
already bounded.

### `caps_for` vs `caps_for_mut`

`caps_for` is the predicate the dispatcher uses for `cap_query` and
for the capability checks the IPC entry points apply before any state
is touched (`AGENTS.md` §5.4 step 2 — check capabilities **before**
state). `caps_for_mut` is the entry point the dispatcher uses for
`cap_delegate` / `cap_revoke`: it returns a mutable borrow into the
same record, on which the caller invokes
`TaskCapabilities::{delegate,revoke,apply_token}`. Those methods
preserve the subset-only delegation invariant in `lib/caps` and emit
the appropriate audit events; the registry itself never widens or
synthesises capability state.

## Wiring / Lifecycle

Stage 2.7 follow-up (f4) wires `CapTable` into `KernelState`, the
in-memory record `kernel_main` builds during the init phases. The
table is placed under a reader-preferring `lib/sync::RwLock`, the
same primitive `Scheduler::tasks` uses, so the syscall dispatcher's
hot path (the `cap_query` predicate and the IPC capability checks)
takes only a shared lock:

```text
KernelState {
    scheduler:  Scheduler<A>,
    caps:       RwLock<CapTable>,   // (f4) — composed under the same lock-ordering policy
    arch:       Arc<A>,
    audit_sink: &'static (dyn Sink + Sync),
    // ...
}
```

Lock-ordering policy: every dispatcher entry point identifies the
caller through the per-CPU current-task slot **first**, then takes
either `caps.read()` (for `cap_query` and the IPC capability checks)
or `caps.write()` (for `cap_delegate` / `cap_revoke`). The lock is
held only for the duration of the in-registry mutation; the
`TaskCapabilities` reference handed to `CallerContext` lives inside
the read-guard, so a concurrent `cap_revoke` waits until the active
syscall returns. `AGENTS.md` §5.4 step 1 ("identify the caller")
explicitly forbids re-locking mid-dispatch: the registry is consulted
exactly once per syscall.

`KernelState` is one-shot `Box::leak`'d during the `Syscall` init
phase so the `KernelDispatchHook` published into the
[`DispatchCallbackSlot`](../architecture/kernel.md#syscall-registration-phase)
can borrow `&state.caps` for the lifetime of the running kernel. The
leak is immutable after publish and is therefore **not** a global
mutable static (`AGENTS.md` §2.1); the interior `RwLock` is the only
sanctioned mutation site. The kernel never returns from
`kernel_main`'s halt, so the leak is a one-shot publish, not a
resource leak in any operational sense.

A boot path that finds the slot already holding a hook (programmer
error: double `kernel_main` entry; test harness pre-population)
surfaces `InitError::DispatcherAlreadyInstalled` under
`phase = "syscall"`, `cause = "syscall_dispatcher_already_installed"`,
and halts. No silent recovery — `AGENTS.md` §5.4.5.

## Out of scope for Stage 2.7 follow-up (f2)

* User-space task creation. The follow-up does not add a user-space
  loader; `CapTable::insert` is exercised by tests and by the future
  init(1) loader (Stage 6).
* The signing-key authority for capability tokens. `lib/crypto` already
  ships the verifier; the per-installation authority key generation
  lives in the installer (Stage 8 / `AGENTS.md` §11.5).
* IPC named-port registry. See the deferred items in PLAN.md
  "Stage 2.7 follow-up" — the `ipc_send`/`ipc_recv` handlers map to
  `Errno::NotFound` until the registry lands.
