# C development header (`abi-v1`)

TAIRiX is written entirely in Rust, but its kernel/user interface is a stable
binary contract (`AGENTS.md` §9) that programs written in other languages —
C in particular — must be able to call. Those programs need a C-language
*view* of the ABI rather than the Rust crate `tairix-abi`.

That view is the **C development header set**, shipped under the top-level
`include/` directory. It is split into one header per `lib/abi` module so a
developer can pull in exactly what they need, plus the umbrella
`tairix_abi.h` that `#include`s them all:

| Header | Declares |
|--------|----------|
| `include/tairix/tairix_abi.h` | umbrella: `TAIRIX_ABI_VERSION` + `#include` of every module header |
| `include/tairix/tairix_error.h` | `TAIRIX_E_*` — the stable error codes (matching the `Errno` discriminants) |
| `include/tairix/tairix_capability.h` | `TAIRIX_CAP_*` and `TAIRIX_CAPABILITY_ID_MAX` — the capability identifiers |
| `include/tairix/tairix_time.h` | `tairix_time64_t` / `tairix_duration64_t` and the `TAIRIX_NANOS_PER_SEC` / `*_WIRE_LEN` constants |
| `include/tairix/tairix_random.h` | `TAIRIX_RANDOM_FLAG_*` request flags and the `TAIRIX_RANDOM_*_BYTES` request limits |
| `include/tairix/tairix_ipc.h` | `tairix_ipc_message_header_t` / `tairix_port_name_t` and the `TAIRIX_IPC_*` / `TAIRIX_PORT_NAME_*` constants |
| `include/tairix/tairix_stdinfo.h` | `TAIRIX_STDINFO_FD`, the `TAIRIX_STDINFO_VERSION_*` framing tags, and the `TAIRIX_STDINFO_KIND_*` / `TAIRIX_STDINFO_SEVERITY_*` discriminants |
| `include/tairix/tairix_manifest.h` | `tairix_manifest_header_t` and the `TAIRIX_MANIFEST_*` / `TAIRIX_SYSCALL_TABLE_HASH_LEN` constants |
| `include/tairix/tairix_input.h` | the pointer/keyboard record magics and wire sizes, the `TAIRIX_INPUT_KIND_*` / `TAIRIX_INPUT_BUTTON_NONE` / `TAIRIX_KEY_CLASS_*` / `TAIRIX_MOD_*` codes, and the `TAIRIX_POINTER_BUTTON_*` / `TAIRIX_KEY_*` discriminants |
| `include/tairix/tairix_appinfo.h` | `tairix_appinfo_header_t` and the `TAIRIX_APPINFO_*` / `TAIRIX_BUNDLE_*` / `TAIRIX_MIME_*` constants, `TAIRIX_SYSTEM_LIBRARIES_DIR`, the `TAIRIX_BUNDLE_ENTRY_*` names, and the `TAIRIX_LIBRARY_SCOPE_*` discriminants |
| `include/tairix/tairix_rxe.h` | `tairix_load_header_t` and the `TAIRIX_LOAD_MAGIC` / `TAIRIX_RXE_PAGE_SIZE` / `TAIRIX_LOAD_MAX_SEGMENTS` / `TAIRIX_LOAD_MAX_NEEDED` / `TAIRIX_LIBREF_MAX` / `TAIRIX_LOAD_FLAG_PIE` / `TAIRIX_SEG_FLAG_*` / `*_WIRE_LEN` constants and the `TAIRIX_RXE_PERMISSION_*` discriminants |
| `include/tairix/tairix_process.h` | `tairix_process_start_header_t` / `tairix_string_slot_t` — the process startup vector handed to a freshly spawned program — and the `TAIRIX_PROCESS_START_MAGIC` / `TAIRIX_PROCESS_START_MAX_*` / `*_WIRE_LEN` constants |
| `include/tairix/tairix_sysinfo.h` | the eight System Information wire types (`tairix_sysinfo_request_header_t`, `tairix_process_list_request_t`, `tairix_process_record_t`, `tairix_kernel_memory_stats_t`, `tairix_uptime_t`, `tairix_system_identity_t`, `tairix_mount_list_request_t`, `tairix_mount_record_t`) and the `TAIRIX_SYSINFO_*` framing / query-id / registry constants, the `TAIRIX_PROCESS_STATE_*` discriminants, the `TAIRIX_MOUNT_*` mount-availability and `TAIRIX_MOUNT_MEDIUM_*` storage-medium discriminants (`TAIRIX_MOUNT_MEDIUM_UNKNOWN` is what a decoder yields for a medium it does not recognise), the `TAIRIX_*_MAX` / `TAIRIX_*_LEN` buffer caps, and the `*_WIRE_LEN` sizes |
| `include/tairix/tairix_driver.h` | the driver-class ABI: `tairix_driver_manifest_t` + `TAIRIX_DRIVER_MANIFEST_*` / `TAIRIX_DRIVER_SIGNER_PUBKEY_LEN` / `TAIRIX_DRIVER_SIGNATURE_LEN` constants, the `TAIRIX_DRIVER_KIND_*` / `TAIRIX_BUFFER_CLASS_*` discriminants, the `TAIRIX_DRIVER_ERROR_*` codes, the `TAIRIX_DRIVER_HANDLE_NONE` sentinel; **and the driver-class POD types**: the storage/bus/display/filesystem/input/net structs (`tairix_block_geometry_t`, `tairix_discard_capability_t`, `tairix_health_snapshot_t`, `tairix_bus_device_t`, `tairix_display_mode_t`, `tairix_accel_caps_t`, `tairix_node_info_t`, `tairix_dir_entry_t`, `tairix_node_times_t`, `tairix_input_event_t`, `tairix_mac_address_t`), the `TAIRIX_VIRTIO_PCI_*` / `TAIRIX_MAC_ADDRESS_LEN` / `TAIRIX_MOUNT_FLAG_*` / `TAIRIX_NODE_ID_NONE` constants, and the `TAIRIX_DISPLAY_FORMAT_*` / `TAIRIX_NODE_KIND_*` / `TAIRIX_INPUT_EVENT_KIND_*` discriminants |
| `include/tairix/tairix_syscall.h` | `TAIRIX_SYS_*`, `TAIRIX_SYSCALL_MAX_ARGS`, and one prototype per syscall entry point |

Including the umbrella `tairix_abi.h` pulls in the whole surface; a program
that only needs, say, the time types can include `tairix_time.h` directly.

The header set now covers the whole `lib/abi` public `#[repr(C)]` type
surface; a completeness test in the generator pins every such type's
size/align and asserts it has a C `typedef`, so a new type cannot silently
escape the C view. The `tairix_sys_*` trap-stub runtime (`lib/abi-sys`), the
crt0 startup object (`lib/crt0`), and the loader/bundle integration (below)
have all landed; the remaining C-ABI work (an end-to-end C program + its
fuzzing) is staged in `plans/CCOMPAT.md` (stage CC5).

A handful of `driver/*` items are deliberately **not** in the header: the
Rust-only error enums (`WindowError`, `MmioMapError`) and the opaque
arch-built `MsiMessage` carry no `#[repr(C)]`/explicit-primitive layout and
never cross the C boundary (errors collapse to `TAIRIX_DRIVER_ERROR_*`); the
in-process policy records (`NodeSecurity`, `SecurityAcl`, `SecuritySubject`)
and runtime objects (`RegisterWindow`, `DmaSlab`, `PoolId`) are not wire
types; and the driver-host traits have no C form — all skipped for the same
reason as the syscall traits (`AGENTS.md` §2.3).

## Generated, never hand-written

Every header is generated from the single source of truth in `lib/abi`, so
the set can never drift into a parallel, hand-maintained ABI definition
(`AGENTS.md` §2.2, §9). Every value — error-code numbers, capability ids,
syscall numbers, struct sizes, and constant values — is read straight from
`lib/abi`; only the C *spelling* (the `TAIRIX_*` macro name, the
`tairix_<name>_t` type name) lives in the generator, because Rust offers no
run-time reflection over a type's name. The generator lives in `tools/xtask`:

```text
cargo xtask c-header --write   # regenerate the include/tairix/ header set
cargo xtask c-header           # verify it is in sync (fails closed on drift)
```

`cargo xtask ci` runs the verifying form over the whole set, so a change to
`lib/abi` that forgets to regenerate a header fails the pipeline rather than
silently shipping a stale contract.

## Calling convention and symbol names

Each syscall is exposed to C under the symbol `tairix_sys_<name>` — for
example `tairix_sys_ipc_send`. The C-visible surface uses the short `tairix_` /
`TAIRIX_` prefix (symbols `tairix_sys_*`, macros `TAIRIX_*`, `#[repr(C)]` types
`tairix_<name>_t`); it namespaces the surface so it survives C's single flat
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
| `Time64` / `Duration64` | `tairix_time64_t` / `tairix_duration64_t` (`{ int64_t secs; uint32_t nanos; }`) |
| `IpcMessageHeader` / `PortName` | `tairix_ipc_message_header_t` / `tairix_port_name_t` (mirroring the `#[repr(C)]` layout) |

### Endianness and wire vs. in-memory form

The `#[repr(C)]` struct types (`tairix_time64_t`, `tairix_duration64_t`,
`tairix_ipc_message_header_t`, `tairix_port_name_t`) mirror the Rust in-memory
layout: naturally aligned, so `sizeof(tairix_time64_t) == 16` (8-byte seconds +
4-byte nanos + 4 bytes of tail padding). The separate `*_WIRE_LEN` macros
give the **packed little-endian wire size** (12 bytes for a time value, 32
bytes for an IPC message header or a port name) used when a value is
serialised into a byte buffer. The encode/decode helpers in `lib/abi` are
little-endian on every target, so the serialised byte image does not depend
on host endianness.

The trap-issuing implementation of each `tairix_sys_<name>` lives in the
user-space stub library `lib/abi-sys` (`tairix-abi-sys`), the curated
`/System/Libraries/` *System runtime / C ABI* class (`AGENTS.md` §16.4). Each
stub marshals its typed arguments into the syscall registers and issues the
per-architecture trap — `syscall` (x86_64, number in `rax`, arguments
`rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`, result `rax`), `svc #0` (AArch64, `x8` /
`x0`–`x5` / `x0`), or `ecall` (RISC-V, `a7` / `a0`–`a5` / `a0`). The trap
*instruction* itself — the §1 assembly carve-out, compiled in only for the
three native targets — lives once in `lib/abi-trap` (`tairix-abi-trap`), shared
with the pure-Rust userland runtime (`lib/rt`) so it is not duplicated
(`AGENTS.md` §2.2); `lib/abi-sys` only marshals and hands off to it. The stub
pins
each symbol with `#[export_name = "tairix_sys_<name>"]` so the Rust compiler does
not mangle it; this header is the contract those exports satisfy. Every entry
point is panic-free (an unwind across `extern "C"` is undefined behaviour,
`AGENTS.md` §2.9). The kernel still performs every capability and input check
on the far side of the trap — the C entry point is not a privileged bypass
(`AGENTS.md` §5.4 / `plans/CCOMPAT.md` §4), and a C program reaches no syscall
a Rust program could not.

### Verifying the trap end-to-end

The marshalling each `tairix_sys_*` stub performs is host-tested behind an
injectable trap seam (`lib/abi-trap`'s `host-seam`), but the trap *instruction* itself is
exercised under QEMU. There is one CC2 round-trip test per native target
(`cargo xtask test --qemu`); each installs a syscall dispatch callback, issues
the `tairix_sys_cap_query` stub so the **real** trap instruction runs, and the
callback asserts the kernel-observed `(number, arguments)` are exactly what the
stub should have placed in the registers before exiting QEMU — proving
`lib/abi-sys` and the kernel agree on the syscall register layout:

| Target | Test crate | How the trap is raised |
|--------|------------|------------------------|
| `x86_64-unknown-none` | `tairix-test-abi-sys-syscall-qemu` | boots the production kernel, then issues `syscall` from ring 0 — which enters the `IA32_LSTAR` entry stub identically to a ring-3 call |
| `riscv64gc-unknown-none-elf` | `tairix-test-abi-sys-syscall-qemu-riscv64` | stands up a minimal **U-mode** context (identity-mapped kernel + a U-bit alias of the stub page + a user stack) and `sret`s to U-mode so the stub's `ecall` is a genuine environment-call-from-U |
| `aarch64-unknown-none` | `tairix-test-abi-sys-syscall-qemu-aarch64` | stands up a minimal **EL0** context (identity-mapped kernel + an EL0-executable alias of the stub page + an EL0 stack) and `eret`s to EL0 so the stub's `svc` is a genuine lower-EL synchronous exception |

Unlike x86_64's `syscall` (which traps identically from any privilege level),
a riscv64 `ecall` from S-mode / an aarch64 `svc` from EL1 is **not** the
user-syscall path — the kernel routes only `ecall`-from-U / `svc`-from-EL0 to
the dispatch callback. The riscv64/aarch64 tests therefore raise the trap from
a real lower-privilege context built with the Stage-3 paging primitives; the
aarch64 EL0 `svc` dispatch wiring (`kernel/arch/aarch64/src/exceptions.rs`) is
the analogue of riscv64's already-wired `ecall` path. These round-trips are
**not** part of the host-only `cargo xtask ci` gate; they run under
`cargo xtask test --qemu`.

## Process startup vector

When the loader drops into a freshly spawned program it materialises a single
contiguous **startup-vector block** in the new address space and hands the
program's entry trampoline (crt0) a pointer to it. `tairix_process.h` declares
that block's wire format — the one definition the kernel (which builds it) and
crt0 (which parses it) share (`AGENTS.md` §2.2):

- `tairix_process_start_header_t` is the fixed prefix: the `TAIRIX_PROCESS_START_MAGIC`
  magic, the ABI version, the argument and environment counts, the block's
  `total_len`, and a per-process random seed for the §19.2 stack canary.
- It is followed by `arg_count + env_count` `tairix_string_slot_t` records
  (arguments first, then environment), each an `(offset, len)` reference into
  the trailing string region.
- The block is **position-independent** — strings are referenced by offset from
  the block base, never an absolute pointer — so it works wherever the loader
  places it in a PIE address space. The strings carry no NUL terminator; crt0
  copies and NUL-terminates them when it builds the C `argv` / `envp` vectors.

The block is untrusted input: `tairix_abi::process::ProcessStart::parse`
bounds-checks every field against the frozen `TAIRIX_PROCESS_START_MAX_*` limits
and the declared `total_len`, rejects an embedded NUL, and fails closed rather
than ever indexing out of range (`AGENTS.md` §2.9, §19.5/§19.6).

## Building and linking a C program (crt0)

> This whole C-ABI class (`lib/crt0` + `lib/abi-sys`) exists **solely** for
> programs **not** written in Rust (`AGENTS.md` §1, §16.4). TAIRiX's own
> first-party programs are Rust and link the pure-Rust userland runtime
> `lib/rt` (`tairix-rt`) instead — its own `_start`, stack canary, panic
> handler, and idiomatic syscall wrappers — over the same shared
> `lib/abi-trap` trap. `lib/rt` validates the same startup vector and
> exposes the arguments through `tairix_rt::arg` / `arg_count` (no C
> `argv` is built). A TAIRiX program never routes through this C path.

The startup object that consumes the startup vector is `lib/crt0`
(`tairix-crt0`) — the crt0 half of the curated `/System/Libraries/` *System
runtime / C ABI* class (`AGENTS.md` §16.4), alongside the `lib/abi-sys` syscall
stubs. A non-Rust program links it as its startup object; it provides the
program's `_start` entry symbol and does the minimum a freestanding C program
needs before `main`:

1. The kernel transfers control to `_start` with the startup-vector base in the
   platform's first integer-argument register (`rdi` on x86_64, `x0` on
   aarch64, `a0` on riscv64) and a valid stack. `_start` (the §1 assembly
   carve-out) aligns the stack to the platform C ABI and carves a bounded
   scratch region from it.
2. `build_c_runtime` validates the block (via `ProcessStart::parse`) and lays
   out the C `argv` / `envp` in the scratch: each NUL-free string is copied and
   NUL-terminated, and the two NULL-terminated pointer arrays are built ahead of
   them. Nothing is allocated; an oversized vector fails closed rather than
   truncating.
3. crt0 seeds the compiler's `__stack_chk_guard` with the block's per-process
   random canary (`AGENTS.md` §19.2), calls
   `int main(int argc, char **argv, char **envp)`, and routes its return value
   through `tairix_sys_exit`. A startup-vector validation failure exits with a
   reserved non-zero code instead.

The `rxe` hardening invariants a hosted image must satisfy — position-independence
(PIE), `R`/`RX`/`RW`-only segments (no `RWX`), and the syscall-hash CFI tag
(`AGENTS.md` §9/§19.2) — are enforced at load time by the single point
`tairix_abi::rxe::LoadImage::parse`; a non-conforming image is refused, not
patched. The marshalling core is host-tested; the per-target `_start`
trampoline is exercised under QEMU (`plans/CCOMPAT.md` stage CC3).

## Bundles and the dynamic-loader policy

A C program ships exactly like any other TAIRiX application: a signed
`/Apps/<Name>.app/` bundle whose `Run` binary is a PIE `rxe` image
(`AGENTS.md` §16.5). The application-bundle loader (`userland/system/appmgr`)
treats a C bundle no differently from a Rust one — the whole pipeline is
language-agnostic:

1. The `AppInfo` manifest is decoded, its ABI version and syscall-table hash
   matched against the kernel's, its signature verified, and its content hash
   checked. The granted capability set is the manifest request **intersected**
   with the launching user's grants — a hosted C program gains no ambient
   authority (`AGENTS.md` §4, §5.2; `plans/CCOMPAT.md` §4).
2. The `Run` image is validated through `tairix_abi::rxe::LoadImage::parse`,
   which enforces the §19.2 hardening invariants — PIE, `R`/`RX`/`RW`-only
   segments (no `RWX`), and the syscall-hash **CFI tag** — on a C binary
   identically to a Rust one. A mismatched CFI tag is a load-time refusal.
3. Each shared library the `Run` image declares it needs (an `rxe`
   needed-library record, the analogue of an ELF `DT_NEEDED`) is resolved
   under the §16.4 dynamic-loader policy: it must lie inside the bundle's own
   `Libraries/` directory or the curated `/System/Libraries/`
   (`TAIRIX_SYSTEM_LIBRARIES_DIR`). This is where the curated *System runtime /
   C ABI* library (`lib/abi-sys` + `lib/crt0`, which export `tairix_sys_*` and
   `_start`) is bound. A reference to any other path — or one containing a
   `..` component — fails closed, and nothing is launched.

The needed-library list is carried in the `rxe` load header
(`tairix_load_header_t.needed_count`, bounded by `TAIRIX_LOAD_MAX_NEEDED`) followed
by that many `TAIRIX_NEEDED_LIBRARY_WIRE_LEN`-byte records after the segment
table; each record is a NUL-free path no longer than `TAIRIX_LIBREF_MAX` bytes.
Like every other `rxe` field it is untrusted input and is bounds-checked,
fail-closed, and fuzzed (`AGENTS.md` §19.5/§19.6).

## Stability

`abi-v1` is not frozen yet. Once it is released, the numbers, error codes,
type layout, and symbol names in this header become immutable; new behaviour
ships as `abi-v2` rather than mutating `abi-v1` in place (`AGENTS.md` §9).
