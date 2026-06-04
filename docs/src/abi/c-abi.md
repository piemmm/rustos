# C development header (`abi-v1`)

RustOS is written entirely in Rust, but its kernel/user interface is a stable
binary contract (`AGENTS.md` §9) that programs written in other languages —
C in particular — must be able to call. Those programs need a C-language
*view* of the ABI rather than the Rust crate `rustos-abi`.

That view is the **C development header set**, shipped under the top-level
`include/` directory. It is split into one header per `lib/abi` module so a
developer can pull in exactly what they need, plus the umbrella
`rustos_abi.h` that `#include`s them all:

| Header | Declares |
|--------|----------|
| `include/rustos/rustos_abi.h` | umbrella: `ROS_ABI_VERSION` + `#include` of every module header |
| `include/rustos/rustos_error.h` | `ROS_E_*` — the stable error codes (matching the `Errno` discriminants) |
| `include/rustos/rustos_capability.h` | `ROS_CAP_*` and `ROS_CAPABILITY_ID_MAX` — the capability identifiers |
| `include/rustos/rustos_time.h` | `ros_time64_t` / `ros_duration64_t` and the `ROS_NANOS_PER_SEC` / `*_WIRE_LEN` constants |
| `include/rustos/rustos_random.h` | `ROS_RANDOM_FLAG_*` request flags and the `ROS_RANDOM_*_BYTES` request limits |
| `include/rustos/rustos_syscall.h` | `ROS_SYS_*`, `ROS_SYSCALL_MAX_ARGS`, and one prototype per syscall entry point |

Including the umbrella `rustos_abi.h` pulls in the whole surface; a program
that only needs, say, the time types can include `rustos_time.h` directly.

Growing the set to cover the rest of `lib/abi` (`appinfo`, `capability`
queries, `driver/*`, `input`, `ipc`, `manifest`, `rxe`, `sysinfo`,
`stdinfo`) is staged in `plans/CCOMPAT.md` (stage CC1).

## Generated, never hand-written

Every header is generated from the single source of truth in `lib/abi`, so
the set can never drift into a parallel, hand-maintained ABI definition
(`AGENTS.md` §2.2, §9). Every value — error-code numbers, capability ids,
syscall numbers, struct sizes, and constant values — is read straight from
`lib/abi`; only the C *spelling* (the `ROS_*` macro name, the
`ros_<name>_t` type name) lives in the generator, because Rust offers no
run-time reflection over a type's name. The generator lives in `tools/xtask`:

```text
cargo xtask c-header --write   # regenerate the include/rustos/ header set
cargo xtask c-header           # verify it is in sync (fails closed on drift)
```

`cargo xtask ci` runs the verifying form over the whole set, so a change to
`lib/abi` that forgets to regenerate a header fails the pipeline rather than
silently shipping a stale contract.

## Calling convention and symbol names

Each syscall is exposed to C under the symbol `ros_sys_<name>` — for
example `ros_sys_ipc_send`. The C-visible surface uses the short `ros_` /
`ROS_` prefix (symbols `ros_sys_*`, macros `ROS_*`, `#[repr(C)]` types
`ros_<name>_t`); it namespaces the surface so it survives C's single flat
symbol namespace, and it belongs only on the C boundary, never on internal
`lib/abi` Rust items (`AGENTS.md` §9). These names are frozen alongside the
rest of `abi-v1`.

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
| `Time64` / `Duration64` | `ros_time64_t` / `ros_duration64_t` (`{ int64_t secs; uint32_t nanos; }`) |

### Endianness and wire vs. in-memory form

The `#[repr(C)]` struct types (`ros_time64_t`, `ros_duration64_t`) mirror the
Rust in-memory layout: naturally aligned, so `sizeof(ros_time64_t) == 16`
(8-byte seconds + 4-byte nanos + 4 bytes of tail padding). The separate
`*_WIRE_LEN` macros give the **packed little-endian wire size** (12 bytes for
a time value) used when a value is serialised into a byte buffer. The
encode/decode helpers in `lib/abi` are little-endian on every target, so the
serialised byte image does not depend on host endianness.

The trap-issuing implementation of each `ros_sys_<name>` lives in the
user-space stub library (future work, gated on the per-architecture trap
layer). It pins each symbol with `#[export_name = "ros_sys_<name>"]` so
the Rust compiler does not mangle it; this header is the contract those
exports satisfy. The kernel still performs every capability and input check
on the far side of the trap — the C entry point is not a privileged bypass
(`AGENTS.md` §5.4).

## Stability

`abi-v1` is not frozen yet. Once it is released, the numbers, error codes,
type layout, and symbol names in this header become immutable; new behaviour
ships as `abi-v2` rather than mutating `abi-v1` in place (`AGENTS.md` §9).
