# C development header (`abi-v1`)

RustOS is written entirely in Rust, but its kernel/user interface is a stable
binary contract (`AGENTS.md` §9) that programs written in other languages —
C in particular — must be able to call. Those programs need a C-language
*view* of the ABI rather than the Rust crate `rustos-abi`.

That view is the **C development header**, shipped under the top-level
`include/` directory:

- `include/rustos/rustos_abi.h`

It declares, for `abi-v1`:

- `RUSTOS_ABI_VERSION` — the ABI version the header describes.
- `RUSTOS_E_*` — the stable error codes (matching the `Errno` discriminants).
- `RUSTOS_CAP_*` and `RUSTOS_CAPABILITY_ID_MAX` — the capability identifiers.
- `RUSTOS_SYS_*` and `RUSTOS_SYSCALL_MAX_ARGS` — the syscall numbers.
- One `extern "C"` prototype per syscall entry point.

## Generated, never hand-written

The header is generated from the single source of truth in `lib/abi`, so it
can never drift into a parallel, hand-maintained ABI definition (`AGENTS.md`
§2.2, §9). The generator lives in `tools/xtask`:

```text
cargo xtask c-header --write   # regenerate include/rustos/rustos_abi.h
cargo xtask c-header           # verify it is in sync (fails closed on drift)
```

`cargo xtask ci` runs the verifying form, so a change to `lib/abi` that
forgets to regenerate the header fails the pipeline rather than silently
shipping a stale contract.

## Calling convention and symbol names

Each syscall is exposed to C under the symbol `rustos_sys_<name>` — for
example `rustos_sys_ipc_send`. These names are namespaced and frozen
alongside the rest of `abi-v1`.

The Rust types map to fixed-width C types so the header means the same thing
on every Tier-1 target:

| ABI type      | C type      |
|---------------|-------------|
| 32-bit signed | `int32_t`   |
| 32-bit unsigned / capability id | `uint32_t` / `uint16_t` |
| 64-bit / handle / IPC endpoint | `uint64_t` |
| length        | `uintptr_t` |
| user pointer  | `void *`    |
| error code    | `int32_t`   |

The trap-issuing implementation of each `rustos_sys_<name>` lives in the
user-space stub library (future work, gated on the per-architecture trap
layer). It pins each symbol with `#[export_name = "rustos_sys_<name>"]` so
the Rust compiler does not mangle it; this header is the contract those
exports satisfy. The kernel still performs every capability and input check
on the far side of the trap — the C entry point is not a privileged bypass
(`AGENTS.md` §5.4).

## Stability

`abi-v1` is not frozen yet. Once it is released, the numbers, error codes,
type layout, and symbol names in this header become immutable; new behaviour
ships as `abi-v2` rather than mutating `abi-v1` in place (`AGENTS.md` §9).
