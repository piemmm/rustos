# The parser sandbox: minimum-capability worker processes

`AGENTS.md` §19.5 requires every parser of untrusted input to run in a
minimum-capability sandbox process. This page documents the kernel
primitive that makes such a process exist: the **sandbox spawn mode**,
requested by a flag in the spawn attach block and enforced entirely
kernel-side. The user-space seam that hands bytes to a sandboxed parser
and receives the typed result (with crash containment and worker
replacement) builds on this primitive and is staged in
`.junie/fstree-next-plan.md` S8b.

## Requesting a sandbox

A spawn becomes a sandbox spawn by setting `SPAWN_FLAG_SANDBOX` in the
`SpawnAttach` block's `flags` word (`lib/abi/src/process.rs`; C callers
use `ROS_SPAWN_FLAG_SANDBOX`). The flag can only ever *narrow* the child,
so requesting it needs no capability.

A sandbox block is canonical only when nothing ambient can flow into the
child, and `SpawnAttach::parse` refuses any other shape fail-closed —
one definition shared by the kernel's staging path and every userland
encoder:

- **Every fd wire is explicit.** Each of the four standard-descriptor
  wires must be `Closed` or `Handle` — never `Inherit` or `InheritSlot`.
  The only channels a sandbox holds are the descriptors its parent
  deliberately handed over (typically a pipe pair).
- **No credential switch.** `target_uid` must be `SPAWN_UID_INHERIT`.
- **No console.** The console selector must be `CONSOLE_INHERIT`; a
  console index would attach console-backed streams, which a sandbox
  never receives.
- **No reserved flag bits.** Any undefined `flags` bit refuses the block.

## What the kernel enforces

Three layers, each fail-closed, each with its own tests:

1. **Empty capability sets, structurally** (`kernel/sec`). The spawn
   admit path brands the child's `TaskCapabilities` with
   `as_sandboxed()`, which discards the user grant, the manifest
   request, and the effective set — whatever the program's manifest
   asked for. Because all three sets are dropped, no later re-derivation
   can resurrect a capability. `delegate` and `apply_token` refuse a
   sandboxed target outright (`PermissionDenied`, audited as a widening
   attempt) before looking at the payload, so not even a validly signed
   token can land capabilities on a sandbox.
2. **A closed syscall allow-list** (`kernel/syscall`). The dispatcher
   refuses every syscall from a sandboxed task except
   `sandbox_allows`'s list, *before* the per-syscall capability check
   and before any handler runs:

   `yield`, `exit`, `stream_read`, `stream_write`, `fs_read`,
   `fs_write`, `fs_close`, `mem_map`, `mem_unmap`

   These are exactly the self-scoped and descriptor-scoped operations a
   worker needs: run, block on and talk over the wired descriptors, and
   manage its own heap. Everything that names an object outside the
   task — a path (`fs_open`), an IPC endpoint, a resource reference, a
   process (`spawn`/`signal`/`wait`), a device, system state — is
   refused, so a compromised parser cannot even probe those surfaces.
   Each denial is audited with the stable `SyscallPermissionDenied`
   event. Widening the list is a security decision held to the
   capability-minimalism bar, and the exact list is frozen by a unit
   test.
3. **Descriptor-scoped I/O only.** `fs_read`/`fs_write`/`fs_close` and
   `stream_read`/`stream_write` operate on the caller's own descriptor
   table — authority the parent established at spawn — and a
   console-backed stream additionally requires `CAP_CONSOLE_READ`/
   `CAP_CONSOLE_WRITE` in-handler, which a sandbox (empty set) can never
   hold. With canonical wires a sandbox has no console-backed stream in
   the first place.

The parent keeps full lifecycle authority over its child: `wait` reaps
it, `signal` can kill it, and a crashed worker is observed exactly like
any other abnormal child exit. Nothing about the sandbox brand weakens
the parent's side.

## What this deliberately is not

- It is not a general jail configuration surface: there is exactly one
  sandbox shape, so review is over one list, not a policy language.
- It is not seccomp-style per-process filter state: the brand is a
  single kernel-side bit on the task's capability record, checked at
  the one existing dispatch checkpoint — no per-syscall filter tables,
  no new hot-path cost for non-sandboxed tasks beyond one boolean read.
- It adds no syscall and no privileged path: the flag rides the
  existing attach block and only ever narrows.

## Test coverage

- `lib/abi`: sandbox block round-trip; refusal of every ambient shape
  (inherit-form wires, uid switch, console index) and of reserved flag
  bits.
- `kernel/sec`: `as_sandboxed` strips all three sets; `delegate` and
  `apply_token` refuse a sandboxed target (empty payload included), and
  the refusals are audited.
- `kernel/syscall`: the allow-list is frozen exactly; an exhaustive walk
  of the whole `abi-v1` table proves every non-listed syscall is refused
  for a sandboxed caller before its handler runs, with the denial
  audited.
- `kernel/core`: an end-to-end spawn with a sandbox attach block admits
  a child whose record is sandboxed and empty despite a manifest that
  requests a capability, with every standard stream closed.
