# User-fault kill diagnostics (identity, cause, offset)

When a user task takes a memory fault the kernel cannot resolve — a wild
pointer, a store to a read-only file mapping, a stack growth past the
task's limit, a page at or past end-of-file — the fault is fatal to
**that task and only that task**. The kernel isolates the crash, records
a crash exit (`128 + SIGSEGV`, the `139` `wait` status a parent reaps),
reclaims exactly what a clean exit reclaims, and audits the kill with a
stable event id. This page describes what that audit record contains, the
deliberate address-leak policy that governs it, and why the diagnostics
cost a running program nothing.

It is the survivable, per-task sibling of
[kernel panic diagnostics](./panic-diagnostics.md): a panic is fatal and
halting, so its dump may carry *kernel* addresses; a user-fault kill is
survivable — the rest of the system keeps running — so it must **never**
carry raw *user* addresses that would let a process probe address-space
layout by faulting repeatedly.

## What a user-fault kill record contains

The fault-kill path emits exactly one `AuditEvent::TaskFaultKilled` record
through the audit sink. Its fields:

| Field           | Meaning                                                        |
| --------------- | -------------------------------------------------------------- |
| `task`          | The reusable scheduler id of the killed task.                  |
| `name`          | The task's kernel-attested executable basename — *which* program crashed. Never caller-supplied bytes. |
| `proc_id`       | The CSPRNG-minted process-instance identity, so the crash stays correlatable across the log after the scheduler recycles the `task` id. |
| `write`         | `true` if the fatal access was a store, `false` if a load — a wild write and a wild read are sharply different bug classes. |
| `fault_class`   | A coarse class of *why* the resolver refused the access (below). |
| `fault_offset`  | A coarse, non-leaking locality bucket (below).                 |
| `region_offset` | Present only when `fault_offset` carries a distance: the *distance* from a fixed anchor, never an absolute address. |

`fault_class` is one of:

- `stack_limit` — stack growth was refused because the task's `StackBytes`
  soft limit is exhausted.
- `stack` — growth room the resolver could not back (e.g. frame
  exhaustion).
- `file_region` — a miss inside a live file mapping the resolver refused
  (e.g. a page at or past end-of-file, the `SIGBUS` analogue).
- `anon` — a miss inside a reserved anonymous region the resolver could
  not back (deterministic OOM fatal to the task alone).
- `wild` — an address outside every mapping the task owns.

`fault_offset` refines `fault_class` with *where*, without publishing the
address:

- `null_page` — within the first page (offset measured from virtual
  address 0): a null-pointer dereference, the most common `wild` cause.
- `below_stack_guard` — just below the stack guard page under the reserved
  span: a classic overflow *past* the guard.
- `region` — a small, bounded run past the end of a specific mapping the
  task owns; `region_offset` is *how far past*, relative to that region's
  end.
- `wild` — genuinely far from every mapping; no meaningful offset, so no
  `region_offset` is emitted.

The locality classification is a single definition
(`AddressSpaceRegistry::classify_fault_locality`), computed
allocation-free on the already-dying task, so the fault path adds nothing
to any running program.

## Address-leak policy

The record deliberately omits the raw *user* faulting address. The shared,
hash-chained audit log must not publish address-space layout: a survivable
process that could read back the exact address of each fault would have an
ASLR/layout oracle. Instead the log carries only:

- the identity (`name`, `proc_id`) — program state, not layout;
- the cause *class* (`fault_class`, `write`) — a bug class, not an address;
- a coarse locality (`fault_offset`) and, at most, a *distance from a fixed
  anchor* (`region_offset`) — how far past a region's end / below the
  guard / from address 0, never *where* the region, guard, or task lives.

This is the same line [panic diagnostics](./panic-diagnostics.md) draw
from the other side: a halting kernel panic may print kernel addresses
because the machine is stopping; a survivable user-fault kill may not print
user addresses because the process (and its peers) keep running.

## Cost

Everything above runs **only** on a task that is already dying — inside the
fault resolver, on a task that will never execute another instruction. A
living program pays nothing: the kernel reads facts it already holds
(`name`/`proc_id` from the task's capability record, `write` from the
resolver, the mapping tables it already maintains) and classifies the
locality with a handful of comparisons. There is no change to how running
programs are built or scheduled, and no work on any hot path (syscall
dispatch, the capability check, the scheduler, the allocator). The record
therefore ships in **every** image, production and debug alike.

## The capability-gated crash record

Beyond the coarse audit record, the fault-kill path builds a richer
post-mortem **crash record** for a privileged debugger — the TAIRiX
analogue of a Linux kernel oops. It is read back through the System
Information API's `SysinfoQueryId::CRASH_RECORD` query, which is gated on
`CAP_SYSINFO_KERNEL` (the same kernel-introspection capability as the
memory-stats queries) and audited on every read. It never touches the
shared, hash-chained audit log, because it carries the one datum that log
must not: the absolute general-purpose register **values**.

Each `CrashRecord` carries:

- the faulting identity (`proc_id`, numeric `pid`, `name`, `uid`, `gid`);
- the cause codes (`fault_class`, `fault_bucket`, and the same non-leaking
  `fault_offset` distance the audit record uses, plus the `write` flag);
- the faulting **program counter** and a **frame-pointer backtrace**, each
  expressed as a **load-relative offset** from the task's PIE load base
  (recorded per task by the spawn path) when the base is known — never an
  absolute user address, so even this privileged record is not an ASLR
  oracle;
- the register file: `sp`, `fp`, and the named general-purpose registers,
  which *are* absolute — the privileged-debugger datum the whole record is
  capability-gated for.

The backtrace is produced by the **one** shared, bounds-checked, monotonic,
depth-capped unwinder (`tairix_arch_api::backtrace::walk`) — the same
definition the kernel-panic path uses — reading the crashing task's user
stack through a **fallible, `copy_in`-backed reader** (`crate::crash::UserStackReader`).
A corrupt, unmapped, or reclaimed user frame pointer makes the read return
`None`, which ends the walk cleanly: the kernel never dereferences an
untrusted user pointer directly, so a fault inside the fault handler is
impossible. Each architecture port captures the faulting register frame
from the state it already saved at trap entry (aarch64 from the EL0
exception frame, x86_64 from the `#PF` saved-GPR block plus the interrupt
frame's user `rsp`, riscv64 from the trap frame — whose vector was extended
to persist the callee-saved set, including the `s0` frame pointer, so its
backtrace works too) and threads it, by shared reference, through the
architecture-neutral user-fault resolver ABI. The user-fault *terminator*
ABI — the one an unresolvable ring-3 exception (a wild jump, an illegal
instruction) reaches — threads the same frame, so a task killed by a bad
instruction carries the same record as one killed by a bad access.

### Offline symbolication

Because the `pc` and every frame are load-relative offsets, a developer
resolves them offline against the **unstripped** binary with the same
`addr2line` workflow [panic diagnostics](./panic-diagnostics.md) documents
— no on-target symbol table is needed in a production image. Debug images
may additionally carry the symbol table for on-target `fn+0x40` rendering
(a size cost, not a CPU cost). The record ships in every image at zero
running-program cost; only the debug symbol table is image-conditional.

## The terminal breadcrumb

The reaping session states the crash on the terminal so a user whose
command segfaults sees *why* without trawling any log. When the shell
(`elsh`) reaps a foreground or background child that exited with the `139`
fault-kill status, it writes a concise line to `stderr`:

```
shell: <name>: killed by fault (segmentation fault)
```

and keeps `$?` at `139` for scripts to test. The breadcrumb names the
class every user understands and **never** carries an address, register,
secret, or capability token — the precise cause class and the backtrace
stay behind the `CAP_SYSINFO_KERNEL` crash-record query. Where no terminal
consumer exists (a daemon or detached session), the observing component
records the termination through `lib/log` instead, best-effort on both
channels.
