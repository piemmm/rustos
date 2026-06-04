# RustOS development headers

C-language development headers for the RustOS ABI. These let a program
written in a language other than Rust (C, C++, anything with a C FFI) call
the RustOS kernel/user interface.

The surface is split into one header per `lib/abi` module so a developer can
pull in exactly what they need, plus an umbrella that includes them all:

- `rustos/rustos_abi.h` — umbrella header: the ABI version plus an `#include`
  of every module header below.
- `rustos/rustos_error.h` — the stable error codes (`ROS_E_*`).
- `rustos/rustos_capability.h` — the capability identifiers (`ROS_CAP_*`).
- `rustos/rustos_time.h` — the 64-bit-native time types (`ros_time64_t`,
  `ros_duration64_t`) and their constants.
- `rustos/rustos_random.h` — the random-request flags (`ROS_RANDOM_FLAG_*`)
  and the per-request byte limits (`ROS_RANDOM_*_BYTES`).
- `rustos/rustos_ipc.h` — the IPC message header and port-name wire types
  (`ros_ipc_message_header_t`, `ros_port_name_t`) and their constants
  (`ROS_IPC_*`, `ROS_PORT_NAME_*`).
- `rustos/rustos_stdinfo.h` — the Standard Information Stream ABI: the
  reserved file descriptor (`ROS_STDINFO_FD`), the framing version tags
  (`ROS_STDINFO_VERSION_*`), and the record-kind and severity discriminants
  (`ROS_STDINFO_KIND_*`, `ROS_STDINFO_SEVERITY_*`).
- `rustos/rustos_manifest.h` — the signed `rxe` manifest header
  (`ros_manifest_header_t`) and its constants (`ROS_MANIFEST_*`,
  `ROS_SYSCALL_TABLE_HASH_LEN`).
- `rustos/rustos_syscall.h` — the syscall numbers (`ROS_SYS_*`) and a
  prototype for each syscall entry point.

Growing this set to the rest of `lib/abi` is staged in `plans/CCOMPAT.md`
(stage CC1).

## These files are generated

Every header here is **generated** from the single source of truth in
`lib/abi` (`AGENTS.md` §2.2, §9). Do not edit them by hand. To regenerate
after changing `lib/abi`:

```
cargo xtask c-header --write
```

`cargo xtask ci` runs `cargo xtask c-header` (no `--write`), which fails
closed if a committed header has drifted from `lib/abi`.

## Calling convention

Each syscall is exported by the user-space stub library under the symbol
`ros_sys_<name>` (for example `ros_sys_ipc_send`). The stub library
implements each entry with an explicit `#[export_name = "ros_sys_<name>"]`
so the Rust compiler does not mangle the symbol. The short `ros_` / `ROS_`
prefix namespaces the C-visible surface so it survives C's single flat symbol
namespace (`AGENTS.md` §9). Link against that library and include
`rustos/rustos_abi.h` to call the kernel.

`abi-v1` is not frozen yet; once it is released its layout, numbers, and
symbol names become immutable and new behaviour ships as `abi-v2`.
