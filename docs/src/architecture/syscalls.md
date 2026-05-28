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

### Capability matrix

The dispatcher consults `kernel/sec`'s `TaskCapabilities::has` against
the syscall's `required_capability` before any handler runs. The matrix
is exhaustive — anything not listed below is ungated:

| Capability         | Syscalls gated by it |
| ------------------ | -------------------- |
| `CAP_USER_ADMIN`   | `cap_revoke`         |

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
