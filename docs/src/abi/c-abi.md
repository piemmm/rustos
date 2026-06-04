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
| `include/rustos/rustos_ipc.h` | `ros_ipc_message_header_t` / `ros_port_name_t` and the `ROS_IPC_*` / `ROS_PORT_NAME_*` constants |
| `include/rustos/rustos_stdinfo.h` | `ROS_STDINFO_FD`, the `ROS_STDINFO_VERSION_*` framing tags, and the `ROS_STDINFO_KIND_*` / `ROS_STDINFO_SEVERITY_*` discriminants |
| `include/rustos/rustos_manifest.h` | `ros_manifest_header_t` and the `ROS_MANIFEST_*` / `ROS_SYSCALL_TABLE_HASH_LEN` constants |
| `include/rustos/rustos_input.h` | the pointer/keyboard record magics and wire sizes, the `ROS_INPUT_KIND_*` / `ROS_INPUT_BUTTON_NONE` / `ROS_KEY_CLASS_*` / `ROS_MOD_*` codes, and the `ROS_POINTER_BUTTON_*` / `ROS_KEY_*` discriminants |
| `include/rustos/rustos_appinfo.h` | `ros_appinfo_header_t` and the `ROS_APPINFO_*` / `ROS_BUNDLE_*` / `ROS_MIME_*` constants, `ROS_SYSTEM_LIBRARIES_DIR`, the `ROS_BUNDLE_ENTRY_*` names, and the `ROS_LIBRARY_SCOPE_*` discriminants |
| `include/rustos/rustos_rxe.h` | `ros_load_header_t` and the `ROS_LOAD_MAGIC` / `ROS_RXE_PAGE_SIZE` / `ROS_LOAD_MAX_SEGMENTS` / `ROS_LOAD_FLAG_PIE` / `ROS_SEG_FLAG_*` / `*_WIRE_LEN` constants and the `ROS_RXE_PERMISSION_*` discriminants |
| `include/rustos/rustos_process.h` | `ros_process_start_header_t` / `ros_string_slot_t` — the process startup vector handed to a freshly spawned program — and the `ROS_PROCESS_START_MAGIC` / `ROS_PROCESS_START_MAX_*` / `*_WIRE_LEN` constants |
| `include/rustos/rustos_sysinfo.h` | the eight System Information wire types (`ros_sysinfo_request_header_t`, `ros_process_list_request_t`, `ros_process_record_t`, `ros_kernel_memory_stats_t`, `ros_uptime_t`, `ros_system_identity_t`, `ros_mount_list_request_t`, `ros_mount_record_t`) and the `ROS_SYSINFO_*` framing / query-id / registry constants, the `ROS_PROCESS_STATE_*` discriminants, the `ROS_*_MAX` / `ROS_*_LEN` buffer caps, and the `*_WIRE_LEN` sizes |
| `include/rustos/rustos_driver.h` | the driver-class ABI: `ros_driver_manifest_t` + `ROS_DRIVER_MANIFEST_*` / `ROS_DRIVER_SIGNER_PUBKEY_LEN` / `ROS_DRIVER_SIGNATURE_LEN` constants, the `ROS_DRIVER_KIND_*` / `ROS_BUFFER_CLASS_*` discriminants, the `ROS_DRIVER_ERROR_*` codes, the `ROS_DRIVER_HANDLE_NONE` sentinel; **and the driver-class POD types**: the storage/bus/display/filesystem/input/net structs (`ros_block_geometry_t`, `ros_discard_capability_t`, `ros_health_snapshot_t`, `ros_bus_device_t`, `ros_display_mode_t`, `ros_accel_caps_t`, `ros_node_info_t`, `ros_dir_entry_t`, `ros_node_times_t`, `ros_input_event_t`, `ros_mac_address_t`), the `ROS_VIRTIO_PCI_*` / `ROS_MAC_ADDRESS_LEN` / `ROS_MOUNT_FLAG_*` / `ROS_NODE_ID_NONE` constants, and the `ROS_DISPLAY_FORMAT_*` / `ROS_NODE_KIND_*` / `ROS_INPUT_EVENT_KIND_*` discriminants |
| `include/rustos/rustos_syscall.h` | `ROS_SYS_*`, `ROS_SYSCALL_MAX_ARGS`, and one prototype per syscall entry point |

Including the umbrella `rustos_abi.h` pulls in the whole surface; a program
that only needs, say, the time types can include `rustos_time.h` directly.

The header set now covers the whole `lib/abi` public `#[repr(C)]` type
surface; a completeness test in the generator pins every such type's
size/align and asserts it has a C `typedef`, so a new type cannot silently
escape the C view. The `ros_sys_*` trap-stub runtime that backs the syscall
prototypes has landed in `lib/abi-sys` (see below); the remaining C-ABI work
(crt0 and the loader/bundle integration) is staged in `plans/CCOMPAT.md`
(stages CC3+).

A handful of `driver/*` items are deliberately **not** in the header: the
Rust-only error enums (`WindowError`, `MmioMapError`) and the opaque
arch-built `MsiMessage` carry no `#[repr(C)]`/explicit-primitive layout and
never cross the C boundary (errors collapse to `ROS_DRIVER_ERROR_*`); the
in-process policy records (`NodeSecurity`, `SecurityAcl`, `SecuritySubject`)
and runtime objects (`RegisterWindow`, `DmaSlab`, `PoolId`) are not wire
types; and the driver-host traits have no C form — all skipped for the same
reason as the syscall traits (`AGENTS.md` §2.3).

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
| `IpcMessageHeader` / `PortName` | `ros_ipc_message_header_t` / `ros_port_name_t` (mirroring the `#[repr(C)]` layout) |

### Endianness and wire vs. in-memory form

The `#[repr(C)]` struct types (`ros_time64_t`, `ros_duration64_t`,
`ros_ipc_message_header_t`, `ros_port_name_t`) mirror the Rust in-memory
layout: naturally aligned, so `sizeof(ros_time64_t) == 16` (8-byte seconds +
4-byte nanos + 4 bytes of tail padding). The separate `*_WIRE_LEN` macros
give the **packed little-endian wire size** (12 bytes for a time value, 32
bytes for an IPC message header or a port name) used when a value is
serialised into a byte buffer. The encode/decode helpers in `lib/abi` are
little-endian on every target, so the serialised byte image does not depend
on host endianness.

The trap-issuing implementation of each `ros_sys_<name>` lives in the
user-space stub library `lib/abi-sys` (`rustos-abi-sys`), the curated
`/System/Libraries/` *System runtime / C ABI* class (`AGENTS.md` §16.4). Each
stub marshals its typed arguments into the syscall registers and issues the
per-architecture trap — `syscall` (x86_64, number in `rax`, arguments
`rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`, result `rax`), `svc #0` (AArch64, `x8` /
`x0`–`x5` / `x0`), or `ecall` (RISC-V, `a7` / `a0`–`a5` / `a0`) — the §1
assembly carve-out, compiled in only for the three native targets. It pins
each symbol with `#[export_name = "ros_sys_<name>"]` so the Rust compiler does
not mangle it; this header is the contract those exports satisfy. Every entry
point is panic-free (an unwind across `extern "C"` is undefined behaviour,
`AGENTS.md` §2.9). The kernel still performs every capability and input check
on the far side of the trap — the C entry point is not a privileged bypass
(`AGENTS.md` §5.4 / `plans/CCOMPAT.md` §4), and a C program reaches no syscall
a Rust program could not.

### Verifying the trap end-to-end

The marshalling each `ros_sys_*` stub performs is host-tested behind an
injectable trap seam (`lib/abi-sys`), but the trap *instruction* itself is
exercised under QEMU. There is one CC2 round-trip test per native target
(`cargo xtask test --qemu`); each installs a syscall dispatch callback, issues
the `ros_sys_cap_query` stub so the **real** trap instruction runs, and the
callback asserts the kernel-observed `(number, arguments)` are exactly what the
stub should have placed in the registers before exiting QEMU — proving
`lib/abi-sys` and the kernel agree on the syscall register layout:

| Target | Test crate | How the trap is raised |
|--------|------------|------------------------|
| `x86_64-unknown-none` | `rustos-test-abi-sys-syscall-qemu` | boots the production kernel, then issues `syscall` from ring 0 — which enters the `IA32_LSTAR` entry stub identically to a ring-3 call |
| `riscv64gc-unknown-none-elf` | `rustos-test-abi-sys-syscall-qemu-riscv64` | stands up a minimal **U-mode** context (identity-mapped kernel + a U-bit alias of the stub page + a user stack) and `sret`s to U-mode so the stub's `ecall` is a genuine environment-call-from-U |
| `aarch64-unknown-none` | `rustos-test-abi-sys-syscall-qemu-aarch64` | stands up a minimal **EL0** context (identity-mapped kernel + an EL0-executable alias of the stub page + an EL0 stack) and `eret`s to EL0 so the stub's `svc` is a genuine lower-EL synchronous exception |

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
program's entry trampoline (crt0) a pointer to it. `rustos_process.h` declares
that block's wire format — the one definition the kernel (which builds it) and
crt0 (which parses it) share (`AGENTS.md` §2.2):

- `ros_process_start_header_t` is the fixed prefix: the `ROS_PROCESS_START_MAGIC`
  magic, the ABI version, the argument and environment counts, the block's
  `total_len`, and a per-process random seed for the §19.2 stack canary.
- It is followed by `arg_count + env_count` `ros_string_slot_t` records
  (arguments first, then environment), each an `(offset, len)` reference into
  the trailing string region.
- The block is **position-independent** — strings are referenced by offset from
  the block base, never an absolute pointer — so it works wherever the loader
  places it in a PIE address space. The strings carry no NUL terminator; crt0
  copies and NUL-terminates them when it builds the C `argv` / `envp` vectors.

The block is untrusted input: `rustos_abi::process::ProcessStart::parse`
bounds-checks every field against the frozen `ROS_PROCESS_START_MAX_*` limits
and the declared `total_len`, rejects an embedded NUL, and fails closed rather
than ever indexing out of range (`AGENTS.md` §2.9, §19.5/§19.6). The crt0
runtime that consumes this block is staged in `plans/CCOMPAT.md` (stage CC3).

## Stability

`abi-v1` is not frozen yet. Once it is released, the numbers, error codes,
type layout, and symbol names in this header become immutable; new behaviour
ships as `abi-v2` rather than mutating `abi-v1` in place (`AGENTS.md` §9).
