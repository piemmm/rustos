# Resource limits and scalability

RustOS sizes resource *capacities* from the hardware it discovers at boot
and grows them on demand; it never hard-wires a `const` ceiling that caps a
large machine or wastes a small one (`AGENTS.md` §24). On top of those
discovered, growable defaults a principal may *impose* a lower ceiling on
itself or its children — the RustOS equivalent of POSIX `ulimit`/`rlimit`.

This page describes the binding §24 contract and its staged build-out. The
ABI surface (`LimitKind`, `ResourceLimit`, the `rlimit_get`/`rlimit_set`
syscalls, and the `CAP_RLIMIT_RAISE` capability) is **landed**; the kernel
enforcement, the growable kernel-stack arena, and the `ulimit` shell command
are staged behind it (see *Status* below).

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

## Status

- **L1 — ABI (landed).** `lib/abi` `LimitKind` / `ResourceLimit` /
  `RLIMIT_INFINITY`, the `rlimit_get`/`rlimit_set` syscalls, the
  `CAP_RLIMIT_RAISE` capability, the `abi-sys` C stubs, the `lib/rt`
  wrappers, and the generated C header. The dispatcher routes both syscalls;
  the kernel handler default fails closed with `NotImplemented` until L2.
- **L2 — kernel enforcement (planned).** Per-task limit storage, inheritance
  on spawn, intersection (never widened) on delegation, the typed-`Result`
  denial-and-audit path, and the `CAP_RLIMIT_RAISE` gate on raising a hard
  bound.
- **L3 — growable capacities (planned).** A growable kernel-stack arena and
  discovered-hardware sizing for the per-arch CPU/hart arrays, preserving the
  §17.2 break-before-make and §4 guard-page invariants, plus a release-tuned
  per-task stack size.
- **L4 — `ulimit` + `sysinfo` (planned).** The `ulimit` shell command over the
  L1 ABI and a System Information query (§16.6) exposing effective limits and
  live usage behind the appropriate capability.
