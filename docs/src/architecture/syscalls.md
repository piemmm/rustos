# Kernel syscall subsystem

This page documents the architecture-neutral half of the RustOS syscall
ABI delivered by Stage 2.7 of `PLAN.md`: the frozen `abi-v1` table in
`rustos_abi::syscalls` and the generated kernel dispatcher in
`rustos_kernel_syscall::table`. The full rustdoc for those modules is
published alongside this book; refer to the `cargo doc --no-deps` output
in `target/doc/` for the per-item documentation.

Per-architecture entry stubs that marshal real syscall registers into a
`RawArgs` tuple are delivered separately by Stage 3 and are *out of
scope* for this page.

## Cross-checked source of truth

The user/kernel syscall contract is split across two files that
`cargo xtask abi-check` keeps in lock-step:

| Half       | File                                | Owner                        |
| ---------- | ----------------------------------- | ---------------------------- |
| Source     | `lib/abi/src/syscalls.rs`           | Frozen `abi-v1` declaration. |
| Generated  | `kernel/syscall/src/table.rs`       | Dispatcher + table hash.     |

Both halves must ship together. Either half existing without the other
is a hard error; `cargo xtask abi-check` fails the build at that point.

The source half exposes a `&'static [SyscallSpec]` table and a
deterministic byte encoding `ENCODED_TABLE`. The kernel half stores the
SHA-256 fingerprint of that encoding as `SYSCALL_TABLE_HASH`. The
`xtask` recomputes the SHA-256 of `ENCODED_TABLE` and demands that:

1. the byte literal parsed from `kernel/syscall/src/table.rs` matches
   the freshly computed digest, and
2. the linked `rustos_kernel_syscall::SYSCALL_TABLE_HASH` matches it
   too (catches stale `target/` caches).

A negative-path test in `tools/xtask/src/commands/abi_check.rs`
mutates one byte of the on-disk hash literal and asserts the check
fails — proving the diff tool is not a no-op.

## `abi-v1` syscall table

The table is **frozen**; entries may not be re-numbered, removed, or
re-typed. New behaviour ships as `abi-v2`.

| No. | Name           | Args                                    | Returns | Required capability     | Audited |
| ---:| -------------- | --------------------------------------- | ------- | ----------------------- | ------- |
|   0 | `yield`        | —                                       | `unit`  | —                       | no      |
|   1 | `exit`         | `i32 code`                              | `unit`  | —                       | yes     |
|   2 | `ipc_send`     | `endpoint`, `user_ptr`, `len`           | `errno` | —                       | yes     |
|   3 | `ipc_recv`     | `endpoint`, `user_ptr`, `len`           | `errno` | —                       | no      |
|   4 | `cap_query`    | `cap`                                   | `u32`   | —                       | no      |
|   5 | `cap_delegate` | `target_handle`, `user_ptr`             | `errno` | —                       | yes     |
|   6 | `cap_revoke`   | `target_handle`, `cap`                  | `errno` | `CAP_USER_ADMIN`        | yes     |
|   7 | `clock_get`    | —                                       | `u64`   | —                       | no      |
|   8 | `irq_bind`     | `u32 line`                              | `IrqHandle` | `CAP_IRQ_BIND`      | yes     |
|   9 | `irq_wait`     | `IrqHandle handle`, `u64 timeout_ns`    | `errno` | `CAP_IRQ_BIND`          | no      |

### Capability matrix

The dispatcher consults `kernel/sec`'s `TaskCapabilities::has` against
the syscall's `required_capability` before any handler runs. The matrix
is exhaustive — anything not listed below is ungated:

| Capability         | Syscalls gated by it       |
| ------------------ | -------------------------- |
| `CAP_USER_ADMIN`   | `cap_revoke`               |
| `CAP_IRQ_BIND`     | `irq_bind`, `irq_wait`     |

The `CAP_IRQ_BIND` rationale, the wake-up contract, and the failure
modes are documented in
[`security/irq.md`](../security/irq.md).

A future syscall that needs e.g. `CAP_DRV_LOAD` lands as a new entry in
the table and a new row here; existing rows never move.

## Argument validation

Every register slot of `RawArgs` is validated against the `AbiType`
declared in the source table:

| `AbiType`      | Acceptance rule                                                          | Reject `Errno`         |
| -------------- | ------------------------------------------------------------------------ | ---------------------- |
| `Unit`         | Slot must be exactly zero.                                               | `LengthOutOfRange`     |
| `I32`          | Upper 32 bits equal the sign extension of the low 32.                    | `OutOfRange`           |
| `U32`          | Upper 32 bits are zero.                                                  | `OutOfRange`           |
| `U64`          | Any value.                                                               | —                      |
| `Cap`          | `>> 16 == 0` and within `CAPABILITY_ID_MAX` (`= 255`).                   | `OutOfRange`           |
| `UserPtr`      | Non-null. Page-table walks are the owning subsystem's job.               | `BadAlignment`         |
| `Len`          | Fits in `usize` on the target.                                           | `LengthOutOfRange`     |
| `IpcEndpoint`  | Any value (opaque handle).                                               | —                      |
| `Handle`       | Any value (opaque handle).                                               | —                      |
| `Errno`        | Never an input; never appears in `args`.                                 | `OutOfRange`           |

In addition the dispatcher refuses non-zero data in slots **past**
`arg_count` with `LengthOutOfRange`. This prevents a buggy
trampoline from smuggling extra register state past a syscall's
declared arity.

## Error map

| `Errno`                 | When the dispatcher returns it                                                       |
| ----------------------- | ------------------------------------------------------------------------------------ |
| `OutOfRange`            | Syscall number above `SyscallNumber::MAX`, or an argument fails its type check.      |
| `NotFound`              | Number in range but no entry assigned at that index.                                 |
| `PermissionDenied`      | Caller lacks the syscall's `required_capability`.                                    |
| `LengthOutOfRange`      | Trailing slot non-zero, or `Len` exceeds host `usize`.                               |
| `BadAlignment`          | `UserPtr` argument is null.                                                          |
| `AbiVersionUnsupported` | `verify_table_hash` ran at kernel-init time and the recomputed digest disagreed.     |
| *(propagated)*          | Anything else a handler returns is delivered to user space verbatim.                 |

## Audit events

`kernel/syscall` reserves the `5_000..6_000` `EventId` range. Successful
dispatches of *security-relevant* syscalls (`SyscallSpec::audit == true`)
emit `SYSCALL_INVOKED`; refusals always emit, regardless of audit flag.

| ID    | Level | Name                          | When |
| ----: | ----- | ----------------------------- | ---- |
| 5000  | Info  | `SYSCALL_INVOKED`             | A security-relevant syscall passed every check and was dispatched. |
| 5001  | Error | `SYSCALL_PERMISSION_DENIED`   | Caller lacked the required capability. |
| 5002  | Error | `SYSCALL_UNKNOWN`             | Number was outside the `abi-v1` table. |
| 5003  | Error | `SYSCALL_BAD_ARGUMENTS`       | Argument validation failed. |
| 5004  | Error | `SYSCALL_HANDLER_REJECTED`    | Owning subsystem rejected the call. |

Adding an event takes the next free identifier and a new row in this
table.

## Handler wiring (Stage 2.7 follow-up (f3))

The dispatcher trait `SyscallHandlers` is implemented in `kernel/core`
by `KernelSyscallHandlers<'a, A>` (see
`rustos_kernel_core::syscalls`). The struct borrows kernel state and
forwards every call to the owning subsystem; nothing in this layer
re-validates arguments — the dispatcher does that first.

| Handler         | Forwards to                                                                                                   | Error map                                                                 |
| --------------- | ------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `yield_now`     | `Scheduler::yield_current(caller.task_id)`                                                                    | `NoSuchTask → NotFound`, otherwise `OutOfRange`.                          |
| `exit`          | `CapTable::remove(caller.task_id)` then `Scheduler::exit(caller.task_id)`                                     | `NoSuchTask → NotFound`, otherwise `OutOfRange`.                          |
| `ipc_send`      | *deferred* — named-port registry not landed                                                                   | Emits `SYSCALL_FEATURE_UNAVAILABLE` (`feature = ipc_named_ports`) + `NotFound`. |
| `ipc_recv`      | *deferred* — same as `ipc_send`                                                                               | Same.                                                                     |
| `cap_query`     | `caller.caps.has(cap)` mapped to `0` / `1`                                                                    | —                                                                         |
| `cap_delegate`  | *deferred* — user-memory copy-in not landed (the `set_ptr` argument cannot be read until Stage 5 / Stage 6)   | Emits `SYSCALL_FEATURE_UNAVAILABLE` (`feature = user_memory_copyin`) + `NotImplemented`. |
| `cap_revoke`    | `CapTable::caps_for_mut(target).revoke(cap, audit)`                                                           | Unknown `target` → `NotFound`.                                            |
| `clock_get`     | `KernelArch::monotonic_ns(arch.current_cpu())`                                                                | —                                                                         |
| `irq_bind`      | *deferred* — kernel IRQ table + per-handle wait queue not landed                                              | Emits `SYSCALL_FEATURE_UNAVAILABLE` (`feature = irq_subsystem`) + `NotImplemented`. |
| `irq_wait`      | *deferred* — same as `irq_bind`                                                                               | Same.                                                                     |

`KernelArch::monotonic_ns` is a new trait method with **no default
impl**: every architecture port must opt in so an arch that cannot
ship a monotonic clock cannot silently leak that flaw into the
`clock_get` syscall (`AGENTS.md` §5.4.5 — fail closed). The x86_64
port wires it through `apic_timer::Calibration`'s `tsc_per_second`
field, sampled across the same PIT calibration window the LAPIC is
measured over; the conversion goes through
`Calibration::tsc_ticks_to_ns` (saturating).

The two deferred-feature branches return a stable `Errno` and emit
exactly one extra audit record — `SYSCALL_FEATURE_UNAVAILABLE`
(id 4020, see `kernel/core::audit`) — so an external consumer can
tell apart "handler rejected because the call failed" from "handler
rejected because the backing subsystem is intentionally inert"
(`AGENTS.md` §15.1 — announce the deferral, never stub). The
dispatcher's standard `SYSCALL_HANDLER_REJECTED` record is *also*
emitted for syscalls whose `SyscallSpec::audit == true`
(`ipc_send`, `cap_delegate`); `ipc_recv` is unaudited so only the
`SYSCALL_FEATURE_UNAVAILABLE` record reaches the sink.

The Stage 2.7 follow-up tracker in `PLAN.md` records the
named-port registry and the user-memory copy-in path as the two
remaining pieces required to lift these deferrals.

## Dispatcher contract

`Dispatcher::dispatch` is the *only* entry point. Calling it runs the
following sequence — the order matches `AGENTS.md` §5.4 step for step:

1. Caller identification — the `CallerContext` comes from the per-CPU
   current-task slot owned by `kernel/sched`; the dispatcher does not
   accept caller-supplied identity.
2. Capability check via `TaskCapabilities::has`.
3. Argument validation against the declared `AbiType`s and trailing-zero
   rule.
4. Dispatch through the `SyscallHandlers` trait. `kernel/core` provides
   the production implementation; tests substitute a mock.
5. Audit emission via the structured sink — exactly one record per
   security-relevant decision.

## Per-architecture entry stubs

The architecture-neutral dispatcher above is reached through a thin
per-target stub that marshals the platform's syscall-instruction
registers into a `RawArgs` tuple. Stage 3a (c6) landed the x86_64
stub; Stage 3b/3c/3d will add the remaining Tier-1 ports.

| Arch | Module | Instruction | Argument registers |
| --- | --- | --- | --- |
| x86_64 | `rustos_arch_x86_64::syscall_entry` | `syscall` / `sysretq` (`IA32_LSTAR`) | `%rdi`, `%rsi`, `%rdx`, `%r10`, `%r8`, `%r9` (number in `%rax`) |
| aarch64 | — (Stage 3b) | `svc #0` | `x0`..=`x5` (number in `x8`) |
| riscv64 | — (Stage 3c) | `ecall` | `a0`..=`a5` (number in `a7`) |
| wasm32 | — (Stage 3d) | host-imported function | first six i64 arguments |

The stub never duplicates the validation surface in
`kernel/syscall::table`: it builds a `[u64; SYSCALL_MAX_ARGS]` in
the canonical order (matching `RawArgs`'s `#[repr(transparent)]`
layout) and hands it to a binary-installed callback that forwards
to `Dispatcher::dispatch`. The full description of the x86_64 stub
— MSR programming, `SyscallTls` layout, and the naked entry
sequence — lives in
[the x86_64 platform page](../platform/x86_64.md#stage-3a-c6--syscallsysret-entry).

## Out of scope (Stage 2.7)

* New syscalls beyond what Stages 2.1–2.6 require. Adding a syscall
  takes a new `SyscallSpec` row, a new `SyscallHandlers` method, and
  an entry in this document — all in the same commit.
