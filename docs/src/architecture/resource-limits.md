# Resource limits and scalability

RustOS sizes resource *capacities* from the hardware it discovers at boot
and grows them on demand; it never hard-wires a `const` ceiling that caps a
large machine or wastes a small one (`AGENTS.md` §24). On top of those
discovered, growable defaults a principal may *impose* a lower ceiling on
itself or its children — the RustOS equivalent of POSIX `ulimit`/`rlimit`.

This page describes the binding §24 contract and its staged build-out. The
ABI surface (`LimitKind`, `ResourceLimit`, the `rlimit_get`/`rlimit_set`
syscalls, and the `CAP_RLIMIT_RAISE` capability), the **kernel enforcement**
of it, the **`ulimit` shell command**, and the **System Information limits
query** are **landed**; the growable kernel-stack arena is staged behind them
(see *Status* below).

## Capacities scale; security bounds stay fixed

A *capacity* — how many tasks, threads, CPUs, open handles, memory regions,
or stack bytes the system or a process may use — is **derived** from the
discovered hardware (the §18 hardware tree: RAM window, CPU/hart count) and
**grows on demand** rather than failing at a literal ceiling (`AGENTS.md`
§24.1). Each scalable resource has a single default *policy* (a function of
the discovered hardware) tuned to be sensible for both an interactive desktop
and a busy server (§24.2).

This must never be confused with the fixed security/format *bounds* on
untrusted input (§24.4), which stay deliberately fixed and fail closed:
parser parameter/byte caps (`lib/vt` `MAX_PARAMS`/`MAX_STRING`, `lib/fdt`
`MAX_DEPTH`, the `lib/svg` caps), on-disk/wire format constants (ext4/FAT32,
RustFS, ABI record sizes), and the charter-blessed §22 RNG output reserve.
Turning a security bound into a growable capacity, or a capacity into a frozen
ceiling, are both defects — when in doubt, stop and ask (`AGENTS.md` §15.7).

## The limit ABI

The settable facility is defined in `lib/abi/src/rlimit.rs`, held to the same
ABI discipline as the syscall table (`AGENTS.md` §9): versioned, hashed, and
frozen from the first release.

- **`LimitKind`** is the closed, versioned set of resources a limit can
  govern. Today: `AddressSpaceBytes` (the `mem_map` capacity), `OpenStreams`
  (the descriptor-table capacity), `Processes` (the `spawn` fan-out), and
  `StackBytes` (the per-task stack). Discriminants never move; a new resource
  takes the next free discriminant and bumps `LimitKind::COUNT`.
- **`ResourceLimit { soft, hard }`** is the soft/hard pair, each a `u64`.
  `RLIMIT_INFINITY` (`u64::MAX`) means "no ceiling imposed", leaving the
  resource governed only by the discovered, growable default policy. The
  well-formedness invariant is `soft <= hard`; the decoder fails closed on a
  malformed pair or a short buffer.
- **`ResourceLimit::intersect`** is the never-widen combinator used to inherit
  and delegate a limit: a child or delegate receives the minimum of the soft
  bounds and the minimum of the hard bounds, so neither can exceed the
  inherited ceiling (the §24.3 rule, mirroring capability delegation §5.2).

## The `rlimit_get` / `rlimit_set` syscalls

`rlimit_get` (no. 17) and `rlimit_set` (no. 18) carry a `LimitKind`
discriminant (`u32`) and a `ResourceLimit` through a 16-byte user buffer (see
[the syscall page](./syscalls.md)). Both are **ungated at the dispatcher** —
reading one's own limit and *lowering* a bound are the unprivileged
own-process baseline (`AGENTS.md` §16.6). `rlimit_set` performs the finer
check **handler-side**: a request that *raises* a hard bound above the
inherited ceiling is refused with `PermissionDenied` unless the caller holds
**`CAP_RLIMIT_RAISE`** (`AGENTS.md` §24.3). `rlimit_set` is audited (it
changes enforced policy); `rlimit_get` is a pure observer and is not.

The first-party Rust wrappers are `rustos_rt::rlimit_get` /
`rustos_rt::rlimit_set`; non-Rust programs call `ros_sys_rlimit_get` /
`ros_sys_rlimit_set` over the generated `rustos_rlimit.h` view.

## Kernel enforcement

The kernel holds each task's effective limits as a `LimitSet` (one
`ResourceLimit` per `LimitKind`) in the per-task `AddressSpaceRegistry`,
alongside the standard-stream descriptor table — both share the per-process
lifecycle (established at spawn, withdrawn at exit) and the same `TaskId` key,
so there is no parallel registry (`AGENTS.md` §2.2). A task with no imposed
limit reads `LimitSet::DEFAULT`: every resource `RLIMIT_INFINITY` for now, the
single place a discovered-hardware default policy slots in later (L3) without a
second code path.

- **`rlimit_get`** validates `kind` against the closed `LimitKind` set, reads
  the caller's *own* effective limit, and copies it out through the validated
  `copy_to_user` boundary. It is keyed by the kernel-trusted `caller.task_id`,
  never a caller-supplied id (§5.4.1), so a process can only read its own
  limits; an unregistered caller fails closed with `BadAddress` (§19.1).
- **`rlimit_set`** validates `kind`, copies the requested limit in (the decoder
  rejects a malformed `soft > hard` pair, fail closed), then applies the §24.3
  rule: lowering — or any change that does not raise the hard bound above the
  current ceiling — is free, while raising the hard bound requires
  `CAP_RLIMIT_RAISE` and is otherwise refused with `PermissionDenied`. The
  authorised limit is stored against the caller's own id. Because the syscall
  is audited, a rejection is logged automatically (§19.4).
- **Inheritance.** When a process is admitted by `spawn`, the child's limit set
  is the parent's intersected against the system default (`LimitSet::inherit`),
  so a child can never hold a bound wider than either the parent's ceiling or
  the default — the never-widen rule, mirroring capability delegation (§5.2).

## The `ulimit` shell command

The `ulimit` builtin in the default shell (`userland/shell/shell`) is the
command-line face of the facility. It reads and imposes the calling
process's own limits over the L1 ABI through an injected
`rustos_shell::LimitStore` seam — backed by `rustos_rt::rlimit_get` /
`rustos_rt::rlimit_set` in the real `Run` binary, and by an in-memory double
in tests, so the parsing and policy logic is exercised without a kernel (the
same `ProcessHost`/`Console` seam pattern the rest of the shell uses). The
shell holds no ambient authority of its own (`AGENTS.md` §4): every check
stays kernel-side, and the `CAP_RLIMIT_RAISE` denial surfaces as an error the
builtin reports rather than hides (§2.9).

Usage:

```text
ulimit [-a] [-H | -S] [<resource> [<value>]]
```

- `ulimit` or `ulimit -a` reports every resource's soft bound (its hard
  bound with `-H`), one aligned line each.
- `ulimit <resource>` reports that resource's soft bound (`-H` for the hard
  bound).
- `ulimit <resource> <value>` imposes the limit. With neither flag both
  bounds are set (POSIX); `-S` sets only the soft bound and `-H` only the
  hard bound, leaving the other as the current limit reads it. `<value>` is a
  decimal byte/count or the word `unlimited` (`RLIMIT_INFINITY`).

`<resource>` is one of the canonical `LimitKind` names
(`LimitKind::name`): `address-space-bytes`, `open-streams`, `processes`,
`stack-bytes`. An unknown resource, an unknown flag, a malformed value, or a
soft bound set above its hard ceiling is rejected without touching the
kernel (fail closed, `AGENTS.md` §2.1/§2.9); the store is never written on a
rejected request.

## The `sysinfo limits` query

Because there is no `/proc`, a principal observes its *own* effective limits
and its current live usage of each through the System Information API
(`AGENTS.md` §16.6), not a virtual file. The query is
`SysinfoQueryId::RESOURCE_LIMITS` (id 7); its response is exactly
`LimitKind::COUNT` `ResourceLimitRecord`s packed back-to-back in `LimitKind`
discriminant order (`RESOURCE_LIMITS_REPORT_LEN` bytes), each carrying the
resource's `kind`, its effective `ResourceLimit { soft, hard }`, and the
caller's current `usage` (bytes for the `*Bytes` kinds, a count otherwise).

The query is **self-scoped** — its answer describes the caller's own task
only — so, like `self_process_list`, it carries no capability gate and is not
audited (§16.6); observing *another* principal's limits would be a separate,
gated query. The kernel exposes no path that bypasses this; `sysinfod` serves
it from the per-task `LimitSet` (effective limits) and the live accounting
behind each `LimitKind` (usage), keyed by the kernel-trusted caller identity.

The command-line face is `sysinfo limits` (alias `rlimits`) in
`userland/shell/sysinfo`: it issues the query and prints one aligned row per
resource (soft, hard, usage), spelling `RLIMIT_INFINITY` as `unlimited`. A
reply of the wrong length fails closed rather than rendering a partial table
(`AGENTS.md` §2.1). Where `ulimit` *changes* a principal's own limits, the
`sysinfo limits` query *observes* limits and usage together.

## Discovered-hardware capacity policies

The kthread kernel-stack capacity is the first capacity converted off a
hand-picked constant onto a §24.2 *policy* (a function of discovered
hardware), under two knobs:

- **Per-task kernel-stack size** (`rustos_kernel_core::KTHREAD_STACK_BYTES`)
  is **release-tuned**, not a single worst-case constant (§24.2): a release
  image — the form that ships — reserves 32 KiB per kthread kernel stack,
  while an unoptimised debug build keeps the proven-ample 64 KiB its deeper
  frames need. Both are whole 4 KiB pages so the guard page below the usable
  region lands on a clean boundary in either profile. Halving the release
  reservation doubles how many guarded stacks a given arena block holds — the
  server profile's win.
- **Guard-arena size** (`rustos_kernel::mem_map::stack_arena_bytes`) is
  **derived from the discovered RAM window**, not a fixed 2 MiB block (§24.1).
  The boot path reserves roughly 1/64 of discovered RAM for kthread kernel
  stacks, clamped to `[2 MiB, 64 MiB]` and rounded down to a whole 2 MiB
  block (so every guard page still becomes its own L3 leaf after
  `prepare_guard_arena`). A 64 MiB embedded board floors at one 2 MiB block
  (tens of stacks); a 1 GiB desktop gets 16 MiB (hundreds); a large server
  caps at 64 MiB (over a thousand) rather than reserving an unbounded slab up
  front. A RAM window too small to carve even one block degrades to no arena
  and the software-canary `BoxStack` fallback (fail closed, §2.17).

Growing the arena *past* its policy size on genuine exhaustion — chaining a
fresh, independently block-split arena rather than failing over to a
`BoxStack` — is the staged growable-arena follow-on (L3b), as is sizing the
per-arch CPU/hart arrays from §18 discovery.

## Status

- **L1 — ABI (landed).** `lib/abi` `LimitKind` / `ResourceLimit` /
  `RLIMIT_INFINITY`, the `rlimit_get`/`rlimit_set` syscalls, the
  `CAP_RLIMIT_RAISE` capability, the `abi-sys` C stubs, the `lib/rt`
  wrappers, and the generated C header. The dispatcher routes both syscalls;
  the kernel handler default fails closed with `NotImplemented` until L2.
- **L2 — kernel enforcement (landed).** Per-task `LimitSet` storage in the
  address-space registry, inheritance on spawn intersected against the default
  policy (never widened), the typed-`Result` denial-and-audit path, and the
  `CAP_RLIMIT_RAISE` gate on raising a hard bound. The `rlimit_get`/`rlimit_set`
  handlers are wired in `kernel/core`.
- **L3a — discovered-hardware capacity policies (landed).** The release-tuned
  per-task kernel-stack size (`KTHREAD_STACK_BYTES`, 32 KiB release / 64 KiB
  debug) and the RAM-window-derived guard-arena policy
  (`stack_arena_bytes`, ≈1/64 of RAM clamped to `[2 MiB, 64 MiB]`,
  2 MiB-rounded). See *Discovered-hardware capacity policies* above.
- **L3b — growable arena + per-arch arrays (planned).** Growing the
  kthread-stack arena *past* its policy size on genuine exhaustion (chaining a
  fresh, independently block-split arena rather than failing over to a
  `BoxStack`), and sizing the per-arch CPU/hart arrays from §18 discovery,
  preserving the §17.2 break-before-make and §4 guard-page invariants.
- **L4a — `ulimit` shell command (landed).** The `ulimit` builtin in the
  default shell over the L1 ABI, through the injected `LimitStore` seam
  (`RtLimitStore` over `rustos_rt::rlimit_get`/`rlimit_set` in the `Run`
  binary). See *The `ulimit` shell command* above.
- **L4b — `sysinfo` limits query (landed).** The
  `SysinfoQueryId::RESOURCE_LIMITS` System Information query (§16.6) returning
  one `ResourceLimitRecord` per `LimitKind` (effective soft/hard bound + live
  usage), self-scoped and ungated; served by `sysinfod` over the per-task
  `LimitSet`, with the `sysinfo limits` command-line face. See *The `sysinfo
  limits` query* above.
